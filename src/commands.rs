//! The subcommands a coordinator runs. Each is one deterministic mechanic:
//! the coordinator decides whether, what and where; these commands check names
//! against the config, enforce the limits, and queue Linear writes for the
//! ticker. None of them reads the Keychain.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::config::{Config, Limits};
use crate::coordinator::{self, View};
use crate::herdr::{self, AgentStatus as HerdrStatus, Herdr};
use crate::linear::api::{Activity, Content};
use crate::outbox::{self, Op, StateTarget};
use crate::paths::Ctx;
use crate::process::{Cmd, Runner};
use crate::run::{AgentStatus, Run, RunRecord, Status};
use crate::supervisor::{self, DecisionProposal, SupervisorStore};
use crate::worker::{
    self, DecisionRequest, Group, MergeAttempt, MergeAuthorization, MergeDecisionState,
    PlacementIntent, PlacementRef, RestartOperation, RestartPhase, StartOperation, StartPhase,
    Worker, WorkerOperation,
};
use crate::{files, inbox, names, ticker};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const UNREACHABLE: &str = "the configured Herdr session is not reachable";

fn supervisor_state(ctx: &Ctx<'_>) -> Result<(Config, SupervisorStore)> {
    let config = Config::load_with_state_dir(&ctx.config_dir(), &ctx.state_dir())?;
    if config.supervisor.is_none() {
        bail!("[supervisor] is not configured");
    }
    let store = SupervisorStore::new(ctx.supervisor_dir());
    Ok((config, store))
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn supervisor_context(ctx: &Ctx<'_>, generation: u64) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let policy = supervisor::policy_revision(&config)?;
    if store.repair_reports(100)? {
        bail!("supervisor report recovery is still in progress; request context again shortly");
    }
    let context = store.context(
        generation,
        &policy,
        &supervisor::authorized_scopes(&config),
        &supervisor::authorized_profile_policies(&config)?,
        &files::now(),
    )?;
    print_json(&context)
}

pub fn supervisor_decide(ctx: &Ctx<'_>, generation: u64, text: &str) -> Result<()> {
    if text.len() > 64_000 {
        bail!("decision proposal exceeds 64000 bytes");
    }
    let proposal: DecisionProposal = serde_json::from_str(text)
        .context("decision file is not a valid supervisor decision proposal")?;
    let (config, store) = supervisor_state(ctx)?;
    let policy = supervisor::policy_revision(&config)?;
    let mut receipt = store.decide(
        generation,
        &policy,
        &supervisor::authorized_scopes(&config),
        &config,
        proposal,
    )?;
    receipt.projections_pending = store.rebuild_memory_index().is_err();
    ticker::poke(&ctx.state_dir());
    print_json(&receipt)
}

pub fn supervisor_memory_search(ctx: &Ctx<'_>, query: &str) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let scopes = supervisor::authorized_scopes(&config);
    let results = match store.search_memory(&scopes, query, 20) {
        Ok(results) => results,
        Err(_) => {
            store.rebuild_memory_index()?;
            store.search_memory(&scopes, query, 20)?
        }
    };
    print_json(&results)
}

pub fn supervisor_show(ctx: &Ctx<'_>, handle: &str) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let scopes = supervisor::authorized_scopes(&config);
    if let Some(id) = handle.strip_prefix("report:") {
        return print_json(&store.report_in_scopes(id, &scopes)?);
    }
    if let Some(id) = handle.strip_prefix("decision:") {
        return print_json(&store.decision_in_scopes(id, &scopes)?);
    }
    if let Some(topic) = handle.strip_prefix("topic:") {
        let (id, revision) = match topic.rsplit_once('@') {
            Some((id, revision)) => (
                id,
                Some(
                    revision
                        .parse::<u64>()
                        .context("topic revision must be an integer")?,
                ),
            ),
            None => (topic, None),
        };
        let (metadata, content) = store.topic(id, revision)?;
        if !scopes.contains(&metadata.scope) {
            bail!("knowledge topic is outside the current authorization scope");
        }
        return print_json(&serde_json::json!({ "metadata": metadata, "content_md": content }));
    }
    bail!("unknown supervisor handle; use report:<id>, decision:<id> or topic:<id>@<revision>")
}

pub fn supervisor_profiles_list(ctx: &Ctx<'_>) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let scopes = supervisor::authorized_scopes(&config);
    let catalog = store
        .profile_catalog()?
        .into_iter()
        .filter(|entry| scopes.contains(&entry.revision.scope))
        .collect::<Vec<_>>();
    print_json(&catalog)
}

pub fn supervisor_profiles_show(
    ctx: &Ctx<'_>,
    id: &str,
    requested_revision: Option<u64>,
) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let scopes = supervisor::authorized_scopes(&config);
    let entry = store
        .profile_catalog()?
        .into_iter()
        .filter(|entry| entry.revision.id == id)
        .filter(|entry| {
            requested_revision.is_none_or(|revision| entry.revision.revision == revision)
        })
        .max_by_key(|entry| entry.revision.revision)
        .context("generated profile revision does not exist")?;
    if !scopes.contains(&entry.revision.scope) {
        bail!("generated profile is outside the current authorization scope");
    }
    print_json(&entry)
}

pub fn supervisor_profiles_retire(
    ctx: &Ctx<'_>,
    id: &str,
    expected_revision: u64,
    reason: &str,
    source_report_ids: &[String],
) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let entry = store
        .profile_catalog()?
        .into_iter()
        .find(|entry| entry.revision.id == id && entry.current)
        .context("generated profile does not exist in the current catalog")?;
    if entry.revision.revision != expected_revision {
        bail!(
            "generated profile is at revision {}; refresh the catalog before retiring it",
            entry.revision.revision
        );
    }
    let receipt = store.operator_profile_change(
        &config,
        supervisor::ProfileChange::Retire {
            id: id.into(),
            expected_revision,
            reason: reason.into(),
            source_report_ids: source_report_ids.to_vec(),
        },
        &format!("profile-retire:{id}@{expected_revision}"),
        reason,
        &files::now().to_string(),
    )?;
    let mut receipt = receipt;
    receipt.projections_pending = store.rebuild_memory_index().is_err();
    ticker::poke(&ctx.state_dir());
    print_json(&receipt)
}

pub fn supervisor_profiles_rollback(
    ctx: &Ctx<'_>,
    id: &str,
    revision: u64,
    reason: &str,
) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let catalog = store.profile_catalog()?;
    let target = catalog
        .iter()
        .find(|entry| entry.revision.id == id && entry.revision.revision == revision)
        .context("rollback target is not a committed profile revision")?;
    let current = catalog
        .iter()
        .find(|entry| entry.revision.id == id && entry.current)
        .context("generated profile does not exist in the current catalog")?;
    if current.revision.status != supervisor::GeneratedProfileStatus::Active {
        bail!("generated profile is retired; publish it through an explicit operator review");
    }
    let receipt = store.operator_profile_change(
        &config,
        supervisor::ProfileChange::Revise {
            id: id.into(),
            expected_revision: current.revision.revision,
            base_profile: Some(target.revision.base_profile.clone()),
            description: target.revision.description.clone(),
            instructions_md: target.revision.instructions_md.clone(),
            source_report_ids: target.revision.source_report_ids.clone(),
        },
        &format!("profile-rollback:{id}@{revision}"),
        reason,
        &files::now().to_string(),
    )?;
    let mut receipt = receipt;
    receipt.projections_pending = store.rebuild_memory_index().is_err();
    ticker::poke(&ctx.state_dir());
    print_json(&receipt)
}

pub fn supervisor_profiles_export(
    ctx: &Ctx<'_>,
    id: &str,
    revision: u64,
    name: &str,
    output: &Path,
) -> Result<()> {
    let (config, store) = supervisor_state(ctx)?;
    let entry = store
        .profile_catalog()?
        .into_iter()
        .find(|entry| entry.revision.id == id && entry.revision.revision == revision)
        .context("generated profile revision is not committed")?;
    if !supervisor::authorized_scopes(&config).contains(&entry.revision.scope) {
        bail!("generated profile is outside the current authorization scope");
    }
    store.export_profile(&config, id, revision, name, output)?;
    println!(
        "exported `{id}@{revision}` as `{name}` to {}",
        output.display()
    );
    Ok(())
}

/// The configured Herdr session: a client and the socket path its panes see.
pub struct Session<H> {
    pub herdr: H,
    pub socket: String,
}

impl Session<herdr::Client> {
    pub async fn configured(ctx: &Ctx<'_>) -> Result<Self> {
        let config = Config::load(&ctx.config_dir())?;
        let socket = herdr::session_socket(&ctx.env.herdr_bin(), config.herdr.session.as_deref())
            .await
            .context(UNREACHABLE)?;
        Ok(Session {
            socket: socket.to_string_lossy().into_owned(),
            herdr: herdr::Client::new(socket),
        })
    }
}

async fn load_active(ctx: &Ctx<'_>, key: &str) -> Result<(Config, Run, RunRecord)> {
    // Every command an agent runs keeps the ticker alive.
    ticker::start(ctx).await?;
    let config = Config::load_with_state_dir(&ctx.config_dir(), &ctx.state_dir())?;
    let run = Run::load(&ctx.runs_dir(), key)?;
    let record = run.record()?;
    if record.status != Status::Active {
        bail!(
            "run {key} is {:?}; nothing more is done for it",
            record.status
        );
    }
    Ok((config, run, record))
}

fn ensure_generated_profile_authorized(
    ctx: &Ctx<'_>,
    config: &Config,
    record: &RunRecord,
    name: &str,
    role: crate::config::GeneratedRole,
    require_active: bool,
) -> Result<()> {
    if !name.starts_with("generated-") {
        return Ok(());
    }
    let scope = crate::supervisor::Scope {
        workspace: record.workspace.clone(),
        team: record.team_key.clone(),
        routing: config
            .team(&record.workspace, &record.team_key)?
            .routing
            .clone(),
    };
    ensure!(
        SupervisorStore::new(ctx.supervisor_dir()).profile_is_authorized(
            config,
            name,
            &scope,
            role,
            require_active,
        )?,
        "generated profile `{name}` is no longer authorized for this routing"
    );
    Ok(())
}

fn non_empty(text: &str, what: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("the {what} is empty");
    }
    Ok(())
}

pub fn skill(key: Option<&str>) -> Result<()> {
    print!(
        "{}",
        coordinator::sheet(
            &coordinator::binary_command()?,
            key.unwrap_or("<ISSUE-KEY>")
        )
    );
    Ok(())
}

pub async fn context<H: Herdr>(
    ctx: &Ctx<'_>,
    session: Option<&Session<H>>,
    key: &str,
) -> Result<()> {
    ticker::start(ctx).await?;
    let config = Config::load_with_state_dir(&ctx.config_dir(), &ctx.state_dir())?;
    let run = Run::load(&ctx.runs_dir(), key)?;
    let state_dir = ctx.state_dir();
    let snapshot = match session {
        Some(session) => session.herdr.snapshot().await.ok(),
        None => None,
    };
    let view = snapshot
        .as_ref()
        .zip(session)
        .map(|(snapshot, session)| View {
            snapshot,
            state_dir: &state_dir,
            socket: &session.socket,
            now: jiff::Timestamp::now(),
        });
    let rows = coordinator::worker_rows(&run, view.as_ref());
    let (text, shown) = coordinator::digest(&run, &config, &coordinator::binary_command()?, &rows)?;
    print!("{text}");
    inbox::mark_seen(&run, &shown)
}

pub fn inbox_done(ctx: &Ctx, key: &str, ids: &[String], all: bool) -> Result<()> {
    let run = Run::load(&ctx.runs_dir(), key)?;
    if ids.is_empty() && !all {
        bail!("name the inbox item ids, or pass --all");
    }
    let moved = inbox::done(&run, ids, all)?;
    ticker::poke(&ctx.state_dir());
    println!("{}", done_message(&run, moved));
    Ok(())
}

