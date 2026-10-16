//! Launching agents: the coordinator's placement, starts in panes at a shell
//! prompt, and launch prompts. Placements and starts are slow, so they run
//! as effect tasks; their results come back as [`EffectDone`].

use anyhow::{Context, Result};
use jiff::Timestamp;

use super::reconcile::{
    AgentKey, DETECTION_GRACE, Deps, Effect, EffectDone, MAX_LAUNCH_ATTEMPTS, Reconciler,
    error_activity, update_run,
};
use crate::config::Config;
use crate::herdr::{Herdr, HerdrError, PaneId, Placed, Snapshot, WorkspaceId};
use crate::run::{
    AgentRecord, AgentStatus, CoordinatorPlacementOperation, CoordinatorPlacementPhase,
    CoordinatorPlacementRef, Run, RunRecord, Status,
};
use crate::supervisor::{Scope as SupervisorScope, SupervisorStore};
use crate::{agents, claude_trust, coordinator, worker};

/// The session a start begins: the recorded one for a resume, a new id the
/// plugin picks for Claude, and else none yet (the kind picks its own, found
/// later from `started_at`).
fn start_session(record: &AgentRecord) -> String {
    let resumes =
        record.resume && agents::resume_args(&record.kind, &record.agent_session).is_some();
    match (resumes, record.kind.as_str()) {
        (true, _) => record.agent_session.clone(),
        (false, "claude") => uuid::Uuid::new_v4().to_string(),
        _ => String::new(),
    }
}

/// The agent CLI's arguments for a start in `session`: the profile's flags
/// and, for a resume, the session; a new Claude session gets its id. Codex
/// takes its `resume <id>` words first.
fn start_args(config: &Config, record: &AgentRecord, session: &str) -> Result<Vec<String>> {
    Ok(agents::start_args(
        config.profile(&record.profile)?,
        record.resume,
        session,
    ))
}

/// Sets a placed coordinator open in its root pane, ready to be started.
fn place_coordinator(record: &mut RunRecord, placed: &Placed, session: &str) {
    let c = &mut record.coordinator;
    c.placed_in(placed, session);
    c.last_state.clear();
    c.last_state_change.clear();
    c.last_state_seq = 0;
    c.blocked_reported = false;
    record.coordinator_tab_named = false;
}

/// Canonical matches for an uncertain placement. Never choose arbitrarily
/// when two workspaces contain the same run directory.
fn placed_before(
    snapshot: &Snapshot,
    record: &AgentRecord,
    cwd: &str,
    target_workspace: Option<&str>,
    tab_label: &str,
) -> Vec<Placed> {
    let Ok(cwd) = std::path::Path::new(cwd).canonicalize() else {
        return Vec::new();
    };
    let ours = |agent: &crate::herdr::Agent| {
        agent.kind.as_deref() == Some(record.kind.as_str())
            && agent
                .name
                .as_deref()
                .is_none_or(|n| n.is_empty() || n == record.agent_name)
    };
    snapshot
        .panes
        .values()
        .filter(|p| target_workspace.is_none_or(|workspace| p.workspace.0 == workspace))
        .filter(|p| snapshot.tab_label(&p.tab) == Some(tab_label))
        .filter(|p| {
            [p.cwd.as_deref(), p.foreground_cwd.as_deref()]
                .into_iter()
                .flatten()
                .any(|path| {
                    std::path::Path::new(path)
                        .canonicalize()
                        .is_ok_and(|p| p == cwd)
                })
        })
        .filter(|p| snapshot.agents.iter().filter(|a| a.pane == p.id).all(ours))
        .map(|p| Placed {
            workspace: p.workspace.clone(),
            tab: p.tab.clone(),
            pane: p.id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
        })
        .collect()
}

/// A prompt whose answer was lost counts as delivered: a missed one is
/// recovered by the nudge, a doubled one is not.
pub(super) fn delivered(result: Result<(), HerdrError>) -> Result<(), HerdrError> {
    match result {
        Err(HerdrError::OutcomeUnknown(_)) => Ok(()),
        other => other,
    }
}

fn pane_is_empty(snapshot: &Snapshot, pane: &str) -> bool {
    let id = PaneId(pane.to_string());
    snapshot.panes.contains_key(&id) && !snapshot.agents.iter().any(|a| a.pane == id)
}

