//! The Linear side of a pass: Linear events, run reads (close, detach,
//! issue edits, relay), intake and claim, and routing.

use std::time::Duration;

use anyhow::{Context, Result};
use jiff::Timestamp;

use super::reconcile::{
    Deps, Reconciler, RoutingDone, blocking, inbox_item, thought, update_run, update_worker,
};
use crate::herdr::{Herdr, Snapshot, WorkspaceId};
use crate::linear::api::{Activity, Content, IssueDetail, IssueRef, Prompt, RunUpdate};
use crate::linear::task::{Decline, Levels, LinearEvent, LinearLevel};
use crate::outbox::{self, Op, StateTarget};
use crate::run::{
    AgentRecord, AgentStatus, Interrupt, IssueStateCursor, ResumeDecision, ResumeRequest, Run,
    RunRecord, Status, WaitReason,
};
use crate::{coordinator, files, inbox, routing, worker};

/// Whether a prompt created at `created` is newer than the cursor. Both are
/// RFC 3339; text that does not parse is compared as text.
fn after_cursor(created: &str, cursor: &str) -> bool {
    match (created.parse::<Timestamp>(), cursor.parse::<Timestamp>()) {
        (Ok(created), Ok(cursor)) => created > cursor,
        _ => cursor.is_empty() || created > cursor,
    }
}

fn at_cursor(created: &str, cursor: &str) -> bool {
    match (created.parse::<Timestamp>(), cursor.parse::<Timestamp>()) {
        (Ok(created), Ok(cursor)) => created == cursor,
        _ => created == cursor,
    }
}

fn prompt_is_new(record: &RunRecord, id: &str, created: &str) -> bool {
    if record
        .pending_prompt_ids
        .iter()
        .any(|pending| pending == id)
        || after_cursor(created, &record.prompt_cursor)
    {
        return true;
    }
    at_cursor(created, &record.prompt_cursor)
        && record
            .prompt_cursor_ids
            .as_ref()
            .is_some_and(|ids| !ids.iter().any(|seen| seen == id))
}

fn advance_prompt_cursor(record: &mut RunRecord, created: &str, id: &str) {
    if after_cursor(created, &record.prompt_cursor) {
        record.prompt_cursor = created.to_string();
        record.prompt_cursor_ids = Some(vec![id.to_string()]);
    } else if at_cursor(created, &record.prompt_cursor)
        && let Some(ids) = &mut record.prompt_cursor_ids
        && !ids.iter().any(|seen| seen == id)
    {
        ids.push(id.to_string());
    }
}

fn remove_pending_prompt(record: &mut RunRecord, id: &str) {
    record.pending_prompt_ids.retain(|pending| pending != id);
}

/// What intake does with an issue's latest delegation.
enum Delegation {
    /// Someone the team allows to delegate delegated it.
    Allowed,
    /// Someone else, or no person, and the app's session was answered after
    /// the delegation: it was declined already.
    Answered,
    /// Someone else, or no person (automation or an agent): the decline for
    /// the app's session, or `None` when there is no session to answer.
    Declined(Option<Decline>),
}

fn delegation(key: &str, issue: &IssueRef, delegators: &[String]) -> Delegation {
    let delegator = issue.delegator.as_ref();
    if delegator
        .and_then(|d| d.user.as_ref())
        .is_some_and(|user| delegators.contains(&user.id))
    {
        return Delegation::Allowed;
    }
    let Some(session) = &issue.session else {
        return Delegation::Declined(None);
    };
    let at = |text: &str| text.parse::<Timestamp>().ok();
    let responded = session.responded_at.as_deref().and_then(at);
    let delegated = delegator.and_then(|d| at(&d.at));
    if responded.is_some() && responded >= delegated {
        Delegation::Answered
    } else {
        Delegation::Declined(Some(Decline {
            key: key.to_string(),
            session_id: session.id.clone(),
        }))
    }
}

/// The session of the issue's delegation, unless it has ended.
fn open_session(issue: &IssueRef) -> Option<String> {
    issue
        .session
        .as_ref()
        .filter(|session| session.status != "complete")
        .map(|session| session.id.clone())
}

/// Logs and notifies a delegation the ticker does not take, with the
/// delegator's ID for the team's `allowed_delegator_ids`.
async fn tell_declined<H: Herdr>(d: &Deps<'_, H>, key: &str, issue: &IssueRef) {
    let why = match issue.delegator.as_ref().map(|d| &d.user) {
        None => "Linear does not tell who delegated it".to_string(),
        Some(None) => "no person delegated it (automation or an agent did)".to_string(),
        Some(Some(user)) => format!(
            "delegated by {} ({}), who is not in allowed_delegator_ids of team {}",
            user.name, user.id, issue.team
        ),
    };
    d.log.line(&format!("{key}: not picked up: {why}"));
    d.notify(&format!("{key} not picked up"), &format!("{why}."))
        .await;
}

fn run_of<H>(d: &Deps<'_, H>, issue_id: &str) -> Option<(Run, RunRecord)> {
    Run::list(&d.ctx.runs_dir()).into_iter().find_map(|run| {
        let record = run.record().ok()?;
        (record.issue_id == issue_id).then_some((run, record))
    })
}

