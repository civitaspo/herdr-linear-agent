//! Watching coordinators and workers from a snapshot, and the heartbeat.

use anyhow::Result;
use jiff::Timestamp;

use super::reconcile::{
    COORDINATOR_IDLE, Deps, HEARTBEAT, Reconciler, apply_tracked, elicitation, error_activity,
    inbox_item, since, update_run, update_worker,
};
use crate::agents;
use crate::herdr::{Herdr, Snapshot};
use crate::linear::api::{Activity, Content, ExternalUrl};
use crate::outbox::{self, Op};
use crate::run::{AgentRecord, AgentStatus, Run};
use crate::worker::{self, Group, Live, Worker};
use crate::{coordinator, files, inbox};

/// Why a worker waits on a person, for its inbox item.
fn waiting_reason(w: &Worker, live: &Live) -> String {
    let asked = live
        .self_report
        .as_ref()
        .is_some_and(crate::progress::Record::waiting);
    if w.agent.status == AgentStatus::Failed {
        format!("failed: {}", w.agent.error)
    } else if !live.pane_exists {
        "its pane closed before it wrote a report".into()
    } else if asked {
        "it asked a question in its report".into()
    } else if worker::needs_person(&w.agent, live) {
        format!("it waits on a dialog in pane {}", w.agent.pane_id)
    } else {
        live.agent_state
            .map_or_else(|| "no agent".into(), |s| s.as_str().to_string())
    }
}