impl Reconciler {
    pub(super) async fn apply_effect<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        done: EffectDone,
        now: Timestamp,
    ) {
        let key = match &done {
            EffectDone::Placed { key, .. } | EffectDone::Started { key, .. } => key.clone(),
        };
        let Ok(run) = Run::load(&d.ctx.runs_dir(), &key.run) else {
            return;
        };
        if let EffectDone::Placed {
            operation_id,
            session,
            result,
            ..
        } = done
        {
            let current = run
                .record()
                .ok()
                .and_then(|record| record.coordinator_placement)
                .is_some_and(|operation| operation.id == operation_id);
            if current && matches!(self.in_flight.get(&key), Some(Effect::Place)) {
                self.in_flight.remove(&key);
            }
            let result = if current {
                self.placed(d, &run, &key, operation_id, result, now).await
            } else if let Ok(placed) = result {
                self.persist_late_placement(d, &run, operation_id, &session, &placed)
                    .await
            } else {
                Ok(())
            };
            if let Err(error) = result {
                d.fail(&run.key, &error);
            }
            return;
        }
        let effect_matches = match &done {
            EffectDone::Started { recovery_id, .. } => matches!(
                self.in_flight.get(&key),
                Some(Effect::Start { request_id }) if request_id == recovery_id
            ),
            EffectDone::Placed { .. } => unreachable!(),
        };
        if !effect_matches {
            return;
        }
        self.in_flight.remove(&key);
        if let EffectDone::Started {
            recovery_id: Some(id),
            ..
        } = &done
        {
            let current = self.agent_record(&run, &key).is_ok_and(|a| matches!(a.recovery, crate::run::Recovery::Starting { request_id, .. } if request_id == *id));
            if !current {
                return;
            }
        }
        let result = match done {
            EffectDone::Started {
                pane,
                recovery_id,
                result,
                ..
            } => {
                self.started(d, &run, &key, (pane, recovery_id, result), now)
                    .await
            }
            EffectDone::Placed { .. } => unreachable!(),
        };
        if let Err(error) = result {
            d.fail(&run.key, &error);
        }
    }

    async fn persist_late_placement<H: Herdr>(
        &self,
        d: &Deps<'_, H>,
        run: &Run,
        operation_id: uuid::Uuid,
        session: &str,
        placed: &Placed,
    ) -> Result<()> {
        let orphan = CoordinatorPlacementRef {
            session: session.to_string(),
            workspace: placed.workspace.0.clone(),
            tab: placed.tab.clone(),
            pane: placed.pane.0.clone(),
            cwd: placed.cwd.clone(),
        };
        let saved = orphan.clone();
        let saved_for_update = saved.clone();
        let saved_for_slot = orphan.clone();
        let operation = CoordinatorPlacementOperation {
            id: operation_id,
            session: session.to_string(),
            cwd: placed.cwd.clone(),
            label: coordinator::tab_label().into(),
            target_workspace: Some(placed.workspace.0.clone()),
            phase: CoordinatorPlacementPhase::Orphan(orphan),
        };
        let updated = update_run(run, move |r| {
            if r.coordinator_placement.is_none() {
                r.coordinator_placement = Some(operation);
            } else if r.coordinator_placement.as_ref().is_some_and(|current| {
                current.id == operation_id
                    && matches!(&current.phase, CoordinatorPlacementPhase::Orphan(existing) if existing == &saved_for_slot)
            }) {
                // The same late result was already durably queued.
            } else if !r.coordinator_orphans.contains(&saved_for_update) {
                r.coordinator_orphans.push(saved_for_update);
            }
        })
        .await?;
        if let Some(CoordinatorPlacementOperation {
            id,
            phase: CoordinatorPlacementPhase::Orphan(orphan),
            ..
        }) = updated.coordinator_placement
            && id == operation_id
        {
            self.close_orphan(d, run, operation_id, &orphan).await?;
        } else {
            self.close_orphan(d, run, uuid::Uuid::nil(), &saved).await?;
        }
        Ok(())
    }

    async fn placed<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        key: &AgentKey,
        operation_id: uuid::Uuid,
        result: Result<Placed, HerdrError>,
        now: Timestamp,
    ) -> Result<()> {
        match result {
            Ok(placed) => {
                let operation = run
                    .record()?
                    .coordinator_placement
                    .filter(|operation| operation.id == operation_id)
                    .context("coordinator placement result no longer has its durable intent")?;
                self.accept_observed_placement(d, run, &operation, &placed)
                    .await
            }
            // The next snapshot shows whether it was placed.
            Err(HerdrError::OutcomeUnknown(_)) => {
                let _ = now;
                update_run(run, move |r| {
                    if let Some(operation) = &mut r.coordinator_placement
                        && operation.id == operation_id
                    {
                        operation.phase = CoordinatorPlacementPhase::Unknown;
                    }
                })
                .await?;
                Ok(())
            }
            // Herdr down is not an attempt; the next one waits 15 s.
            Err(HerdrError::NotSent(_)) => {
                update_run(run, move |r| {
                    if r.coordinator_placement
                        .as_ref()
                        .is_some_and(|operation| operation.id == operation_id)
                    {
                        r.coordinator_placement = None;
                    }
                })
                .await?;
                self.not_sent.insert(key.clone(), now);
                Ok(())
            }
            Err(error) => {
                update_run(run, move |r| {
                    if r.coordinator_placement
                        .as_ref()
                        .is_some_and(|operation| operation.id == operation_id)
                    {
                        r.coordinator_placement = None;
                    }
                })
                .await?;
                self.attempt_failed(run, key, None, &error.to_string(), now)
                    .await
            }
        }
    }

    async fn started<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        key: &AgentKey,
        (pane, recovery_id, result): (String, Option<String>, Result<(), HerdrError>),
        now: Timestamp,
    ) -> Result<()> {
        // A result for a pane the agent left (a restart, a close) is dropped.
        let current = self
            .agent_record(run, key)
            .is_ok_and(|a| a.status == AgentStatus::Open && a.pane_id == pane)
            && run.record().is_ok_and(|r| {
                r.status == Status::Active
                    && !r.stopped
                    && !r.finished
                    && r.awaiting_reply.is_none()
            });
        if !current {
            return Ok(());
        }
        if let Some(id) = recovery_id {
            let expected = self.agent_record(run, key)?.recovery;
            if !matches!(&expected, crate::run::Recovery::Starting { request_id, .. } if *request_id == id)
            {
                return Ok(());
            }
            let (failed, reason) = match result {
                Ok(()) | Err(HerdrError::OutcomeUnknown(_)) => return Ok(()),
                Err(HerdrError::NotSent(_)) => {
                    (true, "Herdr did not send the resume request".to_string())
                }
                Err(error) => (true, error.to_string()),
            };
            if failed {
                let mut stale = None;
                let mut due = None;
                if let crate::run::Recovery::Starting {
                    attempt, session, ..
                } = self.agent_record(run, key)?.recovery
                {
                    if attempt >= 3 {
                        stale = Some(attempt);
                    } else {
                        let next = attempt + 1;
                        due = Some((
                            next,
                            now + jiff::SignedDuration::from_secs(if next == 2 { 30 } else { 60 }),
                            session,
                        ));
                    }
                }
                if let Some(attempts) = stale {
                    self.mark_stale(d, key, expected, attempts, reason).await?;
                } else if let Some((attempt, due, session)) = due {
                    self.retry_failed_start(run, key, expected, attempt, due, session)
                        .await?;
                }
            }
            return Ok(());
        }
        match result {
            // Herdr may not show the agent yet: it is not started again while
            // the grace lasts.
            Ok(()) | Err(HerdrError::OutcomeUnknown(_)) => {
                self.launched.insert(key.clone(), (pane, now));
                Ok(())
            }
            Err(HerdrError::NotSent(_)) => {
                self.not_sent.insert(key.clone(), now);
                Ok(())
            }
            Err(error) => {
                self.attempt_failed(run, key, Some(pane), &error.to_string(), now)
                    .await
            }
        }
    }

    /// Counts a failed placement (`pane` none: the agent must still be
    /// pending) or start (the agent must still be open in `pane`) of an
    /// active run; the third fails the agent. Decided under the lock.
    async fn attempt_failed(
        &mut self,
        run: &Run,
        key: &AgentKey,
        pane: Option<String>,
        error: &str,
        now: Timestamp,
    ) -> Result<()> {
        let (message, at) = (error.to_string(), now.to_string());
        let activity = error_activity(format!("Could not start the {} agent: {error}", key.role()));
        let worker_id = key.worker.clone();
        self.guarded(run, move |run, lock| {
            let record = run.record()?;
            let agent = match &worker_id {
                None => record.coordinator.clone(),
                Some(id) => worker::load(run, id)?.agent,
            };
            let current = match &pane {
                None => agent.status == AgentStatus::Pending,
                Some(pane) => agent.status == AgentStatus::Open && agent.pane_id == *pane,
            };
            if record.status != Status::Active || !current {
                return Ok(((), Vec::new()));
            }
            let count = |a: &mut AgentRecord| {
                a.launch_attempts += 1;
                a.last_attempt_at = at;
                if a.launch_attempts >= MAX_LAUNCH_ATTEMPTS {
                    a.status = AgentStatus::Failed;
                    a.error = message;
                }
            };
            let failed = match &worker_id {
                None => {
                    run.update_held(lock, |r| count(&mut r.coordinator))?
                        .coordinator
                }
                Some(id) => worker::update_held(run, lock, id, |w| count(&mut w.agent))?.agent,
            }
            .status
                == AgentStatus::Failed;
            Ok(((), if failed { vec![activity] } else { Vec::new() }))
        })
        .await
    }

    /// A start Herdr has not detected once the grace ended counts as an
    /// unsuccessful attempt, so a start that never shows ends after three.
    async fn expire_undetected(
        &mut self,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let expired: Vec<(AgentKey, String)> = self
            .launched
            .iter()
            .filter(|(key, (_, at))| {
                key.run == run.key && now.duration_since(*at) >= DETECTION_GRACE
            })
            .map(|(key, (pane, _))| (key.clone(), pane.clone()))
            .collect();
        for (key, pane) in expired {
            self.launched.remove(&key);
            if pane_is_empty(snapshot, &pane) {
                let grace = DETECTION_GRACE.as_secs();
                let error = format!("Herdr did not detect the agent within {grace} s");
                self.attempt_failed(run, &key, Some(pane), &error, now)
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) fn agent_record(&self, run: &Run, key: &AgentKey) -> Result<AgentRecord> {
        Ok(match &key.worker {
            None => run.record()?.coordinator,
            Some(id) => worker::load(run, id)?.agent,
        })
    }

    async fn retry_failed_start(
        &mut self,
        run: &Run,
        key: &AgentKey,
        expected: crate::run::Recovery,
        attempt: u8,
        due: Timestamp,
        session: String,
    ) -> Result<()> {
        let worker_id = key.worker.clone();
        self.guarded(run, move |run, lock| {
            let record = run.record()?;
            if record.status != Status::Active
                || record.stopped
                || record.finished
                || record.awaiting_reply.is_some()
            {
                return Ok((false, Vec::new()));
            }
            let matches = match &worker_id {
                None => {
                    record.coordinator.recovery == expected
                        && record.coordinator.status == AgentStatus::Open
                }
                Some(id) => {
                    let w = worker::load(run, id)?;
                    w.agent.recovery == expected
                        && w.agent.status == AgentStatus::Open
                        && !w.restarting
                        && w.report_hash.is_empty()
                        && worker::report_hash(&w).is_none()
                }
            };
            if !matches {
                return Ok((false, Vec::new()));
            }
            let change = |a: &mut AgentRecord| {
                a.recovery = crate::run::Recovery::RetryWait {
                    attempt,
                    due_at: due.to_string(),
                    session,
                }
            };
            match worker_id {
                None => {
                    run.update_held(lock, |r| change(&mut r.coordinator))?;
                }
                Some(id) => {
                    worker::update_held(run, lock, &id, |w| change(&mut w.agent))?;
                }
            }
            Ok((true, Vec::new()))
        })
        .await?;
        Ok(())
    }

    async fn start_recovery<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        key: &AgentKey,
        agent: &AgentRecord,
        trusted: &[String],
        now: Timestamp,
    ) -> Result<()> {
        let crate::run::Recovery::RetryWait {
            attempt,
            due_at,
            session,
        } = &agent.recovery
        else {
            return Ok(());
        };
        if agent.status != AgentStatus::Open
            || agent.prompt_pending
            || self.in_flight.contains_key(key)
            || !pane_is_empty(snapshot, &agent.pane_id)
            || self.waits(key, agent, now)
            || now
                < due_at
                    .parse::<Timestamp>()
                    .unwrap_or(now + jiff::SignedDuration::from_secs(3600))
        {
            return Ok(());
        }
        let current_run = Run::load(&d.ctx.runs_dir(), &key.run)?;
        let run_state = current_run.record()?;
        if run_state.status != Status::Active
            || run_state.stopped
            || run_state.finished
            || run_state.awaiting_reply.is_some()
        {
            return Ok(());
        }
        if !generated_profile_allowed(d, &run_state, agent, key.worker.is_some(), false)? {
            block_revoked_profile(&current_run, key)?;
            d.log.line(&format!("{}: blocked a pinned generated profile after its routing authorization was revoked", key.run));
            return Ok(());
        }
        if let Some(id) = &key.worker {
            let w = worker::load(&current_run, id)?;
            if w.restarting
                || !w.report_hash.is_empty()
                || worker::report_hash(&w).is_some()
                || w.agent.last_group == "waiting_on_you"
                || crate::progress::load(&d.ctx.state_dir(), d.socket, &w.agent.pane_id)
                    .is_some_and(|r| r.waiting())
            {
                return Ok(());
            }
        }
        if agents::resume_args(&agent.kind, session).is_none() {
            self.mark_stale(
                d,
                key,
                agent.recovery.clone(),
                *attempt,
                "native session cannot be resumed".into(),
            )
            .await?;
            return Ok(());
        }
        let mut resumed = agent.clone();
        resumed.resume = true;
        resumed.agent_session = session.clone();
        let args = start_args(d.config, &resumed, session)?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (session, pane, at) = (session.clone(), agent.pane_id.clone(), now.to_string());
        let count = *attempt;
        let expected = agent.recovery.clone();
        let reserve_key = key.clone();
        let reserve_pane = pane.clone();
        let reserve_id = request_id.clone();
        let reserve_at = at.clone();
        let reserve_session = session.clone();
        let reserve_id_worker = reserve_id.clone();
        let reserve_at_worker = reserve_at.clone();
        let reserve_session_worker = reserve_session.clone();
        let reserve_socket = d.socket.to_string();
        let reserve_state_dir = d.ctx.state_dir();
        let run = Run::load(&d.ctx.runs_dir(), &key.run)?;
        let reserved = self
            .guarded(&run, move |run, lock| {
                let record = run.record()?;
                if record.status != Status::Active
                    || record.stopped
                    || record.finished
                    || record.awaiting_reply.is_some()
                    || (reserve_key.worker.is_none() && record.turn_complete.is_some())
                {
                    return Ok((false, Vec::new()));
                }
                let ok = match &reserve_key.worker {
                    None => {
                        let a = &record.coordinator;
                        if record.coordinator_lost
                            || a.status != AgentStatus::Open
                            || a.pane_id != reserve_pane
                            || a.recovery != expected
                        {
                            false
                        } else {
                            run.update_held(lock, |r| {
                                r.coordinator.recovery = crate::run::Recovery::Starting {
                                    attempt: count,
                                    request_id: reserve_id,
                                    since: reserve_at,
                                    session: reserve_session.clone(),
                                };
                                r.coordinator.resume = true;
                                r.coordinator.agent_session = reserve_session.clone();
                            })?;
                            true
                        }
                    }
                    Some(id) => {
                        let w = worker::load(run, id)?;
                        if w.restarting
                            || !w.report_hash.is_empty()
                            || worker::report_hash(&w).is_some()
                            || w.agent.last_group == "waiting_on_you"
                            || crate::progress::load(
                                &reserve_state_dir,
                                &reserve_socket,
                                &w.agent.pane_id,
                            )
                            .is_some_and(|r| r.waiting())
                            || w.agent.status != AgentStatus::Open
                            || w.agent.pane_id != reserve_pane
                            || w.agent.recovery != expected
                        {
                            false
                        } else {
                            worker::update_held(run, lock, id, |w| {
                                w.agent.recovery = crate::run::Recovery::Starting {
                                    attempt: count,
                                    request_id: reserve_id_worker,
                                    since: reserve_at_worker,
                                    session: reserve_session_worker.clone(),
                                };
                                w.agent.resume = true;
                                w.agent.agent_session = reserve_session_worker.clone();
                            })?;
                            true
                        }
                    }
                };
                Ok((ok, Vec::new()))
            })
            .await?;
        if !reserved {
            return Ok(());
        }
        if d.config.claude.auto_accept_trust_dialog && agent.kind == "claude" {
            let dirs: Vec<&str> = trusted.iter().map(String::as_str).collect();
            if let Err(error) = claude_trust::trust(d.ctx.env, &dirs) {
                d.fail(&key.run, &error);
            }
        }
        // Trust setup can take time. A stop, manual restart or report may
        // have invalidated the reservation before the native request starts.
        let run = Run::load(&d.ctx.runs_dir(), &key.run)?;
        let expected_start = crate::run::Recovery::Starting {
            attempt: count,
            request_id: request_id.clone(),
            since: at.clone(),
            session: session.clone(),
        };
        let worker_id = key.worker.clone();
        let pane_check = pane.clone();
        let (state_dir, socket) = (d.ctx.state_dir(), d.socket.to_string());
        let still_eligible = self
            .guarded(&run, move |run, _lock| {
                let record = run.record()?;
                if record.status != Status::Active
                    || record.stopped
                    || record.finished
                    || record.awaiting_reply.is_some()
                    || (worker_id.is_none() && record.turn_complete.is_some())
                {
                    return Ok((false, Vec::new()));
                }
                let matches = match worker_id {
                    None => {
                        !record.coordinator_lost
                            && record.coordinator.status == AgentStatus::Open
                            && record.coordinator.pane_id == pane_check
                            && record.coordinator.recovery == expected_start
                    }
                    Some(id) => {
                        let w = worker::load(run, &id)?;
                        w.agent.status == AgentStatus::Open
                            && w.agent.pane_id == pane_check
                            && w.agent.recovery == expected_start
                            && !w.restarting
                            && w.report_hash.is_empty()
                            && worker::report_hash(&w).is_none()
                            && w.agent.last_group != "waiting_on_you"
                            && !crate::progress::load(&state_dir, &socket, &w.agent.pane_id)
                                .is_some_and(|r| r.waiting())
                    }
                };
                Ok((matches, Vec::new()))
            })
            .await?;
        if !still_eligible {
            return Ok(());
        }
        self.in_flight.insert(
            key.clone(),
            Effect::Start {
                request_id: Some(request_id.clone()),
            },
        );
        let (herdr, done) = (d.herdr.clone(), self.effects.clone());
        let (key, name, kind, recovery_id) = (
            key.clone(),
            agent.agent_name.clone(),
            agent.kind.clone(),
            request_id,
        );
        tokio::spawn(async move {
            let result = herdr
                .agent_start(&name, &kind, &PaneId(pane.clone()), &args)
                .await;
            let _ = done
                .send(EffectDone::Started {
                    key,
                    pane,
                    recovery_id: Some(recovery_id),
                    result,
                })
                .await;
        });
        Ok(())
    }

    async fn mark_stale<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        key: &AgentKey,
        expected: crate::run::Recovery,
        attempts: u8,
        reason: String,
    ) -> Result<()> {
        let worker_id = key.worker.clone();
        let (state_dir, socket) = (d.ctx.state_dir(), d.socket.to_string());
        self.guarded(
            &Run::load(&d.ctx.runs_dir(), &key.run)?,
            move |run, lock| {
                let current = match &worker_id {
                    None => run.record()?.coordinator.recovery,
                    Some(id) => worker::load(run, id)?.agent.recovery,
                };
                let run_record = run.record()?;
                if run_record.status != Status::Active
                    || run_record.stopped
                    || run_record.finished
                    || run_record.awaiting_reply.is_some()
                {
                    return Ok((false, Vec::new()));
                }
                if current != expected
                    || matches!(
                        current,
                        crate::run::Recovery::Stale { .. }
                            | crate::run::Recovery::None
                            | crate::run::Recovery::Recovered { .. }
                    )
                {
                    return Ok((false, Vec::new()));
                }
                let eligible = match &worker_id {
                    None => run.record()?.coordinator.status == AgentStatus::Open,
                    Some(id) => {
                        let worker = worker::load(run, id)?;
                        worker.agent.status == AgentStatus::Open
                            && !worker.restarting
                            && worker.report_hash.is_empty()
                            && worker::report_hash(&worker).is_none()
                            && worker.agent.last_group != "waiting_on_you"
                            && !crate::progress::load(&state_dir, &socket, &worker.agent.pane_id)
                                .is_some_and(|r| r.waiting())
                    }
                };
                if !eligible {
                    return Ok((false, Vec::new()));
                }
                let stale = crate::run::Recovery::Stale {
                    attempts,
                    reason,
                    reported: false,
                };
                match worker_id.clone() {
                    None => {
                        run.update_held(lock, |r| r.coordinator.recovery = stale)?;
                    }
                    Some(id) => {
                        worker::update_held(run, lock, &id, |w| w.agent.recovery = stale)?;
                    }
                }
                Ok((true, Vec::new()))
            },
        )
        .await?;
        Ok(())
    }

    fn start_in_flight(&self, run: &str) -> bool {
        self.in_flight
            .iter()
            .any(|(key, effect)| key.run == run && matches!(effect, Effect::Start { .. }))
    }

    pub(super) async fn launch<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        if let Some(orphan) = record.coordinator_orphans.first() {
            self.close_orphan(d, run, uuid::Uuid::nil(), orphan).await?;
            return Ok(());
        }
        if let Some(operation) = &record.coordinator_placement
            && let CoordinatorPlacementPhase::Orphan(placement) = &operation.phase
        {
            self.close_orphan(d, run, operation.id, placement).await?;
            return Ok(());
        }
        if !record.coordinator.herdr_session.is_empty()
            && record.coordinator.herdr_session != d.socket
        {
            d.log.line(&format!(
                "{}: coordinator belongs to Herdr socket {}; refusing to reinterpret its pane IDs in {}",
                run.key, record.coordinator.herdr_session, d.socket
            ));
            return Ok(());
        }
        if record.status != Status::Active {
            if let Some(operation) = &record.coordinator_placement
                && operation.session == d.socket
                && matches!(
                    &operation.phase,
                    CoordinatorPlacementPhase::Requested | CoordinatorPlacementPhase::Unknown
                )
            {
                let matches = placed_before(
                    snapshot,
                    &record.coordinator,
                    &operation.cwd,
                    operation.target_workspace.as_deref(),
                    if operation.target_workspace.is_some() {
                        coordinator::tab_label()
                    } else {
                        &operation.label
                    },
                );
                if let [placed] = matches.as_slice() {
                    let id = operation.id;
                    let orphan = CoordinatorPlacementRef {
                        session: operation.session.clone(),
                        workspace: placed.workspace.0.clone(),
                        tab: placed.tab.clone(),
                        pane: placed.pane.0.clone(),
                        cwd: placed.cwd.clone(),
                    };
                    update_run(run, move |r| {
                        if let Some(current) = &mut r.coordinator_placement
                            && current.id == id
                        {
                            current.phase = CoordinatorPlacementPhase::Orphan(orphan);
                        }
                    })
                    .await?;
                    if let Some(current) = run.record()?.coordinator_placement
                        && current.id == id
                        && let CoordinatorPlacementPhase::Orphan(orphan) = current.phase
                    {
                        self.close_orphan(d, run, id, &orphan).await?;
                    }
                }
            }
            return Ok(());
        }
        // Placing and starting judge panes absent or empty.
        let trusted = self.trusted;
        if trusted {
            self.expire_undetected(snapshot, run, now).await?;
        }
        let record = run.record()?;
        let coordinator_key = AgentKey::coordinator(&run.key);
        let c = &record.coordinator;
        if trusted
            && c.status == AgentStatus::Pending
            && !record.coordinator_misplaced
            && !c.profile.is_empty()
            && !self.in_flight.contains_key(&coordinator_key)
        {
            self.place(d, snapshot, run, &record, now).await?;
        }

        let record = run.record()?;
        let workers: Vec<_> = worker::list(run)
            .into_iter()
            .filter(|w| w.operation.is_none())
            .collect();
        let mut candidates: Vec<(AgentKey, AgentRecord, Vec<String>)> = vec![(
            coordinator_key,
            record.coordinator.clone(),
            vec![run.canonical_dir().to_string_lossy().into_owned()],
        )];
        candidates.extend(workers.iter().map(|w| {
            (
                AgentKey::worker(&run.key, &w.id),
                w.agent.clone(),
                vec![w.agent.cwd.clone(), w.repo_path.clone()],
            )
        }));
        if trusted && !self.start_in_flight(&run.key) {
            for (key, agent, trusted) in &candidates {
                if record.coordinator_misplaced {
                    break;
                }
                self.start_recovery(d, snapshot, key, agent, trusted, now)
                    .await?;
                if self.in_flight.contains_key(key) {
                    break;
                }
                if self.start(d, snapshot, key, agent, trusted, now)? {
                    break;
                }
            }
        }
        if !record.coordinator_misplaced {
            self.prompt_coordinator(d, snapshot, run, now).await?;
        }
        for w in &workers {
            self.prompt_worker(d, snapshot, run, &record, w, now)
                .await?;
        }
        Ok(())
    }

    /// Adopts a workspace whose creation was not answered, or creates one.
    async fn place<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        record: &RunRecord,
        now: Timestamp,
    ) -> Result<()> {
        // Rewritten at every placement, so an updated binary's path is what
        // the coordinator sees.
        let instructions = d
            .config
            .profiles
            .get(&record.coordinator.profile)
            .map_or(&[][..], |p| p.instructions.as_slice());
        coordinator::write_priming(run, record, &self.bin, instructions)?;
        let cwd = run.canonical_dir().to_string_lossy().into_owned();
        let existing_target = (!record.coordinator.workspace_id.is_empty()
            && snapshot
                .panes
                .values()
                .any(|pane| pane.workspace.0 == record.coordinator.workspace_id))
        .then(|| record.coordinator.workspace_id.clone());
        let operation = record.coordinator_placement.clone();
        if let Some(mut operation) = operation {
            if operation.session != d.socket {
                d.log.line(&format!(
                    "{}: coordinator placement belongs to Herdr socket {}; waiting for that session",
                    run.key, operation.session
                ));
                return Ok(());
            }
            if matches!(operation.phase, CoordinatorPlacementPhase::Requested) {
                // A process restart cannot tell whether the request was sent.
                // Reconcile by identity and never issue a duplicate create.
                operation.phase = CoordinatorPlacementPhase::Unknown;
                let id = operation.id;
                update_run(run, move |r| {
                    if let Some(current) = &mut r.coordinator_placement
                        && current.id == id
                    {
                        current.phase = CoordinatorPlacementPhase::Unknown;
                    }
                })
                .await?;
            }
            if matches!(operation.phase, CoordinatorPlacementPhase::Unknown) {
                let matches = placed_before(
                    snapshot,
                    &record.coordinator,
                    &operation.cwd,
                    operation.target_workspace.as_deref(),
                    if operation.target_workspace.is_some() {
                        coordinator::tab_label()
                    } else {
                        &operation.label
                    },
                );
                if matches.len() == 1 {
                    self.accept_observed_placement(d, run, &operation, &matches[0])
                        .await?;
                } else if matches.len() > 1 {
                    d.log.line(&format!(
                        "{}: coordinator placement is ambiguous ({} matching panes); refusing to create another workspace",
                        run.key,
                        matches.len()
                    ));
                }
                return Ok(());
            }
            return Ok(());
        }
        let matches = placed_before(
            snapshot,
            &record.coordinator,
            &cwd,
            existing_target.as_deref(),
            coordinator::tab_label(),
        );
        if matches.len() == 1 {
            let observed = CoordinatorPlacementOperation {
                id: uuid::Uuid::new_v4(),
                session: d.socket.to_string(),
                cwd: cwd.clone(),
                label: coordinator::workspace_label(record),
                target_workspace: Some(matches[0].workspace.0.clone()),
                phase: CoordinatorPlacementPhase::Requested,
            };
            update_run(run, {
                let observed = observed.clone();
                move |r| {
                    if r.coordinator_placement.is_none() {
                        r.coordinator_placement = Some(observed);
                    }
                }
            })
            .await?;
            self.accept_observed_placement(d, run, &observed, &matches[0])
                .await?;
            return Ok(());
        }
        if matches.len() > 1 {
            d.log.line(&format!(
                "{}: found {} matching coordinator panes; refusing to choose one",
                run.key,
                matches.len()
            ));
            return Ok(());
        }
        let key = AgentKey::coordinator(&run.key);
        if self.waits(&key, &record.coordinator, now) {
            return Ok(());
        }
        let operation = CoordinatorPlacementOperation {
            id: uuid::Uuid::new_v4(),
            session: d.socket.to_string(),
            cwd: cwd.clone(),
            label: coordinator::workspace_label(record),
            target_workspace: existing_target,
            phase: CoordinatorPlacementPhase::Requested,
        };
        let operation_id = operation.id;
        let operation_session = operation.session.clone();
        let operation_to_store = operation.clone();
        let updated = update_run(run, move |r| {
            if r.status == Status::Active
                && r.coordinator.status == AgentStatus::Pending
                && r.coordinator_placement.is_none()
            {
                r.coordinator_placement = Some(operation_to_store);
            }
        })
        .await?;
        if updated
            .coordinator_placement
            .as_ref()
            .is_none_or(|current| current.id != operation_id)
        {
            return Ok(());
        }
        self.not_sent.remove(&key);
        self.in_flight.insert(key.clone(), Effect::Place);
        let (herdr, done) = (d.herdr.clone(), self.effects.clone());
        let target_workspace = operation.target_workspace.map(WorkspaceId);
        let workspace_label = operation.label;
        tokio::spawn(async move {
            let result = match target_workspace {
                Some(workspace) => {
                    herdr
                        .tab_create(&workspace, &cwd, coordinator::tab_label())
                        .await
                }
                None => herdr.workspace_create(&cwd, &workspace_label).await,
            };
            let _ = done
                .send(EffectDone::Placed {
                    key,
                    operation_id,
                    session: operation_session,
                    result,
                })
                .await;
        });
        Ok(())
    }

    async fn accept_observed_placement<H: Herdr>(
        &self,
        d: &Deps<'_, H>,
        run: &Run,
        operation: &CoordinatorPlacementOperation,
        placed: &Placed,
    ) -> Result<()> {
        let id = operation.id;
        let placed = placed.clone();
        let accepted_placed = placed.clone();
        let session = operation.session.clone();
        let orphan = CoordinatorPlacementRef {
            session: session.clone(),
            workspace: placed.workspace.0.clone(),
            tab: placed.tab.clone(),
            pane: placed.pane.0.clone(),
            cwd: placed.cwd.clone(),
        };
        let updated = update_run(run, move |r| {
            if let Some(current) = &mut r.coordinator_placement
                && current.id == id
            {
                if r.status == Status::Active && r.coordinator.status == AgentStatus::Pending {
                    place_coordinator(r, &accepted_placed, &session);
                    r.coordinator_placement = None;
                } else {
                    current.phase = CoordinatorPlacementPhase::Orphan(orphan);
                }
            }
        })
        .await?;
        if !updated.coordinator_tab_named
            && updated.coordinator.herdr_session == d.socket
            && updated.coordinator.pane_id == placed.pane.0
            && d.herdr
                .tab_rename(&placed.tab, coordinator::tab_label())
                .await
                .is_ok()
        {
            let pane = placed.pane.0.clone();
            update_run(run, move |r| {
                if r.coordinator.pane_id == pane {
                    r.coordinator_tab_named = true;
                }
            })
            .await?;
        }
        if let Some(current) = updated.coordinator_placement
            && current.id == id
            && let CoordinatorPlacementPhase::Orphan(orphan) = current.phase
        {
            self.close_orphan(d, run, id, &orphan).await?;
        }
        Ok(())
    }

    async fn close_orphan<H: Herdr>(
        &self,
        d: &Deps<'_, H>,
        run: &Run,
        operation_id: uuid::Uuid,
        orphan: &CoordinatorPlacementRef,
    ) -> Result<()> {
        let pane = PaneId(orphan.pane.clone());
        let result = if orphan.session == d.socket {
            d.herdr.pane_close(&pane).await
        } else if let Some(herdr) = d.herdr.for_socket(&orphan.session) {
            herdr.pane_close(&pane).await
        } else {
            return Ok(());
        };
        let closed = match result {
            Ok(()) => true,
            Err(HerdrError::Api { code, .. }) if code == "pane_not_found" => true,
            Err(_) => false,
        };
        if closed {
            let orphan = orphan.clone();
            update_run(run, move |r| {
                if operation_id.is_nil() {
                    r.coordinator_orphans.retain(|saved| saved != &orphan);
                } else if r.coordinator_placement.as_ref().is_some_and(|operation| {
                    operation.id == operation_id
                        && matches!(operation.phase, CoordinatorPlacementPhase::Orphan(_))
                }) {
                    r.coordinator_placement = None;
                }
            })
            .await?;
        }
        Ok(())
    }

    /// Starts an open agent whose launch prompt is pending in its pane, when
    /// the pane is at its shell prompt. Returns whether a start went out.
    fn start<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        key: &AgentKey,
        agent: &AgentRecord,
        trusted: &[String],
        now: Timestamp,
    ) -> Result<bool> {
        if agent.status != AgentStatus::Open
            || !agent.prompt_pending
            || self.in_flight.contains_key(key)
            || !pane_is_empty(snapshot, &agent.pane_id)
            || self.waits(key, agent, now)
        {
            return Ok(false);
        }
        if let Some((pane, at)) = self.launched.get(key)
            && *pane == agent.pane_id
            && now.duration_since(*at) < DETECTION_GRACE
        {
            return Ok(false);
        }
        // Recorded before the start goes out, so a session begun by a start
        // whose answer is lost is still known.
        let run = Run::load(&d.ctx.runs_dir(), &key.run)?;
        let run_record = run.record()?;
        if !generated_profile_allowed(
            d,
            &run_record,
            agent,
            key.worker.is_some(),
            agent.started_at.is_empty() && !agent.resume,
        )? {
            block_revoked_profile(&run, key)?;
            d.log.line(&format!("{}: blocked a pinned generated profile after its routing authorization was revoked", key.run));
            return Ok(true);
        }
        let session = start_session(agent);
        let args = start_args(d.config, agent, &session)?;
        let at = now.to_string();
        let set = |a: &mut AgentRecord| {
            a.agent_session = session.clone();
            a.started_at = at.clone();
        };
        match &key.worker {
            None => run.update(|r| set(&mut r.coordinator)).map(|_| ())?,
            Some(id) => worker::update(&run, id, |w| set(&mut w.agent)).map(|_| ())?,
        }
        if d.config.claude.auto_accept_trust_dialog && agent.kind == "claude" {
            let dirs: Vec<&str> = trusted.iter().map(String::as_str).collect();
            if let Err(error) = claude_trust::trust(d.ctx.env, &dirs) {
                d.fail(&key.run, &error);
            }
        }
        self.launched.remove(key);
        self.not_sent.remove(key);
        self.in_flight
            .insert(key.clone(), Effect::Start { request_id: None });
        let (herdr, done) = (d.herdr.clone(), self.effects.clone());
        let (key, name, kind, pane) = (
            key.clone(),
            agent.agent_name.clone(),
            agent.kind.clone(),
            agent.pane_id.clone(),
        );
        tokio::spawn(async move {
            let result = herdr
                .agent_start(&name, &kind, &PaneId(pane.clone()), &args)
                .await;
            let _ = done
                .send(EffectDone::Started {
                    key,
                    pane,
                    recovery_id: None,
                    result,
                })
                .await;
        });
        Ok(true)
    }

    async fn prompt_coordinator<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let current = run.record()?;
        let c = &current.coordinator;
        if c.status != AgentStatus::Open
            || !c.prompt_pending
            || current.status != crate::run::Status::Active
            || current.stopped
            || current.finished
            || current.awaiting_reply.is_some()
        {
            return Ok(());
        }
        let Some(agent) = worker::find_agent(c, &snapshot.agents).filter(|a| a.status.is_idle())
        else {
            return Ok(());
        };
        self.launched.remove(&AgentKey::coordinator(&run.key));
        let text = coordinator::launch_prompt(&run.key, c.resume);
        delivered(d.herdr.agent_prompt(&agent.pane, &text).await)?;
        self.prompted.insert(run.key.clone());
        let seq = agent.state_change_seq;
        update_run(run, move |r| {
            r.coordinator.prompt_pending = false;
            r.coordinator.prompted_at = now.to_string();
            r.coordinator.prompted_seq = seq;
        })
        .await?;
        Ok(())
    }

    /// The worker's launch prompt; the `Start worker` action goes to Linear
    /// once it is delivered.
    async fn prompt_worker<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        record: &RunRecord,
        w: &worker::Worker,
        now: Timestamp,
    ) -> Result<()> {
        if w.agent.status != AgentStatus::Open || !w.agent.prompt_pending || record.stopped {
            return Ok(());
        }
        let Some(agent) =
            worker::find_agent(&w.agent, &snapshot.agents).filter(|a| a.status.is_idle())
        else {
            return Ok(());
        };
        self.launched.remove(&AgentKey::worker(&run.key, &w.id));
        let prompt = worker::launch_prompt(&run.key, &w.id);
        delivered(d.herdr.agent_prompt(&agent.pane, &prompt).await)?;
        let seq = agent.state_change_seq;
        let id = w.id.clone();
        self.guarded(run, move |run, lock| {
            let w = worker::update_held(run, lock, &id, |w| {
                w.agent.prompt_pending = false;
                w.agent.prompted_at = now.to_string();
                w.agent.prompted_seq = seq;
            })?;
            Ok(((), vec![worker::start_action(&w)]))
        })
        .await
    }
}