impl Reconciler {
    pub(super) async fn apply_event<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        levels: &Levels,
        snap: Option<&Snapshot>,
        event: LinearEvent,
        now: Timestamp,
    ) {
        let result = match event {
            LinearEvent::SessionFound {
                issue_id,
                session_id,
            } => match run_of(d, &issue_id) {
                Some((run, record)) if record.session_id.is_empty() => update_run(&run, move |r| {
                    if r.session_id.is_empty() {
                        r.session_id = session_id;
                    }
                })
                .await
                .map(|_| ()),
                _ => Ok(()),
            },
            LinearEvent::ActivitySent { issue_id, at } => match run_of(d, &issue_id) {
                Some((run, _)) => {
                    if self
                        .heartbeats
                        .get(&run.key)
                        .is_some_and(|queued| *queued <= at)
                    {
                        self.heartbeats.remove(&run.key);
                    }
                    update_run(&run, move |r| r.last_activity = at.to_string())
                        .await
                        .map(|_| ())
                }
                None => Ok(()),
            },
            LinearEvent::WritesFailing { issue_id, since } => {
                self.failing.insert(issue_id, since);
                Ok(())
            }
            LinearEvent::WritesRecovered { issue_id } => {
                self.failing.remove(&issue_id);
                Ok(())
            }
            LinearEvent::RunRead {
                issue_id,
                read_started_at,
                update,
                detail,
            } => {
                let Some((run, record)) = run_of(d, &issue_id) else {
                    return;
                };
                // A read made before the run's last status change (a
                // re-delegation, a reopen) must not undo it.
                let stale = self
                    .changed_at
                    .get(&issue_id)
                    .is_some_and(|at| read_started_at < *at);
                if record.status == Status::Closed || stale {
                    return;
                }
                let level = levels.get(&record.workspace).cloned().unwrap_or_default();
                let result = self
                    .apply_read(
                        d,
                        &level,
                        snap,
                        &run,
                        &record,
                        update,
                        detail,
                        read_started_at,
                        now,
                    )
                    .await;
                if let Err(error) = result {
                    d.fail(&run.key, &error);
                }
                return;
            }
        };
        if let Err(error) = result {
            d.log.line(&format!("Linear event: {error:#}"));
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_read<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        level: &LinearLevel,
        snap: Option<&Snapshot>,
        run: &Run,
        record: &RunRecord,
        update: RunUpdate,
        detail: Option<Box<IssueDetail>>,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        let issue = &update.issue;
        if matches!(issue.state_type.as_str(), "completed" | "canceled") {
            self.changed_at.insert(record.issue_id.clone(), read_at);
            return self.close_run(d, snap, run, &issue.state_name, now).await;
        }
        if let Some(request) = &record.resume_request {
            if let Some(ref detail) = detail {
                self.refresh_issue(run, detail).await?;
            }
            if matches!(request.decision, ResumeDecision::Denied) {
                return Ok(());
            }
            if let Some((token, comments)) = &update.consent
                && token == &request.token
                && d.config
                    .team(&record.workspace, &record.team_key)
                    .is_ok_and(|team| {
                        comments.iter().any(|c| {
                            !c.app
                                && team.allowed_user_ids.contains(&c.user_id)
                                && team.delegators().contains(&c.user_id)
                                && c.body == format!("stop {token}")
                        })
                    })
            {
                let token = token.clone();
                self.update_and_push(run, move |r| {
                    if let Some(request) = &mut r.resume_request && request.token == token {
                        request.decision = ResumeDecision::Denied;
                        request.restored_session_id.clear();
                        return vec![Op::Comment { body: "Resume was declined. The run remains paused; its worktrees and history are kept.".into() }];
                    }
                    Vec::new()
                }).await?;
                return Ok(());
            }
            if !request.restore_error.is_empty() {
                update_run(run, |r| r.resume_request = None).await?;
                return self.detach_run(d, run, "Linear refused the previous restoration. Correct the issue ownership or permissions before approving a new attempt.").await;
            }
            if !request.restored_session_id.is_empty() {
                // Older in-flight reads cannot confirm or invalidate the new session.
                if update.session_id.as_deref() != Some(&request.restored_session_id) {
                    return Ok(());
                }
                if !update.session_hidden
                    && level.app_user.is_some()
                    && issue.delegate_id == level.app_user
                {
                    return self.resume_approved(run, request, read_at, now).await;
                }
                update_run(run, |r| r.resume_request = None).await?;
                return self
                    .detach_run(
                        d,
                        run,
                        "The restored delegation or session was removed again.",
                    )
                    .await;
            }
            return self.accept_consent(d, run, record, &update).await;
        }
        if record.status == Status::Active
            && !record.session_id.is_empty()
            && update.session_id.as_deref() != Some(&record.session_id)
        {
            return Ok(());
        }
        if level.app_user.is_some() && issue.delegate_id != level.app_user {
            self.changed_at.insert(record.issue_id.clone(), read_at);
            return self
                .detach_run(d, run, "Linear removed this agent's delegation.")
                .await;
        }
        if update.session_hidden || record.status == Status::Detached {
            self.changed_at.insert(record.issue_id.clone(), read_at);
            return self
                .detach_run(d, run, "The Linear session was archived or dismissed.")
                .await;
        }
        if let Some(detail) = detail {
            if record.coordinator.profile.is_empty() && !self.routing.contains(&run.key) {
                self.finish_claim(d, run, &detail).await?;
            }
            self.refresh_issue(run, &detail).await?;
        }
        self.relay(d, run, &update.prompts, now).await
    }

    /// Rewrites `issue.md` and tells the coordinator when a person edited
    /// the issue. The first write, at the claim, is not an edit.
    async fn refresh_issue(&mut self, run: &Run, detail: &IssueDetail) -> Result<()> {
        let hash = crate::run::issue_hash(detail);
        let (updated_at, title) = (detail.updated_at.clone(), detail.title.clone());
        let labels: Vec<String> = detail.labels.iter().map(|l| l.name.clone()).collect();
        let snapshot = crate::run::issue_markdown(detail);
        let cursor_updated_at = detail.updated_at.clone();
        let state_name = detail.state.name.clone();
        let state_type = detail.state.r#type.clone();
        blocking({
            let run = run.clone();
            let hash = hash.clone();
            let state_summary = format!("The issue moved to {} ({}).", state_name, state_type);
            move || {
                let lock = run.lock()?;
                let current = run.record()?;
                let observed = IssueStateCursor {
                    name: state_name,
                    state_type,
                    updated_at: cursor_updated_at,
                    pickup_state: String::new(),
                };
                if current.issue_state.as_ref().is_some_and(|cursor| {
                    !cursor.updated_at.is_empty() && observed.updated_at < cursor.updated_at
                }) {
                    return Ok(false);
                }
                let pickup = current.issue_state.as_ref().is_some_and(|cursor| {
                    !cursor.pickup_state.is_empty() && cursor.pickup_state == observed.name
                });
                if current.status == Status::Active
                    && current
                        .issue_state
                        .as_ref()
                        .is_some_and(|cursor| !pickup && cursor.name != observed.name)
                {
                    inbox::write_held(&run, &lock, "issue", "state", &state_summary)?;
                }
                if !current.issue_hash.is_empty() && current.issue_hash != hash {
                    inbox::write_held(
                        &run,
                        &lock,
                        "issue",
                        "issue",
                        "The issue was edited in Linear; issue.md is updated.",
                    )?;
                }
                files::write_atomic(&run.issue_md(), snapshot.as_bytes())?;
                run.update_held(&lock, move |r| {
                    r.issue_updated_at = updated_at;
                    r.issue_hash = hash;
                    r.title = title;
                    r.labels = labels;
                    let next = match &r.issue_state {
                        Some(cursor)
                            if pickup
                                || cursor.pickup_state.is_empty()
                                || observed.state_type == "started" =>
                        {
                            IssueStateCursor {
                                pickup_state: String::new(),
                                ..observed
                            }
                        }
                        Some(cursor) => IssueStateCursor {
                            pickup_state: cursor.pickup_state.clone(),
                            ..observed
                        },
                        None => observed,
                    };
                    r.issue_state = Some(next);
                })?;
                Ok(true)
            }
        })
        .await?;
        Ok(())
    }

    /// Replies from allowed users reach the coordinator through
    /// `conversation.md` and the inbox; a stop signal interrupts the run's
    /// agents; anyone else's message is only recorded. The Linear task may
    /// read with an older cursor. Capture eligible IDs together before
    /// delivery order advances the creation cursor past an older prompt.
    async fn relay<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        prompts: &[Prompt],
        now: Timestamp,
    ) -> Result<()> {
        // The run's team decides whose replies count; a team no longer in the
        // config lets nobody's through.
        let record = run.record()?;
        let team = record.team_key.clone();
        let allowed = d
            .config
            .team(&record.workspace, &team)
            .map(|team| team.allowed_user_ids.clone())
            .unwrap_or_default();
        let merge_enabled = d
            .config
            .team(&record.workspace, &team)
            .is_ok_and(|team| team.merge_on_authorization);

        // Delivery order is sentAt, but the cursor is createdAt. Capture all
        // eligible IDs against one cursor before processing can advance it.
        let candidates: Vec<_> = prompts
            .iter()
            .filter(|prompt| prompt_is_new(&record, &prompt.id, &prompt.created_at))
            .map(|prompt| (prompt.id.clone(), prompt.created_at.clone()))
            .collect();
        if !candidates.is_empty() {
            self.guarded(run, move |run, lock| {
                run.update_held(lock, |record| {
                    let initial = record.clone();
                    let eligible: Vec<_> = candidates
                        .iter()
                        .filter(|(id, created)| prompt_is_new(&initial, id, created))
                        .collect();
                    if !eligible.is_empty() {
                        for (id, created) in eligible {
                            if !record
                                .pending_prompt_ids
                                .iter()
                                .any(|pending| pending == id)
                            {
                                record.pending_prompt_ids.push(id.clone());
                            }
                            advance_prompt_cursor(record, created, id);
                        }
                    }
                })?;
                Ok(((), Vec::new()))
            })
            .await?;
        }

        for prompt in prompts {
            let (created, id, user, body) = (
                prompt.created_at.clone(),
                prompt.id.clone(),
                prompt.user_id.clone(),
                prompt.body.clone(),
            );
            if prompt.queued {
                continue;
            }
            if !prompt_is_new(&run.record()?, &id, &created) {
                continue;
            }
            let displayed_at = prompt.sent_at.as_deref().unwrap_or(&created).to_string();
            if let Some(token) = worker::merge_token(&body).map(str::to_string) {
                let authorized_user = allowed.contains(&user);
                let merge_id = id.clone();
                let merge_created = created.clone();
                let merge_user = user.clone();
                let handled = self
                    .guarded(run, move |run, lock| {
                        let current = run.record()?;
                        if !prompt_is_new(&current, &merge_id, &merge_created) {
                            return Ok((false, Vec::new()));
                        }
                        let pending = worker::list(run).into_iter().find_map(|w| {
                            let Some(worker::MergeAuthorization::Pending {
                                token: expected,
                                pr_url,
                            }) = w.merge_authorization
                            else {
                                return None;
                            };
                            (expected == token && w.pr_url == pr_url)
                                .then_some((w.id, expected, pr_url))
                        });
                        let mut authorized_worker = None;
                        let accepted = match pending {
                            Some((worker_id, expected, pr_url))
                                if authorized_user && merge_enabled =>
                            {
                                worker::update_held(run, lock, &worker_id, |current| {
                                    current.merge_authorization =
                                        Some(worker::MergeAuthorization::Authorized {
                                            token: expected,
                                            pr_url,
                                            by: merge_user.clone(),
                                        });
                                })?;
                                authorized_worker = Some(worker_id);
                                true
                            }
                            Some((worker_id, _, _)) => {
                                authorized_worker = Some(worker_id);
                                false
                            }
                            _ => false,
                        };
                        let (subject, summary) = match (accepted, authorized_worker.as_deref()) {
                            (true, Some(worker_id)) => (
                                worker_id,
                                format!(
                                    "An allowed user authorized worker {worker_id}'s pull request merge."
                                ),
                            ),
                            (false, Some(worker_id)) => (
                                worker_id,
                                format!(
                                    "A merge authorization reply for worker {worker_id} was ignored."
                                ),
                            ),
                            (_, None) => (
                                "authorization",
                                "A merge authorization reply was ignored.".to_string(),
                            ),
                        };
                        inbox::write_held(
                            run,
                            lock,
                            "merge",
                            subject,
                            &summary,
                        )?;
                        run.update_held(lock, |r| {
                            advance_prompt_cursor(r, &merge_created, &merge_id);
                            remove_pending_prompt(r, &merge_id);
                        })?;
                        Ok((true, Vec::new()))
                    })
                    .await?;
                if handled {
                    if authorized_user && merge_enabled {
                        d.log.line(&format!(
                            "{}: processed a merge authorization reply",
                            run.key
                        ));
                    }
                    continue;
                }
            }
            if !allowed.contains(&prompt.user_id) {
                let at = displayed_at.clone();
                let recorded = self
                    .guarded(run, move |run, lock| {
                        let mut recorded = false;
                        if prompt_is_new(&run.record()?, &id, &created) {
                            run.record_ignored_prompt_held(lock, &at, &user, &body)?;
                            run.update_held(lock, |record| {
                                advance_prompt_cursor(record, &created, &id);
                                remove_pending_prompt(record, &id);
                            })?;
                            recorded = true;
                        }
                        Ok((recorded, Vec::new()))
                    })
                    .await?;
                if recorded {
                    let from = match prompt.user_name.as_str() {
                        "" => prompt.user_id.clone(),
                        name => format!("{name} ({})", prompt.user_id),
                    };
                    let hint =
                        format!("add the id to allowed_user_ids of team {team} to let it through");
                    d.log
                        .line(&format!("{}: ignored a reply from {from}; {hint}", run.key));
                    d.notify(
                        &format!("{} ignored a reply", run.key),
                        &format!("From {from}; {hint}."),
                    )
                    .await;
                }
                continue;
            }
            if prompt.signal.as_deref() == Some("stop") {
                // The keys and the response follow in the first pass with a
                // snapshot, which may be this one.
                update_run(run, move |r| {
                    if prompt_is_new(r, &id, &created) {
                        advance_prompt_cursor(r, &created, &id);
                        remove_pending_prompt(r, &id);
                        r.reply_generation = r.reply_generation.saturating_add(1);
                        r.turn_complete = None;
                        r.stopped = true;
                        if let Some(wait) = r.awaiting_reply.take() {
                            r.cleared_wait_id = wait.activity_id;
                        }
                        r.interrupt = Some(Interrupt::Stop);
                    }
                })
                .await?;
                continue;
            }
            // The inbox item comes first: a failure before the cursor moves
            // may repeat it, never the conversation entry.
            inbox_item(
                run,
                "reply",
                "reply",
                format!(
                    "A new reply from user {} is in conversation.md.",
                    prompt.user_id
                ),
            )
            .await?;
            let resume = prompt.body.trim().eq_ignore_ascii_case("resume");
            let restart_window = now.to_string();
            self.guarded(run, move |run, lock| {
                let current = run.record()?;
                if !prompt_is_new(&current, &id, &created) {
                    return Ok((false, Vec::new()));
                }
                run.append_conversation_held(lock, &displayed_at, &user, &body)?;
                run.update_held(lock, |record| {
                    advance_prompt_cursor(record, &created, &id);
                    remove_pending_prompt(record, &id);
                    record.reply_generation = record.reply_generation.saturating_add(1);
                    record.turn_complete = None;
                    record.stopped = false;
                    record.finished = false;
                    if resume {
                        record.coordinator.recovery = crate::run::Recovery::None;
                    }
                    if let Some(wait) = record.awaiting_reply.take() {
                        if wait.reason == WaitReason::RunTimeout {
                            record.timeout_since = restart_window.clone();
                        }
                        record.cleared_wait_id = wait.activity_id;
                    }
                    if record.timeout_asked {
                        record.timeout_asked = false;
                        record.timeout_since = restart_window.clone();
                    }
                    if record.coordinator_lost && resume {
                        record.coordinator_lost = false;
                        record.coordinator.repend();
                    }
                })?;
                Ok((true, Vec::new()))
            })
            .await?;
        }
        Ok(())
    }

    /// Sends the Escape keys a stop or a detach left pending, and after a
    /// stop posts how many agents it reached.
    pub(super) async fn deliver_interrupts<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
    ) {
        for run in Run::list(&d.ctx.runs_dir()) {
            if !run.record().is_ok_and(|r| r.interrupt.is_some()) {
                continue;
            }
            let stopped = self.interrupt_agents(d, Some(snapshot), &run).await;
            let result = self
                .update_and_push(&run, move |r| match r.interrupt.take() {
                    Some(Interrupt::Stop) => vec![Op::activity(Activity::new(Content::Response {
                            body: format!(
                                "Stopped {stopped} agent(s) as asked. Their worktrees are kept; reply here to continue."
                            ),
                        }))],
                    _ => Vec::new(),
                })
                .await;
            if let Err(error) = result {
                d.fail(&run.key, &error);
            }
        }
    }

    /// Escape in the coordinator's and every open worker's pane, for each
    /// agent found by identity. Returns how many were interrupted.
    async fn interrupt_agents<H: Herdr>(
        &self,
        d: &Deps<'_, H>,
        snap: Option<&Snapshot>,
        run: &Run,
    ) -> usize {
        let Some(snapshot) = snap else {
            return 0;
        };
        let mut records: Vec<AgentRecord> = worker::list(run)
            .into_iter()
            .filter(|w| w.agent.status == AgentStatus::Open)
            .map(|w| w.agent)
            .collect();
        if let Ok(record) = run.record() {
            records.insert(0, record.coordinator);
        }
        let mut stopped = 0;
        for record in &records {
            if let Some(agent) = worker::find_agent(record, &snapshot.agents)
                && d.herdr.send_escape(&agent.pane).await.is_ok()
            {
                stopped += 1;
            }
        }
        stopped
    }

    /// The issue was completed or canceled: copy the reports home, stop the
    /// agents and close their workspaces. Checkouts and branches stay.
    async fn close_run<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snap: Option<&Snapshot>,
        run: &Run,
        state: &str,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        self.interrupt_agents(d, snap, run).await;
        // The live workspace, or the recorded one; without a snapshot every
        // open agent's recorded workspace.
        let workspace_of = |agent: &AgentRecord| -> Option<String> {
            if agent.status != AgentStatus::Open {
                return None;
            }
            let Some(snapshot) = snap else {
                return Some(agent.workspace_id.clone()).filter(|w| !w.is_empty());
            };
            if let Some(found) = worker::find_agent(agent, &snapshot.agents) {
                return snapshot
                    .panes
                    .get(&found.pane)
                    .map(|p| p.workspace.0.clone())
                    .or_else(|| Some(agent.workspace_id.clone()));
            }
            let live = worker::live_state(agent, snapshot, now, &d.ctx.state_dir(), d.socket);
            live.pane_exists.then(|| agent.workspace_id.clone())
        };
        let mut workspaces = Vec::new();
        for w in worker::list(run) {
            let _ = worker::copy_report_home(run, &w);
            workspaces.extend(workspace_of(&w.agent));
            update_worker(run, &w.id, |w| w.agent.status = AgentStatus::Stopped).await?;
        }
        workspaces.extend(workspace_of(&record.coordinator));
        for workspace in workspaces {
            if let Err(error) = d
                .herdr
                .workspace_close(&WorkspaceId(workspace.clone()))
                .await
            {
                d.log.line(&format!(
                    "{}: could not close workspace {workspace}: {error}",
                    run.key
                ));
            }
        }
        // Linear shows the session working until a response ends it.
        let body = format!("The issue is {state}; this run is closed.");
        let closed_state = state.to_string();
        self.update_and_push(run, move |r| {
            r.status = Status::Closed;
            r.closed_state = closed_state;
            r.coordinator.status = AgentStatus::Stopped;
            r.postmortem_due = Some(crate::postmortem::Stage::Final);
            if let Some(wait) = r.awaiting_reply.take() {
                r.cleared_wait_id = wait.activity_id;
            }
            if r.resume_request.take().is_some() {
                r.session_id.clear();
                return Vec::new();
            }
            if r.session_id.is_empty() {
                return Vec::new();
            }
            vec![Op::activity(Activity::new(Content::Response { body }))]
        })
        .await?;
        d.log
            .line(&format!("{}: closed (the issue is {state})", run.key));
        self.keep_transcripts(d, run);
        Ok(())
    }

    /// The delegation was removed: stop the agents, keep the workspaces.
    /// The keys go out in the first pass with a snapshot.
    async fn detach_run<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        reason: &str,
    ) -> Result<()> {
        let reason = reason.to_string();
        let requested = self.guarded(run, move |run, lock| {
            if run.record()?.resume_request.is_some() { return Ok((false, Vec::new())); }
            let token = uuid::Uuid::new_v4().to_string();
            let body = format!("{reason} Work is paused and the existing worktrees and history are kept.\n\nTo authorize restoring this agent as delegate and continuing this run in a new Linear session, add an issue comment containing exactly:\n\n`resume {token}`\n\nTo keep the run paused, comment `stop {token}`. Approval requires a human in both this team's allowed_user_ids and allowed_delegator_ids. Old approvals never authorize a later incident. Completed or canceled issues still receive cleanup and a final postmortem.");
            let comment_id = outbox::push_held(run, lock, Op::ResumeQuestion { token: token.clone(), body })?
                .context("resume comment was not queued")?;
            run.update_held(lock, |r| {
                r.status = Status::Detached;
                r.interrupt.get_or_insert(Interrupt::Detach);
                if let Some(wait) = r.awaiting_reply.take() { r.cleared_wait_id = wait.activity_id; }
                r.resume_request = Some(ResumeRequest { token, comment_id, decision: ResumeDecision::Waiting,
                    restored_session_id: String::new(), restore_error: String::new(), delegate_attempted: false, session_attempted: false });
            })?;
            Ok((true, Vec::new()))
        }).await?;
        self.queued |= requested;
        d.log
            .line(&format!("{}: detached (no longer delegated)", run.key));
        self.keep_transcripts(d, run);
        Ok(())
    }

    async fn accept_consent<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        record: &RunRecord,
        update: &RunUpdate,
    ) -> Result<()> {
        let Some(request) = &record.resume_request else {
            return Ok(());
        };
        let Some((token, comments)) = &update.consent else {
            return Ok(());
        };
        if token != &request.token {
            return Ok(());
        }
        let Ok(team) = d.config.team(&record.workspace, &record.team_key) else {
            return Ok(());
        };
        if let ResumeDecision::Approved { user_id, .. } = &request.decision {
            if !team.allowed_user_ids.contains(user_id) || !team.delegators().contains(user_id) {
                return Ok(());
            }
            let token = token.clone();
            let allowed_users = team
                .allowed_user_ids
                .iter()
                .filter(|id| team.delegators().contains(id))
                .cloned()
                .collect();
            // Recover a crash between persisting consent and queuing its
            // write. External attempt markers still prevent blind retries.
            self.guarded(run, move |run, _lock| {
                let current = run.record()?;
                let ready = current.status == Status::Detached
                    && current.resume_request.is_some_and(|r| {
                        r.token == token
                            && r.restored_session_id.is_empty()
                            && r.restore_error.is_empty()
                            && matches!(r.decision, ResumeDecision::Approved { .. })
                    });
                let queued = outbox::queued(run).iter().any(|r| {
                    matches!(&r.op,
                    Op::RestoreSession { token: queued, .. } if queued == &token)
                });
                Ok((
                    (),
                    if ready && !queued {
                        vec![Op::RestoreSession {
                            token,
                            allowed_users,
                        }]
                    } else {
                        Vec::new()
                    },
                ))
            })
            .await?;
            return Ok(());
        }
        if !matches!(request.decision, ResumeDecision::Waiting) {
            return Ok(());
        }
        let allowed = |c: &&crate::linear::api::ConsentComment| {
            !c.app
                && team.allowed_user_ids.contains(&c.user_id)
                && team.delegators().contains(&c.user_id)
        };
        // Refusals have already won in apply_read, before restoration.
        let Some(answer) = comments
            .iter()
            .filter(allowed)
            .find(|c| c.body == format!("resume {token}"))
        else {
            return Ok(());
        };
        let decision = ResumeDecision::Approved {
            comment_id: answer.id.clone(),
            user_id: answer.user_id.clone(),
        };
        let token = token.clone();
        let allowed_users = team
            .allowed_user_ids
            .iter()
            .filter(|id| team.delegators().contains(id))
            .cloned()
            .collect();
        self.update_and_push(run, move |r| {
            let Some(request) = &mut r.resume_request else {
                return Vec::new();
            };
            if request.token != token || !matches!(request.decision, ResumeDecision::Waiting) {
                return Vec::new();
            }
            request.decision = decision;
            vec![Op::RestoreSession {
                token,
                allowed_users,
            }]
        })
        .await?;
        Ok(())
    }

    async fn resume_approved(
        &mut self,
        run: &Run,
        request: &ResumeRequest,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        let token = request.token.clone();
        let session = request.restored_session_id.clone();
        let now = now.to_string();
        let record = self.update_and_push(run, move |r| {
            if !r.resume_request.as_ref().is_some_and(|q| q.token == token && q.restored_session_id == session) {
                return Vec::new();
            }
            r.session_id = session;
            r.status = Status::Active;
            r.resume_request = None;
            r.prompt_cursor = now.clone();
            r.prompt_cursor_ids = Some(Vec::new());
            r.pending_prompt_ids.clear();
            r.turn_complete = None;
            r.timeout_since = now;
            r.stopped = false;
            r.coordinator_lost = false;
            if r.interrupt == Some(Interrupt::Detach) { r.interrupt = None; }
            if r.coordinator.status == AgentStatus::Stopped { r.coordinator.repend(); }
            if !r.finished && r.coordinator.status == AgentStatus::Open {
                r.coordinator.resume = !r.coordinator.agent_session.is_empty();
                r.coordinator.prompt_pending = true;
            }
            let acknowledgement = if r.finished {
                Op::activity(Activity::new(Content::Response { body: "Resume was approved; delegation and the new Linear session are verified. The existing work is finished; this run is waiting for issue completion or cancellation.".into() }))
            } else {
                thought("Resume was approved in an issue comment; delegation and the new Linear session are verified. The existing run continues.")
            };
            let mut ops = vec![acknowledgement];
            if !r.external_urls.is_empty() { ops.push(Op::ExternalUrls { urls: r.external_urls.clone() }); }
            ops
        }).await?;
        self.changed_at.insert(record.issue_id, read_at);
        inbox_item(run, "issue", "issue", "Resume was approved in Linear. Continue the existing task from its current worktrees and history.".into()).await
    }

    /// Picks up delegated issues from a delegated list this pass has not
    /// seen yet, while the limits leave room.
    pub(super) async fn intake<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        levels: &Levels,
        now: Timestamp,
    ) {
        for (workspace, level) in levels {
            self.intake_workspace(d, workspace, level, now).await;
        }
    }

    async fn intake_workspace<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        workspace: &str,
        level: &LinearLevel,
        now: Timestamp,
    ) {
        let Some(delegated) = &level.delegated else {
            return;
        };
        if self
            .intake_read
            .get(workspace)
            .is_some_and(|at| *at >= delegated.read_at)
        {
            return;
        }
        self.intake_read
            .insert(workspace.to_string(), delegated.read_at);
        let runs_dir = d.ctx.runs_dir();
        let paused = d.ctx.state_dir().join("paused").exists();
        let mut declined = std::collections::BTreeMap::new();
        for issue in &delegated.issues {
            let key = crate::run::run_key(workspace, &issue.identifier);
            let run = Run::load(&runs_dir, &key).ok();
            // The app's own restoration is an automation, not a human
            // delegation. Only the approved, verified path may resume it.
            if run
                .as_ref()
                .is_some_and(|run| run.record().is_ok_and(|r| r.resume_request.is_some()))
            {
                continue;
            }
            let active = run
                .as_ref()
                .and_then(|run| run.record().ok())
                .is_some_and(|record| record.status == Status::Active);
            // Only a claim or a restart takes a delegation: a running run
            // keeps going whoever opens another session on its issue.
            if !active {
                let delegators = d
                    .config
                    .team(workspace, &issue.team)
                    .map(|team| team.delegators().to_vec())
                    .unwrap_or_default();
                match delegation(&key, issue, &delegators) {
                    Delegation::Allowed => {}
                    Delegation::Answered => continue,
                    Delegation::Declined(decline) => {
                        if self.declined.get(&key) != Some(&decline) {
                            tell_declined(d, &key, issue).await;
                        }
                        declined.insert(key, decline);
                        continue;
                    }
                }
            }
            if let Some(run) = run {
                if let Err(error) = self.reactivate(&run, delegated.read_at).await {
                    d.fail(&run.key, &error);
                }
                continue;
            }
            if paused {
                continue;
            }
            let runs = Run::list(&runs_dir);
            let active = runs
                .iter()
                .filter(|r| r.record().is_ok_and(|r| r.status == Status::Active))
                .count();
            if active >= d.config.limits.max_runs as usize
                || worker::agent_count(&runs) + 1 > d.config.limits.max_agents as usize
            {
                break;
            }
            if let Err(error) = self
                .claim(d, workspace, issue, delegated.read_at, now)
                .await
            {
                d.log.line(&format!("{key}: could not pick up: {error:#}"));
            }
        }
        self.declined
            .retain(|key, _| crate::run::split_key(key).0 != workspace);
        self.declined.extend(declined);
    }

    /// A detached or closed run whose issue is delegated again, in a list
    /// read after the run stopped, becomes active again.
    async fn reactivate(&mut self, run: &Run, read_at: Timestamp) -> Result<()> {
        let record = run.record()?;
        if record.status == Status::Active
            || self
                .changed_at
                .get(&record.issue_id)
                .is_some_and(|at| read_at <= *at)
        {
            return Ok(());
        }
        self.changed_at.insert(record.issue_id.clone(), read_at);
        self.update_and_push(run, |r| {
            if r.status == Status::Active {
                return Vec::new();
            }
            r.status = Status::Active;
            if r.interrupt == Some(Interrupt::Detach) {
                r.interrupt = None;
            }
            // A closed run's coordinator was stopped: bring it back.
            if r.coordinator.status == AgentStatus::Stopped {
                r.coordinator.repend();
            }
            vec![thought("The issue was delegated again; the run continues.")]
        })
        .await?;
        inbox_item(
            run,
            "issue",
            "issue",
            "The issue was delegated to this agent again; the run is active again.".into(),
        )
        .await
    }

    /// Creates the run and queues the first thought and the started state.
    /// The Linear task opens the session with the flush, and the first run
    /// read brings the detail that writes `issue.md` and routes the run.
    async fn claim<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        workspace: &str,
        issue: &IssueRef,
        read_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        crate::run::validate_issue_key(&issue.identifier)?;
        d.ctx.ensure_state_dir()?;
        std::fs::create_dir_all(d.ctx.runs_dir())?;
        let now = now.to_string();
        let record = RunRecord {
            workspace: workspace.to_string(),
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
            title: issue.title.clone(),
            url: issue.url.clone(),
            team_key: issue.team.clone(),
            session_id: open_session(issue).unwrap_or_default(),
            created: now.clone(),
            prompt_cursor: now.clone(),
            prompt_cursor_ids: Some(Vec::new()),
            last_activity: now.clone(),
            timeout_since: now,
            announce_pending: true,
            ..RunRecord::default()
        };
        let runs_dir = d.ctx.runs_dir();
        let run = blocking(move || Run::create(&runs_dir, record)).await?;
        self.changed_at.insert(issue.id.clone(), read_at);
        d.log.line(&format!("{}: picked up", run.key));
        let key = issue.identifier.clone();
        let (state_name, state_type, state_updated_at) = (
            issue.state.name.clone(),
            issue.state.r#type.clone(),
            issue.updated_at.clone(),
        );
        self.update_and_push(&run, move |r| {
            r.announce_pending = false;
            r.issue_state = Some(IssueStateCursor {
                name: state_name,
                state_type,
                updated_at: state_updated_at,
                pickup_state: String::new(),
            });
            vec![
                thought(format!("Picked up {key}.")),
                Op::IssueState {
                    target: StateTarget::Started,
                },
            ]
        })
        .await
        .map(|_| ())
    }

    /// A run without a decided coordinator: queues the claim's first thought
    /// when a crash cut the claim short, moves the issue to a started state
    /// unless that is queued or done, and routes it.
    async fn finish_claim<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        detail: &IssueDetail,
    ) -> Result<()> {
        let moved = matches!(
            detail.state.r#type.as_str(),
            "started" | "completed" | "canceled"
        );
        let queued = outbox::queued(run)
            .iter()
            .any(|request| matches!(request.op, Op::IssueState { .. }));
        let expected_started = detail
            .team
            .states
            .iter()
            .filter(|state| state.r#type == "started")
            .min_by(|a, b| a.position.total_cmp(&b.position))
            .map(|state| state.name.clone())
            .unwrap_or_default();
        let key = crate::run::split_key(&run.key).1.to_string();
        self.update_and_push(run, move |r| {
            let mut ops = Vec::new();
            if std::mem::take(&mut r.announce_pending) {
                ops.push(thought(format!("Picked up {key}.")));
            }
            if !moved && !queued {
                ops.push(Op::IssueState {
                    target: StateTarget::Started,
                });
            }
            if !moved && let Some(cursor) = &mut r.issue_state {
                cursor.pickup_state = expected_started;
            }
            ops
        })
        .await?;
        self.route(d, run, detail).await
    }

    /// Picks the coordinator with the run's routing: at once when it lists
    /// one, else by the routing agent in its own task, whose pick comes back
    /// as a `RoutingDone`.
    async fn route<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        detail: &IssueDetail,
    ) -> Result<()> {
        let record = run.record()?;
        let routing = d.config.routing_of(&record.workspace, &record.team_key)?;
        if let [only] = &routing.coordinators[..] {
            return self
                .decide(d, run, &routing::Choice::Only(only.clone()))
                .await;
        }
        let coordinators = routing::candidates(d.config, &routing.coordinators);
        let agent = routing
            .agent
            .as_deref()
            .context("the routing has no agent to pick with")?;
        let profile = d.config.profile(agent)?.clone();
        let timeout = Duration::from_secs(
            profile
                .timeout_seconds
                .unwrap_or(d.config.limits.routing_agent_timeout_seconds),
        );
        let input = routing::input(detail);
        let path = d.ctx.env.var("PATH").map(str::to_string);
        let parent = std::env::temp_dir();
        let key = run.key.clone();
        let done = self.routing_done.clone();
        self.routing.insert(key.clone());
        tokio::spawn(async move {
            let choice = routing::choose(
                &profile,
                routing::Pick::Coordinator,
                &coordinators,
                &input,
                timeout,
                path.as_deref(),
                &parent,
            )
            .await;
            let _ = done.send(RoutingDone { key, choice }).await;
        });
        Ok(())
    }

    pub(super) async fn apply_routing<H: Herdr>(&mut self, d: &Deps<'_, H>, done: RoutingDone) {
        self.routing.remove(&done.key);
        let Ok(run) = Run::load(&d.ctx.runs_dir(), &done.key) else {
            return;
        };
        if let routing::Choice::Default(_, routing::Fallback::Failed(error)) = &done.choice {
            d.log
                .line(&format!("{}: the routing agent failed: {error}", run.key));
        }
        if let Err(error) = self.decide(d, &run, &done.choice).await {
            d.fail(&run.key, &error);
        }
    }

    /// Records the coordinator profile and reserves the coordinator's launch.
    async fn decide<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        choice: &routing::Choice,
    ) -> Result<()> {
        let record = run.record()?;
        if !record.coordinator.profile.is_empty() || record.status != Status::Active {
            return Ok(());
        }
        let name = choice.profile().to_string();
        let profile = d.config.profile(&name)?;
        let mut pending = coordinator::pending_record(&record, &name, &profile.kind);
        pending.agent_session = record.coordinator.agent_session.clone();
        let source = choice.source();
        self.update_and_push(run, move |r| {
            if !r.coordinator.profile.is_empty() {
                return Vec::new();
            }
            r.routing_source = source.clone();
            r.coordinator = pending;
            vec![thought(format!(
                "The coordinator uses the `{name}` profile ({source})."
            ))]
        })
        .await
        .map(|_| ())
    }
}
