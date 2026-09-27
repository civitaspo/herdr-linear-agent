//! The reconciler: the one task that decides. Herdr events, Linear events,
//! effect results and timers only wake it; each pass reads one
//! `session.snapshot` and the run folders and acts on them.
//!
//! A pass, in order:
//!
//! 1. take one snapshot; the rules that read panes or agents run only when it
//!    succeeded
//! 2. apply the Linear events: sessions, sent activities, write failures, and
//!    run reads (close, detach, issue edits, relay)
//! 3. intake from the latest delegated list
//! 4. apply the results of effect tasks (placements, starts) and routing
//!    agents; they are facts and are applied even without a snapshot
//! 5. per active run: watch the coordinator and the workers, launch, nudge
//!    (with a snapshot), then the heartbeat and inbox pruning
//! 6. the write-failure notice, and progress records of gone panes
//!
//! Record writes are field-level read-modify-writes under the run lock, on a
//! blocking thread, after the Herdr request they follow; a record read
//! before an await is never saved whole.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use jiff::{SignedDuration, Timestamp};
use serde_json::json;
use tokio::sync::{Notify, mpsc, watch};

use super::Log;
use crate::config::{Config, Size};
use crate::herdr::{Herdr, HerdrError, Link, PaneId, Placed, Snapshot};
use crate::linear::api::{Activity, Content};
use crate::linear::task::{LinearEvent, LinearLevel, RunQuery};
use crate::outbox::{self, Op};
use crate::paths::Ctx;
use crate::run::{AgentRecord, AgentStatus, Run, RunLock, RunRecord, Status};
use crate::worker::{self, Live, Worker};
use crate::{coordinator, inbox, progress};

/// A blocked agent is asked about after this long (`worker::BLOCKED_SECS`).
const BLOCKED: SignedDuration = SignedDuration::from_secs(worker::BLOCKED_SECS);
/// A launch whose status stays unknown this long waits on a dialog.
const LAUNCH_DIALOG: SignedDuration = SignedDuration::from_secs(worker::LAUNCH_DIALOG_SECS);
/// An idle coordinator is nudged about unseen inbox items after this long.
pub(super) const COORDINATOR_IDLE: SignedDuration = SignedDuration::from_secs(60);
pub(super) const HEARTBEAT: SignedDuration = SignedDuration::from_secs(20 * 60);
const WRITE_FAILURE_NOTICE: SignedDuration = SignedDuration::from_secs(10 * 60);
/// Half the pane token TTL: a token is refreshed at least this often.
pub(super) const METADATA_REFRESH: SignedDuration = SignedDuration::from_secs(150);
/// The first wait after an unsuccessful placement or start; it doubles with
/// every counted attempt.
pub(super) const LAUNCH_SPACING: SignedDuration = SignedDuration::from_secs(15);
pub(super) const MAX_LAUNCH_ATTEMPTS: u32 = 3;
/// A started agent Herdr has not detected yet is not started again for this
/// long.
pub(super) const DETECTION_GRACE: SignedDuration = SignedDuration::from_secs(60);
/// A recorded pane this ticker has never seen is judged gone only after it
/// has been missing this long (Herdr may answer a placement before its pane
/// list shows the pane).
pub(super) const PANE_GRACE: SignedDuration = SignedDuration::from_secs(30);
const EFFECT_QUEUE: usize = 64;

/// Everything the loop reads from and writes to.
pub struct Inputs<'a, H> {
    pub ctx: &'a Ctx<'a>,
    pub config: &'a Config,
    pub herdr: H,
    /// The configured session's socket path, which keys progress records.
    pub socket: String,
    pub log: Arc<Log>,
    /// Wakes on every Herdr event.
    pub link: watch::Receiver<Link>,
    pub level: watch::Receiver<LinearLevel>,
    pub events: mpsc::Receiver<LinearEvent>,
    /// What the Linear task reads and flushes.
    pub queries: watch::Sender<Vec<RunQuery>>,
    /// `notify_one` after queuing outbox requests, so they go out at once.
    pub linear_wake: Arc<Notify>,
    /// Becomes `true` when the ticker should exit.
    pub shutdown: watch::Receiver<bool>,
    /// The present; nothing in a pass reads the wall clock.
    pub clock: fn() -> Timestamp,
}