impl Reconciler {
    /// Keeps an agent record in step with Herdr: a renumbered pane, a lost
    /// name, the native session, and the status with when its episode began.
    /// The same status with a new `state_change_seq` is a new episode.
    async fn track<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        record: &AgentRecord,
        now: Timestamp,
    ) -> (AgentRecord, Live) {
        let seen = self.live(d, snapshot, record, now);
        let mut next = record.clone();
        if let Some((workspace, tab, pane)) = &seen.moved_to {
            next.workspace_id = workspace.clone();
            next.tab_id = tab.clone();
            next.pane_id = pane.clone();
        }
        if let Some(agent) = &seen.agent {
            if agent.name.as_deref().is_none_or(str::is_empty) && !record.agent_name.is_empty() {
                let _ = d.herdr.agent_rename(&agent.pane, &record.agent_name).await;
            }
            if let Some(session) = agent.session.as_deref().filter(|s| !s.is_empty()) {
                next.agent_session = session.to_string();
            }
        }
        let state = seen
            .agent_state
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let seq = seen.agent.as_ref().map_or(0, |a| a.state_change_seq);
        if state != record.last_state || seq != record.last_state_seq {
            next.last_state = state;
            next.last_state_seq = seq;
            next.last_state_change = now.to_string();
            next.blocked_reported = false;
        }
        let live = self.live(d, snapshot, &next, now);
        if !worker::needs_person(&next, &live) {
            next.blocked_reported = false;
        }
        (next, live)
    }

    /// Asks a person, once per episode, to answer a dialog in a Herdr pane.
    async fn ask_for_person<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        run: &Run,
        who: &str,
        record: &AgentRecord,
    ) -> Result<()> {
        let label = coordinator::workspace_label(&run.record()?);
        let body = format!(
            "{who} needs someone in Herdr: it is waiting on a dialog in pane `{}` (session `{}`, run {label}). Answer it there.",
            record.pane_id,
            d.config.herdr.session.as_deref().unwrap_or("default")
        );
        self.push(run, elicitation(body.clone(), &[])).await?;
        d.notify(&format!("{} needs you", run.key), &body).await;
        Ok(())
    }

    pub(super) async fn watch<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        let c = &record.coordinator;
        if c.status == AgentStatus::Open {
            let (mut next, live) = self.track(d, snapshot, c, now).await;
            let needs = worker::needs_person(&next, &live);
            if needs && !next.blocked_reported {
                self.ask_for_person(d, run, "The coordinator", &next)
                    .await?;
                next.blocked_reported = true;
            }
            let lost = !live.pane_exists && !record.coordinator_lost;
            if lost {
                let resumable = !next.agent_session.is_empty()
                    && agents::resume_args(&next.kind, &next.agent_session).is_some();
                let how = if resumable {
                    "Reply `resume` to start it again with its previous session."
                } else {
                    "Reply `resume` to start a new coordinator."
                };
                self.push(
                    run,
                    elicitation(
                        format!("The coordinator's pane for {} is gone. {how}", run.key),
                        &[("Resume", "resume")],
                    ),
                )
                .await?;
                d.notify(&format!("{} coordinator is gone", run.key), how)
                    .await;
            } else if live.pane_exists {
                let state = if needs {
                    "needs you"
                } else {
                    live.agent_state.map_or("starting", |s| s.as_str())
                };
                let display = format!("{} \u{b7} coordinator", run.key);
                self.report_pane(d, snapshot, &next.pane_id, &display, state, now)
                    .await;
            }
            if next != *c || lost {
                let before = c.clone();
                update_run(run, move |r| {
                    apply_tracked(&mut r.coordinator, &before, &next);
                    r.coordinator_lost |= lost;
                })
                .await?;
            }
        }
        for w in worker::list(run)
            .into_iter()
            .filter(|w| matches!(w.agent.status, AgentStatus::Open | AgentStatus::Failed))
        {
            self.watch_worker(d, snapshot, run, &w, now).await?;
        }
        Ok(())
    }

    async fn watch_worker<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        w: &Worker,
        now: Timestamp,
    ) -> Result<()> {
        let (tracked, live) = self.track(d, snapshot, &w.agent, now).await;
        let mut next = w.clone();
        next.agent = tracked;

        // A report written in this pass counts for the group at once.
        if let Some(hash) = worker::report_hash(w).filter(|h| *h != w.report_hash) {
            worker::copy_report_home(run, w)?;
            next.report_hash = hash;
            let report =
                std::fs::read_to_string(worker::home_report_path(run, &w.id)).unwrap_or_default();
            if let Some(url) = worker::pr_line(&report).filter(|url| *url != w.pr_url) {
                self.push(
                    run,
                    Op::Activity {
                        activity: Activity::new(Content::Action {
                            action: "Pull request".into(),
                            parameter: format!("{url} (worker {}, repo {})", w.id, w.repo),
                            result: None,
                        }),
                    },
                )
                .await?;
                let (label, added) = (format!("{} {} PR", w.id, w.repo), url.clone());
                let urls = update_run(run, move |r| {
                    if !r.external_urls.iter().any(|u| u.url == added) {
                        r.external_urls.push(ExternalUrl { label, url: added });
                    }
                })
                .await?
                .external_urls;
                self.push(run, Op::ExternalUrls { urls }).await?;
                next.pr_url = url;
            }
        }

        let group = worker::group(&next, &live);
        if group.token() != w.agent.last_group {
            let summary = match group {
                Group::WaitingOnYou => Some(format!(
                    "{} ({}) is Waiting on you: {}.",
                    w.id,
                    w.repo,
                    waiting_reason(&next, &live)
                )),
                Group::Idle => Some(format!(
                    "{} ({}) is idle without a report; check its pane {}.",
                    w.id, w.repo, next.agent.pane_id
                )),
                Group::Reported | Group::Working => None,
            };
            if let Some(summary) = summary {
                inbox_item(run, "worker", &w.id, summary).await?;
            }
            next.agent.last_group = group.token().to_string();
        }
        if group == Group::Reported && next.report_hash != w.announced_report_hash {
            inbox_item(
                run,
                "worker",
                &w.id,
                format!(
                    "{} ({}) has a new report: workers/{}.md",
                    w.id, w.repo, w.id
                ),
            )
            .await?;
            next.announced_report_hash = next.report_hash.clone();
        }
        if worker::needs_person(&next.agent, &live) && !next.agent.blocked_reported {
            let who = format!("Worker {} ({})", w.id, w.repo);
            self.ask_for_person(d, run, &who, &next.agent).await?;
            next.agent.blocked_reported = true;
        }
        if w.agent.status == AgentStatus::Open
            && !live.pane_exists
            && next.report_hash.is_empty()
            && !w.gone_reported
        {
            self.push(
                run,
                error_activity(format!(
                    "Worker {} ({}) lost its pane before it wrote a report.",
                    w.id, w.repo
                )),
            )
            .await?;
            next.gone_reported = true;
        }
        if live.pane_exists {
            let display = format!("{} \u{b7} {} {}", run.key, w.id, w.title);
            self.report_pane(
                d,
                snapshot,
                &next.agent.pane_id,
                &display,
                group.label(),
                now,
            )
            .await;
        }
        if next != *w {
            let before = w.clone();
            update_worker(run, &w.id, move |r| {
                apply_tracked(&mut r.agent, &before.agent, &next.agent);
                macro_rules! changed {
                    ($($field:ident).+) => {
                        if before.$($field).+ != next.$($field).+ {
                            r.$($field).+ = next.$($field).+.clone();
                        }
                    };
                }
                changed!(agent.last_group);
                changed!(report_hash);
                changed!(announced_report_hash);
                changed!(pr_url);
                changed!(gone_reported);
            })
            .await?;
        }
        Ok(())
    }

    /// An ephemeral thought after 20 minutes without an activity keeps the
    /// session from going stale; past `run_timeout_hours` a person is asked
    /// whether to go on. Neither goes out without a session or while stopped.
    pub(super) async fn heartbeat<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snap: Option<&Snapshot>,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        if record.session_id.is_empty() || record.stopped {
            return Ok(());
        }
        let quiet = since(&record.last_activity, now).is_some_and(|d| d >= HEARTBEAT);
        if quiet && outbox::pending(run).is_empty() {
            let mut counts: Vec<(Group, usize)> = Vec::new();
            for w in worker::list(run)
                .iter()
                .filter(|w| w.agent.status == AgentStatus::Open)
            {
                let group = match snap {
                    Some(snapshot) => {
                        Some(worker::group(w, &self.live(d, snapshot, &w.agent, now)))
                    }
                    None => Group::from_token(&w.agent.last_group),
                };
                let Some(group) = group else { continue };
                match counts.iter_mut().find(|(g, _)| *g == group) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((group, 1)),
                }
            }
            let summary = if counts.is_empty() {
                "no workers".to_string()
            } else {
                counts
                    .iter()
                    .map(|(g, n)| format!("{n} {}", g.label().to_lowercase()))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut activity = Activity::new(Content::Thought {
                body: format!("Still on it: {summary}."),
            });
            activity.ephemeral = true;
            self.push(run, Op::Activity { activity }).await?;
        }
        let hours = d.config.limits.run_timeout_hours;
        let limit = i64::try_from(hours.saturating_mul(3600)).unwrap_or(i64::MAX);
        if !record.timeout_asked && files::seconds_since(&record.timeout_since, now) >= limit {
            self.push(
                run,
                elicitation(
                    format!(
                        "This run has been going for {hours} hours. Reply to let it continue; until then the coordinator gets no prompts."
                    ),
                    &[("Continue", "continue")],
                ),
            )
            .await?;
            update_run(run, |r| r.timeout_asked = true).await?;
            d.notify(
                &format!("{} ran {hours} hours", run.key),
                "Reply in the Linear session to let it continue.",
            )
            .await;
        }
        Ok(())
    }

    /// Prompts an idle coordinator once per set of unseen inbox items.
    pub(super) async fn nudge<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        run: &Run,
        now: Timestamp,
    ) -> Result<()> {
        let record = run.record()?;
        let c = &record.coordinator;
        if c.status != AgentStatus::Open
            || record.coordinator_lost
            || c.prompt_pending
            || record.stopped
            || record.timeout_asked
        {
            return Ok(());
        }
        let Some(agent) = worker::find_agent(c, &snapshot.agents) else {
            return Ok(());
        };
        let idle_for = since(&c.last_state_change, now).unwrap_or_default();
        if !agent.status.is_idle()
            || c.last_state != agent.status.as_str()
            || idle_for < COORDINATOR_IDLE
        {
            return Ok(());
        }
        let seen = inbox::seen(run);
        let unseen: Vec<inbox::Item> = inbox::unhandled(run)
            .into_iter()
            .filter(|item| !seen.contains(&item.id))
            .collect();
        if unseen.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = unseen.iter().map(|i| i.id.as_str()).collect();
        let hash = files::sha256_hex(ids.join("\n").as_bytes());
        if self.nudged.get(&run.key) == Some(&hash) {
            return Ok(());
        }
        let text = if unseen.iter().any(|i| i.kind == "reply") {
            coordinator::NUDGE_REPLY
        } else {
            coordinator::NUDGE_INBOX
        };
        if d.herdr.agent_prompt(&agent.pane, text).await.is_ok() {
            self.nudged.insert(run.key.clone(), hash);
        }
        Ok(())
    }
}
