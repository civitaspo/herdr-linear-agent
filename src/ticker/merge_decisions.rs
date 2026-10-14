//! Durable merge-decision requests, one volatile assessor slot, and
//! idempotent receipt delivery.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
use std::{collections::VecDeque, sync::Mutex};

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::reconcile::{Deps, Reconciler, blocking};
use crate::config::{Config, Profile};
use crate::files;
use crate::inbox::{self, Item};
use crate::merge_decision::{self, DecisionFailure, FreshMetadata};
use crate::process::Runner;
use crate::run::{Run, Status};
use crate::worker::{self, Assessment, DecisionRequest, FailureCode, MergeDecisionState, Worker};

const APPLY_RETRY: SignedDuration = SignedDuration::from_secs(60);
const TASK_POLL: SignedDuration = SignedDuration::from_secs(5);
const MAX_FAILURE_DETAIL_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DecisionKey {
    run: String,
    worker: String,
    request: Uuid,
}

#[derive(Debug, Clone)]
pub(super) enum DecisionOutcome {
    Assessed {
        assessment: Assessment,
        freshness: Box<FreshMetadata>,
    },
    Failed {
        request: DecisionRequest,
        code: FailureCode,
        detail: String,
        completed_at: String,
    },
}

pub(super) enum DecisionTask {
    Running {
        key: DecisionKey,
        handle: Option<JoinHandle<DecisionOutcome>>,
    },
    PendingApply {
        key: DecisionKey,
        outcome: Box<DecisionOutcome>,
        retry_at: Timestamp,
    },
}

#[cfg(test)]
pub(super) struct TestReply {
    pub response: std::result::Result<serde_json::Value, (FailureCode, String)>,
    pub started: Option<oneshot::Sender<()>>,
    pub release: Option<oneshot::Receiver<()>>,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct TestReplies(Arc<Mutex<TestReplyQueue>>);

#[cfg(test)]
#[derive(Default)]
struct TestReplyQueue {
    replies: VecDeque<TestReply>,
    calls: usize,
}

#[cfg(test)]
impl TestReplies {
    pub(super) fn push(&self, reply: TestReply) {
        self.0
            .lock()
            .expect("decision reply queue poisoned")
            .replies
            .push_back(reply);
    }

    fn pop(&self) -> Option<TestReply> {
        let mut queue = self.0.lock().expect("decision reply queue poisoned");
        let reply = queue.replies.pop_front();
        queue.calls += usize::from(reply.is_some());
        reply
    }

    pub(super) fn calls(&self) -> usize {
        self.0.lock().expect("decision reply queue poisoned").calls
    }
}

impl DecisionTask {
    fn key(&self) -> &DecisionKey {
        match self {
            DecisionTask::Running { key, .. } | DecisionTask::PendingApply { key, .. } => key,
        }
    }

    fn next_wake(&self, now: Timestamp) -> Timestamp {
        match self {
            DecisionTask::Running { .. } => now + TASK_POLL,
            DecisionTask::PendingApply { retry_at, .. } => *retry_at,
        }
    }
}

impl Drop for DecisionTask {
    fn drop(&mut self) {
        if let DecisionTask::Running {
            handle: Some(handle),
            ..
        } = self
        {
            handle.abort();
        }
    }
}

struct TaskInput {
    run: Run,
    worker: Worker,
    request: DecisionRequest,
    profile_name: String,
    profile: Profile,
    max_risk_score: u8,
    expected_repository_path: PathBuf,
    config_dir: PathBuf,
    #[cfg(not(test))]
    path: Option<String>,
    runner: Arc<dyn Runner>,
    timeout: Duration,
}

impl TaskInput {
    fn key(&self) -> DecisionKey {
        DecisionKey {
            run: self.run.key.clone(),
            worker: self.worker.id.clone(),
            request: self.request.id,
        }
    }
}

impl Reconciler {
    /// Runs after current-run watch/launch/heartbeat work and before nudges.
    /// It also handles closed/stopped-run revocation and notification recovery.
    pub(super) async fn merge_decisions<H>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        self.revoke_unused(d).await;
        self.cancel_stale_task(d).await;
        self.harvest_decision(d, now).await;
        self.notify_decisions(d, now).await;
        self.start_next_decision(d).await;
    }