/// One agent of one run: the coordinator, or a worker by id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgentKey {
    pub run: String,
    pub worker: Option<String>,
}

impl AgentKey {
    pub fn coordinator(run: &str) -> Self {
        AgentKey {
            run: run.into(),
            worker: None,
        }
    }

    pub fn worker(run: &str, id: &str) -> Self {
        AgentKey {
            run: run.into(),
            worker: Some(id.into()),
        }
    }

    /// How the error activity names the agent.
    pub fn role(&self) -> String {
        match &self.worker {
            None => "coordinator".into(),
            Some(id) => format!("worker {id}"),
        }
    }
}

/// The result of one slow Herdr request an effect task made.
#[derive(Debug)]
pub enum EffectDone {
    Placed {
        key: AgentKey,
        result: Result<Placed, HerdrError>,
    },
    Started {
        key: AgentKey,
        pane: String,
        result: Result<(), HerdrError>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    Place,
    Start,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RoutingOutcome {
    Answered(Size),
    TimedOut,
    /// The agent could not be started or waited for.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutingDone {
    pub key: String,
    pub outcome: RoutingOutcome,
}

/// What woke the pass, each handled exactly once.
#[derive(Debug, Default)]
pub struct Wake {
    pub events: Vec<LinearEvent>,
    pub effects: Vec<EffectDone>,
    pub routing: Vec<RoutingDone>,
}

/// What a pass borrows from the ticker.
pub struct Deps<'a, H> {
    pub ctx: &'a Ctx<'a>,
    pub config: &'a Config,
    pub herdr: &'a H,
    pub socket: &'a str,
    pub log: &'a Log,
}

impl<H> Deps<'_, H> {
    pub(super) fn fail(&self, key: &str, error: &anyhow::Error) {
        self.log.line(&format!("{key}: {error:#}"));
    }
}

impl<H: Herdr> Deps<'_, H> {
    /// A Herdr notification, when the config allows them.
    pub(super) async fn notify(&self, title: &str, body: &str) {
        if self.config.notifications.herdr {
            let _ = self.herdr.notification_show(title, body).await;
        }
    }
}

/// The reconciler's memory between passes. Everything that must survive a
/// restart is in the run folders.
pub struct Reconciler {
    pub(super) effects: mpsc::Sender<EffectDone>,
    pub(super) routing_done: mpsc::Sender<RoutingDone>,
    pub(super) bin: String,
    pub(super) started_up: bool,
    /// At most one effect per agent; at most one start per run.
    pub(super) in_flight: BTreeMap<AgentKey, Effect>,
    /// Runs whose routing agent runs.
    pub(super) routing: BTreeSet<String>,
    /// Agents started in a pane where Herdr has not detected them yet.
    pub(super) launched: BTreeMap<AgentKey, (String, Timestamp)>,
    pub(super) seen_panes: BTreeSet<String>,
    pub(super) missing_since: BTreeMap<String, Timestamp>,
    /// Per run, the hash of the unseen inbox ids last nudged about.
    pub(super) nudged: BTreeMap<String, String>,
    /// Per pane, the token last reported and when.
    pub(super) reported: BTreeMap<String, (String, Timestamp)>,
    /// Per issue, when its run last changed status, as the time of the fact
    /// that changed it. A read or a delegated list made earlier is stale.
    pub(super) changed_at: BTreeMap<String, Timestamp>,
    pub(super) intake_read: Option<Timestamp>,
    /// Runs whose outbox is blocked, by issue id, and since when.
    pub(super) failing: BTreeMap<String, Timestamp>,
    pub(super) failure_notified: bool,
    /// Per run, when a heartbeat was queued and no activity was reported
    /// sent since; the flush and its `ActivitySent` may be a pass apart.
    pub(super) heartbeats: BTreeMap<String, Timestamp>,
    pub(super) queued: bool,
    pub(super) queries: Vec<RunQuery>,
}

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| anyhow!("a file task failed: {e}"))?
}