/// `--all` leaves the items written after the last `context`; the message
/// says how many, so the coordinator reads them before it ends its turn.
fn done_message(run: &Run, moved: usize) -> String {
    let seen = inbox::seen(run);
    let new = inbox::unhandled(run)
        .iter()
        .filter(|item| !seen.contains(&item.id))
        .count();
    if new == 0 {
        format!("{moved} item(s) handled")
    } else {
        format!("{moved} item(s) handled; {new} new item(s) since your last context, run context")
    }
}

pub async fn plan_set(ctx: &Ctx<'_>, key: &str, text: &str) -> Result<()> {
    let (_, run, _) = load_active(ctx, key).await?;
    let plan = outbox::parse_plan(text)?;
    outbox::push(&run, Op::Plan { plan })?;
    ticker::poke(&ctx.state_dir());
    println!("the plan is queued for Linear");
    Ok(())
}

pub async fn say(ctx: &Ctx<'_>, key: &str, text: &str) -> Result<()> {
    non_empty(text, "text")?;
    let (_, run, _) = load_active(ctx, key).await?;
    if outbox::push(
        &run,
        Op::activity(Activity::new(Content::Thought {
            body: text.trim().to_string(),
        })),
    )?
    .is_none()
    {
        bail!("run {key} is awaiting a reply or has stopped");
    }
    ticker::poke(&ctx.state_dir());
    println!("queued for the Linear session");
    Ok(())
}

/// `--option label=value`, or a bare label used as its own value.
pub fn parse_option(text: &str) -> Result<(String, String)> {
    let (label, value) = text.split_once('=').unwrap_or((text, text));
    let (label, value) = (label.trim(), value.trim());
    if label.is_empty() || value.is_empty() {
        bail!("an option looks like `label=value`; got `{text}`");
    }
    Ok((label.to_string(), value.to_string()))
}

pub async fn ask(ctx: &Ctx<'_>, key: &str, text: &str, options: &[(String, String)]) -> Result<()> {
    non_empty(text, "question")?;
    let (_, run, _) = load_active(ctx, key).await?;
    let options: Vec<_> = options
        .iter()
        .map(|(label, value)| (label.as_str(), value.as_str()))
        .collect();
    if outbox::push(&run, Op::elicitation(text.trim(), &options))?.is_none() {
        bail!("run {key} is already awaiting a reply or has stopped");
    }
    ticker::poke(&ctx.state_dir());
    println!(
        "the question is queued for the Linear session; end your turn, the answer arrives in your inbox"
    );
    Ok(())
}

/// Ends the coordinator's current turn without finishing the Linear run.
pub async fn wait<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    text: &str,
) -> Result<()> {
    non_empty(text, "summary")?;
    let (_, run, initial) = load_active(ctx, key).await?;
    if initial.turn_complete.is_some() {
        println!("the coordinator turn is already complete; the run remains active");
        return Ok(());
    }
    let snapshot = session.herdr.snapshot().await.context(UNREACHABLE)?;
    let blocked = blocked_workers(
        &run,
        &snapshot,
        jiff::Timestamp::now(),
        &ctx.state_dir(),
        &session.socket,
    );
    if !blocked.is_empty() {
        bail!(
            "not waiting: {}. Every worker must have written a report and be neither working nor waiting",
            blocked.join(", ")
        );
    }

    let lock = run.lock()?;
    let current = run.record()?;
    if current.status != Status::Active
        || current.stopped
        || current.finished
        || current.awaiting_reply.is_some()
    {
        bail!("run {key} cannot wait while stopped, finished or awaiting a reply");
    }
    if current.reply_generation != initial.reply_generation {
        bail!("new Linear input arrived while checking workers; read context before waiting");
    }
    if current.turn_complete.is_some() {
        println!("the coordinator turn is already complete; the run remains active");
        return Ok(());
    }
    let seen = inbox::seen(&run);
    if inbox::unhandled(&run)
        .iter()
        .any(|item| !seen.contains(&item.id))
    {
        bail!("run {key} has new inbox items; read context before waiting");
    }
    let blocked = blocked_workers(
        &run,
        &snapshot,
        jiff::Timestamp::now(),
        &ctx.state_dir(),
        &session.socket,
    );
    if !blocked.is_empty() {
        bail!(
            "not waiting: {}. Every worker must have written a report and be neither working nor waiting",
            blocked.join(", ")
        );
    }
    if outbox::push_held(
        &run,
        &lock,
        Op::TurnComplete {
            body: text.trim().to_string(),
        },
    )?
    .is_none()
    {
        bail!("run {key} cannot wait while stopped, finished or awaiting a reply");
    }
    ticker::poke(&ctx.state_dir());
    println!("the coordinator turn is complete; the run remains active");
    Ok(())
}

fn blocked_workers(
    run: &Run,
    snapshot: &herdr::Snapshot,
    now: jiff::Timestamp,
    state_dir: &Path,
    socket: &str,
) -> Vec<String> {
    worker::list(run)
        .into_iter()
        .filter(|w| {
            matches!(
                w.agent.status,
                AgentStatus::Open | AgentStatus::Failed | AgentStatus::Pending
            )
        })
        .map(|w| {
            let live = worker::live_state(&w.agent, snapshot, now, state_dir, socket);
            (worker::group(&w, &live), w)
        })
        .filter(|(group, _)| *group != Group::Reported)
        .map(|(group, w)| format!("{} is {}", w.id, group.label()))
        .collect()
}

/// Posts the final summary and moves the issue to review, once every worker
/// has reported and none is working or waiting.
pub async fn finish<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    text: &str,
) -> Result<()> {
    non_empty(text, "summary")?;
    let (config, run, record) = load_active(ctx, key).await?;
    let snapshot = session.herdr.snapshot().await.context(UNREACHABLE)?;
    let now = jiff::Timestamp::now();
    let state_dir = ctx.state_dir();
    let blocking = blocked_workers(&run, &snapshot, now, &state_dir, &session.socket);
    if !blocking.is_empty() {
        bail!(
            "not finished: {}. Every worker must have written a report and be neither working nor waiting",
            blocking.join(", ")
        );
    }
    let lock = run.lock()?;
    let target = if run.record()?.workflow.confirmed == Some(crate::run::WorkflowStage::InDeploy) {
        crate::run::WorkflowStage::InPostmortem
    } else {
        crate::run::WorkflowStage::InReview
    };
    outbox::push_held(
        &run,
        &lock,
        Op::activity(Activity::new(Content::Response {
            body: text.trim().to_string(),
        })),
    )?;
    outbox::push_held(
        &run,
        &lock,
        Op::IssueState {
            target: StateTarget::Workflow(target),
        },
    )?;
    run.update_held(&lock, |r| {
        if let Some(wait) = r.awaiting_reply.take() {
            r.cleared_wait_id = wait.activity_id;
        }
        r.finished = true;
        r.turn_complete = None;
    })?;
    ticker::poke(&ctx.state_dir());
    let target_state = config
        .team(&record.workspace, &record.team_key)
        .map(|team| match target {
            crate::run::WorkflowStage::InPostmortem => team.postmortem_state.as_str(),
            _ => team.review_state.as_str(),
        })
        .unwrap_or(match target {
            crate::run::WorkflowStage::InPostmortem => "In Postmortem",
            _ => "In Review",
        });
    println!("the summary is queued; the issue moves to `{target_state}`");
    Ok(())
}

/// Adds the brief folder to the repository's shared `info/exclude`, so every
/// worktree of it ignores the worker's brief and report.
pub async fn exclude_from_git(runner: &dyn Runner, cwd: &str) -> Result<()> {
    let out = runner
        .run(&Cmd::new("git", GIT_TIMEOUT).args(["-C", cwd, "rev-parse", "--git-common-dir"]))
        .await?;
    if !out.success() {
        bail!("git rev-parse failed: {}", out.error_text());
    }
    let common = Path::new(out.stdout.trim());
    let exclude = Path::new(cwd).join(common).join("info/exclude");
    let line = format!("{}/", worker::BRIEF_FOLDER);
    let mut text = match std::fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", exclude.display()));
        }
    };
    if text.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&line);
    text.push('\n');
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    files::write_atomic(&exclude, text.as_bytes())
}

pub struct WorkerStart {
    pub repo: String,
    pub profile: String,
    pub title: String,
    pub task: String,
}

/// Workers that are not stopped count for one worker per repository and for
/// `max_workers_per_run`.
fn check_room(workers: &[Worker], repo: &str, limits: Limits) -> Result<()> {
    let live: Vec<&Worker> = workers
        .iter()
        .filter(|w| w.agent.status != AgentStatus::Stopped)
        .collect();
    if let Some(other) = live.iter().find(|w| w.repo == repo) {
        bail!(
            "{} already works on `{repo}`: one worker per repository; prompt or restart it instead",
            other.id
        );
    }
    if live.len() >= limits.max_workers_per_run as usize {
        bail!(
            "the run already has {} worker(s); max_workers_per_run is {}",
            live.len(),
            limits.max_workers_per_run
        );
    }
    Ok(())
}

fn check_agents(ctx: &Ctx, config: &Config) -> Result<()> {
    let count = worker::agent_count(&Run::list(&ctx.runs_dir()));
    let reserved = usize::from(config.supervisor.is_some());
    if count + 1 + reserved > config.limits.max_agents as usize {
        bail!(
            "the limit of {} agents is reached",
            config.limits.max_agents
        );
    }
    Ok(())
}

/// Places a worker in a new worktree of a catalog repository. The ticker
/// starts its agent in a later pass.
pub async fn worker_start<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    args: &WorkerStart,
) -> Result<Worker> {
    let (config, run, record) = load_active(ctx, key).await?;
    ensure!(
        !record.coordinator.workspace_id.is_empty(),
        "the coordinator has no Herdr workspace yet"
    );
    ensure!(
        record.coordinator.herdr_session.is_empty()
            || record.coordinator.herdr_session == session.socket,
        "the coordinator belongs to a different Herdr session"
    );
    ensure!(
        record.coordinator_placement.is_none(),
        "the coordinator is still being placed"
    );
    ensure!(
        !record.coordinator_misplaced,
        "the coordinator pane is outside its recorded workspace; move it back before starting a worker"
    );
    let repo = config.repository(&args.repo)?.clone();
    let profile = config
        .worker_profile_for(&record.workspace, &record.team_key, &args.profile)?
        .clone();
    ensure_generated_profile_authorized(
        ctx,
        &config,
        &record,
        &args.profile,
        crate::config::GeneratedRole::Worker,
        true,
    )?;
    let limits = config.limits;
    check_room(&worker::list_checked(&run)?, &args.repo, limits)?;
    check_agents(ctx, &config)?;
    let repo_path = repo.path.to_string_lossy().into_owned();
    let socket = session.socket.clone();
    let run_key = run.key.clone();
    let hooks = repo.worktree.clone();
    let task = args.task.trim().to_string();
    let worker = worker::allocate_with(
        &run,
        |workers| {
            ensure!(
                run.record()?.status == Status::Active,
                "run {key} is no longer active"
            );
            let current = run.record()?;
            ensure!(
                !current.coordinator.workspace_id.is_empty(),
                "the coordinator has no Herdr workspace yet"
            );
            ensure!(
                current.coordinator_placement.is_none(),
                "the coordinator is still being placed"
            );
            ensure!(
                !current.coordinator_misplaced,
                "the coordinator pane is outside its recorded workspace; move it back before starting a worker"
            );
            ensure!(
                current.coordinator.workspace_id == record.coordinator.workspace_id,
                "the coordinator workspace changed; retry worker start"
            );
            ensure!(
                current.coordinator.herdr_session.is_empty()
                    || current.coordinator.herdr_session == socket,
                "the coordinator belongs to a different Herdr session"
            );
            check_room(workers, &args.repo, limits)?;
            check_agents(ctx, &config)
        },
        |w| -> Result<()> {
            w.title = args.title.clone();
            w.repo = args.repo.clone();
            w.repo_path = repo_path.clone();
            w.base = repo.base.clone();
            w.branch = worker::branch_name(key, &w.id, &args.title);
            w.agent.status = AgentStatus::Pending;
            w.cleanup_completed = false;
            w.agent.profile = args.profile.clone();
            w.agent.kind = profile.kind.clone();
            w.agent.waiting_episodes = Some(0);
            w.agent.agent_name = names::agent_name(key, &record.issue_id, &w.id);
            let path = crate::worktree::path(ctx, &run_key, &w.id, &args.repo)?
                .to_string_lossy()
                .into_owned();
            w.worktree_path = path.clone();
            w.brief_dir = worker::brief_dir(&path, &run_key, &w.id);
            w.operation = Some(WorkerOperation::Start(StartOperation {
                id: uuid::Uuid::new_v4(),
                retry_counted: false,
                phase: StartPhase::SaveTask,
                task: Some(task.clone()),
                setup_cursor: 0,
                hooks: hooks.clone(),
                instructions: profile.instructions.clone(),
                placement: PlacementIntent::Requested {
                    session: socket.clone(),
                    workspace: record.coordinator.workspace_id.clone(),
                    cwd: path,
                    label: worker::tab_label(&w.id, &w.repo, &w.title),
                },
            }));
            Ok(())
        },
    )?;
    outbox::clear_turn_complete(&run)?;
    ticker::poke(&ctx.state_dir());
    println!(
        "{} is queued for placement in {}",
        worker.id, worker.worktree_path
    );
    Ok(worker)
}

