//! A run's outbox: every Linear write the run needs, as one JSON file per
//! request under `.state/outbox/`, sent by the ticker in the order written.
//!
//! Agents never talk to Linear. `say`, `ask`, `wait`, `plan set` and `finish` write a
//! request here and exit without touching the Keychain. The ticker sends each
//! request once; when the outcome is unknown it reads Linear before sending
//! again, and it never resends a write unconditionally.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::files;
use crate::linear::ApiError;
use crate::linear::api::{Activity, Content, ExternalUrl, IssueDetail};
use crate::linear::client::LinearApi;
use crate::run::{AwaitingReply, ResumeDecision, Run, RunLock, Status, WaitReason, WorkflowStage};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateTarget {
    /// The team's first started state, unless the issue is already started,
    /// completed or canceled.
    Started,
    /// The configured review state, unless the issue is already there,
    /// completed or canceled.
    Review,
    Workflow(WorkflowStage),
    Selected {
        stage: WorkflowStage,
        state_id: String,
        expected_current_state_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateNames {
    pub started: String,
    pub review: String,
    pub deploy: String,
    pub postmortem: String,
    pub done: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Ends the coordinator's current turn while keeping the run active.
    TurnComplete {
        body: String,
    },
    Activity {
        activity: Activity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_reason: Option<WaitReason>,
    },
    Plan {
        plan: Value,
    },
    ExternalUrls {
        urls: Vec<ExternalUrl>,
    },
    IssueState {
        target: StateTarget,
    },
    MergeAuthorizationRequest {
        worker_id: String,
        token: String,
    },
    /// A comment on the issue, created with the request's id.
    Comment {
        body: String,
    },
    /// A consent comment becomes obsolete when its incident ends.
    ResumeQuestion {
        token: String,
        body: String,
    },
    /// Labels added to the issue, `Label` or `Group/Label`.
    Labels {
        names: Vec<String>,
    },
    /// Restore only the currently approved incident, never an old approval.
    RestoreSession {
        token: String,
        allowed_users: Vec<String>,
    },
}

impl Op {
    pub fn needs_session(&self) -> bool {
        matches!(
            self,
            Self::Activity { .. }
                | Self::TurnComplete { .. }
                | Self::MergeAuthorizationRequest { .. }
                | Self::Plan { .. }
                | Self::ExternalUrls { .. }
        )
    }
    /// A question; with options, Linear shows them as a select.
    pub fn elicitation(body: impl Into<String>, options: &[(&str, &str)]) -> Op {
        Self::awaiting_reply(body, options, WaitReason::CoordinatorQuestion)
    }

    pub fn awaiting_reply(
        body: impl Into<String>,
        options: &[(&str, &str)],
        reason: WaitReason,
    ) -> Op {
        let mut activity = Activity::new(Content::Elicitation { body: body.into() });
        if !options.is_empty() {
            activity.signal = Some("select".into());
            let options: Vec<_> = options
                .iter()
                .map(|(label, value)| json!({ "label": label, "value": value }))
                .collect();
            activity.signal_metadata = Some(json!({ "options": options }));
        }
        Op::Activity {
            activity,
            wait_reason: Some(reason),
        }
    }

    pub fn activity(activity: Activity) -> Op {
        Op::Activity {
            activity,
            wait_reason: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    /// A UUIDv4; an activity is created with it as its ID.
    pub id: String,
    pub created: String,
    /// A send was started; its outcome is checked by a read before any resend.
    #[serde(default)]
    pub attempted: bool,
    /// Read back before the first send. Stable comments use this to recover
    /// after the queue file was removed but the caller's run marker was not saved.
    #[serde(default)]
    pub check_before_send: bool,
    /// Accepted-prompt generation when the question was queued. This uses a
    /// local monotonic token, not a comparison between server and local time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_generation: Option<u64>,
    #[serde(flatten)]
    pub op: Op,
}

fn outbox_dir(run: &Run) -> PathBuf {
    run.state_dir().join("outbox")
}

/// Queues one request. File names carry a counter allocated under the run
/// lock, so requests are sent in the order they were written.
pub fn push(run: &Run, op: Op) -> Result<Option<String>> {
    let lock = run.lock()?;
    push_held(run, &lock, op)
}

/// Marks a yielded coordinator turn as resumed before a non-Linear prompt.
pub fn clear_turn_complete(run: &Run) -> Result<()> {
    let lock = run.lock()?;
    run.update_held(&lock, |record| record.turn_complete = None)?;
    Ok(())
}

/// `push` for a caller that holds the run lock, so a request is queued in
/// the same critical section as the record field that guards it.
pub fn push_held(run: &Run, lock: &RunLock, op: Op) -> Result<Option<String>> {
    let mut record = run.record()?;
    let turn_complete = matches!(op, Op::TurnComplete { .. });
    if turn_complete
        && (record.status != crate::run::Status::Active
            || record.stopped
            || record.finished
            || record.awaiting_reply.is_some()
            || record.turn_complete.is_some())
    {
        return Ok(None);
    }
    let wait_reason = match &op {
        Op::Activity {
            activity,
            wait_reason,
        } => {
            let terminal = matches!(activity.content, Content::Response { .. });
            if record.turn_complete.is_some() && activity.ephemeral {
                return Ok(None);
            }
            if !terminal
                && (record.status != crate::run::Status::Active
                    || record.stopped
                    || (record.finished && *wait_reason != Some(WaitReason::WorkflowState))
                    || record.awaiting_reply.is_some())
            {
                if wait_reason.is_some() {
                    return Ok(None);
                }
                return Ok(None);
            }
            *wait_reason
        }
        Op::MergeAuthorizationRequest { .. }
            if record.status == crate::run::Status::Active && !record.stopped =>
        {
            None
        }
        Op::MergeAuthorizationRequest { .. } => return Ok(None),
        _ => None,
    };
    let counter_path = run.state_dir().join("outbox-counter.json");
    let n: u64 = files::read_json::<u64>(&counter_path).unwrap_or(0) + 1;
    files::write_json(&counter_path, &n)?;
    let request = Request {
        id: uuid::Uuid::new_v4().to_string(),
        created: files::now(),
        attempted: false,
        check_before_send: false,
        wait_generation: wait_reason.map(|_| record.reply_generation),
        op,
    };
    files::write_json(&outbox_dir(run).join(format!("{n:010}.json")), &request)?;
    let mut record_changed = false;
    if turn_complete {
        record.turn_complete = Some(request.id.clone());
        record_changed = true;
    } else {
        if matches!(request.op, Op::Activity { .. }) {
            record_changed = record.turn_complete.take().is_some();
        }
        if let Some(reason) = wait_reason {
            record.awaiting_reply = Some(AwaitingReply {
                activity_id: request.id.clone(),
                asked_at: request.created.clone(),
                reason,
            });
            record.timeout_asked = false;
            record_changed = true;
        }
    }
    if record_changed {
        run.update_held(lock, |current| *current = record)?;
    }
    Ok(Some(request.id))
}

/// Queues a comment with a stable UUID and reads it back before the first
/// send. A caller can safely recreate this request after its run marker lags
/// the outbox because Linear comments are identified by that UUID.
pub fn push_idempotent_comment_held(
    run: &Run,
    _lock: &RunLock,
    id: &str,
    body: &str,
) -> Result<Option<String>> {
    if uuid::Uuid::parse_str(id).is_err() {
        bail!("idempotent comment id must be a UUID");
    }
    let op = Op::Comment {
        body: body.to_string(),
    };
    for path in queued_paths(run) {
        if let Some(existing) = files::read_json::<Request>(&path)
            && existing.id == id
        {
            if existing.op != op {
                bail!("outbox request `{id}` already has different content");
            }
            return Ok(Some(id.to_string()));
        }
    }
    let counter_path = run.state_dir().join("outbox-counter.json");
    let n: u64 = files::read_json::<u64>(&counter_path).unwrap_or(0) + 1;
    files::write_json(&counter_path, &n)?;
    let request = Request {
        id: id.to_string(),
        created: files::now(),
        attempted: false,
        check_before_send: true,
        wait_generation: None,
        op,
    };
    files::write_json(&outbox_dir(run).join(format!("{n:010}.json")), &request)?;
    Ok(Some(id.to_string()))
}

fn queued_paths(run: &Run) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(outbox_dir(run)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    paths
}

/// Whether any request file waits, parsed or not. Read-only: the Linear
/// task alone moves, rewrites or removes outbox files.
pub fn is_empty(run: &Run) -> bool {
    queued_paths(run).is_empty()
}

/// The requests that parse, oldest first, read-only like [`is_empty`].
pub fn queued(run: &Run) -> Vec<Request> {
    queued_paths(run)
        .iter()
        .filter_map(|path| files::read_json::<Request>(path))
        .collect()
}

/// Queued requests, oldest first. A file that does not parse is moved
/// aside. Only the Linear task calls it.
pub fn pending(run: &Run) -> Vec<(PathBuf, Request)> {
    queued_paths(run)
        .into_iter()
        .filter_map(|path| match files::read_json::<Request>(&path) {
            Some(request) => Some((path, request)),
            None => {
                set_aside(&path);
                None
            }
        })
        .collect()
}

fn set_aside(path: &Path) {
    let failed = path.parent().map(|p| p.join("failed")).unwrap_or_default();
    let _ = std::fs::create_dir_all(&failed);
    if let Some(name) = path.file_name() {
        let _ = std::fs::rename(path, failed.join(name));
    }
}

/// Linear refused the request itself; sending it again would fail the same way.
fn definitive(error: &ApiError) -> bool {
    matches!(error, ApiError::Graphql(_) | ApiError::Configuration | ApiError::HttpStatus(400..=428 | 430..=499))
}

pub struct Sent {
    pub count: usize,
    /// Requests Linear refused, moved to `outbox/failed/`: (request, error).
    pub refused: Vec<(Request, ApiError)>,
    /// The error that stopped the queue; the rest waits for the next tick.
    pub blocked: Option<ApiError>,
    /// An activity went out.
    pub activity_sent: bool,
}

/// Sends a run's queued requests in order until one cannot be sent. An
/// attempted activity is looked up before it is sent again; a refusal is set
/// aside; `RateLimited` is not definitive, so such a request stays queued.
pub async fn send(
    run: &Run,
    session_id: &str,
    issue_id: &str,
    state_names: &StateNames,
    linear: &impl LinearApi,
) -> Sent {
    let mut sent = Sent {
        count: 0,
        refused: Vec::new(),
        blocked: None,
        activity_sent: false,
    };
    for (path, mut request) in pending(run) {
        // A dismissed session must not receive stale work or have its
        // questions resurrected. Ordinary comments and recovery still flow.
        let suspended = run.record().is_ok_and(|r| {
            r.resume_request.is_some() || (r.status == Status::Closed && r.session_id.is_empty())
        });
        if suspended && (request.op.needs_session() || matches!(request.op, Op::IssueState { .. }))
        {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let outcome = send_one(
            &path,
            &mut request,
            run,
            session_id,
            issue_id,
            state_names,
            linear,
        )
        .await;
        match outcome {
            Ok(activity_confirmed) => {
                let _ = std::fs::remove_file(&path);
                sent.count += 1;
                sent.activity_sent |= activity_confirmed
                    && matches!(
                        request.op,
                        Op::Activity { .. }
                            | Op::TurnComplete { .. }
                            | Op::MergeAuthorizationRequest { .. }
                    );
            }
            Err(error) if definitive(&error) => {
                if let Op::RestoreSession { token, .. } = &request.op {
                    let run = run.clone();
                    let token = token.clone();
                    let failure = error.to_string();
                    let saved = tokio::task::spawn_blocking(move || {
                        run.update(|r| {
                            if let Some(request) = &mut r.resume_request
                                && request.token == token
                            {
                                request.restore_error = failure;
                            }
                        })
                    })
                    .await;
                    if !matches!(saved, Ok(Ok(_))) {
                        sent.blocked = Some(ApiError::Configuration);
                        break;
                    }
                }
                let is_wait = matches!(
                    request.op,
                    Op::Activity {
                        wait_reason: Some(_),
                        ..
                    }
                );
                let is_turn_complete = matches!(request.op, Op::TurnComplete { .. });
                let refused_wait = if is_wait {
                    match clear_refused_wait(run, &request.id) {
                        Ok(cleared) => cleared,
                        Err(failure) => {
                            sent.blocked = Some(failure);
                            break;
                        }
                    }
                } else {
                    false
                };
                if is_turn_complete {
                    match clear_refused_turn_complete(run, &request.id) {
                        Ok(_) => {}
                        Err(failure) => {
                            sent.blocked = Some(failure);
                            break;
                        }
                    }
                }
                set_aside(&path);
                if refused_wait {
                    let _ = crate::inbox::write(
                        run,
                        "linear",
                        "question",
                        "Linear refused a question from this run. The coordinator may continue; see the ticker log for the error.",
                    );
                }
                sent.refused.push((request, error));
            }
            Err(error) => {
                sent.blocked = Some(error);
                break;
            }
        }
    }
    sent
}

async fn send_one(
    path: &Path,
    request: &mut Request,
    run: &Run,
    session_id: &str,
    issue_id: &str,
    state_names: &StateNames,
    linear: &impl LinearApi,
) -> Result<bool, ApiError> {
    if let Op::ResumeQuestion { token, .. } = &request.op {
        let run = run.clone();
        let token = token.clone();
        let id = request.id.clone();
        let current = tokio::task::spawn_blocking(move || {
            let _lock = run.lock().map_err(|_| ApiError::Configuration)?;
            let record = run.record().map_err(|_| ApiError::Configuration)?;
            Ok::<_, ApiError>(
                record.status == Status::Detached
                    && record.resume_request.is_some_and(|r| {
                        r.token == token
                            && r.comment_id == id
                            && !matches!(r.decision, ResumeDecision::Denied)
                    }),
            )
        })
        .await
        .map_err(|_| ApiError::Configuration)??;
        if !current {
            return Ok(false);
        }
    }
    if let Op::MergeAuthorizationRequest { worker_id, token } = &request.op {
        let run = run.clone();
        let worker_id = worker_id.clone();
        let token = token.clone();
        let current = tokio::task::spawn_blocking(move || {
            let _lock = run.lock().map_err(|_| ApiError::Configuration)?;
            let record = run.record().map_err(|_| ApiError::Configuration)?;
            if record.status != Status::Active || record.stopped {
                return Ok::<_, ApiError>(false);
            }
            let worker =
                crate::worker::load(&run, &worker_id).map_err(|_| ApiError::Configuration)?;
            Ok::<_, ApiError>(matches!(
                worker.merge_authorization,
                Some(crate::worker::MergeAuthorization::Pending {
                    token: current,
                    pr_url,
                }) if current == token && pr_url == worker.pr_url && !worker.pr_url.is_empty()
            ))
        })
        .await
        .map_err(|_| ApiError::Configuration)??;
        if !current {
            return Ok(false);
        }
    }
    let is_wait = matches!(
        &request.op,
        Op::Activity {
            wait_reason: Some(_),
            ..
        }
    );
    let is_turn_complete = matches!(&request.op, Op::TurnComplete { .. });
    let checked = match &request.op {
        Op::Activity { .. } | Op::TurnComplete { .. } | Op::MergeAuthorizationRequest { .. }
            if request.attempted =>
        {
            linear.activity_exists(session_id, &request.id).await?
        }
        Op::Comment { .. } | Op::ResumeQuestion { .. }
            if request.attempted || request.check_before_send =>
        {
            linear.comment_exists(&request.id).await?
        }
        // Replacing the plan or the URL list and adding labels are
        // idempotent, and a state move reads the issue before it writes.
        _ => false,
    };
    if checked {
        return Ok(true);
    }
    if is_wait {
        if !mark_wait_attempt_if_current(path, request, run)? {
            return Ok(false);
        }
    } else if is_turn_complete {
        if !mark_turn_complete_attempt_if_current(path, request, run)? {
            return Ok(false);
        }
    } else {
        request.attempted = true;
        files::write_json(path, request).map_err(|_| ApiError::Configuration)?;
    }
    let outcome = match &request.op {
        Op::TurnComplete { body } => {
            let activity = Activity::new(Content::Response { body: body.clone() });
            linear
                .create_activity(session_id, &request.id, &activity)
                .await
        }
        Op::Activity { activity, .. } => {
            linear
                .create_activity(session_id, &request.id, activity)
                .await
        }
        Op::Plan { plan } => linear.set_plan(session_id, plan).await,
        Op::ExternalUrls { urls } => linear.set_external_urls(session_id, urls).await,
        Op::Comment { body } | Op::ResumeQuestion { body, .. } => {
            linear.create_comment(issue_id, &request.id, body).await
        }
        Op::Labels { names } => linear.add_labels(issue_id, names).await,
        Op::RestoreSession {
            token,
            allowed_users,
        } => restore_session(run, issue_id, token, allowed_users, linear).await,
        Op::IssueState { target } => {
            let issue = linear.issue(issue_id).await?;
            if matches!(
                target,
                StateTarget::Workflow(WorkflowStage::Done)
                    | StateTarget::Selected {
                        stage: WorkflowStage::Done,
                        ..
                    }
            ) && !workflow_done_current(run, &issue)?
            {
                clear_stage_choice(run, WorkflowStage::Done)?;
                return Ok(true);
            }
            if let StateTarget::Selected {
                stage,
                state_id,
                expected_current_state_id,
            } = target
            {
                if matches!(issue.state.r#type.as_str(), "completed" | "canceled") {
                    if issue.state.r#type == "completed" && *stage == WorkflowStage::Done {
                        confirm_stage(
                            run,
                            *stage,
                            &issue.state.id,
                            &issue.state.name,
                            &issue.updated_at,
                            true,
                        )?;
                    } else {
                        clear_stage_choice(run, *stage)?;
                    }
                    return Ok(true);
                }
                let Some(selected) = issue.team.states.iter().find(|state| state.id == *state_id)
                else {
                    queue_state_question(run, &issue, *stage)?;
                    return Ok(true);
                };
                if *stage == WorkflowStage::Done && selected.r#type != "completed" {
                    queue_state_question(run, &issue, *stage)?;
                    return Ok(true);
                }
                if issue.state.id == *state_id {
                    confirm_stage(
                        run,
                        *stage,
                        &issue.state.id,
                        &issue.state.name,
                        &issue.updated_at,
                        true,
                    )?;
                    return Ok(true);
                }
                if issue.state.id != *expected_current_state_id {
                    queue_state_question(run, &issue, *stage)?;
                    return Ok(true);
                }
                if *stage == WorkflowStage::InProgress {
                    retarget_pickup_state(run, &selected.name)?;
                }
                linear.set_issue_state(issue_id, state_id).await?;
                let confirmed = linear.issue(issue_id).await?;
                if confirmed.state.id != *state_id {
                    return Err(ApiError::RequestFailed);
                }
                confirm_stage(
                    run,
                    *stage,
                    &confirmed.state.id,
                    &confirmed.state.name,
                    &confirmed.updated_at,
                    true,
                )?;
                return Ok(true);
            }
            let state_target = target.clone();
            let Some(state_id) = (match target_state(&issue, state_target.clone(), state_names) {
                Ok(state) => state,
                Err(ApiError::Graphql(_)) => {
                    if let StateTarget::Workflow(stage) = state_target {
                        queue_state_question(run, &issue, stage)?;
                        return Ok(true);
                    }
                    return Err(ApiError::Graphql(format!(
                        "team {} has no configured Linear state",
                        issue.team.key
                    )));
                }
                Err(error) => return Err(error),
            }) else {
                if let StateTarget::Workflow(stage) = state_target {
                    let matches_target = match stage {
                        WorkflowStage::InProgress => {
                            issue.state.name.eq_ignore_ascii_case(&state_names.started)
                                && issue.state.r#type == "started"
                        }
                        WorkflowStage::InReview => {
                            issue.state.name.eq_ignore_ascii_case(&state_names.review)
                        }
                        WorkflowStage::InDeploy => {
                            issue.state.name.eq_ignore_ascii_case(&state_names.deploy)
                        }
                        WorkflowStage::InPostmortem => issue
                            .state
                            .name
                            .eq_ignore_ascii_case(&state_names.postmortem),
                        WorkflowStage::Done => {
                            issue.state.name.eq_ignore_ascii_case(&state_names.done)
                                && issue.state.r#type == "completed"
                        }
                    };
                    if matches_target {
                        confirm_stage(
                            run,
                            stage,
                            &issue.state.id,
                            &issue.state.name,
                            &issue.updated_at,
                            false,
                        )?;
                    }
                }
                return Ok(true);
            };
            linear.set_issue_state(issue_id, &state_id).await?;
            // The write is confirmed by reading the issue again.
            let confirmed = linear.issue(issue_id).await?;
            if confirmed.state.id != state_id {
                return Err(ApiError::RequestFailed);
            }
            if let StateTarget::Workflow(stage) = state_target {
                confirm_stage(
                    run,
                    stage,
                    &confirmed.state.id,
                    &confirmed.state.name,
                    &confirmed.updated_at,
                    false,
                )?;
            }
            Ok(())
        }
        Op::MergeAuthorizationRequest { worker_id, token } => {
            let activity = Activity::new(Content::Thought {
                body: format!(
                    "To authorize a squash merge of worker {worker_id}'s pull request, reply exactly `merge {token}`."
                ),
            });
            linear
                .create_activity(session_id, &request.id, &activity)
                .await
        }
    };
    outcome?;
    Ok(true)
}

fn workflow_done_current(run: &Run, issue: &IssueDetail) -> Result<bool, ApiError> {
    let current = run.record().map_err(|_| ApiError::Configuration)?;
    Ok(current.status == Status::Active
        && current.workflow.postmortem_succeeded
        && current.workflow.confirmed == Some(WorkflowStage::InPostmortem)
        && (current.workflow_state_id == issue.state.id || issue.state.r#type == "completed"))
}

fn confirm_stage(
    run: &Run,
    stage: WorkflowStage,
    state_id: &str,
    state_name: &str,
    updated_at: &str,
    selected: bool,
) -> Result<(), ApiError> {
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    run.update_held(&lock, |record| {
        if record.status != Status::Active
            || (stage == WorkflowStage::Done
                && (!record.workflow.postmortem_succeeded
                    || record.workflow.confirmed != Some(WorkflowStage::InPostmortem)))
        {
            return;
        }
        let entered_postmortem = stage == WorkflowStage::InPostmortem
            && record.workflow.confirmed != Some(WorkflowStage::InPostmortem);
        record.workflow.confirmed = Some(stage);
        record.workflow.selected_state_id = selected.then(|| state_id.to_string());
        record.workflow_state_id = state_id.to_string();
        record.workflow_state_updated_at = updated_at.to_string();
        if stage == WorkflowStage::InProgress
            && let Some(cursor) = &mut record.issue_state
        {
            cursor.pickup_state = state_name.to_string();
        }
        if record
            .workflow
            .pending_choice
            .as_ref()
            .is_some_and(|choice| choice.stage == stage)
        {
            record.workflow.pending_choice = None;
        }
        if entered_postmortem
            && !record.workflow.postmortem_succeeded
            && record.postmortem_due.is_none()
        {
            record.postmortem_due = Some(crate::postmortem::Stage::Workflow);
        }
    })
    .map(|_| ())
    .map_err(|_| ApiError::Configuration)
}

fn clear_stage_choice(run: &Run, stage: WorkflowStage) -> Result<(), ApiError> {
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    run.update_held(&lock, |record| {
        let matches_choice = record
            .workflow
            .pending_choice
            .as_ref()
            .is_some_and(|choice| choice.stage == stage);
        if matches_choice {
            record.workflow.pending_choice = None;
        }
        if matches_choice
            && record
                .awaiting_reply
                .as_ref()
                .is_some_and(|wait| wait.reason == WaitReason::WorkflowState)
        {
            record.cleared_wait_id = record.awaiting_reply.take().unwrap().activity_id;
        }
    })
    .map(|_| ())
    .map_err(|_| ApiError::Configuration)
}

fn retarget_pickup_state(run: &Run, state_name: &str) -> Result<(), ApiError> {
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    run.update_held(&lock, |record| {
        if let Some(cursor) = &mut record.issue_state {
            cursor.pickup_state = state_name.to_string();
        }
    })
    .map(|_| ())
    .map_err(|_| ApiError::Configuration)
}

fn queue_state_question(
    run: &Run,
    issue: &IssueDetail,
    stage: WorkflowStage,
) -> Result<(), ApiError> {
    let options: Vec<_> = issue
        .team
        .states
        .iter()
        .filter(|state| {
            if stage == WorkflowStage::Done {
                state.r#type == "completed"
            } else {
                !matches!(state.r#type.as_str(), "completed" | "canceled")
            }
        })
        .map(|state| crate::run::WorkflowStateOption {
            id: state.id.clone(),
            name: state.name.clone(),
        })
        .collect();
    let labels: Vec<_> = options
        .iter()
        .map(|option| (option.name.as_str(), option.id.as_str()))
        .collect();
    let stage_name = match stage {
        WorkflowStage::InProgress => "In Progress",
        WorkflowStage::InReview => "In Review",
        WorkflowStage::InDeploy => "In Deploy",
        WorkflowStage::InPostmortem => "In Postmortem",
        WorkflowStage::Done => "Done",
    };
    let question = format!(
        "The configured Linear state for {stage_name} is missing. Choose a state by its label or ID."
    );
    let mut activity = match Op::awaiting_reply(question, &labels, WaitReason::WorkflowState) {
        Op::Activity { activity, .. } => activity,
        _ => unreachable!(),
    };
    activity.ephemeral = false;
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let current = run.record().map_err(|_| ApiError::Configuration)?;
    if current
        .awaiting_reply
        .is_some_and(|wait| wait.reason != WaitReason::WorkflowState)
    {
        return Err(ApiError::RequestFailed);
    }
    let mut should_ask = true;
    run.update_held(&lock, |record| {
        if record
            .awaiting_reply
            .as_ref()
            .is_some_and(|wait| wait.reason == WaitReason::WorkflowState)
            && record
                .workflow
                .pending_choice
                .as_ref()
                .is_some_and(|choice| {
                    choice.stage == stage && choice.expected_current_state_id == issue.state.id
                })
        {
            should_ask = false;
            return;
        }
        if let Some(wait) = record.awaiting_reply.take()
            && wait.reason == WaitReason::WorkflowState
        {
            record.cleared_wait_id = wait.activity_id;
        }
        record.workflow.pending_choice = Some(crate::run::WorkflowStateChoice {
            stage,
            expected_current_state_id: issue.state.id.clone(),
            options: options.clone(),
        });
    })
    .map_err(|_| ApiError::Configuration)?;
    if should_ask {
        let queued = push_held(
            run,
            &lock,
            Op::Activity {
                activity,
                wait_reason: Some(WaitReason::WorkflowState),
            },
        )
        .map_err(|_| ApiError::Configuration)?;
        if queued.is_none() {
            return Err(ApiError::RequestFailed);
        }
    }
    Ok(())
}

fn mark_turn_complete_attempt_if_current(
    path: &Path,
    request: &mut Request,
    run: &Run,
) -> Result<bool, ApiError> {
    let _lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    if record.status != crate::run::Status::Active
        || record.stopped
        || record.finished
        || record.awaiting_reply.is_some()
        || record.turn_complete.as_deref() != Some(&request.id)
    {
        return Ok(false);
    }
    request.attempted = true;
    files::write_json(path, request).map_err(|_| ApiError::Configuration)?;
    Ok(true)
}

fn clear_refused_turn_complete(run: &Run, activity_id: &str) -> Result<bool, ApiError> {
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    if record.turn_complete.as_deref() != Some(activity_id) {
        return Ok(false);
    }
    run.update_held(&lock, |record| record.turn_complete = None)
        .map_err(|_| ApiError::Configuration)?;
    crate::inbox::write_held(
        run,
        &lock,
        "linear",
        "wait",
        "Linear refused the coordinator turn completion. The run remains active; see the ticker log for the error.",
    )
    .map_err(|_| ApiError::Configuration)?;
    Ok(true)
}

async fn restore_session(
    run: &Run,
    issue_id: &str,
    token: &str,
    allowed_users: &[String],
    linear: &impl LinearApi,
) -> Result<(), ApiError> {
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    let Some(request) = record.resume_request.as_ref().filter(|r| r.token == token) else {
        return Ok(());
    };
    let ResumeDecision::Approved {
        comment_id,
        user_id,
    } = &request.decision
    else {
        return Ok(());
    };
    if record.status != Status::Detached {
        return Ok(());
    }
    let read = || linear.resume_context(issue_id, &request.comment_id, token);
    let check = |context: &crate::linear::api::ResumeContext| {
        if context.terminal
            || context.session_hidden
            || context.team != record.team_key
            || !context.question_exists
            || context
                .delegate_id
                .as_ref()
                .is_some_and(|id| id != &context.app_user)
            || !context.comments.iter().any(|c| {
                !c.app
                    && c.id == *comment_id
                    && c.user_id == *user_id
                    && c.body == format!("resume {token}")
            })
            || context.comments.iter().any(|c| {
                !c.app && allowed_users.contains(&c.user_id) && c.body == format!("stop {token}")
            })
        {
            return Err(ApiError::Graphql(
                "Resume consent or issue ownership changed; no automatic retry is allowed.".into(),
            ));
        }
        Ok(())
    };
    let context = read().await?;
    check(&context)?;
    // Delegation is idempotent. A lost response is verified by this same
    // read on retry before another mutation is attempted.
    if context.delegate_id.as_ref() != Some(&context.app_user) {
        if request.delegate_attempted {
            return Err(ApiError::Graphql(
                "The previous delegate restoration cannot be verified; fresh consent is required."
                    .into(),
            ));
        }
        mark_restore_attempt(run, token, false).await?;
        linear.restore_delegate(issue_id, &context.app_user).await?;
    }
    let context = read().await?;
    check(&context)?;
    if context.delegate_id.as_ref() != Some(&context.app_user) {
        return Err(ApiError::RequestFailed);
    }
    if context.session_id.is_none() {
        if request.session_attempted {
            return Err(ApiError::Graphql(
                "The previous session creation cannot be verified; fresh consent is required."
                    .into(),
            ));
        }
        mark_restore_attempt(run, token, true).await?;
        linear
            .create_session_on_comment(&request.comment_id)
            .await?;
    }
    // The public comment.agentSession relation verifies unknown creations,
    // avoiding another session when a successful response was lost.
    let context = read().await?;
    check(&context)?;
    if context.delegate_id.as_ref() != Some(&context.app_user) {
        return Err(ApiError::RequestFailed);
    }
    let session = context.session_id.ok_or(ApiError::RequestFailed)?;
    let run = run.clone();
    let token = token.to_string();
    tokio::task::spawn_blocking(move || {
        run.update(|r| {
            if r.status == Status::Detached
                && let Some(request) = &mut r.resume_request
                && request.token == token
                && matches!(request.decision, ResumeDecision::Approved { .. })
            {
                request.restored_session_id = session;
            }
        })
    })
    .await
    .map_err(|_| ApiError::Configuration)?
    .map_err(|_| ApiError::Configuration)?;
    Ok(())
}

async fn mark_restore_attempt(run: &Run, token: &str, session: bool) -> Result<(), ApiError> {
    let run = run.clone();
    let token = token.to_string();
    tokio::task::spawn_blocking(move || {
        let lock = run.lock().map_err(|_| ApiError::Configuration)?;
        let current = run.record().map_err(|_| ApiError::Configuration)?;
        if current.status != Status::Detached
            || !current.resume_request.as_ref().is_some_and(|r| {
                r.token == token && matches!(r.decision, ResumeDecision::Approved { .. })
            })
        {
            return Err(ApiError::RequestFailed);
        }
        run.update_held(&lock, |r| {
            if let Some(request) = &mut r.resume_request {
                if session {
                    request.session_attempted = true;
                } else {
                    request.delegate_attempted = true;
                }
            }
        })
        .map_err(|_| ApiError::Configuration)?;
        Ok(())
    })
    .await
    .map_err(|_| ApiError::Configuration)?
}

fn mark_wait_attempt_if_current(
    path: &Path,
    request: &mut Request,
    run: &Run,
) -> Result<bool, ApiError> {
    let _lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    if record.status != crate::run::Status::Active
        || record.stopped
        || record.finished
        || !record
            .awaiting_reply
            .is_some_and(|wait| wait.activity_id == request.id)
    {
        return Ok(false);
    }
    request.attempted = true;
    files::write_json(path, request).map_err(|_| ApiError::Configuration)?;
    Ok(true)
}

fn clear_refused_wait(run: &Run, activity_id: &str) -> Result<bool, ApiError> {
    let lock = run.lock().map_err(|_| ApiError::Configuration)?;
    let record = run.record().map_err(|_| ApiError::Configuration)?;
    if !record
        .awaiting_reply
        .is_some_and(|wait| wait.activity_id == activity_id)
    {
        return Ok(false);
    }
    run.update_held(&lock, |record| {
        if let Some(wait) = record.awaiting_reply.take() {
            record.cleared_wait_id = wait.activity_id;
        }
    })
    .map_err(|_| ApiError::Configuration)?;
    Ok(true)
}

/// The state to move the issue to, or `None` when it should stay where it is.
pub fn target_state(
    issue: &IssueDetail,
    target: StateTarget,
    state_names: &StateNames,
) -> Result<Option<String>, ApiError> {
    let current = &issue.state;
    match target {
        StateTarget::Started => {
            if matches!(
                current.r#type.as_str(),
                "started" | "completed" | "canceled"
            ) {
                return Ok(None);
            }
            let first = issue
                .team
                .states
                .iter()
                .filter(|s| s.r#type == "started")
                .min_by(|a, b| a.position.total_cmp(&b.position));
            first.map(|s| Some(s.id.clone())).ok_or_else(|| {
                ApiError::Graphql(format!("team {} has no started state", issue.team.key))
            })
        }
        StateTarget::Review => named_state(issue, current, &state_names.review, None),
        StateTarget::Workflow(stage) => {
            let (name, expected_type) = match stage {
                WorkflowStage::InProgress => (&state_names.started, Some("started")),
                WorkflowStage::InReview => (&state_names.review, None),
                WorkflowStage::InDeploy => (&state_names.deploy, None),
                WorkflowStage::InPostmortem => (&state_names.postmortem, None),
                WorkflowStage::Done => (&state_names.done, Some("completed")),
            };
            named_state(issue, current, name, expected_type)
        }
        StateTarget::Selected { .. } => {
            unreachable!("selected targets are validated before resolution")
        }
    }
}

fn named_state(
    issue: &IssueDetail,
    current: &crate::linear::api::WorkflowState,
    name: &str,
    expected_type: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if (current.name.eq_ignore_ascii_case(name) && expected_type.is_none())
        || matches!(current.r#type.as_str(), "completed" | "canceled")
    {
        return Ok(None);
    }
    let state = issue.team.states.iter().find(|state| {
        state.name.eq_ignore_ascii_case(name)
            && expected_type.is_none_or(|expected| state.r#type == expected)
    });
    state.map(|state| Some(state.id.clone())).ok_or_else(|| {
        ApiError::Graphql(format!(
            "team {} has no state named `{name}`",
            issue.team.key
        ))
    })
}

/// Parses the coordinator's Markdown checklist into Linear's plan: a list of
/// `{content, status}`.
pub fn parse_plan(text: &str) -> Result<Value> {
    let mut steps = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(rest) = line
            .strip_prefix("- [")
            .or_else(|| line.strip_prefix("* ["))
        else {
            bail!("plan lines look like `- [ ] step`; got `{line}`");
        };
        let (mark, content) = rest
            .split_once(']')
            .filter(|(mark, _)| mark.chars().count() == 1)
            .ok_or_else(|| anyhow::anyhow!("plan lines look like `- [ ] step`; got `{line}`"))?;
        let status = match mark {
            " " => "pending",
            ">" => "inProgress",
            "x" | "X" => "completed",
            "-" => "canceled",
            other => bail!("`[{other}]` is not a plan mark; use [ ], [>], [x] or [-]"),
        };
        let content = content.trim();
        if content.is_empty() {
            bail!("a plan step is empty: `{line}`");
        }
        steps.push(serde_json::json!({ "content": content, "status": status }));
    }
    if steps.is_empty() {
        bail!("the plan has no steps");
    }
    Ok(Value::Array(steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::api::Content;
    use std::sync::Mutex;

    use crate::linear::api::fake::FakeLinear;

    fn state_names() -> StateNames {
        StateNames {
            started: "In Progress".into(),
            review: "In Review".into(),
            deploy: "In Deploy".into(),
            postmortem: "In Postmortem".into(),
            done: "Done".into(),
        }
    }
    use crate::run::RunRecord;

    fn thought(body: &str) -> Op {
        Op::activity(Activity::new(Content::Thought { body: body.into() }))
    }

    fn setup() -> (tempfile::TempDir, Run, Mutex<FakeLinear>, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let run = Run::create(
            dir.path(),
            RunRecord {
                workspace: "acme".into(),
                identifier: "DATA-1".into(),
                ..RunRecord::default()
            },
        )
        .unwrap();
        let mut fake = FakeLinear::default();
        let issue = fake.add_issue("DATA-1", "DATA", "First");
        let session = fake.delegate_session("DATA-1");
        (dir, run, Mutex::new(fake), session, issue)
    }

    #[tokio::test]
    async fn a_queued_turn_completion_is_discarded_after_closing_without_a_session() {
        let (_dir, run, linear, _session, issue) = setup();
        push(
            &run,
            Op::TurnComplete {
                body: "Done for now.".into(),
            },
        )
        .unwrap();
        run.update(|record| {
            record.status = Status::Closed;
            record.session_id.clear();
        })
        .unwrap();
        let sent = send(&run, "", &issue, &state_names(), &linear).await;
        assert!(sent.blocked.is_none());
        assert!(pending(&run).is_empty());
        assert_eq!(
            linear
                .lock()
                .unwrap()
                .count("HerdrLinearAgentActivityCreate"),
            0
        );
    }

    #[tokio::test]
    async fn requests_go_out_in_order_and_are_removed() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, thought("one")).unwrap();
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Started,
            },
        )
        .unwrap();
        push(&run, thought("two")).unwrap();
        push(
            &run,
            Op::Plan {
                plan: parse_plan("- [x] a\n- [ ] b").unwrap(),
            },
        )
        .unwrap();
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.count, 4);
        assert!(sent.activity_sent && sent.blocked.is_none());
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        let bodies: Vec<Value> = fake.sessions[0]
            .sent("thought")
            .iter()
            .map(|a| a["content"]["body"].clone())
            .collect();
        assert_eq!(bodies, ["one", "two"]);
        assert_eq!(
            fake.issue("DATA-1")["state"]["name"],
            "In Progress",
            "the started state with the lowest position"
        );
        assert_eq!(
            fake.sessions[0].plan.as_ref().unwrap()[1]["status"],
            "pending"
        );
    }

    #[tokio::test]
    async fn a_lost_response_is_checked_by_a_read_and_never_sent_twice() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, thought("once")).unwrap();
        linear.lock().unwrap().lose_next_response = true;
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert!(pending(&run)[0].1.attempted);
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.count, 1);
        let fake = linear.lock().unwrap();
        assert_eq!(fake.sessions[0].sent("thought").len(), 1);
        assert_eq!(fake.count("HerdrLinearAgentActivityFind"), 1);
    }

    #[tokio::test]
    async fn a_replayed_postmortem_comment_keeps_its_identity_across_queue_and_marker_gaps() {
        let (_dir, run, linear, session, issue) = setup();
        let id = uuid::Uuid::new_v4().to_string();
        let body = "Postmortem summary.";
        {
            let lock = run.lock().unwrap();
            push_idempotent_comment_held(&run, &lock, &id, body).unwrap();
            push_idempotent_comment_held(&run, &lock, &id, body).unwrap();
            assert!(push_idempotent_comment_held(&run, &lock, &id, "changed").is_err());
        }
        assert_eq!(
            pending(&run).len(),
            1,
            "a pre-marker replay reuses the queue item"
        );
        assert!(pending(&run)[0].1.check_before_send);

        linear.lock().unwrap().lose_next_response = true;
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert_eq!(pending(&run).len(), 1);
        {
            let lock = run.lock().unwrap();
            push_idempotent_comment_held(&run, &lock, &id, body).unwrap();
        }
        let replay = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(
            replay.count, 1,
            "the uncertain write is confirmed by its stable id"
        );
        assert!(pending(&run).is_empty());

        // A crash after outbox removal but before the caller's marker requeues
        // the same identity. The preflight read sees the existing comment.
        {
            let lock = run.lock().unwrap();
            push_idempotent_comment_held(&run, &lock, &id, body).unwrap();
        }
        assert_eq!(pending(&run).len(), 1);
        assert!(pending(&run)[0].1.check_before_send);
        let after_delivery = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(after_delivery.count, 1);
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        assert_eq!(fake.count("HerdrLinearAgentCommentCreate"), 1);
        assert_eq!(fake.count("HerdrLinearAgentCommentFind"), 3);
    }

    #[tokio::test]
    async fn a_cleared_wait_with_an_unknown_write_is_read_back_but_never_resent() {
        let (_dir, run, linear, session, issue) = setup();
        let id = push(&run, Op::elicitation("Proceed?", &[]))
            .unwrap()
            .unwrap();
        linear.lock().unwrap().lose_next_response = true;
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        run.update(|r| r.awaiting_reply = None).unwrap();

        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.count, 1, "the existing Linear activity is reconciled");
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        assert_eq!(fake.sessions[0].sent("elicitation").len(), 1);
        assert_eq!(fake.count("HerdrLinearAgentActivityFind"), 1);
        assert_ne!(id, "");
    }

    #[tokio::test]
    async fn a_cleared_turn_completion_with_an_unknown_write_is_read_back_but_never_resent() {
        let (_dir, run, linear, session, issue) = setup();
        push(
            &run,
            Op::TurnComplete {
                body: "The investigation is queued.".into(),
            },
        )
        .unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::RequestFailed);
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert!(run.record().unwrap().turn_complete.is_some());

        // New work resumes the run before retry; the old completion must not
        // be sent after its failed read-back.
        push(&run, thought("Continuing the investigation.")).unwrap();
        assert!(run.record().unwrap().turn_complete.is_none());
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.count, 2, "stale response is removed, new thought sent");
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        assert!(fake.sessions[0].sent("response").is_empty());
        assert_eq!(fake.sessions[0].sent("thought").len(), 1);
        assert_eq!(fake.count("HerdrLinearAgentActivityFind"), 1);
    }

    #[tokio::test]
    async fn a_lost_turn_completion_response_is_confirmed_once_by_activity_id() {
        let (_dir, run, linear, session, issue) = setup();
        let id = push(
            &run,
            Op::TurnComplete {
                body: "The investigation is queued.".into(),
            },
        )
        .unwrap()
        .unwrap();
        linear.lock().unwrap().lose_next_response = true;

        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert_eq!(
            run.record().unwrap().turn_complete.as_deref(),
            Some(id.as_str())
        );
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;

        assert_eq!(sent.count, 1);
        assert!(sent.activity_sent);
        assert!(pending(&run).is_empty());
        let fake = linear.lock().unwrap();
        let responses = fake.sessions[0].sent("response");
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]["content"]["body"],
            "The investigation is queued."
        );
        assert_eq!(fake.count("HerdrLinearAgentActivityFind"), 1);
        assert_eq!(
            run.record().unwrap().turn_complete.as_deref(),
            Some(id.as_str())
        );
    }

    #[tokio::test]
    async fn a_definitive_turn_completion_refusal_clears_marker_and_writes_inbox_notice() {
        let (_dir, run, linear, session, issue) = setup();
        push(
            &run,
            Op::TurnComplete {
                body: "The investigation is queued.".into(),
            },
        )
        .unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::Graphql("invalid activity".into()));

        let sent = send(&run, &session, &issue, &state_names(), &linear).await;

        assert_eq!(sent.refused.len(), 1);
        assert!(run.record().unwrap().turn_complete.is_none());
        assert!(pending(&run).is_empty());
        assert!(crate::inbox::unhandled(&run).iter().any(|item| {
            item.kind == "linear"
                && item.subject == "wait"
                && item.summary.contains("turn completion")
        }));
    }

    #[test]
    fn a_queued_question_repairs_a_crash_but_not_a_question_cleared_by_a_reply() {
        let (_dir, run, _linear, _session, _issue) = setup();
        let id = push(&run, Op::elicitation("Proceed?", &[]))
            .unwrap()
            .unwrap();
        run.update(|r| r.awaiting_reply = None).unwrap();
        assert_eq!(
            run.record().unwrap().awaiting_reply.unwrap().activity_id,
            id
        );

        run.update(|r| {
            r.awaiting_reply = None;
            r.cleared_wait_id = id.clone();
            r.reply_generation += 1;
        })
        .unwrap();
        let runs_dir = run.dir.parent().unwrap().parent().unwrap();
        let restarted = Run::load(runs_dir, &run.key).unwrap();
        assert!(restarted.record().unwrap().awaiting_reply.is_none());
    }

    #[tokio::test]
    async fn a_definitive_question_refusal_clears_only_its_matching_wait() {
        let (_dir, run, linear, session, issue) = setup();
        let id = push(&run, Op::elicitation("Proceed?", &[]))
            .unwrap()
            .unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::Graphql("invalid activity".into()));
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.refused.len(), 1);
        assert!(run.record().unwrap().awaiting_reply.is_none());
        assert!(
            crate::inbox::unhandled(&run)
                .iter()
                .any(|item| item.summary.contains("refused a question"))
        );

        let next = push(&run, Op::elicitation("Another question?", &[]))
            .unwrap()
            .unwrap();
        assert_ne!(id, next);
        assert_eq!(
            run.record().unwrap().awaiting_reply.unwrap().activity_id,
            next
        );
    }

    #[tokio::test]
    async fn unknown_question_outcomes_keep_the_wait_and_rate_limited_questions_stay_queued() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, Op::elicitation("Proceed?", &[])).unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::RateLimited);
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RateLimited));
        assert!(run.record().unwrap().awaiting_reply.is_some());
        assert_eq!(pending(&run).len(), 1);

        linear.lock().unwrap().fail_next = Some(ApiError::RequestFailed);
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.blocked, Some(ApiError::RequestFailed));
        assert!(run.record().unwrap().awaiting_reply.is_some());
        assert_eq!(pending(&run).len(), 1);
    }

    #[test]
    fn stopped_and_inactive_runs_hold_progress_but_still_allow_terminal_responses() {
        let (_dir, run, _linear, _session, _issue) = setup();
        run.update(|r| r.status = crate::run::Status::Detached)
            .unwrap();
        assert!(push(&run, thought("progress")).unwrap().is_none());
        assert!(
            push(
                &run,
                Op::activity(Activity::new(Content::Response {
                    body: "Stopped.".into(),
                }))
            )
            .unwrap()
            .is_some()
        );
        assert_eq!(pending(&run).len(), 1);
    }

    #[tokio::test]
    async fn a_failure_keeps_the_rest_in_order_and_a_refusal_is_set_aside() {
        let (_dir, run, linear, session, issue) = setup();
        push(&run, thought("a")).unwrap();
        push(&run, thought("b")).unwrap();
        linear.lock().unwrap().fail_next = Some(ApiError::HttpStatus(503));
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!((sent.count, pending(&run).len()), (0, 2));
        // An attempted request whose send never reached Linear is read, then sent.
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.count, 2);

        linear.lock().unwrap().issue_mut("DATA-1")["team"]["states"]["nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|state| state["name"] != "In Review");
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Review,
            },
        )
        .unwrap();
        push(&run, thought("after")).unwrap();
        let sent = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(sent.refused.len(), 1);
        assert_eq!(sent.count, 1);
        assert!(
            run.state_dir()
                .join("outbox/failed")
                .read_dir()
                .unwrap()
                .count()
                == 1
        );
    }

    #[tokio::test]
    async fn state_targets_leave_later_states_alone() {
        let (_dir, _run, linear, _session, issue) = setup();
        let mut detail = linear.issue(&issue).await.unwrap();
        assert_eq!(
            target_state(&detail, StateTarget::Started, &state_names())
                .unwrap()
                .as_deref(),
            Some("state-progress")
        );
        assert_eq!(
            target_state(&detail, StateTarget::Review, &state_names())
                .unwrap()
                .as_deref(),
            Some("state-review")
        );
        detail.state = detail
            .team
            .states
            .iter()
            .find(|s| s.name == "In Review")
            .cloned()
            .unwrap();
        assert_eq!(
            target_state(&detail, StateTarget::Started, &state_names()).unwrap(),
            None
        );
        assert_eq!(
            target_state(&detail, StateTarget::Review, &state_names()).unwrap(),
            None
        );
        detail.state = detail
            .team
            .states
            .iter()
            .find(|s| s.name == "Done")
            .cloned()
            .unwrap();
        assert_eq!(
            target_state(&detail, StateTarget::Review, &state_names()).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_missing_workflow_state_persists_a_correlated_choice_question() {
        let (_dir, run, linear, session, issue) = setup();
        linear.lock().unwrap().issue_mut("DATA-1")["team"]["states"]["nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|state| state["name"] != "In Review");
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Workflow(WorkflowStage::InReview),
            },
        )
        .unwrap();

        let sent = send(&run, &session, &issue, &state_names(), &linear).await;

        assert!(sent.blocked.is_none());
        let record = run.record().unwrap();
        let choice = record.workflow.pending_choice.unwrap();
        assert_eq!(choice.stage, WorkflowStage::InReview);
        assert_eq!(choice.expected_current_state_id, "state-todo");
        assert!(
            choice
                .options
                .iter()
                .any(|option| option.id == "state-progress")
        );
        assert_eq!(
            record.awaiting_reply.unwrap().reason,
            crate::run::WaitReason::WorkflowState
        );
        assert_eq!(pending(&run).len(), 1);
    }

    #[tokio::test]
    async fn selected_initial_started_state_retargets_the_pickup_cursor() {
        let (_dir, run, linear, session, issue) = setup();
        run.update(|record| {
            record.issue_state = Some(crate::run::IssueStateCursor {
                name: "Todo".into(),
                state_type: "unstarted".into(),
                updated_at: "2026-10-12T00:00:00Z".into(),
                pickup_state: "Missing Started State".into(),
            });
        })
        .unwrap();
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Selected {
                    stage: WorkflowStage::InProgress,
                    state_id: "state-review".into(),
                    expected_current_state_id: "state-todo".into(),
                },
            },
        )
        .unwrap();

        let sent = send(&run, &session, &issue, &state_names(), &linear).await;

        assert!(sent.blocked.is_none());
        let cursor = run.record().unwrap().issue_state.unwrap();
        assert_eq!(cursor.name, "Todo");
        assert_eq!(cursor.updated_at, "2026-10-12T00:00:00Z");
        assert_eq!(cursor.pickup_state, "In Review");
    }

    #[tokio::test]
    async fn selected_started_state_readback_after_a_lost_response_does_not_reask() {
        let (_dir, run, linear, session, issue) = setup();
        run.update(|record| {
            record.issue_state = Some(crate::run::IssueStateCursor {
                name: "Todo".into(),
                state_type: "unstarted".into(),
                updated_at: "2026-10-12T00:00:00Z".into(),
                pickup_state: "Missing Started State".into(),
            });
        })
        .unwrap();
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Selected {
                    stage: WorkflowStage::InProgress,
                    state_id: "state-review".into(),
                    expected_current_state_id: "state-todo".into(),
                },
            },
        )
        .unwrap();
        linear.lock().unwrap().lose_next_response = true;

        let first = send(&run, &session, &issue, &state_names(), &linear).await;
        assert_eq!(first.blocked, Some(ApiError::RequestFailed));
        assert_eq!(
            run.record().unwrap().issue_state.unwrap().pickup_state,
            "In Review"
        );
        assert_eq!(
            linear.lock().unwrap().issue("DATA-1")["state"]["name"],
            "In Review"
        );

        let second = send(&run, &session, &issue, &state_names(), &linear).await;
        assert!(second.blocked.is_none());
        let record = run.record().unwrap();
        assert_eq!(record.workflow.confirmed, Some(WorkflowStage::InProgress));
        assert!(record.workflow.pending_choice.is_none());
        assert!(pending(&run).is_empty());
    }

    #[tokio::test]
    async fn a_canceled_issue_does_not_confirm_a_pending_done_choice() {
        let (_dir, run, linear, session, issue) = setup();
        let canceled = linear.lock().unwrap().issue("DATA-1")["team"]["states"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| state["type"] == "canceled")
            .unwrap()
            .clone();
        linear.lock().unwrap().issue_mut("DATA-1")["state"] = canceled;
        run.update(|record| {
            record.workflow.confirmed = Some(WorkflowStage::InPostmortem);
            record.workflow.pending_choice = Some(crate::run::WorkflowStateChoice {
                stage: WorkflowStage::Done,
                expected_current_state_id: "state-todo".into(),
                options: vec![crate::run::WorkflowStateOption {
                    id: "state-done".into(),
                    name: "Done".into(),
                }],
            });
            record.awaiting_reply = Some(AwaitingReply {
                activity_id: "choice-question".into(),
                asked_at: "2026-10-12T00:00:00Z".into(),
                reason: WaitReason::WorkflowState,
            });
        })
        .unwrap();
        push(
            &run,
            Op::IssueState {
                target: StateTarget::Selected {
                    stage: WorkflowStage::Done,
                    state_id: "state-done".into(),
                    expected_current_state_id: "state-todo".into(),
                },
            },
        )
        .unwrap();

        let sent = send(&run, &session, &issue, &state_names(), &linear).await;

        assert!(sent.blocked.is_none());
        assert!(pending(&run).is_empty());
        let record = run.record().unwrap();
        assert_eq!(record.workflow.confirmed, Some(WorkflowStage::InPostmortem));
        assert!(record.workflow.pending_choice.is_none());
        assert!(record.awaiting_reply.is_none());
    }

    #[test]
    fn plans_parse_from_a_checklist() {
        let plan = parse_plan(
            "- [ ] Read the code\n- [>] Start workers\n* [x] Done thing\n- [-] Dropped\n",
        )
        .unwrap();
        let statuses: Vec<&str> = plan
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["pending", "inProgress", "completed", "canceled"]);
        assert_eq!(plan[0]["content"], "Read the code");
        for bad in ["", "Read the code", "- [?] x", "- [ ]   ", "- [xx] y"] {
            assert!(parse_plan(bad).is_err(), "{bad}");
        }
    }

    async fn approved_recovery() -> (tempfile::TempDir, Run, Mutex<FakeLinear>, String, String) {
        let (dir, run, linear, session, issue) = setup();
        let token = uuid::Uuid::new_v4().to_string();
        let question = uuid::Uuid::new_v4().to_string();
        linear
            .create_comment(&issue, &question, "Request consent")
            .await
            .unwrap();
        let approval = linear.lock().unwrap().consent_comment(
            "DATA-1",
            "user-1",
            &format!("resume {token}"),
            false,
        );
        run.update(|r| {
            r.issue_id = issue.clone();
            r.team_key = "DATA".into();
            r.status = Status::Detached;
            r.session_id = session.clone();
            r.resume_request = Some(crate::run::ResumeRequest {
                token: token.clone(),
                comment_id: question,
                decision: ResumeDecision::Approved {
                    comment_id: approval,
                    user_id: "user-1".into(),
                },
                restored_session_id: String::new(),
                restore_error: String::new(),
                delegate_attempted: false,
                session_attempted: false,
            });
        })
        .unwrap();
        push(
            &run,
            Op::RestoreSession {
                token,
                allowed_users: vec!["user-1".into()],
            },
        )
        .unwrap();
        (dir, run, linear, session, issue)
    }

    #[tokio::test]
    async fn a_lost_recovery_response_is_verified_before_retrying_either_write() {
        for delegation_missing in [false, true] {
            let (_dir, run, linear, _session, issue) = approved_recovery().await;
            {
                let mut fake = linear.lock().unwrap();
                if delegation_missing {
                    fake.issue_mut("DATA-1")["delegate"] = Value::Null;
                }
                fake.lose_next_response = true;
            }
            let first = send(&run, "", &issue, &state_names(), &linear).await;
            assert_eq!(first.blocked, Some(ApiError::RequestFailed));
            let second = send(&run, "", &issue, &state_names(), &linear).await;
            assert!(second.blocked.is_none());
            assert!(second.refused.is_empty());
            assert_eq!(
                linear
                    .lock()
                    .unwrap()
                    .count("HerdrLinearAgentResumeSession"),
                1
            );
            assert_eq!(
                linear
                    .lock()
                    .unwrap()
                    .count("HerdrLinearAgentRestoreDelegate"),
                usize::from(delegation_missing)
            );
            assert!(
                !run.record()
                    .unwrap()
                    .resume_request
                    .unwrap()
                    .restored_session_id
                    .is_empty()
            );
            assert!(pending(&run).is_empty());
        }
    }

    #[tokio::test]
    async fn edited_consent_and_terminal_issues_are_rechecked_before_recovery_writes() {
        for terminal in [false, true] {
            let (_dir, run, linear, _session, issue) = approved_recovery().await;
            {
                let mut fake = linear.lock().unwrap();
                fake.issue_mut("DATA-1")["delegate"] = Value::Null;
                if terminal {
                    fake.set_state("DATA-1", "Done");
                } else {
                    let comments = fake.issue_mut("DATA-1")["comments"]["nodes"]
                        .as_array_mut()
                        .unwrap();
                    let approval = comments
                        .iter_mut()
                        .find(|c| c["body"].as_str().is_some_and(|b| b.starts_with("resume ")))
                        .unwrap();
                    approval["body"] = json!("I withdraw my approval.");
                }
            }
            let sent = send(&run, "", &issue, &state_names(), &linear).await;
            assert_eq!(sent.refused.len(), 1);
            assert_eq!(
                linear
                    .lock()
                    .unwrap()
                    .count("HerdrLinearAgentRestoreDelegate"),
                0
            );
            assert_eq!(
                linear
                    .lock()
                    .unwrap()
                    .count("HerdrLinearAgentResumeSession"),
                0
            );
            assert!(
                !run.record()
                    .unwrap()
                    .resume_request
                    .unwrap()
                    .restore_error
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn a_removed_uncertain_recovery_result_requires_new_consent_instead_of_recreation() {
        for delegation_missing in [false, true] {
            let (_dir, run, linear, _session, issue) = approved_recovery().await;
            {
                let mut fake = linear.lock().unwrap();
                if delegation_missing {
                    fake.issue_mut("DATA-1")["delegate"] = Value::Null;
                }
                fake.lose_next_response = true;
            }
            assert_eq!(
                send(&run, "", &issue, &state_names(), &linear)
                    .await
                    .blocked,
                Some(ApiError::RequestFailed)
            );
            {
                let mut fake = linear.lock().unwrap();
                if delegation_missing {
                    fake.issue_mut("DATA-1")["delegate"] = Value::Null;
                } else {
                    fake.sessions.last_mut().unwrap().dismissed_at =
                        Some("2026-10-09T05:30:14Z".into());
                }
            }
            let sent = send(&run, "", &issue, &state_names(), &linear).await;
            assert_eq!(sent.refused.len(), 1);
            assert_eq!(
                linear
                    .lock()
                    .unwrap()
                    .count("HerdrLinearAgentRestoreDelegate"),
                usize::from(delegation_missing)
            );
            assert_eq!(
                linear
                    .lock()
                    .unwrap()
                    .count("HerdrLinearAgentResumeSession"),
                usize::from(!delegation_missing)
            );
            assert!(
                !run.record()
                    .unwrap()
                    .resume_request
                    .unwrap()
                    .restore_error
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn an_undelivered_consent_question_is_discarded_after_its_run_closes() {
        let (_dir, run, linear, _session, issue) = setup();
        let token = uuid::Uuid::new_v4().to_string();
        let id = push(
            &run,
            Op::ResumeQuestion {
                token: token.clone(),
                body: "Request consent".into(),
            },
        )
        .unwrap()
        .unwrap();
        run.update(|r| {
            r.status = Status::Detached;
            r.resume_request = Some(crate::run::ResumeRequest {
                token,
                comment_id: id,
                decision: ResumeDecision::Waiting,
                restored_session_id: String::new(),
                restore_error: String::new(),
                delegate_attempted: false,
                session_attempted: false,
            });
        })
        .unwrap();
        run.update(|r| {
            r.status = Status::Closed;
            r.resume_request = None;
        })
        .unwrap();
        let sent = send(&run, "", &issue, &state_names(), &linear).await;
        assert!(sent.blocked.is_none() && sent.refused.is_empty());
        assert_eq!(
            linear
                .lock()
                .unwrap()
                .count("HerdrLinearAgentCommentCreate"),
            0
        );
        assert!(pending(&run).is_empty());
    }
}