    async fn revoke_unused<H>(&mut self, d: &Deps<'_, H>) {
        let config_dir = d.ctx.config_dir();
        let fingerprint = blocking(move || Config::fingerprint(&config_dir))
            .await
            .ok()
            .map(|value| Config::fingerprint_hex(&value));
        for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else { continue };
            for worker in worker::list(&run) {
                let Some((request, reason)) =
                    revocation_reason(d.config, &record, &worker, fingerprint.as_deref())
                else {
                    continue;
                };
                let worker_id = worker.id.clone();
                let detail = reason.1.clone();
                if let Err(error) = self
                    .guarded(&run, move |run, lock| {
                        let current = worker::load(run, &worker_id)?;
                        if !is_unused_for(&current, &request) {
                            return Ok((false, Vec::new()));
                        }
                        worker::update_held(run, lock, &worker_id, |current| {
                            if is_unused_for(current, &request) {
                                current.merge_decision = Some(MergeDecisionState::Failed {
                                    request: request.clone(),
                                    code: reason.0,
                                    detail,
                                    completed_at: files::now(),
                                    notified: false,
                                });
                            }
                        })?;
                        Ok((true, Vec::new()))
                    })
                    .await
                {
                    d.fail(&run.key, &error);
                }
            }
        }
    }

    async fn cancel_stale_task<H>(&mut self, d: &Deps<'_, H>) {
        let Some(key) = self.decision_task.as_ref().map(|task| task.key().clone()) else {
            return;
        };
        let run = match Run::load(&d.ctx.runs_dir(), &key.run) {
            Ok(run) => run,
            Err(_) => {
                self.decision_task = None;
                return;
            }
        };
        let worker_id = key.worker.clone();
        let current = blocking(move || {
            let record = run.record()?;
            let worker = worker::load(&run, &worker_id)?;
            Ok((record, worker))
        })
        .await;
        let still_current = current.is_ok_and(|(record, worker)| {
            record.status == Status::Active
                && !record.stopped
                && d.config
                    .team(&record.workspace, &record.team_key)
                    .is_ok_and(|team| team.merge_decision.is_some())
                && decision_request(&worker).is_some_and(|request| request.id == key.request)
        });
        if !still_current {
            self.decision_task = None;
        }
    }

    async fn harvest_decision<H>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        let completed = match self.decision_task.as_mut() {
            Some(DecisionTask::Running { key, handle })
                if handle.as_ref().is_some_and(|handle| handle.is_finished()) =>
            {
                Some((
                    key.clone(),
                    handle.take().expect("finished decision handle"),
                ))
            }
            _ => None,
        };
        if let Some((key, handle)) = completed {
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(error) => DecisionOutcome::Failed {
                    request: request_for_key(d, &key)
                        .await
                        .unwrap_or_else(|| DecisionRequest {
                            id: key.request,
                            pr_url: String::new(),
                            report_hash: String::new(),
                            issue_hash: String::new(),
                            config_fingerprint: String::new(),
                            requested_at: files::now(),
                        }),
                    code: FailureCode::TaskFailure,
                    detail: format!("the assessment task ended unexpectedly: {error}"),
                    completed_at: files::now(),
                },
            };
            self.decision_task = None;
            self.apply_or_defer(d, key, outcome, now).await;
        }

        let retry = match self.decision_task.as_ref() {
            Some(DecisionTask::PendingApply {
                key,
                outcome,
                retry_at,
            }) if *retry_at <= now => Some((key.clone(), outcome.clone())),
            _ => None,
        };
        if let Some((key, outcome)) = retry {
            let handle = tokio::spawn(make_retry_task(d, key.clone(), *outcome));
            self.decision_task = Some(DecisionTask::Running {
                key,
                handle: Some(handle),
            });
        }
    }

    async fn apply_or_defer<H>(
        &mut self,
        d: &Deps<'_, H>,
        key: DecisionKey,
        outcome: DecisionOutcome,
        now: Timestamp,
    ) {
        match persist_outcome(d, &key, outcome.clone()).await {
            Ok(true) | Ok(false) => {}
            Err(error) => {
                d.fail(&key.run, &error);
                self.decision_task = Some(DecisionTask::PendingApply {
                    key,
                    outcome: Box::new(outcome),
                    retry_at: now + APPLY_RETRY,
                });
            }
        }
    }

    async fn notify_decisions<H>(&mut self, d: &Deps<'_, H>, now: Timestamp) {
        for run in Run::list(&d.ctx.runs_dir()) {
            for worker in worker::list(&run) {
                let Some((item, request_id)) = decision_notification(&worker) else {
                    continue;
                };
                let key = DecisionKey {
                    run: run.key.clone(),
                    worker: worker.id.clone(),
                    request: request_id,
                };
                if self
                    .decision_notification_retry_at
                    .get(&key)
                    .is_some_and(|retry_at| *retry_at > now)
                {
                    continue;
                }
                let worker_id = worker.id.clone();
                let expected_request_id = key.request;
                let run_for_write = run.clone();
                let result = blocking(move || {
                    let lock = run_for_write.lock()?;
                    let current = worker::load(&run_for_write, &worker_id)?;
                    let Some((current_item, current_request_id)) = decision_notification(&current)
                    else {
                        return Ok(false);
                    };
                    if current_item != item || current_request_id != expected_request_id {
                        return Ok(false);
                    }
                    inbox::write_once_held(&run_for_write, &lock, &item)?;
                    worker::update_held(&run_for_write, &lock, &worker_id, |worker| {
                        match &mut worker.merge_decision {
                            Some(MergeDecisionState::Assessed {
                                assessment,
                                notified,
                            }) if assessment.request.id == expected_request_id => *notified = true,
                            Some(MergeDecisionState::Failed {
                                request, notified, ..
                            }) if request.id == expected_request_id => *notified = true,
                            Some(MergeDecisionState::Consumed {
                                assessment,
                                notified,
                            }) if assessment.request.id == expected_request_id => *notified = true,
                            _ => {}
                        }
                    })?;
                    Ok(true)
                })
                .await;
                match result {
                    Ok(true) => {
                        self.decision_notification_retry_at.remove(&key);
                    }
                    Ok(false) => {
                        self.decision_notification_retry_at.remove(&key);
                    }
                    Err(error) => {
                        d.fail(&key.run, &error);
                        self.decision_notification_retry_at
                            .insert(key, now + APPLY_RETRY);
                    }
                }
            }
        }
    }

    async fn start_next_decision<H>(&mut self, d: &Deps<'_, H>) {
        if self.decision_task.is_some() {
            return;
        }
        let config_dir = d.ctx.config_dir();
        let fingerprint = match blocking(move || Config::fingerprint(&config_dir)).await {
            Ok(value) => Config::fingerprint_hex(&value),
            Err(_) => return,
        };
        'runs: for run in Run::list(&d.ctx.runs_dir()) {
            let Ok(record) = run.record() else { continue };
            if record.status != Status::Active || record.stopped {
                continue;
            }
            for worker in worker::list(&run) {
                let Some(request) = decision_request(&worker) else {
                    continue;
                };
                if request.config_fingerprint != fingerprint {
                    continue;
                }
                let Ok(team) = d.config.team(&record.workspace, &record.team_key) else {
                    continue;
                };
                let Some(decision) = team.merge_decision.as_ref() else {
                    continue;
                };
                let Ok(profile) = d.config.profile(&decision.profile) else {
                    continue;
                };
                let Ok(repository) = d.config.repository(&worker.repo) else {
                    continue;
                };
                let input = TaskInput {
                    run: run.clone(),
                    worker: worker.clone(),
                    request: request.clone(),
                    profile_name: decision.profile.clone(),
                    profile: profile.clone(),
                    max_risk_score: decision.max_risk_score,
                    expected_repository_path: repository.path.clone(),
                    config_dir: d.ctx.config_dir(),
                    #[cfg(not(test))]
                    path: std::env::var("PATH").ok(),
                    runner: d.ctx.runner.shared(),
                    timeout: Duration::from_secs(
                        profile
                            .timeout_seconds
                            .unwrap_or(d.config.limits.routing_agent_timeout_seconds),
                    ),
                };
                let key = input.key();
                #[cfg(test)]
                let handle =
                    tokio::spawn(run_decision_task(input, self.decision_test_replies.pop()));
                #[cfg(not(test))]
                let handle = tokio::spawn(run_decision_task(input));
                self.decision_task = Some(DecisionTask::Running {
                    key,
                    handle: Some(handle),
                });
                break 'runs;
            }
        }
    }

    pub(super) fn decision_deadline(&self, now: Timestamp) -> Option<Timestamp> {
        self.decision_task
            .as_ref()
            .map(|task| task.next_wake(now))
            .into_iter()
            .chain(self.decision_notification_retry_at.values().copied())
            .filter(|at| *at > now)
            .min()
    }
}