pub(super) async fn update_run(
    run: &Run,
    change: impl FnOnce(&mut RunRecord) + Send + 'static,
) -> Result<RunRecord> {
    let run = run.clone();
    blocking(move || run.update(change)).await
}

pub(super) async fn update_worker(
    run: &Run,
    id: &str,
    change: impl FnOnce(&mut Worker) + Send + 'static,
) -> Result<Worker> {
    let (run, id) = (run.clone(), id.to_string());
    blocking(move || worker::update(&run, &id, change)).await
}

pub(super) async fn inbox_item(
    run: &Run,
    kind: &str,
    subject: &str,
    summary: String,
) -> Result<()> {
    let (run, kind, subject) = (run.clone(), kind.to_string(), subject.to_string());
    blocking(move || inbox::write(&run, &kind, &subject, &summary).map(|_| ())).await
}

pub(super) fn thought(body: impl Into<String>) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Thought { body: body.into() }),
    }
}

pub(super) fn elicitation(body: impl Into<String>, options: &[(&str, &str)]) -> Op {
    let mut activity = Activity::new(Content::Elicitation { body: body.into() });
    if !options.is_empty() {
        activity.signal = Some("select".into());
        let options: Vec<_> = options
            .iter()
            .map(|(label, value)| json!({ "label": label, "value": value }))
            .collect();
        activity.signal_metadata = Some(json!({ "options": options }));
    }
    Op::Activity { activity }
}

pub(super) fn error_activity(body: impl Into<String>) -> Op {
    Op::Activity {
        activity: Activity::new(Content::Error { body: body.into() }),
    }
}

pub(super) fn since(timestamp: &str, now: Timestamp) -> Option<SignedDuration> {
    timestamp
        .parse::<Timestamp>()
        .ok()
        .map(|at| now.duration_since(at))
}

fn after(timestamp: &str, wait: SignedDuration) -> Option<Timestamp> {
    timestamp.parse::<Timestamp>().ok().map(|at| at + wait)
}

/// The wait after `attempts` counted unsuccessful attempts: 15 s, doubling.
pub(super) fn spacing(attempts: u32) -> SignedDuration {
    let doublings = attempts.saturating_sub(1).min(8);
    LAUNCH_SPACING * (1_i32 << doublings)
}

/// When the next placement or start of this agent may be made.
pub(super) fn attempt_due(record: &AgentRecord) -> Option<Timestamp> {
    after(&record.last_attempt_at, spacing(record.launch_attempts))
}

/// The fields `track` owns, copied only where they changed, so a field
/// another process wrote meanwhile is kept.
pub(super) fn apply_tracked(target: &mut AgentRecord, before: &AgentRecord, after: &AgentRecord) {
    macro_rules! changed {
        ($($field:ident),*) => {$(
            if before.$field != after.$field {
                target.$field = after.$field.clone();
            }
        )*};
    }
    changed!(
        workspace_id,
        tab_id,
        pane_id,
        agent_session,
        last_state,
        last_state_seq,
        last_state_change,
        blocked_reported
    );
}

impl Reconciler {
    pub fn new(
        effects: mpsc::Sender<EffectDone>,
        routing_done: mpsc::Sender<RoutingDone>,
    ) -> Result<Self> {
        Ok(Reconciler {
            effects,
            routing_done,
            bin: coordinator::binary_command()?,
            started_up: false,
            in_flight: BTreeMap::new(),
            routing: BTreeSet::new(),
            launched: BTreeMap::new(),
            seen_panes: BTreeSet::new(),
            missing_since: BTreeMap::new(),
            nudged: BTreeMap::new(),
            reported: BTreeMap::new(),
            changed_at: BTreeMap::new(),
            intake_read: None,
            failing: BTreeMap::new(),
            failure_notified: false,
            heartbeats: BTreeMap::new(),
            queued: false,
            queries: Vec::new(),
        })
    }