/// Sends a worker a follow-up and records it in its task file.
pub async fn worker_prompt<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    id: &str,
    text: &str,
) -> Result<()> {
    non_empty(text, "text")?;
    let (_, run, _) = load_active(ctx, key).await?;
    let w = worker::load(&run, id)?;
    if w.agent.status != AgentStatus::Open || w.agent.pane_id.is_empty() {
        bail!("worker {id} is not running");
    }
    let snapshot = session.herdr.snapshot().await.context(UNREACHABLE)?;
    let live = worker::live_state(
        &w.agent,
        &snapshot,
        jiff::Timestamp::now(),
        &ctx.state_dir(),
        &session.socket,
    );
    let Some(agent) = live.agent.as_ref().filter(|_| live.pane_exists) else {
        bail!("worker {id} is not running");
    };
    if agent.status == HerdrStatus::Blocked {
        bail!(
            "worker {id} waits on a dialog in pane {}; a person must answer it in Herdr first",
            agent.pane
        );
    }
    outbox::clear_turn_complete(&run)?;
    worker::append_follow_up(&run, id, text)?;
    session
        .herdr
        .agent_prompt(&agent.pane, text.trim())
        .await
        .with_context(|| format!("could not prompt worker {id}"))?;
    ticker::poke(&ctx.state_dir());
    println!("the follow-up is sent to {id}");
    Ok(())
}

/// Requests one-time authorization for the worker's recorded pull request.
pub async fn worker_request_merge(ctx: &Ctx<'_>, key: &str, id: &str) -> Result<()> {
    let (_, run, record) = load_active(ctx, key).await?;
    worker::validate_id(id)?;
    let config_dir = ctx.config_dir();
    let (config, fingerprint) = Config::load_stable(&config_dir)?;
    let team = config.team(&record.workspace, &record.team_key)?;
    if team.merge_decision.is_some() {
        let worker = worker::load(&run, id)?;
        let issue = crate::merge_decision::read_local_text(&run.issue_md(), "issue")?;
        let report = crate::merge_decision::read_local_text(
            &worker::home_report_path(&run, id),
            "worker report",
        )?;
        let issue_hash = files::sha256_hex(issue.as_bytes());
        let report_hash = files::sha256_hex(report.as_bytes());
        if worker.pr_url.is_empty()
            || worker::pr_line(&report).as_deref() != Some(worker.pr_url.as_str())
            || worker.report_hash != report_hash
        {
            bail!("worker {id} has no current copied report for its recorded GitHub pull request");
        }
        let request = DecisionRequest {
            id: uuid::Uuid::new_v4(),
            pr_url: worker.pr_url.clone(),
            report_hash,
            issue_hash,
            config_fingerprint: Config::fingerprint_hex(&fingerprint),
            requested_at: files::now(),
        };
        let lock = run.lock()?;
        let current_record = run.record()?;
        if current_record.status != Status::Active || current_record.stopped {
            bail!("run {key} is closed or stopped; no merge assessment can be requested");
        }
        if current_record.workspace != record.workspace
            || current_record.team_key != record.team_key
        {
            bail!(
                "run {key}'s workspace or team changed; reload it before requesting an assessment"
            );
        }
        if Config::fingerprint(&config_dir)? != fingerprint {
            bail!("config changed while the merge assessment was being requested; try again");
        }
        let current = worker::load(&run, id)?;
        match &current.merge_attempt {
            Some(MergeAttempt::Attempting { .. } | MergeAttempt::Unknown { .. }) => {
                bail!(
                    "worker {id} has an unresolved merge attempt; run `worker merge` to read it back"
                )
            }
            Some(MergeAttempt::Merged { .. }) => {
                bail!("worker {id}'s pull request is already merged")
            }
            Some(MergeAttempt::Unmerged { .. }) | None => {}
        }
        if let Some(MergeDecisionState::Requested { request: existing }) = &current.merge_decision {
            println!(
                "Merge assessment already queued for {key}/{id} (request {}).",
                existing.id
            );
            return Ok(());
        }
        let current_issue = crate::merge_decision::read_local_text(&run.issue_md(), "issue")?;
        let current_report = crate::merge_decision::read_local_text(
            &worker::home_report_path(&run, id),
            "worker report",
        )?;
        if files::sha256_hex(current_issue.as_bytes()) != request.issue_hash
            || files::sha256_hex(current_report.as_bytes()) != request.report_hash
            || current.report_hash != request.report_hash
            || current.pr_url != request.pr_url
            || worker::pr_line(&current_report).as_deref() != Some(request.pr_url.as_str())
        {
            bail!(
                "the issue or worker report changed while the assessment was being requested; try again"
            );
        }
        worker::update_held(&run, &lock, id, |worker| {
            worker.merge_authorization = None;
            worker.merge_attempt = None;
            worker.merge_decision = Some(MergeDecisionState::Requested {
                request: request.clone(),
            });
        })?;
        drop(lock);
        ticker::poke(&ctx.state_dir());
        println!(
            "Merge assessment queued for {key}/{id} (request {}).",
            request.id
        );
        return Ok(());
    }
    if !team.merge_on_authorization {
        bail!(
            "merging on Linear authorization is disabled for team {}",
            record.team_key
        );
    }
    let token = uuid::Uuid::new_v4().to_string();
    let lock = run.lock()?;
    let current = worker::load(&run, id)?;
    if current.pr_url.is_empty()
        || worker::pr_line(&format!("PR: {}", current.pr_url)).as_deref()
            != Some(current.pr_url.as_str())
    {
        bail!("worker {id} has no recorded GitHub pull request");
    }
    match &current.merge_attempt {
        Some(MergeAttempt::Attempting { .. } | MergeAttempt::Unknown { .. }) => {
            bail!("worker {id} has an unresolved merge attempt; run `worker merge` to read it back")
        }
        Some(MergeAttempt::Merged { .. }) => bail!("worker {id}'s pull request is already merged"),
        Some(MergeAttempt::Unmerged { .. }) | None => {}
    }
    let url = current.pr_url.clone();
    worker::update_held(&run, &lock, id, |w| {
        w.merge_authorization = Some(MergeAuthorization::Pending {
            token: token.clone(),
            pr_url: url,
        });
        w.merge_decision = None;
        w.merge_attempt = None;
    })?;
    outbox::push_held(
        &run,
        &lock,
        Op::MergeAuthorizationRequest {
            worker_id: id.into(),
            token: token.clone(),
        },
    )?;
    ticker::poke(&ctx.state_dir());
    Ok(())
}

/// Consumes a one-use authorization before asking GitHub to merge the PR.
pub async fn worker_merge(ctx: &Ctx<'_>, key: &str, id: &str) -> Result<()> {
    worker::validate_id(id)?;
    let run = Run::load(&ctx.runs_dir(), key)?;
    let record = run.record()?;
    let worker = worker::load(&run, id)?;

    if let Some(attempt @ (MergeAttempt::Attempting { .. } | MergeAttempt::Unknown { .. })) =
        worker.merge_attempt.clone()
    {
        let (pr_url, head_sha, decision_id) = merge_attempt_identity(&attempt);
        if worker.pr_url != pr_url {
            bail!("worker {id}'s unresolved merge attempt is for a different pull request");
        }
        let outcome = match (head_sha, decision_id) {
            (None, None) => read_merge_outcome(ctx, pr_url).await,
            (Some(head_sha), Some(decision_id)) => {
                read_machine_merge_outcome(ctx, pr_url, head_sha, decision_id).await
            }
            _ => MergeAttempt::Unknown {
                pr_url: pr_url.to_string(),
                head_sha: head_sha.map(str::to_string),
                decision_id: decision_id.map(str::to_string),
                attempted_at: merge_attempt_time(&attempt).to_string(),
            },
        };
        record_merge_result(&run, id, &attempt, outcome.clone())?;
        if matches!(outcome, MergeAttempt::Merged { .. }) {
            ticker::poke(&ctx.state_dir());
        }
        return match outcome {
            MergeAttempt::Merged { .. } => Ok(()),
            MergeAttempt::Unmerged { .. } => {
                bail!("the previous merge attempt did not merge; request fresh authorization")
            }
            MergeAttempt::Unknown { .. } | MergeAttempt::Attempting { .. } => {
                bail!("the previous merge outcome is still unknown; it was not retried")
            }
        };
    }

    ticker::start(ctx).await?;
    if record.status != Status::Active || record.stopped {
        bail!("run {key} is closed or stopped; no merge can be started");
    }
    let config = Config::load(&ctx.config_dir())?;
    let team = config.team(&record.workspace, &record.team_key)?;
    if team.merge_decision.is_some() {
        return machine_worker_merge(ctx, &run, &record, &config, id).await;
    }
    if !team.merge_on_authorization {
        bail!(
            "merging on Linear authorization is disabled for team {}",
            record.team_key
        );
    }
    let attempt = {
        let lock = run.lock()?;
        let current = worker::load(&run, id)?;
        if current.pr_url.is_empty()
            || worker::pr_line(&format!("PR: {}", current.pr_url)).as_deref()
                != Some(current.pr_url.as_str())
        {
            bail!("worker {id} has no recorded GitHub pull request");
        }
        match current.merge_authorization.clone() {
            Some(MergeAuthorization::Authorized { pr_url, .. }) => {
                if current.pr_url != pr_url {
                    bail!("worker {id}'s authorized pull request is no longer current");
                }
                let attempted_at = files::now();
                worker::update_held(&run, &lock, id, |w| {
                    w.merge_authorization = None;
                    w.merge_attempt = Some(MergeAttempt::Attempting {
                        pr_url: pr_url.clone(),
                        head_sha: None,
                        decision_id: None,
                        attempted_at: attempted_at.clone(),
                    });
                })?;
                MergeAttempt::Attempting {
                    pr_url,
                    head_sha: None,
                    decision_id: None,
                    attempted_at,
                }
            }
            _ => match current.merge_attempt.clone() {
                Some(MergeAttempt::Attempting { pr_url, .. })
                | Some(MergeAttempt::Unknown { pr_url, .. }) => {
                    if current.pr_url != pr_url {
                        bail!(
                            "worker {id}'s unresolved merge attempt is for a different pull request"
                        );
                    }
                    bail!(
                        "worker {id} has an unresolved merge attempt; run `worker merge` to read it back"
                    )
                }
                Some(MergeAttempt::Merged { .. }) => {
                    bail!("worker {id}'s pull request is already merged")
                }
                Some(MergeAttempt::Unmerged { .. }) => {
                    bail!(
                        "worker {id}'s last merge attempt did not merge; request fresh authorization"
                    )
                }
                None => bail!("worker {id} has no unused merge authorization"),
            },
        }
    };
    let (pr_url, _, _) = merge_attempt_identity(&attempt);
    let pr_url = pr_url.to_string();

    let merge = Cmd::new("gh", FETCH_TIMEOUT)
        .arg("pr")
        .arg("merge")
        .arg(&pr_url)
        .arg("--squash");
    let first = ctx.runner.run(&merge).await;
    if first.as_ref().is_ok_and(|out| out.success()) {
        record_merge_result(
            &run,
            id,
            &attempt,
            MergeAttempt::Merged {
                pr_url: pr_url.clone(),
                head_sha: None,
                decision_id: None,
                attempted_at: files::now(),
            },
        )?;
        ticker::poke(&ctx.state_dir());
        return Ok(());
    }

    let outcome = read_merge_outcome(ctx, &pr_url).await;
    let merged = matches!(outcome, MergeAttempt::Merged { .. });
    record_merge_result(&run, id, &attempt, outcome)?;
    if merged {
        ticker::poke(&ctx.state_dir());
        Ok(())
    } else {
        let detail = first
            .map(|out| out.error_text())
            .unwrap_or_else(|error| format!("{error:#}"));
        bail!(
            "gh pr merge did not complete ({detail}); authorization was consumed and will not be retried"
        )
    }
}