fn decision_request(worker: &Worker) -> Option<DecisionRequest> {
    match &worker.merge_decision {
        Some(MergeDecisionState::Requested { request }) => Some(request.clone()),
        _ => None,
    }
}

fn is_requested_for(worker: &Worker, request: &DecisionRequest) -> bool {
    decision_request(worker).as_ref() == Some(request)
}

fn is_unused_for(worker: &Worker, request: &DecisionRequest) -> bool {
    match &worker.merge_decision {
        Some(MergeDecisionState::Requested { request: current }) => current == request,
        Some(MergeDecisionState::Assessed { assessment, .. }) => assessment.request == *request,
        _ => false,
    }
}

fn revocation_reason(
    config: &Config,
    record: &crate::run::RunRecord,
    worker: &Worker,
    fingerprint: Option<&str>,
) -> Option<(DecisionRequest, (FailureCode, String))> {
    let request = match &worker.merge_decision {
        Some(MergeDecisionState::Requested { request })
        | Some(MergeDecisionState::Assessed {
            assessment: crate::worker::Assessment { request, .. },
            ..
        }) => request.clone(),
        _ => return None,
    };
    let reason = if record.status != Status::Active || record.stopped {
        Some((
            FailureCode::IneligibleRun,
            "the run is closed or stopped; request a new assessment after it becomes active".into(),
        ))
    } else if config
        .team(&record.workspace, &record.team_key)
        .map_or(true, |team| team.merge_decision.is_none())
    {
        Some((
            FailureCode::Disabled,
            "machine merge assessment is disabled for this team".into(),
        ))
    } else if fingerprint != Some(request.config_fingerprint.as_str()) {
        Some((
            FailureCode::StaleConfig,
            "configuration or profile instructions changed; request a fresh assessment".into(),
        ))
    } else {
        None
    };
    reason.map(|reason| (request, reason))
}