    /// Whether an effect task or a routing agent is still running.
    #[cfg(test)]
    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty() || !self.routing.is_empty()
    }

    #[cfg(test)]
    pub fn effects_in_flight(&self) -> usize {
        self.in_flight.len()
    }

    #[cfg(test)]
    pub fn routing_in_flight(&self) -> usize {
        self.routing.len()
    }

    /// The queries for the Linear task, as of the last pass.
    pub fn queries(&self) -> &[RunQuery] {
        &self.queries
    }

    /// Whether the last pass queued outbox requests; clears the flag.
    pub fn take_queued(&mut self) -> bool {
        std::mem::take(&mut self.queued)
    }

    pub(super) async fn push(&mut self, run: &Run, op: Op) -> Result<()> {
        let run = run.clone();
        blocking(move || outbox::push(&run, op).map(|_| ())).await?;
        self.queued = true;
        Ok(())
    }

    /// One critical section under the run lock: `work` writes the fields
    /// that guard its Linear writes and returns those writes, which are
    /// queued before the lock is released. A failure after a write is
    /// queued can then never make the write go out twice.
    pub(super) async fn guarded<T: Send + 'static>(
        &mut self,
        run: &Run,
        work: impl FnOnce(&Run, &RunLock) -> Result<(T, Vec<Op>)> + Send + 'static,
    ) -> Result<T> {
        let run = run.clone();
        let (value, queued) = blocking(move || {
            let lock = run.lock()?;
            let (value, ops) = work(&run, &lock)?;
            let queued = !ops.is_empty();
            for op in ops {
                outbox::push_held(&run, &lock, op)?;
            }
            Ok((value, queued))
        })
        .await?;
        self.queued |= queued;
        Ok(value)
    }

    /// `update_run` whose change also returns the requests it guards.
    pub(super) async fn update_and_push(
        &mut self,
        run: &Run,
        change: impl FnOnce(&mut RunRecord) -> Vec<Op> + Send + 'static,
    ) -> Result<RunRecord> {
        self.guarded(run, move |run, lock| {
            let mut ops = Vec::new();
            let record = run.update_held(lock, |r| ops = change(r))?;
            Ok((record, ops))
        })
        .await
    }

    pub async fn pass<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        level: &LinearLevel,
        wake: Wake,
        now: Timestamp,
    ) {
        if !self.started_up {
            self.started_up = true;
            self.clear_old_routing(d).await;
        }
        let snapshot = d.herdr.snapshot().await.ok();
        let snap = snapshot.as_ref();
        if let Some(snapshot) = snap {
            for pane in snapshot.panes.keys() {
                self.seen_panes.insert(pane.0.clone());
                self.missing_since.remove(&pane.0);
            }
        }
        for event in wake.events {
            self.apply_event(d, level, snap, event, now).await;
        }
        self.intake(d, level, now).await;
        for effect in wake.effects {
            self.apply_effect(d, effect, now).await;
        }
        for done in wake.routing {
            self.apply_routing(d, done).await;
        }
        for run in Run::list(&d.ctx.runs_dir()) {
            if !run.record().is_ok_and(|r| r.status == Status::Active) {
                continue;
            }
            if let Some(snapshot) = snap {
                if let Err(error) = self.watch(d, snapshot, &run, now).await {
                    d.fail(&run.key, &error);
                }
                if let Err(error) = self.launch(d, snapshot, &run, now).await {
                    d.fail(&run.key, &error);
                }
                if let Err(error) = self.nudge(d, snapshot, &run, now).await {
                    d.fail(&run.key, &error);
                }
            }
            if let Err(error) = self.heartbeat(d, snap, &run, now).await {
                d.fail(&run.key, &error);
            }
            let pruned = run.clone();
            let _ = blocking(move || {
                inbox::prune_done(&pruned);
                Ok(())
            })
            .await;
        }
        self.write_failure_notice(d, now).await;
        if let Some(snapshot) = snap {
            let live: Vec<String> = snapshot.panes.keys().map(|p| p.0.clone()).collect();
            progress::prune(&d.ctx.state_dir(), d.socket, &live);
        }
        self.queries = self.compute_queries(d);
    }

    /// A routing job an older build recorded is not this ticker's child:
    /// kill it when it still runs and route the run again.
    async fn clear_old_routing<H>(&mut self, d: &Deps<'_, H>) {
        for run in Run::list(&d.ctx.runs_dir()) {
            let Some(job) = run.record().ok().and_then(|r| r.routing) else {
                continue;
            };
            if crate::routing::process_alive(job.pid) {
                crate::routing::kill(job.pid);
            }
            if let Err(error) = update_run(&run, |r| r.routing = None).await {
                d.fail(&run.key, &error);
            }
        }
    }

    async fn write_failure_notice<H: Herdr>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        let active: BTreeSet<String> = Run::list(&d.ctx.runs_dir())
            .iter()
            .filter_map(|run| run.record().ok())
            .filter(|r| r.status == Status::Active)
            .map(|r| r.issue_id)
            .collect();
        self.failing.retain(|issue_id, _| active.contains(issue_id));
        let Some(since) = self.failing.values().min().copied() else {
            self.failure_notified = false;
            return;
        };
        if !self.failure_notified && now.duration_since(since) >= WRITE_FAILURE_NOTICE {
            let _ = d
                .herdr
                .notification_show(
                    "herdr-linear-agent",
                    "Linear has not accepted writes for 10 minutes. They are kept and retried; see the ticker log.",
                )
                .await;
            self.failure_notified = true;
        }
    }

    /// Active runs, and runs of any status with queued requests (the flush
    /// covers detached and closed runs too). A run whose coordinator is not
    /// decided and whose routing does not run asks for the issue detail.
    fn compute_queries<H>(&self, d: &Deps<'_, H>) -> Vec<RunQuery> {
        Run::list(&d.ctx.runs_dir())
            .into_iter()
            .filter_map(|run| {
                let record = run.record().ok()?;
                let active = record.status == Status::Active;
                if !active && outbox::pending(&run).is_empty() {
                    return None;
                }
                let undecided = active
                    && record.coordinator.profile.is_empty()
                    && !self.routing.contains(&run.key);
                Some(RunQuery {
                    issue_id: record.issue_id,
                    run_dir: run.dir.clone(),
                    session_id: Some(record.session_id).filter(|s| !s.is_empty()),
                    prompt_cursor: record.prompt_cursor,
                    issue_updated_at: (!undecided).then_some(record.issue_updated_at),
                })
            })
            .collect()
    }

    /// The earliest time a time-based rule may become due, at most
    /// `METADATA_REFRESH` away. Only future times count: a rule that is due
    /// but held by another condition waits for the next wake.
    pub fn next_deadline<H>(&self, d: &Deps<'_, H>, now: Timestamp) -> Timestamp {
        let mut times: Vec<Timestamp> = Vec::new();
        let hours = i64::try_from(d.config.limits.run_timeout_hours).unwrap_or(i64::MAX / 7200);
        let timeout = SignedDuration::from_hours(hours);
        for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else { continue };
            if record.status != Status::Active {
                continue;
            }
            let mut agents = vec![record.coordinator.clone()];
            agents.extend(worker::list(&run).into_iter().map(|w| w.agent));
            for agent in &agents {
                agent_deadlines(agent, &mut times);
            }
            if matches!(record.coordinator.last_state.as_str(), "idle" | "done") {
                times.extend(after(
                    &record.coordinator.last_state_change,
                    COORDINATOR_IDLE,
                ));
            }
            if !record.session_id.is_empty() && !record.stopped {
                times.extend(after(&record.last_activity, HEARTBEAT));
                if !record.timeout_asked {
                    times.extend(after(&record.timeout_since, timeout));
                }
            }
        }
        times.extend(self.launched.values().map(|(_, at)| *at + DETECTION_GRACE));
        times.extend(self.missing_since.values().map(|at| *at + PANE_GRACE));
        times.extend(self.reported.values().map(|(_, at)| *at + METADATA_REFRESH));
        if !self.failure_notified {
            times.extend(self.failing.values().map(|at| *at + WRITE_FAILURE_NOTICE));
        }
        times
            .into_iter()
            .filter(|at| *at > now)
            .min()
            .unwrap_or(now + METADATA_REFRESH)
            .min(now + METADATA_REFRESH)
    }

    /// Reports a pane's state token when it changed or is due for a refresh.
    pub(super) async fn report_pane<H: Herdr>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        pane: &str,
        display: &str,
        state: &str,
        now: Timestamp,
    ) {
        let pane_id = PaneId(pane.to_string());
        if !snapshot.panes.contains_key(&pane_id) {
            return;
        }
        let token = format!("{display}\n{state}");
        if let Some((last, at)) = self.reported.get(pane)
            && *last == token
            && now.duration_since(*at) < METADATA_REFRESH
        {
            return;
        }
        let _ = d
            .herdr
            .report_metadata(
                &pane_id,
                progress::SOURCE,
                display,
                &[("hla_state".into(), state.into())],
                progress::TOKEN_TTL_MS,
            )
            .await;
        self.reported.insert(pane.to_string(), (token, now));
    }

    /// What the snapshot shows of an agent. A recorded pane this ticker has
    /// never seen counts as present (and empty) until `PANE_GRACE` passed.
    pub(super) fn live<H>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: &Snapshot,
        record: &AgentRecord,
        now: Timestamp,
    ) -> Live {
        let mut live = worker::live_state(record, snapshot, now, &d.ctx.state_dir(), d.socket);
        let pane = &record.pane_id;
        if !live.pane_exists
            && !pane.is_empty()
            && !snapshot.panes.contains_key(&PaneId(pane.clone()))
            && !self.seen_panes.contains(pane)
        {
            let first = *self.missing_since.entry(pane.clone()).or_insert(now);
            live.pane_exists = now.duration_since(first) < PANE_GRACE;
        }
        live
    }
}