async fn machine_worker_merge(
    ctx: &Ctx<'_>,
    run: &Run,
    record: &RunRecord,
    config: &Config,
    id: &str,
) -> Result<()> {
    let team = config.team(&record.workspace, &record.team_key)?;
    let policy = team
        .merge_decision
        .as_ref()
        .context("machine merge assessment is disabled for this team")?;
    let worker = worker::load(run, id)?;
    let assessment = match worker.merge_decision.as_ref() {
        Some(MergeDecisionState::Assessed { assessment, .. }) => assessment.clone(),
        Some(MergeDecisionState::Failed { .. }) => {
            bail!("worker {id}'s last assessment failed; request a fresh assessment")
        }
        Some(MergeDecisionState::Requested { .. }) => {
            bail!("worker {id}'s merge assessment is still queued")
        }
        Some(MergeDecisionState::Consumed { .. }) => {
            bail!("worker {id}'s assessment was already consumed by a merge attempt")
        }
        None => bail!("worker {id} has no merge assessment; request one first"),
    };
    if assessment.method_version != crate::merge_decision::METHOD_VERSION {
        bail!(
            "worker {id}'s assessment uses an unsupported method version; request a fresh assessment"
        );
    }
    if assessment.output.risk_score > policy.max_risk_score {
        bail!(
            "worker {id}'s risk score {}/100 exceeds the configured threshold {}",
            assessment.output.risk_score,
            policy.max_risk_score
        );
    }

    let config_dir = ctx.config_dir();
    let (fresh_config, fingerprint) = Config::load_stable(&config_dir)?;
    let fingerprint = Config::fingerprint_hex(&fingerprint);
    let fresh_team = fresh_config.team(&record.workspace, &record.team_key)?;
    let fresh_policy = fresh_team
        .merge_decision
        .as_ref()
        .context("machine merge assessment was disabled; request a fresh assessment")?;
    if fingerprint != assessment.request.config_fingerprint
        || fresh_policy.profile != assessment.profile
        || fresh_policy.max_risk_score != assessment.max_risk_score
        || fresh_policy.max_risk_score != policy.max_risk_score
    {
        bail!("configuration changed after the assessment; request a fresh assessment");
    }
    let repository = fresh_config.repository(&worker.repo)?;
    let issue = crate::merge_decision::read_local_text(&run.issue_md(), "issue")?;
    let report = crate::merge_decision::read_local_text(
        &worker::home_report_path(run, id),
        "worker report",
    )?;
    if worker.pr_url != assessment.request.pr_url
        || worker.report_hash != assessment.request.report_hash
        || crate::files::sha256_hex(issue.as_bytes()) != assessment.request.issue_hash
        || crate::files::sha256_hex(report.as_bytes()) != assessment.request.report_hash
        || worker::pr_line(&report).as_deref() != Some(assessment.request.pr_url.as_str())
    {
        bail!("worker {id}'s issue, report, or pull request changed; request a fresh assessment");
    }
    let evidence = crate::merge_decision::acquire_evidence(
        ctx.runner,
        &crate::merge_decision::EvidenceInput {
            run_key: &run.key,
            worker: &worker,
            requested_pr_url: &assessment.request.pr_url,
            expected_repository_path: &repository.path,
            issue: &issue,
            report: &report,
            profile_name: &fresh_policy.profile,
            config_fingerprint: &fingerprint,
            max_risk_score: fresh_policy.max_risk_score,
        },
    )
    .await?;
    if evidence.digest != assessment.evidence_digest
        || evidence.head_sha != assessment.head_sha
        || evidence.base_sha != assessment.base_sha
    {
        bail!("GitHub evidence changed after the assessment; request a fresh assessment");
    }

    let attempt = MergeAttempt::Attempting {
        pr_url: assessment.request.pr_url.clone(),
        head_sha: Some(assessment.head_sha.clone()),
        decision_id: Some(assessment.request.id.to_string()),
        attempted_at: files::now(),
    };
    {
        let lock = run.lock()?;
        let current_record = run.record()?;
        if current_record.status != Status::Active
            || current_record.stopped
            || current_record.workspace != record.workspace
            || current_record.team_key != record.team_key
        {
            bail!("run {} changed while the merge was being checked", run.key);
        }
        let (locked_config, locked_fingerprint) = Config::load_stable(&config_dir)?;
        let locked_fingerprint = Config::fingerprint_hex(&locked_fingerprint);
        let locked_policy = locked_config
            .team(&current_record.workspace, &current_record.team_key)?
            .merge_decision
            .as_ref()
            .context("machine merge assessment was disabled; request a fresh assessment")?;
        let current = worker::load(run, id)?;
        let assessment_still_current = matches!(
            &current.merge_decision,
            Some(MergeDecisionState::Assessed { assessment: current, .. })
                if current == &assessment
        );
        if !assessment_still_current
            || current.merge_attempt.is_some()
            || current.pr_url != assessment.request.pr_url
            || current.report_hash != assessment.request.report_hash
            || locked_fingerprint != assessment.request.config_fingerprint
            || locked_policy.profile != assessment.profile
            || locked_policy.max_risk_score != assessment.max_risk_score
            || assessment.output.risk_score > locked_policy.max_risk_score
        {
            bail!(
                "worker {id}'s assessment or merge eligibility changed; request a fresh assessment"
            );
        }
        let current_issue = crate::merge_decision::read_local_text(&run.issue_md(), "issue")?;
        let current_report = crate::merge_decision::read_local_text(
            &worker::home_report_path(run, id),
            "worker report",
        )?;
        if crate::files::sha256_hex(current_issue.as_bytes()) != assessment.request.issue_hash
            || crate::files::sha256_hex(current_report.as_bytes()) != assessment.request.report_hash
            || worker::pr_line(&current_report).as_deref()
                != Some(assessment.request.pr_url.as_str())
        {
            bail!("worker {id}'s issue or report changed; request a fresh assessment");
        }
        let state = match &current.merge_decision {
            Some(MergeDecisionState::Assessed { notified, .. }) => *notified,
            _ => unreachable!("the assessment was checked above"),
        };
        worker::update_held(run, &lock, id, |current| {
            current.merge_decision = Some(MergeDecisionState::Consumed {
                assessment: assessment.clone(),
                notified: state,
            });
            current.merge_authorization = None;
            current.merge_attempt = Some(attempt.clone());
        })?;
    }

    let merge = Cmd::new("gh", FETCH_TIMEOUT)
        .args([
            "pr",
            "merge",
            assessment.request.pr_url.as_str(),
            "--squash",
            "--match-head-commit",
            assessment.head_sha.as_str(),
        ])
        .max_output_bytes(64 * 1024);
    let _ = ctx.runner.run(&merge).await;
    let decision_id = assessment.request.id.to_string();
    let readback = read_machine_merge_outcome(
        ctx,
        &assessment.request.pr_url,
        &assessment.head_sha,
        &decision_id,
    )
    .await;
    let outcome = if matches!(readback, MergeAttempt::Merged { .. }) {
        readback
    } else {
        MergeAttempt::Unknown {
            pr_url: assessment.request.pr_url.clone(),
            head_sha: Some(assessment.head_sha.clone()),
            decision_id: Some(assessment.request.id.to_string()),
            attempted_at: merge_attempt_time(&attempt).to_string(),
        }
    };
    record_merge_result(run, id, &attempt, outcome.clone())?;
    match outcome {
        MergeAttempt::Merged { .. } => {
            ticker::poke(&ctx.state_dir());
            Ok(())
        }
        MergeAttempt::Unmerged { .. }
        | MergeAttempt::Unknown { .. }
        | MergeAttempt::Attempting { .. } => {
            bail!("the machine merge outcome is unknown; read it back before any further action")
        }
    }
}

fn merge_attempt_identity(attempt: &MergeAttempt) -> (&str, Option<&str>, Option<&str>) {
    match attempt {
        MergeAttempt::Attempting {
            pr_url,
            head_sha,
            decision_id,
            ..
        }
        | MergeAttempt::Merged {
            pr_url,
            head_sha,
            decision_id,
            ..
        }
        | MergeAttempt::Unmerged {
            pr_url,
            head_sha,
            decision_id,
            ..
        }
        | MergeAttempt::Unknown {
            pr_url,
            head_sha,
            decision_id,
            ..
        } => (pr_url, head_sha.as_deref(), decision_id.as_deref()),
    }
}

fn merge_attempt_time(attempt: &MergeAttempt) -> &str {
    match attempt {
        MergeAttempt::Attempting { attempted_at, .. }
        | MergeAttempt::Merged { attempted_at, .. }
        | MergeAttempt::Unmerged { attempted_at, .. }
        | MergeAttempt::Unknown { attempted_at, .. } => attempted_at,
    }
}

fn with_merge_attempt_time(attempt: MergeAttempt, attempted_at: String) -> MergeAttempt {
    match attempt {
        MergeAttempt::Attempting {
            pr_url,
            head_sha,
            decision_id,
            ..
        } => MergeAttempt::Attempting {
            pr_url,
            head_sha,
            decision_id,
            attempted_at,
        },
        MergeAttempt::Merged {
            pr_url,
            head_sha,
            decision_id,
            ..
        } => MergeAttempt::Merged {
            pr_url,
            head_sha,
            decision_id,
            attempted_at,
        },
        MergeAttempt::Unmerged {
            pr_url,
            head_sha,
            decision_id,
            ..
        } => MergeAttempt::Unmerged {
            pr_url,
            head_sha,
            decision_id,
            attempted_at,
        },
        MergeAttempt::Unknown {
            pr_url,
            head_sha,
            decision_id,
            ..
        } => MergeAttempt::Unknown {
            pr_url,
            head_sha,
            decision_id,
            attempted_at,
        },
    }
}