fn decision_notification(worker: &Worker) -> Option<(Item, Uuid)> {
    let (request_id, created, summary, notified) = match worker.merge_decision.as_ref()? {
        MergeDecisionState::Assessed {
            assessment,
            notified,
        }
        | MergeDecisionState::Consumed {
            assessment,
            notified,
        } => {
            let risk = assessment.output.risk_score;
            (
                assessment.request.id,
                assessment.assessed_at.clone(),
                format!(
                    "historical assessment: risk {risk}/100, threshold {}, profile {}, head {}; request {}. Consumption always requires fresh plugin validation.",
                    assessment.max_risk_score,
                    assessment.profile,
                    assessment.head_sha,
                    assessment.request.id
                ),
                *notified,
            )
        }
        MergeDecisionState::Failed {
            request,
            code,
            detail,
            completed_at,
            notified,
        } => (
            request.id,
            completed_at.clone(),
            format!(
                "assessment failed ({:?}): {}. Request a fresh assessment after resolving the reason; no merge authority was granted.",
                code, detail
            ),
            *notified,
        ),
        MergeDecisionState::Requested { .. } => return None,
    };
    if notified {
        return None;
    }
    let id = format!("merge-decision-{request_id}-0");
    Some((
        Item {
            id,
            kind: "merge-decision".into(),
            subject: worker.id.clone(),
            created,
            summary: truncate_chars(&summary, MAX_FAILURE_DETAIL_CHARS),
        },
        request_id,
    ))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

async fn persist_outcome<H>(
    d: &Deps<'_, H>,
    key: &DecisionKey,
    outcome: DecisionOutcome,
) -> Result<bool> {
    let run = Run::load(&d.ctx.runs_dir(), &key.run)?;
    let worker_id = key.worker.clone();
    let request_id = key.request;
    let config = d.config.clone();
    let config_dir = d.ctx.config_dir();
    let request = match &outcome {
        DecisionOutcome::Assessed { assessment, .. } => assessment.request.clone(),
        DecisionOutcome::Failed { request, .. } => request.clone(),
    };
    blocking(move || {
        let lock = run.lock()?;
        let record = run.record()?;
        let current = worker::load(&run, &worker_id)?;
        if !is_requested_for(&current, &request) || request.id != request_id {
            return Ok(false);
        }
        let fingerprint = Config::fingerprint(&config_dir)
            .ok()
            .map(|value| Config::fingerprint_hex(&value));
        let stale = if record.status != Status::Active || record.stopped {
            Some((
                FailureCode::IneligibleRun,
                "the run is closed or stopped; request a fresh assessment after it becomes active"
                    .to_string(),
            ))
        } else if config
            .team(&record.workspace, &record.team_key)
            .map_or(true, |team| team.merge_decision.is_none())
        {
            Some((
                FailureCode::Disabled,
                "machine merge assessment is disabled for this team".to_string(),
            ))
        } else if fingerprint.as_deref() != Some(request.config_fingerprint.as_str()) {
            Some((
                FailureCode::StaleConfig,
                "configuration or profile instructions changed; request a fresh assessment"
                    .to_string(),
            ))
        } else {
            match (
                merge_decision::read_local_text(&run.issue_md(), "issue"),
                merge_decision::read_local_text(
                    &worker::home_report_path(&run, &worker_id),
                    "worker report",
                ),
            ) {
                (Ok(issue), Ok(report))
                    if files::sha256_hex(issue.as_bytes()) == request.issue_hash
                        && files::sha256_hex(report.as_bytes()) == request.report_hash
                        && current.report_hash == request.report_hash
                        && current.pr_url == request.pr_url =>
                {
                    None
                }
                _ => Some((
                    FailureCode::StaleEvidence,
                    "the issue, worker report, or pull request changed; request a fresh assessment"
                        .to_string(),
                )),
            }
        };
        let terminal = if let Some((code, detail)) = stale {
            DecisionOutcome::Failed {
                request: request.clone(),
                code,
                detail,
                completed_at: files::now(),
            }
        } else {
            outcome
        };
        worker::update_held(&run, &lock, &worker_id, |current| {
            if !is_requested_for(current, &request) {
                return;
            }
            current.merge_decision = Some(match terminal {
                DecisionOutcome::Assessed { assessment, .. } => MergeDecisionState::Assessed {
                    assessment,
                    notified: false,
                },
                DecisionOutcome::Failed {
                    request,
                    code,
                    detail,
                    completed_at,
                } => MergeDecisionState::Failed {
                    request,
                    code,
                    detail: truncate_chars(&detail, MAX_FAILURE_DETAIL_CHARS),
                    completed_at,
                    notified: false,
                },
            });
        })?;
        Ok(true)
    })
    .await
}

fn make_retry_task<H>(
    d: &Deps<'_, H>,
    key: DecisionKey,
    outcome: DecisionOutcome,
) -> impl std::future::Future<Output = DecisionOutcome> + Send + 'static {
    let config_dir = d.ctx.config_dir();
    let runner = d.ctx.runner.shared();
    let runs_dir = d.ctx.runs_dir();
    let request = match &outcome {
        DecisionOutcome::Assessed { assessment, .. } => assessment.request.clone(),
        DecisionOutcome::Failed { request, .. } => request.clone(),
    };
    let fallback_request = request.clone();
    let validation = async move {
        let request = match &outcome {
            DecisionOutcome::Assessed { assessment, .. } => assessment.request.clone(),
            DecisionOutcome::Failed { request, .. } => request.clone(),
        };
        let run = Run::load(&runs_dir, &key.run)?;
        let worker_id = key.worker.clone();
        let (record, worker, issue, report, fingerprint) = blocking(move || {
            let record = run.record()?;
            let worker = worker::load(&run, &worker_id)?;
            let issue = merge_decision::read_local_text(&run.issue_md(), "issue")?;
            let report = merge_decision::read_local_text(
                &worker::home_report_path(&run, &worker_id),
                "worker report",
            )?;
            let fingerprint = Config::fingerprint(&config_dir)
                .ok()
                .map(|value| Config::fingerprint_hex(&value));
            Ok((record, worker, issue, report, fingerprint))
        })
        .await?;
        let current = record.status == Status::Active
            && !record.stopped
            && decision_request(&worker).as_ref() == Some(&request)
            && files::sha256_hex(issue.as_bytes()) == request.issue_hash
            && files::sha256_hex(report.as_bytes()) == request.report_hash
            && worker.report_hash == request.report_hash
            && worker.pr_url == request.pr_url
            && fingerprint.as_deref() == Some(request.config_fingerprint.as_str());
        if !current {
            return Ok::<DecisionOutcome, anyhow::Error>(DecisionOutcome::Failed {
                request,
                code: FailureCode::StaleEvidence,
                detail: "evidence changed while retrying persistence; request a fresh assessment"
                    .into(),
                completed_at: files::now(),
            });
        }
        if let DecisionOutcome::Assessed { freshness, .. } = &outcome
            && let Err(error) =
                merge_decision::verify_fresh_metadata(runner.as_ref(), freshness).await
        {
            return Ok(DecisionOutcome::Failed {
                request,
                code: error.code,
                detail: error.to_string(),
                completed_at: files::now(),
            });
        }
        Ok::<DecisionOutcome, anyhow::Error>(outcome)
    };
    async move {
        match validation.await {
            Ok(outcome) => outcome,
            Err(error) => DecisionOutcome::Failed {
                request: fallback_request,
                code: FailureCode::TaskFailure,
                detail: format!("could not revalidate the cached assessment: {error:#}"),
                completed_at: files::now(),
            },
        }
    }
}