fn agent_deadlines(agent: &AgentRecord, times: &mut Vec<Timestamp>) {
    if matches!(agent.status, AgentStatus::Pending) || agent.prompt_pending {
        times.extend(attempt_due(agent));
    }
    if agent.status != AgentStatus::Open || agent.blocked_reported {
        return;
    }
    match agent.last_state.as_str() {
        "blocked" => times.extend(after(&agent.last_state_change, BLOCKED)),
        "unknown" if agent.prompt_pending => {
            times.extend(after(&agent.last_state_change, LAUNCH_DIALOG));
        }
        _ => {}
    }
}

fn fatal(log: &Log, what: &str) -> Result<()> {
    log.line(&format!("ticker failed: {what}"));
    Err(anyhow!("{what}"))
}

/// Runs passes until shutdown. Each wake drains every ready message, then
/// runs one pass; between wakes it sleeps until the next deadline.
pub async fn run<H: Herdr + Clone + 'static>(inputs: Inputs<'_, H>) -> Result<()> {
    let Inputs {
        ctx,
        config,
        herdr,
        socket,
        log,
        mut link,
        mut level,
        mut events,
        queries,
        linear_wake,
        mut shutdown,
        clock,
    } = inputs;
    let (effects_tx, mut effects) = mpsc::channel(EFFECT_QUEUE);
    let (routing_tx, mut routing) = mpsc::channel(EFFECT_QUEUE);
    let mut reconciler = Reconciler::new(effects_tx, routing_tx)
        .context("the reconciler could not find its binary")?;
    let deps = Deps {
        ctx,
        config,
        herdr: &herdr,
        socket: &socket,
        log: &log,
    };
    let mut deadline = tokio::time::Instant::now();
    loop {
        let mut wake = Wake::default();
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            changed = link.changed() => {
                if changed.is_err() {
                    return fatal(&log, "the Herdr wake task ended");
                }
            }
            changed = level.changed() => {
                if changed.is_err() {
                    return fatal(&log, "the Linear task ended");
                }
            }
            event = events.recv() => match event {
                Some(event) => wake.events.push(event),
                None => return fatal(&log, "the Linear task ended"),
            },
            // The reconciler holds a sender of both, so they never close.
            Some(done) = effects.recv() => wake.effects.push(done),
            Some(done) = routing.recv() => wake.routing.push(done),
            () = tokio::time::sleep_until(deadline) => {}
        }
        while let Ok(event) = events.try_recv() {
            wake.events.push(event);
        }
        while let Ok(done) = effects.try_recv() {
            wake.effects.push(done);
        }
        while let Ok(done) = routing.try_recv() {
            wake.routing.push(done);
        }
        drop(link.borrow_and_update());
        let current = level.borrow_and_update().clone();
        let now = clock();
        reconciler.pass(&deps, &current, wake, now).await;
        let next = reconciler.queries().to_vec();
        queries.send_if_modified(|published| {
            let changed = *published != next;
            if changed {
                *published = next;
            }
            changed
        });
        if reconciler.take_queued() {
            linear_wake.notify_one();
        }
        let wait = reconciler.next_deadline(&deps, now).duration_since(now);
        deadline =
            tokio::time::Instant::now() + std::time::Duration::try_from(wait).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::FakeHerdr;
    use crate::linear::api::IssueRef;
    use crate::linear::task::Delegated;
    use crate::paths::Env;
    use crate::process::fake::FakeRunner;

    const T0: &str = "2026-09-28T09:00:00Z";

    fn t0() -> Timestamp {
        T0.parse().unwrap()
    }

    fn delegated(key: &str) -> LinearLevel {
        LinearLevel {
            app_user: Some("app".into()),
            delegated: Some(Delegated {
                read_at: t0(),
                issues: vec![IssueRef {
                    id: format!("id-{key}"),
                    identifier: key.into(),
                    title: "Loop".into(),
                    url: format!("https://linear.app/acme/issue/{key}"),
                    updated_at: T0.into(),
                    state: "unstarted".into(),
                    team: "DATA".into(),
                }],
            }),
            ..LinearLevel::default()
        }
    }

    struct Rig {
        home: tempfile::TempDir,
        env: Env,
        runner: FakeRunner,
        config: Config,
    }

    fn rig() -> Rig {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_string_lossy().into_owned();
        let env = Env::for_test(
            home.path(),
            &[
                ("XDG_STATE_HOME", &format!("{root}/state")),
                ("XDG_CONFIG_HOME", &format!("{root}/config")),
            ],
        );
        Rig {
            config: Config::parse(crate::config::tests::SAMPLE).unwrap(),
            runner: FakeRunner::new(),
            env,
            home,
        }
    }

    #[tokio::test]
    async fn a_wake_runs_a_pass_that_publishes_queries_and_wakes_the_linear_task() {
        let rig = rig();
        let ctx = Ctx {
            env: &rig.env,
            runner: &rig.runner,
            detached_ticker: false,
        };
        let herdr = FakeHerdr::new(rig.home.path());
        let (_link_tx, link) = watch::channel(Link {
            connected: true,
            since: t0(),
            wakes: 0,
            last_error: None,
        });
        let (level_tx, level) = watch::channel(LinearLevel::default());
        let (events_tx, events) = mpsc::channel(8);
        let (queries, mut published) = watch::channel(Vec::new());
        let linear_wake = Arc::new(Notify::new());
        let (shutdown_tx, shutdown) = watch::channel(false);
        let log = Arc::new(Log::new(rig.home.path().join("ticker.log")));
        let reconciler = run(Inputs {
            ctx: &ctx,
            config: &rig.config,
            herdr: herdr.clone(),
            socket: "/tmp/loop.sock".into(),
            log,
            link,
            level,
            events,
            queries,
            linear_wake: linear_wake.clone(),
            shutdown,
            clock: t0,
        });
        let driver = async {
            level_tx.send(delegated("DATA-7")).unwrap();
            published.changed().await.unwrap();
            let queries = published.borrow_and_update().clone();
            assert_eq!(queries.len(), 1);
            assert_eq!(queries[0].issue_id, "id-DATA-7");
            assert_eq!(
                queries[0].issue_updated_at, None,
                "the claim asks for the detail"
            );
            linear_wake.notified().await;
            shutdown_tx.send(true).unwrap();
            drop(events_tx);
        };
        let (result, ()) = tokio::join!(reconciler, driver);
        result.unwrap();
        let run = Run::load(&ctx.runs_dir(), "DATA-7").unwrap();
        assert_eq!(
            outbox::pending(&run).len(),
            2,
            "the thought and the started state"
        );
        assert!(herdr.requests().contains(&"session.snapshot".to_string()));
    }

    #[tokio::test]
    async fn a_linear_task_that_ends_ends_the_loop_with_an_error() {
        let rig = rig();
        let ctx = Ctx {
            env: &rig.env,
            runner: &rig.runner,
            detached_ticker: false,
        };
        let (_link_tx, link) = watch::channel(Link {
            connected: false,
            since: t0(),
            wakes: 0,
            last_error: None,
        });
        let (_level_tx, level) = watch::channel(LinearLevel::default());
        let (events_tx, events) = mpsc::channel(8);
        let (queries, _published) = watch::channel(Vec::new());
        let (_shutdown_tx, shutdown) = watch::channel(false);
        drop(events_tx);
        let error = run(Inputs {
            ctx: &ctx,
            config: &rig.config,
            herdr: FakeHerdr::new(rig.home.path()),
            socket: "/tmp/loop.sock".into(),
            log: Arc::new(Log::new(rig.home.path().join("ticker.log"))),
            link,
            level,
            events,
            queries,
            linear_wake: Arc::new(Notify::new()),
            shutdown,
            clock: t0,
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "the Linear task ended");
    }

    #[test]
    fn attempts_are_spaced_fifteen_seconds_doubling() {
        let table = [(0, 15), (1, 15), (2, 30), (3, 60), (4, 120)];
        for (attempts, seconds) in table {
            assert_eq!(
                spacing(attempts),
                SignedDuration::from_secs(seconds),
                "{attempts}"
            );
        }
        let record = AgentRecord {
            launch_attempts: 2,
            last_attempt_at: T0.into(),
            ..AgentRecord::default()
        };
        assert_eq!(
            attempt_due(&record),
            Some(t0() + SignedDuration::from_secs(30))
        );
        assert_eq!(attempt_due(&AgentRecord::default()), None, "never tried");
    }

    #[test]
    fn a_tracked_update_keeps_fields_another_process_wrote() {
        let before = AgentRecord {
            last_state: "idle".into(),
            prompt_pending: true,
            ..AgentRecord::default()
        };
        let mut after = before.clone();
        after.last_state = "working".into();
        after.last_state_seq = 4;
        // Meanwhile `worker restart` changed the profile and the pane.
        let mut stored = before.clone();
        stored.profile = "deep".into();
        stored.pane_id = "w9:p1".into();
        apply_tracked(&mut stored, &before, &after);
        assert_eq!(
            (stored.last_state.as_str(), stored.last_state_seq),
            ("working", 4)
        );
        assert_eq!(
            (stored.profile.as_str(), stored.pane_id.as_str()),
            ("deep", "w9:p1")
        );
    }
}