async fn read_merge_outcome(ctx: &Ctx<'_>, pr_url: &str) -> MergeAttempt {
    let view = Cmd::new("gh", FETCH_TIMEOUT)
        .arg("pr")
        .arg("view")
        .arg(pr_url)
        .arg("--json")
        .arg("state,mergedAt");
    match ctx.runner.run(&view).await {
        Ok(out) if out.success() => serde_json::from_str::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|v| {
                let state = v.get("state")?.as_str()?;
                let merged_at = v.get("mergedAt")?;
                Some(if state == "MERGED" || !merged_at.is_null() {
                    MergeAttempt::Merged {
                        pr_url: pr_url.to_string(),
                        head_sha: None,
                        decision_id: None,
                        attempted_at: String::new(),
                    }
                } else {
                    MergeAttempt::Unmerged {
                        pr_url: pr_url.to_string(),
                        head_sha: None,
                        decision_id: None,
                        attempted_at: String::new(),
                    }
                })
            })
            .unwrap_or_else(|| MergeAttempt::Unknown {
                pr_url: pr_url.to_string(),
                head_sha: None,
                decision_id: None,
                attempted_at: String::new(),
            }),
        _ => MergeAttempt::Unknown {
            pr_url: pr_url.to_string(),
            head_sha: None,
            decision_id: None,
            attempted_at: String::new(),
        },
    }
}

async fn read_machine_merge_outcome(
    ctx: &Ctx<'_>,
    pr_url: &str,
    head_sha: &str,
    decision_id: &str,
) -> MergeAttempt {
    let unknown = || MergeAttempt::Unknown {
        pr_url: pr_url.to_string(),
        head_sha: Some(head_sha.to_string()),
        decision_id: Some(decision_id.to_string()),
        attempted_at: String::new(),
    };
    let view = Cmd::new("gh", FETCH_TIMEOUT)
        .args(["pr", "view", pr_url, "--json", "state,mergedAt,headRefOid"])
        .max_output_bytes(64 * 1024);
    match ctx.runner.run(&view).await {
        Ok(out) if out.success() => {
            let value = serde_json::from_slice::<serde_json::Value>(out.raw_stdout()).ok();
            let Some(value) = value else { return unknown() };
            if value.get("headRefOid").and_then(serde_json::Value::as_str) != Some(head_sha) {
                return unknown();
            }
            let Some(state) = value.get("state").and_then(serde_json::Value::as_str) else {
                return unknown();
            };
            let Some(merged_at) = value.get("mergedAt") else {
                return unknown();
            };
            let fields = (
                pr_url.to_string(),
                Some(head_sha.to_string()),
                Some(decision_id.to_string()),
            );
            if state == "MERGED" || !merged_at.is_null() {
                MergeAttempt::Merged {
                    pr_url: fields.0,
                    head_sha: fields.1,
                    decision_id: fields.2,
                    attempted_at: String::new(),
                }
            } else if state == "OPEN" {
                MergeAttempt::Unmerged {
                    pr_url: fields.0,
                    head_sha: fields.1,
                    decision_id: fields.2,
                    attempted_at: String::new(),
                }
            } else {
                unknown()
            }
        }
        _ => unknown(),
    }
}

fn record_merge_result(
    run: &Run,
    id: &str,
    expected: &MergeAttempt,
    outcome: MergeAttempt,
) -> Result<()> {
    let lock = run.lock()?;
    let current = worker::load(run, id)?;
    let (pr_url, head_sha, decision_id) = merge_attempt_identity(expected);
    let unresolved = current
        .merge_attempt
        .as_ref()
        .is_some_and(|attempt| match attempt {
            MergeAttempt::Attempting { .. } | MergeAttempt::Unknown { .. } => {
                merge_attempt_identity(attempt) == (pr_url, head_sha, decision_id)
            }
            _ => false,
        });
    if current.pr_url != pr_url
        || !unresolved
        || merge_attempt_identity(&outcome) != (pr_url, head_sha, decision_id)
    {
        return Ok(());
    }
    let attempted_at = if merge_attempt_time(expected).is_empty() {
        current.created.clone()
    } else {
        merge_attempt_time(expected).to_string()
    };
    let outcome = with_merge_attempt_time(outcome, attempted_at.clone());
    if let Some(decision_id) = decision_id {
        let consumed = matches!(
            &current.merge_decision,
            Some(MergeDecisionState::Consumed { assessment, .. })
                if assessment.request.id.to_string() == decision_id
                    && assessment.request.pr_url == pr_url
                    && Some(assessment.head_sha.as_str()) == head_sha
        );
        if !consumed {
            return Ok(());
        }
    } else if head_sha.is_some() {
        return Ok(());
    }
    let merged = matches!(&outcome, MergeAttempt::Merged { .. });
    let record = run.record()?;
    let active = record.status == Status::Active && !record.stopped;
    if merged && active {
        let stage = crate::run::WorkflowStage::InDeploy;
        let already_deploying = record.workflow.confirmed == Some(stage)
            || record
                .workflow
                .pending_choice
                .as_ref()
                .is_some_and(|choice| choice.stage == stage)
            || outbox::queued(run).iter().any(|request| match &request.op {
                Op::IssueState {
                    target: StateTarget::Workflow(queued),
                }
                | Op::IssueState {
                    target: StateTarget::Selected { stage: queued, .. },
                } => *queued == stage,
                _ => false,
            });
        if !already_deploying {
            outbox::push_held(
                run,
                &lock,
                Op::IssueState {
                    target: StateTarget::Workflow(stage),
                },
            )?;
        }
        let receipt_key = format!("{id}\n{pr_url}\n{attempted_at}");
        inbox::write_once_held(
            run,
            &lock,
            &inbox::Item {
                id: format!("merge-{}", files::sha256_hex(receipt_key.as_bytes())),
                kind: "merge".into(),
                subject: id.to_string(),
                created: attempted_at,
                summary: "The worker pull request was merged; deployment work can continue.".into(),
            },
        )?;
        run.update_held(&lock, |record| {
            record.finished = false;
            record.turn_complete = None;
        })?;
    }
    worker::update_held(run, &lock, id, |worker| {
        worker.merge_attempt = Some(outcome)
    })?;
    drop(lock);
    Ok(())
}

/// Starts a worker again in its worktree, optionally with another profile.
pub async fn worker_restart<H: Herdr>(
    ctx: &Ctx<'_>,
    session: &Session<H>,
    key: &str,
    id: &str,
    profile: Option<&str>,
) -> Result<Worker> {
    let (config, run, record) = load_active(ctx, key).await?;
    ensure!(
        !record.stopped && !record.finished && record.awaiting_reply.is_none(),
        "run {key} is stopped or waiting for a reply"
    );
    let w = worker::load(&run, id)?;
    ensure!(
        w.agent.herdr_session.is_empty() || w.agent.herdr_session == session.socket,
        "worker {id} belongs to a different Herdr session"
    );
    let pending = w.operation.clone();
    if w.restarts >= worker::MAX_RESTARTS {
        bail!(
            "worker {id} was restarted {} times; the limit is {}",
            w.restarts,
            worker::MAX_RESTARTS
        );
    }
    let require_active_profile = profile.is_some();
    let (profile_name, profile) = match profile {
        Some(name) => (
            name.to_string(),
            config
                .worker_profile_for(&record.workspace, &record.team_key, name)?
                .clone(),
        ),
        None => {
            let name = match &pending {
                Some(WorkerOperation::Restart(op)) => op.profile.clone(),
                _ => w.agent.profile.clone(),
            };
            (name.clone(), config.profile(&name)?.clone())
        }
    };
    ensure_generated_profile_authorized(
        ctx,
        &config,
        &record,
        &profile_name,
        crate::config::GeneratedRole::Worker,
        require_active_profile,
    )?;
    if !w.counts() {
        check_agents(ctx, &config)?;
    }
    let repo = config.repository(&w.repo)?;
    ensure!(
        repo.path.to_string_lossy() == w.repo_path,
        "repository path changed for worker {id}; restart is unsafe"
    );
    if let Some(mut operation) = pending {
        ensure!(
            w.agent.status == AgentStatus::Failed,
            "worker {id} has a pending lifecycle operation; only failed operations can be resumed"
        );
        ensure!(
            w.restarts < worker::MAX_RESTARTS,
            "worker {id} reached the restart limit"
        );
        ensure!(
            !record.stopped && !record.finished && record.awaiting_reply.is_none(),
            "run {key} is stopped or waiting for a reply"
        );
        let already_placed = match &operation {
            WorkerOperation::Start(op) => matches!(&op.placement, PlacementIntent::Placed(_)),
            WorkerOperation::Restart(op) => matches!(&op.placement, PlacementIntent::Placed(_)),
            WorkerOperation::Cleanup(_) => false,
        };
        let previous_profile = match &operation {
            WorkerOperation::Start(_) => w.agent.profile.as_str(),
            WorkerOperation::Restart(op) => op.profile.as_str(),
            WorkerOperation::Cleanup(_) => "",
        };
        ensure!(
            !matches!(&operation, WorkerOperation::Cleanup(_)),
            "worker {id} has cleanup pending and cannot be restarted"
        );
        ensure!(
            !already_placed || profile_name == previous_profile,
            "worker {id} is already placed; a profile change cannot be applied while resuming its failed operation"
        );
        let new_operation_id = uuid::Uuid::new_v4();
        match &mut operation {
            WorkerOperation::Start(op) => {
                op.id = new_operation_id;
                op.retry_counted = true;
                op.instructions = profile.instructions.clone();
            }
            WorkerOperation::Restart(op) => {
                op.id = new_operation_id;
                op.retry_counted = true;
                op.profile = profile_name.clone();
                op.kind = profile.kind.clone();
                op.instructions = profile.instructions.clone();
            }
            WorkerOperation::Cleanup(_) => unreachable!(),
        }
        let lock = run.lock()?;
        let locked_record = run.record()?;
        ensure!(
            locked_record.status == Status::Active,
            "run {key} is no longer active"
        );
        ensure!(
            !locked_record.stopped
                && !locked_record.finished
                && locked_record.awaiting_reply.is_none(),
            "run {key} is stopped or waiting for a reply"
        );
        ensure!(
            locked_record.coordinator_placement.is_none(),
            "the coordinator is still being placed"
        );
        ensure!(
            locked_record.coordinator.herdr_session.is_empty()
                || locked_record.coordinator.herdr_session == session.socket,
            "the coordinator belongs to a different Herdr session"
        );
        ensure!(
            !locked_record.coordinator_misplaced,
            "the coordinator pane is outside its recorded workspace; move it back before restarting a worker"
        );
        let current = worker::load(&run, id)?;
        ensure!(
            current.operation == w.operation && current.agent.status == AgentStatus::Failed,
            "worker {id} changed while restart was being prepared; retry"
        );
        let started = worker::update_held(&run, &lock, id, |current| {
            current.restarts += 1;
            if matches!(&operation, WorkerOperation::Start(_)) {
                current.agent.profile = profile_name.clone();
                current.agent.kind = profile.kind.clone();
            }
            current.agent.status = if already_placed {
                AgentStatus::Open
            } else {
                AgentStatus::Pending
            };
            current.agent.error.clear();
            current.agent.prompt_pending = false;
            current.restarting = matches!(&operation, WorkerOperation::Restart(_));
            current.cleanup_completed = false;
            current.operation = Some(operation.clone());
        })?;
        drop(lock);
        ticker::poke(&ctx.state_dir());
        println!(
            "{id} failed operation retry is queued in {}",
            started.worktree_path
        );
        return Ok(started);
    }
    let task_path = worker::task_path(&run, id);
    let _task = std::fs::read_to_string(&task_path).with_context(|| {
        format!(
            "could not read task for worker {id} at {}",
            task_path.display()
        )
    })?;
    // The old session's transcript is kept before a new session begins.
    let (roots, kept_run, kept_id) = (
        crate::transcript::Roots::from_env(ctx.env),
        run.clone(),
        id.to_string(),
    );
    let failures = tokio::task::spawn_blocking(move || {
        crate::transcript::keep_agents(&roots, &kept_run, Some(&kept_id))
    })
    .await?;
    for line in failures {
        eprintln!("{line}");
    }
    outbox::clear_turn_complete(&run)?;
    let new_path = if w.worktree_path.is_empty() {
        crate::worktree::path(ctx, key, id, &w.repo)?
            .to_string_lossy()
            .into_owned()
    } else {
        w.worktree_path.clone()
    };
    let create_checkout = !Path::new(&new_path).is_dir();
    let hooks = if create_checkout {
        repo.worktree.clone()
    } else {
        w.worktree_hooks.clone().unwrap_or_default()
    };
    let lock = run.lock()?;
    let locked_record = run.record()?;
    ensure!(
        locked_record.status == Status::Active,
        "run {key} is no longer active"
    );
    ensure!(
        !locked_record.stopped && !locked_record.finished && locked_record.awaiting_reply.is_none(),
        "run {key} is stopped or waiting for a reply"
    );
    ensure!(
        locked_record.coordinator_placement.is_none(),
        "the coordinator is still being placed"
    );
    ensure!(
        locked_record.coordinator.herdr_session.is_empty()
            || locked_record.coordinator.herdr_session == session.socket,
        "the coordinator belongs to a different Herdr session"
    );
    ensure!(
        !locked_record.coordinator_misplaced,
        "the coordinator pane is outside its recorded workspace; move it back before restarting a worker"
    );
    let workspace = locked_record.coordinator.workspace_id.clone();
    ensure!(
        !workspace.is_empty(),
        "the run has no coordinator workspace yet"
    );
    let current = worker::load(&run, id)?;
    ensure!(
        current.operation.is_none(),
        "worker {id} already has a pending lifecycle operation"
    );
    ensure!(
        current.restarts < worker::MAX_RESTARTS,
        "worker {id} reached the restart limit"
    );
    ensure!(
        current.agent.pane_id == w.agent.pane_id
            && current.agent.workspace_id == w.agent.workspace_id
            && current.agent.cwd == w.agent.cwd,
        "worker {id} changed while restart was being prepared; retry"
    );
    ensure!(
        current.branch == w.branch && current.repo_path == w.repo_path,
        "worker {id} checkout identity changed; restart is unsafe"
    );
    let requested = worker::update_held(&run, &lock, id, |w| {
        w.agent.recovery = crate::run::Recovery::None;
        w.restarting = true;
        w.cleanup_completed = false;
        w.gone_reported = false;
        w.agent.status = AgentStatus::Open;
        w.agent.prompt_pending = false;
        w.agent.last_group.clear();
        w.agent.blocked_reported = false;
        w.worktree_path = new_path.clone();
        w.brief_dir = worker::brief_dir(&new_path, key, id);
        w.operation = Some(WorkerOperation::Restart(RestartOperation {
            id: uuid::Uuid::new_v4(),
            retry_counted: false,
            phase: if create_checkout {
                RestartPhase::CreateCheckout
            } else {
                RestartPhase::CreateTab
            },
            previous: PlacementRef {
                session: session.socket.clone(),
                workspace: w.agent.workspace_id.clone(),
                tab: w.agent.tab_id.clone(),
                pane: w.agent.pane_id.clone(),
                cwd: w.agent.cwd.clone(),
            },
            placement: PlacementIntent::Requested {
                session: session.socket.clone(),
                workspace,
                cwd: new_path.clone(),
                label: worker::tab_label(id, &w.repo, &w.title),
            },
            profile: profile_name.clone(),
            kind: profile.kind.clone(),
            repo_path: w.repo_path.clone(),
            base: w.base.clone(),
            branch: w.branch.clone(),
            hooks: hooks.clone(),
            instructions: profile.instructions.clone(),
            create_checkout,
            setup_cursor: 0,
        }));
    })?;
    let worker = requested;
    ticker::poke(&ctx.state_dir());
    println!(
        "{id} restart is queued in {} with the `{profile_name}` profile",
        worker.worktree_path
    );
    Ok(worker)
}