async fn request_for_key<H>(d: &Deps<'_, H>, key: &DecisionKey) -> Option<DecisionRequest> {
    let run = Run::load(&d.ctx.runs_dir(), &key.run).ok()?;
    let worker_id = key.worker.clone();
    blocking(move || Ok(decision_request(&worker::load(&run, &worker_id)?)))
        .await
        .ok()
        .flatten()
}

async fn decision_blocking<T: Send + 'static>(
    work: impl FnOnce() -> merge_decision::DecisionResult<T> + Send + 'static,
) -> merge_decision::DecisionResult<T> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        DecisionFailure::new(
            FailureCode::TaskFailure,
            format!("a local decision task failed: {error}"),
        )
    })?
}

async fn run_decision_task(
    input: TaskInput,
    #[cfg(test)] test_reply: Option<TestReply>,
) -> DecisionOutcome {
    let request = input.request.clone();
    let future = async {
        let run = input.run.clone();
        let worker_snapshot = input.worker.clone();
        let worker_id = input.worker.id.clone();
        let config_dir = input.config_dir.clone();
        let request = input.request.clone();
        let task_request = request.clone();
        let profile_name = input.profile_name.clone();
        let profile = input.profile.clone();
        let max_risk_score = input.max_risk_score;
        let expected_repository_path = input.expected_repository_path.clone();
        let (issue, report) = decision_blocking(move || {
            let (config, fingerprint) = Config::load_stable(&config_dir).map_err(|_| {
                DecisionFailure::new(
                    FailureCode::StaleConfig,
                    "the current configuration is missing, invalid, or changing",
                )
            })?;
            if Config::fingerprint_hex(&fingerprint) != task_request.config_fingerprint {
                return Err(DecisionFailure::new(
                    FailureCode::StaleConfig,
                    "configuration changed after the request; request a fresh assessment",
                ));
            }
            let record = run.record().map_err(|_| {
                DecisionFailure::new(FailureCode::IneligibleRun, "the run is unavailable")
            })?;
            if record.status != Status::Active || record.stopped {
                return Err(DecisionFailure::new(
                    FailureCode::IneligibleRun,
                    "the run is closed or stopped",
                ));
            }
            let worker = worker::load(&run, &worker_id).map_err(|_| {
                DecisionFailure::new(FailureCode::StaleEvidence, "the worker record is unavailable")
            })?;
            if !is_requested_for(&worker, &task_request) {
                return Err(DecisionFailure::new(
                    FailureCode::StaleEvidence,
                    "the decision request was replaced before assessment began",
                ));
            }
            let issue = merge_decision::read_local_text(&run.issue_md(), "issue")?;
            let report = merge_decision::read_local_text(
                &worker::home_report_path(&run, &worker_id),
                "worker report",
            )?;
            if files::sha256_hex(issue.as_bytes()) != task_request.issue_hash
                || files::sha256_hex(report.as_bytes()) != task_request.report_hash
                || worker.report_hash != task_request.report_hash
                || worker.pr_url != task_request.pr_url
                || worker_snapshot.report_hash != task_request.report_hash
            {
                return Err(DecisionFailure::new(
                    FailureCode::StaleEvidence,
                    "the issue, worker report, or pull request changed after the request",
                ));
            }
            let current_team = config
                .team(&record.workspace, &record.team_key)
                .map_err(|_| {
                    DecisionFailure::new(FailureCode::Disabled, "team decision mode is disabled")
                })?;
            let current_decision = current_team.merge_decision.as_ref().ok_or_else(|| {
                DecisionFailure::new(FailureCode::Disabled, "team decision mode is disabled")
            })?;
            if current_decision.profile != profile_name
                || current_decision.max_risk_score != max_risk_score
                || config.profile(&profile_name).ok() != Some(&profile)
                || config
                    .repository(&worker.repo)
                    .map_or(true, |repo| repo.path != expected_repository_path)
            {
                return Err(DecisionFailure::new(
                    FailureCode::StaleConfig,
                    "the resolved decision profile or repository changed; request a fresh assessment",
                ));
            }
            Ok((issue, report))
        })
        .await?;
        let evidence = merge_decision::acquire_evidence(
            input.runner.as_ref(),
            &merge_decision::EvidenceInput {
                run_key: &input.run.key,
                worker: &input.worker,
                requested_pr_url: &input.request.pr_url,
                expected_repository_path: &input.expected_repository_path,
                issue: &issue,
                report: &report,
                profile_name: &input.profile_name,
                config_fingerprint: &input.request.config_fingerprint,
                max_risk_score: input.max_risk_score,
            },
        )
        .await?;
        #[cfg(test)]
        let output = if let Some(reply) = test_reply {
            if let Some(started) = reply.started {
                let _ = started.send(());
            }
            if let Some(release) = reply.release {
                release.await.map_err(|_| {
                    DecisionFailure::new(
                        FailureCode::TaskFailure,
                        "the held test assessment was cancelled",
                    )
                })?;
            }
            reply
                .response
                .map_err(|(code, detail)| DecisionFailure::new(code, detail))
                .and_then(|value| merge_decision::validate_output(&value))?
        } else {
            Err(DecisionFailure::new(
                FailureCode::TaskFailure,
                "the test has no queued assessment reply",
            ))?
        };
        #[cfg(not(test))]
        let output = merge_decision::assess(
            &input.profile,
            &input.profile.instructions,
            &evidence,
            input.timeout,
            input.path.as_deref(),
            &std::env::temp_dir(),
        )
        .await?;
        merge_decision::verify_fresh_metadata(input.runner.as_ref(), &evidence.freshness).await?;
        let (issue_after, report_after, fingerprint_after) = {
            let run = input.run.clone();
            let worker_id = input.worker.id.clone();
            let config_dir = input.config_dir.clone();
            decision_blocking(move || {
                let issue = merge_decision::read_local_text(&run.issue_md(), "issue")?;
                let report = merge_decision::read_local_text(
                    &worker::home_report_path(&run, &worker_id),
                    "worker report",
                )?;
                let (_, fingerprint) = Config::load_stable(&config_dir).map_err(|_| {
                    DecisionFailure::new(
                        FailureCode::StaleConfig,
                        "the config is unavailable or changing",
                    )
                })?;
                Ok((issue, report, Config::fingerprint_hex(&fingerprint)))
            })
            .await?
        };
        if files::sha256_hex(issue_after.as_bytes()) != request.issue_hash
            || files::sha256_hex(report_after.as_bytes()) != request.report_hash
            || fingerprint_after != request.config_fingerprint
        {
            return Err(DecisionFailure::new(
                FailureCode::StaleEvidence,
                "the issue, report, or config changed during assessment",
            ));
        }
        Ok(DecisionOutcome::Assessed {
            assessment: Assessment {
                request: input.request.clone(),
                profile: input.profile_name.clone(),
                method_version: merge_decision::METHOD_VERSION,
                max_risk_score: input.max_risk_score,
                head_sha: evidence.head_sha,
                base_sha: evidence.base_sha,
                evidence_digest: evidence.digest,
                output,
                assessed_at: files::now(),
            },
            freshness: Box::new(evidence.freshness),
        })
    };
    let budget = input.timeout.saturating_add(Duration::from_secs(260));
    match tokio::time::timeout(budget, future).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => DecisionOutcome::Failed {
            request,
            code: error.code,
            detail: error.to_string(),
            completed_at: files::now(),
        },
        Err(_) => DecisionOutcome::Failed {
            request,
            code: FailureCode::Timeout,
            detail: "the overall decision task exceeded its bounded time budget".into(),
            completed_at: files::now(),
        },
    }
}