fn generated_profile_allowed<H>(
    d: &Deps<'_, H>,
    run: &RunRecord,
    agent: &AgentRecord,
    worker: bool,
    require_active: bool,
) -> Result<bool> {
    if !agent.profile.starts_with("generated-") {
        return Ok(true);
    }
    let scope = SupervisorScope {
        workspace: run.workspace.clone(),
        team: run.team_key.clone(),
        routing: d
            .config
            .team(&run.workspace, &run.team_key)?
            .routing
            .clone(),
    };
    SupervisorStore::new(d.ctx.supervisor_dir()).profile_is_authorized(
        d.config,
        &agent.profile,
        &scope,
        if worker {
            crate::config::GeneratedRole::Worker
        } else {
            crate::config::GeneratedRole::Coordinator
        },
        require_active,
    )
}

fn block_revoked_profile(run: &Run, key: &AgentKey) -> Result<()> {
    let message = "generated profile base is no longer authorized for this routing";
    match &key.worker {
        None => {
            run.update(|record| {
                record.coordinator.status = AgentStatus::Failed;
                record.coordinator.prompt_pending = false;
                record.coordinator.recovery = crate::run::Recovery::None;
                record.coordinator.error = message.into();
            })?;
        }
        Some(id) => {
            worker::update(run, id, |worker| {
                worker.agent.status = AgentStatus::Failed;
                worker.agent.prompt_pending = false;
                worker.agent.recovery = crate::run::Recovery::None;
                worker.agent.error = message.into();
            })?;
        }
    };
    Ok(())
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use crate::herdr::{Pane, WorkspaceId};
    use std::collections::BTreeMap;

    #[test]
    fn coordinator_adoption_is_canonical_unique_and_workspace_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("run");
        std::fs::create_dir(&cwd).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&cwd, &alias).unwrap();
        let pane = |workspace: &str, id: &str, path: &std::path::Path| Pane {
            id: PaneId(id.into()),
            workspace: WorkspaceId(workspace.into()),
            tab: format!("{workspace}:tab"),
            terminal: format!("term-{id}"),
            cwd: Some(path.to_string_lossy().into_owned()),
            foreground_cwd: None,
            label: None,
        };
        let mut snapshot = Snapshot {
            version: "0.9.1".into(),
            protocol: 22,
            tabs: BTreeMap::new(),
            panes: BTreeMap::from([
                (PaneId("w1:p1".into()), pane("w1", "w1:p1", &alias)),
                (PaneId("w2:p1".into()), pane("w2", "w2:p1", &cwd)),
            ]),
            agents: Vec::new(),
            skipped: 0,
        };
        let record = AgentRecord {
            kind: "claude".into(),
            agent_name: "coordinator".into(),
            ..AgentRecord::default()
        };
        let cwd = cwd.to_string_lossy();
        snapshot.tabs = BTreeMap::from([
            (
                "w1:tab".into(),
                crate::herdr::Tab {
                    tab_id: "w1:tab".into(),
                    workspace_id: "w1".into(),
                    label: "coordinator".into(),
                },
            ),
            (
                "w2:tab".into(),
                crate::herdr::Tab {
                    tab_id: "w2:tab".into(),
                    workspace_id: "w2".into(),
                    label: "coordinator".into(),
                },
            ),
        ]);
        assert_eq!(
            placed_before(&snapshot, &record, &cwd, None, "coordinator").len(),
            2
        );
        let only = placed_before(&snapshot, &record, &cwd, Some("w2"), "coordinator");
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].workspace.0, "w2");
        assert!(placed_before(&snapshot, &record, &cwd, Some("missing"), "coordinator").is_empty());
    }
}