/// Retry a retained cleanup phase after the operator resolves its cause.
pub fn worker_retry_cleanup(ctx: &Ctx<'_>, key: &str, id: &str) -> Result<()> {
    let run = Run::load(&ctx.runs_dir(), key)?;
    let lock = run.lock()?;
    ensure!(
        run.record()?.status == Status::Closed,
        "cleanup can only be retried for a closed run"
    );
    let worker = worker::load(&run, id)?;
    ensure!(
        matches!(&worker.operation, Some(WorkerOperation::Cleanup(op)) if op.retained_reason.is_some()),
        "worker {id} has no retained cleanup to retry"
    );
    run.update_held(&lock, |r| r.cleanup_pending = true)?;
    worker::update_held(&run, &lock, id, |w| {
        if let Some(WorkerOperation::Cleanup(op)) = &mut w.operation {
            op.id = uuid::Uuid::new_v4();
            op.retained_reason = None;
        }
        w.cleanup_completed = false;
        w.agent.error.clear();
    })?;
    drop(lock);
    ticker::poke(&ctx.state_dir());
    println!("{id} cleanup retry is queued");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::SAMPLE;
    use crate::herdr::{FakeHerdr, PaneId, WorkspaceId};
    use crate::paths::Env;
    use crate::process::fake::{FakeRunner, fail, ok};

    fn pane(record: &crate::run::AgentRecord) -> PaneId {
        PaneId(record.pane_id.clone())
    }

    #[test]
    fn options_parse_as_label_and_value() {
        assert_eq!(
            parse_option("Yes=yes").unwrap(),
            ("Yes".into(), "yes".into())
        );
        assert_eq!(
            parse_option("Maybe").unwrap(),
            ("Maybe".into(), "Maybe".into())
        );
        assert!(parse_option("=x").is_err());
        assert!(parse_option("x=").is_err());
    }

    fn merge_enabled_setup() -> Setup {
        Setup::new(|c| {
            c.replacen(
                "routing = \"default\"",
                "merge_on_authorization = true\nrouting = \"default\"",
                1,
            )
        })
    }

    async fn worker_with_pr(setup: &Setup) -> String {
        let worker = setup.start("api", "standard").await.unwrap();
        let url = "https://github.com/acme/api/pull/7".to_string();
        worker::update(&setup.run(), &worker.id, |current| {
            current.pr_url = url.clone();
        })
        .unwrap();
        url
    }

    #[tokio::test]
    async fn merge_is_disabled_by_default_and_request_binds_a_token_to_the_current_pr() {
        let setup = Setup::new(|c| c);
        let _url = worker_with_pr(&setup).await;
        let error = worker_request_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("merging on Linear authorization is disabled"));
        assert!(outbox::pending(&setup.run()).is_empty());
        assert!(
            worker::load(&setup.run(), "w1")
                .unwrap()
                .merge_authorization
                .is_none()
        );

        let setup = merge_enabled_setup();
        let url = worker_with_pr(&setup).await;
        worker_request_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap();
        let worker = worker::load(&setup.run(), "w1").unwrap();
        let Some(MergeAuthorization::Pending { token, pr_url }) = worker.merge_authorization else {
            panic!("request did not persist a pending authorization");
        };
        assert_eq!(pr_url, url);
        assert!(!token.is_empty());
        assert_eq!(outbox::pending(&setup.run()).len(), 1);
    }

    #[tokio::test]
    async fn merge_consumes_authorization_and_reads_back_an_ambiguous_gh_result() {
        let setup = merge_enabled_setup();
        let url = worker_with_pr(&setup).await;
        worker::update(&setup.run(), "w1", |current| {
            current.merge_authorization = Some(MergeAuthorization::Authorized {
                token: "one-use".into(),
                pr_url: url.clone(),
                by: "user-1".into(),
            });
        })
        .unwrap();
        setup
            .runner
            .on("gh pr merge", fail(1, "connection closed after request"))
            .on(
                "gh pr view",
                ok(r#"{"state":"MERGED","mergedAt":"2026-10-13T01:02:03Z"}"#),
            );

        worker_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap();

        let worker = worker::load(&setup.run(), "w1").unwrap();
        assert!(worker.merge_authorization.is_none());
        assert!(matches!(
            worker.merge_attempt,
            Some(MergeAttempt::Merged {
                pr_url: actual,
                head_sha: None,
                decision_id: None,
                attempted_at,
            }) if actual == url && !attempted_at.is_empty()
        ));
        assert_eq!(setup.runner.count("gh pr merge"), 1);
        let error = worker_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("pull request is already merged"));
        assert_eq!(setup.runner.count("gh pr merge"), 1);
    }

    #[tokio::test]
    async fn unmerged_readback_consumes_authorization_without_retrying() {
        let setup = merge_enabled_setup();
        let url = worker_with_pr(&setup).await;
        worker::update(&setup.run(), "w1", |current| {
            current.merge_authorization = Some(MergeAuthorization::Authorized {
                token: "one-use".into(),
                pr_url: url.clone(),
                by: "user-1".into(),
            });
        })
        .unwrap();
        setup
            .runner
            .on("gh pr merge", fail(1, "merge failed"))
            .on("gh pr view", ok(r#"{"state":"OPEN","mergedAt":null}"#));

        let error = worker_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("authorization was consumed"));
        let worker = worker::load(&setup.run(), "w1").unwrap();
        assert!(worker.merge_authorization.is_none());
        assert!(matches!(
            worker.merge_attempt,
            Some(MergeAttempt::Unmerged {
                pr_url: actual,
                head_sha: None,
                decision_id: None,
                attempted_at,
            }) if actual == url && !attempted_at.is_empty()
        ));
        assert_eq!(setup.runner.count("gh pr merge"), 1);
    }

    #[tokio::test]
    async fn a_restart_reads_back_an_attempt_before_accepting_new_authorization() {
        let setup = merge_enabled_setup();
        let url = worker_with_pr(&setup).await;
        worker::update(&setup.run(), "w1", |current| {
            current.merge_attempt = Some(MergeAttempt::Attempting {
                pr_url: url.clone(),
                head_sha: None,
                decision_id: None,
                attempted_at: files::now(),
            });
        })
        .unwrap();
        setup.runner.on(
            "gh pr view",
            ok(r#"{"state":"MERGED","mergedAt":"2026-10-13T01:02:03Z"}"#),
        );

        let error = worker_request_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unresolved merge attempt"));
        assert!(outbox::pending(&setup.run()).is_empty());

        worker_merge(&setup.ctx(), "acme/DATA-1", "w1")
            .await
            .unwrap();
        let worker = worker::load(&setup.run(), "w1").unwrap();
        assert!(matches!(
            worker.merge_attempt,
            Some(MergeAttempt::Merged {
                pr_url: actual,
                head_sha: None,
                decision_id: None,
                attempted_at,
            }) if actual == url && !attempted_at.is_empty()
        ));
        assert!(worker.merge_authorization.is_none());
        assert_eq!(setup.runner.count("gh pr merge"), 0);
        assert_eq!(setup.runner.count("gh pr view"), 1);
    }

    #[tokio::test]
    async fn merge_finalization_retries_do_not_duplicate_deploy_or_receipt() {
        let setup = Setup::new(|c| c);
        let url = worker_with_pr(&setup).await;
        let attempted_at = "2026-10-14T02:00:00Z".to_string();
        let attempt = MergeAttempt::Attempting {
            pr_url: url.clone(),
            head_sha: None,
            decision_id: None,
            attempted_at: attempted_at.clone(),
        };
        worker::update(&setup.run(), "w1", |worker| {
            worker.merge_attempt = Some(attempt.clone());
        })
        .unwrap();

        let inbox_path = setup.run().dir.join("inbox");
        if inbox_path.is_dir() {
            std::fs::remove_dir_all(&inbox_path).unwrap();
        }
        std::fs::write(&inbox_path, "blocks inbox creation").unwrap();
        let merged = MergeAttempt::Merged {
            pr_url: url.clone(),
            head_sha: None,
            decision_id: None,
            attempted_at: String::new(),
        };
        assert!(record_merge_result(&setup.run(), "w1", &attempt, merged.clone()).is_err());
        assert_eq!(outbox::queued(&setup.run()).len(), 1);
        std::fs::remove_file(&inbox_path).unwrap();

        record_merge_result(&setup.run(), "w1", &attempt, merged.clone()).unwrap();
        record_merge_result(&setup.run(), "w1", &attempt, merged).unwrap();

        let queued = outbox::queued(&setup.run());
        assert_eq!(queued.len(), 1);
        assert!(matches!(
            queued[0].op,
            Op::IssueState {
                target: StateTarget::Workflow(crate::run::WorkflowStage::InDeploy)
            }
        ));
        let receipts: Vec<_> = inbox::unhandled(&setup.run())
            .into_iter()
            .filter(|item| item.kind == "merge")
            .collect();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].created, attempted_at);
        assert!(matches!(
            worker::load(&setup.run(), "w1").unwrap().merge_attempt,
            Some(MergeAttempt::Merged { .. })
        ));
    }

    #[tokio::test]
    async fn exclude_is_added_once_to_the_shared_file() {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        let cwd = repo.path().to_string_lossy().into_owned();
        let runner = crate::process::RealRunner;
        exclude_from_git(&runner, &cwd).await.unwrap();
        exclude_from_git(&runner, &cwd).await.unwrap();
        let text = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".herdr-linear-agent/").count(), 1);
        std::fs::create_dir_all(repo.path().join(".herdr-linear-agent/acme-DATA-1-w1")).unwrap();
        std::fs::write(
            repo.path()
                .join(".herdr-linear-agent/acme-DATA-1-w1/report.md"),
            "r",
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&run(&["status", "--porcelain"]).stdout).is_empty());
    }

    #[tokio::test]
    async fn unreadable_exclude_content_is_preserved() {
        let repo = tempfile::tempdir().unwrap();
        let info = repo.path().join(".git/info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        let original = [0xff, b'\n'];
        std::fs::write(&exclude, original).unwrap();
        let runner = FakeRunner::default();
        runner.on("rev-parse", ok(".git\n"));
        assert!(
            exclude_from_git(&runner, repo.path().to_str().unwrap())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(exclude).unwrap(), original);
    }

    struct Setup {
        _home: tempfile::TempDir,
        env: Env,
        runner: FakeRunner,
        session: Session<FakeHerdr>,
    }

    impl Setup {
        fn new(edit: impl FnOnce(String) -> String) -> Setup {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().to_string_lossy().into_owned();
            let env = Env::for_test(
                home.path(),
                &[
                    ("XDG_STATE_HOME", &format!("{root}/state")),
                    ("XDG_CONFIG_HOME", &format!("{root}/config")),
                ],
            );
            let config = SAMPLE.replace("path = \"/src/", &format!("path = \"{root}/src/"));
            crate::config::tests::write_sample(&env.config_dir(), &edit(config));
            std::fs::create_dir_all(env.state_dir().join("runs")).unwrap();
            let runner = FakeRunner::new();
            runner.on("git -C", fail(128, "not a git repository"));
            runner.on("fetch origin", ok(""));
            let session = Session {
                herdr: FakeHerdr::new(home.path()),
                socket: "/work.sock".into(),
            };
            let setup = Setup {
                _home: home,
                env,
                runner,
                session,
            };
            Run::create(
                &setup.ctx().runs_dir(),
                RunRecord {
                    workspace: "acme".into(),
                    identifier: "DATA-1".into(),
                    issue_id: "issue-1".into(),
                    title: "Fix the login".into(),
                    url: "https://linear.app/acme/issue/DATA-1".into(),
                    team_key: "DATA".into(),
                    ..RunRecord::default()
                },
            )
            .unwrap();
            setup
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                env: &self.env,
                runner: &self.runner,
                detached_ticker: false,
            }
        }

        fn run(&self) -> Run {
            Run::load(&self.ctx().runs_dir(), "acme/DATA-1").unwrap()
        }

        async fn place_coordinator(&self) {
            if !self
                .run()
                .record()
                .unwrap()
                .coordinator
                .workspace_id
                .is_empty()
            {
                return;
            }
            let placed = self
                .session
                .herdr
                .workspace_create(self.run().dir.to_str().unwrap(), "coordinator")
                .await
                .unwrap();
            self.run()
                .update(|r| {
                    r.coordinator.status = AgentStatus::Open;
                    r.coordinator.workspace_id = placed.workspace.0;
                    r.coordinator.tab_id = placed.tab;
                    r.coordinator.pane_id = placed.pane.0;
                    r.coordinator.cwd = self.run().dir.to_string_lossy().into_owned();
                })
                .unwrap();
        }

        async fn enqueue(&self, repo: &str, profile: &str) -> Result<Worker> {
            self.place_coordinator().await;
            let args = WorkerStart {
                repo: repo.into(),
                profile: profile.into(),
                title: format!("Change {repo}"),
                task: "Make the change and open a PR.".into(),
            };
            worker_start(&self.ctx(), &self.session, "acme/DATA-1", &args).await
        }

        // Unrelated command tests use a placed worker fixture; lifecycle
        // advancement itself is exercised by ticker scenarios.
        async fn start(&self, repo: &str, profile: &str) -> Result<Worker> {
            let w = self.enqueue(repo, profile).await?;
            std::fs::create_dir_all(&w.worktree_path)?;
            let brief = worker::brief_dir(&w.worktree_path, "acme/DATA-1", &w.id);
            std::fs::create_dir_all(&brief)?;
            files::write_atomic(
                &worker::task_path(&self.run(), &w.id),
                b"Make the change and open a PR.\n",
            )?;
            let placed = self
                .session
                .herdr
                .tab_create(
                    &WorkspaceId(self.run().record()?.coordinator.workspace_id),
                    &w.worktree_path,
                    "worker fixture",
                )
                .await?;
            worker::update(&self.run(), &w.id, |w| {
                w.operation = None;
                w.worktree_hooks = Some(crate::config::WorktreeHooks::default());
                w.brief_dir = brief;
                w.agent.workspace_id = placed.workspace.0;
                w.agent.tab_id = placed.tab;
                w.agent.pane_id = placed.pane.0;
                w.agent.cwd = w.worktree_path.clone();
                w.agent.status = AgentStatus::Open;
                w.agent.prompt_pending = true;
            })
        }
    }

    #[tokio::test]
    async fn worker_start_persists_the_entire_request_before_external_work() {
        let setup = Setup::new(|c| c);
        setup.place_coordinator().await;
        let requests = setup.session.herdr.requests();
        let w = setup.enqueue("api", "standard").await.unwrap();
        assert_eq!(w.branch, "herdr-linear-agent/acme/data-1/w1-change-api");
        assert_eq!(
            w.worktree_path,
            setup
                .env
                .state_dir()
                .join("worktrees/acme/DATA-1/w1-api")
                .to_string_lossy()
        );
        assert_eq!(w.agent.status, AgentStatus::Pending);
        assert!(!w.agent.prompt_pending);
        assert!(w.agent.pane_id.is_empty());
        let Some(WorkerOperation::Start(operation)) = &w.operation else {
            panic!("missing durable start")
        };
        assert_eq!(operation.phase, StartPhase::SaveTask);
        assert_eq!(
            operation.task.as_deref(),
            Some("Make the change and open a PR.")
        );
        assert_eq!(operation.setup_cursor, 0);
        assert!(
            matches!(&operation.placement, PlacementIntent::Requested { workspace, .. } if workspace == "w1")
        );
        assert_eq!(worker::load(&setup.run(), "w1").unwrap(), w);
        assert!(!Path::new(&w.worktree_path).exists());
        assert!(!worker::task_path(&setup.run(), "w1").exists());
        assert!(setup.runner.calls.lock().unwrap().is_empty());
        assert_eq!(setup.session.herdr.requests(), requests);
        assert!(outbox::pending(&setup.run()).is_empty());
    }

    #[tokio::test]
    async fn limits_and_names_are_checked_before_external_work() {
        let setup =
            Setup::new(|c| c.replace("[herdr]", "[limits]\nmax_workers_per_run = 1\n\n[herdr]"));
        setup.enqueue("api", "standard").await.unwrap();
        assert!(
            setup
                .enqueue("web", "standard")
                .await
                .unwrap_err()
                .to_string()
                .contains("max_workers_per_run")
        );
        assert!(
            setup
                .enqueue("api", "standard")
                .await
                .unwrap_err()
                .to_string()
                .contains("one worker per repository")
        );
        assert!(setup.enqueue("nope", "standard").await.is_err());
        assert!(
            setup
                .enqueue("web", "coordinator")
                .await
                .unwrap_err()
                .to_string()
                .contains("not a worker profile")
        );
        assert_eq!(worker::list(&setup.run()).len(), 1);
        assert!(setup.runner.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_agent_limit_includes_pending_workers() {
        let setup = Setup::new(|c| c.replace("[herdr]", "[limits]\nmax_agents = 2\n\n[herdr]"));
        setup.enqueue("api", "standard").await.unwrap();
        assert_eq!(
            setup
                .enqueue("web", "standard")
                .await
                .unwrap_err()
                .to_string(),
            "the limit of 2 agents is reached"
        );
    }

    #[tokio::test]
    async fn worker_start_without_a_run_workspace_writes_nothing() {
        let setup = Setup::new(|c| c);
        let args = WorkerStart {
            repo: "api".into(),
            profile: "standard".into(),
            title: "Change API".into(),
            task: "Task".into(),
        };
        let error = worker_start(&setup.ctx(), &setup.session, "acme/DATA-1", &args)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("workspace"), "{error:#}");
        assert!(worker::list(&setup.run()).is_empty());
        assert!(setup.runner.calls.lock().unwrap().is_empty());
        assert!(setup.session.herdr.requests().is_empty());
    }

    #[tokio::test]
    async fn cleanup_retry_preserves_the_failed_phase_and_fences_previous_results() {
        let setup = Setup::new(|config| config);
        let worker = setup.start("api", "standard").await.unwrap();
        let old_id = uuid::Uuid::new_v4();
        worker::update(&setup.run(), &worker.id, |w| {
            w.operation = Some(WorkerOperation::Cleanup(crate::worker::CleanupOperation {
                id: old_id,
                phase: crate::worker::CleanupPhase::PostRemove,
                hook_cursor: 2,
                hooks: Default::default(),
                placement: None,
                placements: vec![],
                placement_intent: None,
                retained_reason: Some("hook failed".into()),
            }));
            w.agent.error = "hook failed".into();
        })
        .unwrap();
        assert!(worker_retry_cleanup(&setup.ctx(), "acme/DATA-1", &worker.id).is_err());
        setup.run().update(|r| r.status = Status::Closed).unwrap();
        setup.run().update(|r| r.cleanup_pending = true).unwrap();
        let interrupted = worker::load(&setup.run(), &worker.id).unwrap();
        assert!(
            matches!(&interrupted.operation, Some(WorkerOperation::Cleanup(op)) if op.id == old_id && op.retained_reason.is_some())
        );
        worker_retry_cleanup(&setup.ctx(), "acme/DATA-1", &worker.id).unwrap();
        let saved = worker::load(&setup.run(), &worker.id).unwrap();
        let Some(WorkerOperation::Cleanup(operation)) = saved.operation else {
            panic!("missing cleanup")
        };
        assert_ne!(operation.id, old_id);
        assert_eq!(operation.phase, crate::worker::CleanupPhase::PostRemove);
        assert_eq!(operation.hook_cursor, 2);
        assert!(operation.retained_reason.is_none());
        assert!(saved.agent.error.is_empty());
        assert!(setup.run().record().unwrap().cleanup_pending);
        assert!(worker_retry_cleanup(&setup.ctx(), "acme/DATA-1", &worker.id).is_err());
    }

    #[tokio::test]
    async fn worker_restart_preserves_the_old_pane_and_captures_the_new_intent() {
        let setup = Setup::new(|c| c);
        let original = setup.start("api", "standard").await.unwrap();
        let requests = setup.session.herdr.requests();
        let restarted = worker_restart(
            &setup.ctx(),
            &setup.session,
            "acme/DATA-1",
            "w1",
            Some("deep"),
        )
        .await
        .unwrap();
        assert!(restarted.restarting);
        assert!(!restarted.agent.prompt_pending);
        assert_eq!(restarted.agent.pane_id, original.agent.pane_id);
        assert_eq!(restarted.restarts, 0, "a queued restart has not completed");
        let Some(WorkerOperation::Restart(operation)) = &restarted.operation else {
            panic!("missing durable restart")
        };
        assert_eq!(operation.phase, RestartPhase::CreateTab);
        assert_eq!(operation.previous.pane, original.agent.pane_id);
        assert_eq!(operation.profile, "deep");
        assert_eq!(operation.kind, "codex");
        assert!(!operation.create_checkout);
        assert!(
            matches!(&operation.placement, PlacementIntent::Requested { workspace, .. } if workspace == "w1")
        );
        assert_eq!(setup.session.herdr.requests(), requests);
        assert!(setup.runner.calls.lock().unwrap().is_empty());
        assert!(
            worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
                .await
                .unwrap_err()
                .to_string()
                .contains("pending lifecycle")
        );
    }

    #[tokio::test]
    async fn failed_start_resumes_the_saved_task_and_cursor_without_a_task_file() {
        let setup = Setup::new(|c| c);
        let original = setup.enqueue("api", "standard").await.unwrap();
        worker::update(&setup.run(), &original.id, |w| {
            w.agent.status = AgentStatus::Failed;
            w.agent.error = "disk full".into();
            let Some(WorkerOperation::Start(op)) = &mut w.operation else {
                unreachable!()
            };
            op.setup_cursor = 2;
        })
        .unwrap();
        let retried = worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
            .await
            .unwrap();
        let Some(WorkerOperation::Start(op)) = &retried.operation else {
            panic!("start lost")
        };
        let Some(WorkerOperation::Start(old)) = &original.operation else {
            unreachable!()
        };
        assert_ne!(op.id, old.id);
        assert_eq!(op.phase, StartPhase::SaveTask);
        assert_eq!(op.task, old.task);
        assert_eq!(op.setup_cursor, 2);
        assert_eq!(op.hooks, old.hooks);
        assert!(op.retry_counted);
        assert_eq!(retried.restarts, 1);
        assert!(retried.agent.error.is_empty());
        assert!(!worker::task_path(&setup.run(), "w1").exists());
        assert!(
            worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_restart_keeps_the_old_agent_identity_until_its_pane_is_closed() {
        let setup = Setup::new(|c| c);
        let original = setup.start("api", "standard").await.unwrap();
        let queued = worker_restart(
            &setup.ctx(),
            &setup.session,
            "acme/DATA-1",
            "w1",
            Some("deep"),
        )
        .await
        .unwrap();
        worker::update(&setup.run(), "w1", |w| {
            w.agent.status = AgentStatus::Failed;
            w.agent.error = "close failed".into();
            let Some(WorkerOperation::Restart(op)) = &mut w.operation else {
                unreachable!()
            };
            op.phase = RestartPhase::ClosePreviousPane;
            op.setup_cursor = 3;
        })
        .unwrap();
        let retried = worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
            .await
            .unwrap();
        let Some(WorkerOperation::Restart(op)) = &retried.operation else {
            panic!("restart lost")
        };
        let Some(WorkerOperation::Restart(old)) = &queued.operation else {
            unreachable!()
        };
        assert_ne!(op.id, old.id);
        assert_eq!(op.phase, RestartPhase::ClosePreviousPane);
        assert_eq!(op.setup_cursor, 3);
        assert_eq!(op.previous, old.previous);
        assert_eq!(op.profile, "deep");
        assert_eq!(op.kind, "codex");
        assert_eq!(retried.agent.profile, original.agent.profile);
        assert_eq!(retried.agent.kind, original.agent.kind);
        assert_eq!(retried.agent.pane_id, original.agent.pane_id);
        assert_eq!(retried.restarts, 1);
        assert!(op.retry_counted);
    }

    #[tokio::test]
    async fn restart_keeps_the_task_and_limit_checks_before_queuing() {
        let setup = Setup::new(|c| c);
        let original = setup.start("api", "standard").await.unwrap();
        assert!(
            worker_restart(
                &setup.ctx(),
                &setup.session,
                "acme/DATA-1",
                "w1",
                Some("coordinator")
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("not a worker profile")
        );
        std::fs::remove_file(worker::task_path(&setup.run(), "w1")).unwrap();
        let requests = setup.session.herdr.requests();
        assert!(
            worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
                .await
                .unwrap_err()
                .to_string()
                .contains("could not read task")
        );
        assert_eq!(worker::load(&setup.run(), "w1").unwrap(), original);
        assert_eq!(setup.session.herdr.requests(), requests);
        files::write_atomic(&worker::task_path(&setup.run(), "w1"), b"Task").unwrap();
        worker::update(&setup.run(), "w1", |w| w.restarts = worker::MAX_RESTARTS).unwrap();
        assert!(
            worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
                .await
                .unwrap_err()
                .to_string()
                .contains("limit is 2")
        );
    }

    #[tokio::test]
    async fn restart_targets_the_run_workspace_for_a_legacy_worker() {
        let setup = Setup::new(|c| c);
        setup.start("api", "standard").await.unwrap();
        worker::update(&setup.run(), "w1", |w| {
            w.agent.workspace_id = "legacy".into();
            w.agent.pane_id = "legacy:p1".into();
            w.worktree_hooks = None;
        })
        .unwrap();
        let restarted = worker_restart(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", None)
            .await
            .unwrap();
        let Some(WorkerOperation::Restart(operation)) = restarted.operation else {
            panic!("missing durable restart")
        };
        assert_eq!(operation.previous.workspace, "legacy");
        assert_eq!(operation.previous.pane, "legacy:p1");
        assert!(
            matches!(operation.placement, PlacementIntent::Requested { workspace, .. } if workspace == "w1")
        );
        assert_eq!(operation.hooks, crate::config::WorktreeHooks::default());
    }

    #[tokio::test]
    async fn a_prompt_to_a_worker_is_refused_while_it_waits_on_a_dialog() {
        let setup = Setup::new(|c| c);
        let w = setup.start("api", "standard").await.unwrap();
        let herdr = &setup.session.herdr;
        let error = worker_prompt(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", "Hi")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error, "worker w1 is not running",
            "no agent in the pane yet"
        );
        herdr
            .agent_start(&w.agent.agent_name, "claude", &pane(&w.agent), &[])
            .await
            .unwrap();
        herdr.set_status("acme-data-1-w1", "blocked");
        let error = worker_prompt(&setup.ctx(), &setup.session, "acme/DATA-1", "w1", "Answer")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("dialog"), "{error}");
        herdr.set_status("acme-data-1-w1", "working");
        worker_prompt(
            &setup.ctx(),
            &setup.session,
            "acme/DATA-1",
            "w1",
            "Also add a test.\n",
        )
        .await
        .unwrap();
        assert_eq!(herdr.prompts_to(&w.agent.pane_id), ["Also add a test."]);
        let task = std::fs::read_to_string(worker::task_path(&setup.run(), "w1")).unwrap();
        assert!(task.contains("## Follow-ups") && task.contains("Also add a test."));
    }

    #[tokio::test]
    async fn finish_waits_for_every_worker() {
        let setup = Setup::new(|c| c);
        let w = setup.start("api", "standard").await.unwrap();
        let error = finish(&setup.ctx(), &setup.session, "acme/DATA-1", "Done")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "not finished: w1 is Working. Every worker must have written a report and be neither working nor waiting"
        );
        let herdr = &setup.session.herdr;
        herdr
            .agent_start(&w.agent.agent_name, "claude", &pane(&w.agent), &[])
            .await
            .unwrap();
        worker::update(&setup.run(), "w1", |w| w.report_hash = "h".into()).unwrap();
        finish(
            &setup.ctx(),
            &setup.session,
            "acme/DATA-1",
            " Opened the PR. ",
        )
        .await
        .unwrap();
        let ops: Vec<Op> = outbox::pending(&setup.run())
            .into_iter()
            .map(|(_, r)| r.op)
            .collect();
        assert_eq!(
            ops,
            [
                Op::activity(Activity::new(Content::Response {
                    body: "Opened the PR.".into()
                })),
                Op::IssueState {
                    target: StateTarget::Workflow(crate::run::WorkflowStage::InReview)
                }
            ]
        );
        assert!(setup.run().record().unwrap().finished);

        herdr.set_down(true);
        let error = finish(&setup.ctx(), &setup.session, "acme/DATA-1", "Again")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, UNREACHABLE);
    }

    #[tokio::test]
    async fn commands_that_write_run_files_poke_the_ticker() {
        let setup = Setup::new(|c| c);
        let listener =
            std::os::unix::net::UnixDatagram::bind(ticker::poke_path(&setup.ctx().state_dir()))
                .unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let poked = || {
            let mut byte = [0u8; 8];
            listener.recv(&mut byte).is_ok()
        };
        say(&setup.ctx(), "acme/DATA-1", "Working on it.")
            .await
            .unwrap();
        assert!(poked(), "say");
        plan_set(&setup.ctx(), "acme/DATA-1", "- [ ] One")
            .await
            .unwrap();
        assert!(poked(), "plan set");
        setup.start("api", "standard").await.unwrap();
        assert!(poked(), "worker start");
        inbox_done(&setup.ctx(), "acme/DATA-1", &[], true).unwrap();
        assert!(poked(), "inbox done");
    }

    #[tokio::test]
    async fn a_poke_without_a_ticker_is_ignored() {
        let setup = Setup::new(|c| c);
        say(&setup.ctx(), "acme/DATA-1", "Nobody listens.")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn done_all_leaves_the_items_written_after_the_last_context() {
        let setup = Setup::new(|c| c);
        let run = setup.run();
        inbox::write(&run, "worker", "w1", "shown").unwrap();
        context(&setup.ctx(), Some(&setup.session), "acme/DATA-1")
            .await
            .unwrap();
        let later = inbox::write(&run, "worker", "w2", "later").unwrap();
        inbox_done(&setup.ctx(), "acme/DATA-1", &[], true).unwrap();
        let left: Vec<String> = inbox::unhandled(&run).into_iter().map(|i| i.id).collect();
        assert_eq!(left, std::slice::from_ref(&later));
        assert_eq!(
            done_message(&run, 1),
            "1 item(s) handled; 1 new item(s) since your last context, run context"
        );
        inbox_done(
            &setup.ctx(),
            "acme/DATA-1",
            std::slice::from_ref(&later),
            false,
        )
        .unwrap();
        assert_eq!(done_message(&run, 1), "1 item(s) handled");
    }

    #[tokio::test]
    async fn commands_need_an_active_run_and_context_marks_items_seen() {
        let setup = Setup::new(|c| c);
        let run = setup.run();
        let id = inbox::write(&run, "worker", "w1", "item").unwrap();
        context(&setup.ctx(), Some(&setup.session), "acme/DATA-1")
            .await
            .unwrap();
        assert!(inbox::seen(&run).contains(&id));
        context::<FakeHerdr>(&setup.ctx(), None, "acme/DATA-1")
            .await
            .unwrap();
        inbox_done(&setup.ctx(), "acme/DATA-1", &[], true).unwrap();
        assert!(inbox::unhandled(&run).is_empty());
        assert!(inbox_done(&setup.ctx(), "acme/DATA-1", &[], false).is_err());

        run.update(|r| r.status = Status::Detached).unwrap();
        let error = say(&setup.ctx(), "acme/DATA-1", "x")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "run acme/DATA-1 is Detached; nothing more is done for it"
        );
        assert_eq!(
            say(&setup.ctx(), "acme/DATA-1", " ")
                .await
                .unwrap_err()
                .to_string(),
            "the text is empty"
        );
    }
}
