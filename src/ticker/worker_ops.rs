//! Durable worker checkout and placement transitions. Slow Git, hook, and
//! Herdr work runs in one tracked task per worker, outside the reconciler.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use super::reconcile::{AgentKey, Deps, Reconciler, blocking};
use crate::herdr::{Herdr, HerdrError, PaneId, Placed, Snapshot, WorkspaceId};
use crate::process::Runner;
use crate::run::{Run, Status};
use crate::worker::{
    self, PlacementIntent, PlacementRef, RestartPhase, StartPhase, Worker, WorkerOperation,
};

impl Reconciler {
    /// Starts a tracked task for each pending worker operation. The task
    /// advances durable phases itself and returns at an unresolved placement,
    /// which will be reconsidered with the next Herdr snapshot.
    pub(super) async fn worker_operations<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: Option<&Snapshot>,
        run: &Run,
    ) -> Result<()> {
        let record = run.record()?;
        if record.status != crate::run::Status::Active
            || record.stopped
            || record.finished
            || record.awaiting_reply.is_some()
            || record.coordinator_misplaced
        {
            return Ok(());
        }
        if snapshot
            .and_then(|snapshot| {
                snapshot
                    .panes
                    .get(&PaneId(record.coordinator.pane_id.clone()))
            })
            .is_some_and(|pane| {
                !record.coordinator.workspace_id.is_empty()
                    && pane.workspace.0 != record.coordinator.workspace_id
            })
        {
            return Ok(());
        }
        let runner = d.ctx.runner.shared();
        let herdr = d.herdr.clone();
        let socket = d.socket.to_string();
        let snapshot = snapshot.cloned();

        for worker in worker::list_checked(run)? {
            let Some(operation) = worker.operation.as_ref() else {
                continue;
            };
            if worker.agent.status == crate::run::AgentStatus::Failed
                || matches!(operation, WorkerOperation::Cleanup(_))
            {
                continue;
            }
            let key = AgentKey::worker(&run.key, &worker.id);
            if self.worker_tasks.contains_key(&key) {
                continue;
            }
            let id = operation_id_of(operation);
            let run = run.clone();
            let worker = worker.clone();
            let runner = Arc::clone(&runner);
            let herdr = herdr.clone();
            let socket = socket.clone();
            let snapshot = snapshot.clone();
            self.worker_tasks.insert(
                key,
                tokio::spawn(async move {
                    let result =
                        drive(&run, &worker, id, runner, herdr, &socket, snapshot.as_ref()).await;
                    if let Err(error) = &result {
                        fail_operation(&run, &worker.id, id, &error.to_string()).await?;
                    }
                    result
                }),
            );
        }
        Ok(())
    }
}

async fn drive<H: Herdr + Clone + 'static>(
    run: &Run,
    worker: &Worker,
    operation_id: uuid::Uuid,
    runner: Arc<dyn Runner>,
    herdr: H,
    socket: &str,
    snapshot: Option<&Snapshot>,
) -> Result<()> {
    let record = run.record()?;
    if record.status != crate::run::Status::Active
        || record.stopped
        || record.finished
        || record.awaiting_reply.is_some()
    {
        return Ok(());
    }
    let current = worker::load(run, &worker.id)?;
    let Some(operation) = current.operation.clone() else {
        return Ok(());
    };
    if operation_id_of(&operation) != operation_id {
        return Ok(());
    }
    let log_path = run
        .workers_dir()
        .join(format!("{}.{}.setup.log", worker.id, operation_id));
    match operation {
        WorkerOperation::Start(mut op) => match op.phase {
            StartPhase::SaveTask => {
                let task = op
                    .task
                    .take()
                    .context("worker start has no durable task payload")?;
                let path = worker::task_path(run, &worker.id);
                tokio::task::spawn_blocking(move || {
                    crate::files::write_atomic(&path, task.as_bytes())
                })
                .await
                .context("a worker task file write failed")??;
                op.phase = StartPhase::CreateCheckout;
                save_start(run, &worker.id, operation_id, op).await?;
                Ok(())
            }
            StartPhase::CreateCheckout => {
                crate::worktree::create(
                    &*runner,
                    &current.repo_path,
                    &current.branch,
                    &current.base,
                    &current.worktree_path,
                )
                .await?;
                let hooks = op.hooks.clone();
                let _ = update_active_operation(run, &worker.id, operation_id, move |current| {
                    if let Some(WorkerOperation::Start(saved)) = &mut current.operation
                        && saved.id == operation_id
                    {
                        current.worktree_hooks = Some(hooks);
                        saved.phase = StartPhase::Setup;
                    }
                })
                .await?;
                Ok(())
            }
            StartPhase::Setup => {
                let total = crate::worktree::setup_steps(&op.hooks);
                if op.setup_cursor >= total {
                    op.phase = StartPhase::CreateTab;
                    save_start(run, &worker.id, operation_id, op).await?;
                    Ok(())
                } else {
                    let vars = crate::worktree::HookVars::new(&run.key, &current);
                    crate::worktree::setup_step(
                        &*runner,
                        &op.hooks,
                        &vars,
                        op.setup_cursor,
                        &log_path,
                    )
                    .await?;
                    op.setup_cursor += 1;
                    save_start(run, &worker.id, operation_id, op).await?;
                    Ok(())
                }
            }
            StartPhase::CreateTab => {
                match place(
                    &*runner,
                    &herdr,
                    socket,
                    snapshot,
                    run,
                    &current,
                    &mut op.placement,
                )
                .await?
                {
                    PlacementResult::Waiting => {
                        save_start(run, &worker.id, operation_id, op).await?;
                        Ok(())
                    }
                    PlacementResult::Placed(placed) => {
                        let socket = socket.to_string();
                        let _ = update_active_operation(
                            run,
                            &worker.id,
                            operation_id,
                            move |current| {
                                if let Some(WorkerOperation::Start(saved)) = &mut current.operation
                                    && saved.id == operation_id
                                {
                                    current.agent.placed_in(&placed, &socket);
                                    saved.phase = StartPhase::PrepareBrief;
                                    saved.placement =
                                        PlacementIntent::Placed(placement_ref(&placed, &socket));
                                }
                            },
                        )
                        .await?;
                        Ok(())
                    }
                }
            }
            StartPhase::PrepareBrief => {
                prepare_brief(&*runner, run, &current, &op.instructions, false).await?;
                clear_operation(run, &worker.id, operation_id).await?;
                Ok(())
            }
        },
        WorkerOperation::Restart(mut op) => match op.phase {
            RestartPhase::CreateCheckout => {
                crate::worktree::create(
                    &*runner,
                    &op.repo_path,
                    &op.branch,
                    &op.base,
                    &current.worktree_path,
                )
                .await?;
                op.create_checkout = true;
                op.phase = RestartPhase::Setup;
                save_restart(run, &worker.id, operation_id, op).await?;
                Ok(())
            }
            RestartPhase::Setup => {
                let total = crate::worktree::setup_steps(&op.hooks);
                if op.setup_cursor >= total {
                    op.phase = RestartPhase::CreateTab;
                    save_restart(run, &worker.id, operation_id, op).await?;
                    Ok(())
                } else {
                    let vars = crate::worktree::HookVars::new(&run.key, &current);
                    crate::worktree::setup_step(
                        &*runner,
                        &op.hooks,
                        &vars,
                        op.setup_cursor,
                        &log_path,
                    )
                    .await?;
                    op.setup_cursor += 1;
                    save_restart(run, &worker.id, operation_id, op).await?;
                    Ok(())
                }
            }
            RestartPhase::CreateTab => {
                match place(
                    &*runner,
                    &herdr,
                    socket,
                    snapshot,
                    run,
                    &current,
                    &mut op.placement,
                )
                .await?
                {
                    PlacementResult::Waiting => {
                        save_restart(run, &worker.id, operation_id, op).await?;
                        Ok(())
                    }
                    PlacementResult::Placed(placed) => {
                        let socket = socket.to_string();
                        let _ = update_active_operation(
                            run,
                            &worker.id,
                            operation_id,
                            move |current| {
                                if let Some(WorkerOperation::Restart(saved)) =
                                    &mut current.operation
                                    && saved.id == operation_id
                                {
                                    current.agent.placed_in(&placed, &socket);
                                    saved.placement =
                                        PlacementIntent::Placed(placement_ref(&placed, &socket));
                                    saved.phase = RestartPhase::ClosePreviousPane;
                                }
                            },
                        )
                        .await?;
                        Ok(())
                    }
                }
            }
            RestartPhase::ClosePreviousPane => {
                if !op.previous.pane.is_empty() {
                    ensure!(
                        op.previous.session == socket,
                        "previous worker pane belongs to another Herdr session; it was retained"
                    );
                    let snapshot = snapshot.filter(|s| s.skipped == 0).context(
                        "cannot verify previous worker pane from an incomplete Herdr snapshot",
                    )?;
                    if snapshot
                        .panes
                        .contains_key(&PaneId(op.previous.pane.clone()))
                    {
                        ensure!(
                            pane_matches(snapshot, &op.previous, &current),
                            "previous worker pane identity changed; it was retained"
                        );
                    }
                    match herdr.pane_close(&PaneId(op.previous.pane.clone())).await {
                        Ok(()) => (),
                        Err(HerdrError::Api { code, .. }) if code == "pane_not_found" => (),
                        Err(HerdrError::NotSent(_) | HerdrError::OutcomeUnknown(_)) => {
                            return Ok(());
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                let profile = op.profile.clone();
                let kind = op.kind.clone();
                let _ = update_active_operation(run, &worker.id, operation_id, move |current| {
                    if let Some(WorkerOperation::Restart(saved)) = &mut current.operation
                        && saved.id == operation_id
                    {
                        if current.agent.kind != kind {
                            current.agent.agent_session.clear();
                            current.agent.resume = false;
                        }
                        current.agent.profile = profile;
                        current.agent.kind = kind;
                        current.agent.recovery = crate::run::Recovery::None;
                        current.agent.prompt_pending = true;
                        current.agent.last_state.clear();
                        current.agent.last_state_change.clear();
                        current.agent.last_state_seq = 0;
                        current.agent.blocked_reported = false;
                        current.restarting = false;
                        saved.phase = RestartPhase::PrepareBrief;
                    }
                })
                .await?;
                Ok(())
            }
            RestartPhase::PrepareBrief => {
                prepare_brief(&*runner, run, &current, &op.instructions, true).await?;
                let _ = update_active_operation(run, &worker.id, operation_id, move |current| {
                    if current
                        .operation
                        .as_ref()
                        .is_some_and(|op| operation_id_of(op) == operation_id)
                    {
                        if !op.retry_counted {
                            current.restarts += 1;
                        }
                        current.restarting = false;
                        current.operation = None;
                    }
                })
                .await?;
                Ok(())
            }
        },
        WorkerOperation::Cleanup(_) => Ok(()),
    }
}

enum PlacementResult {
    Waiting,
    Placed(Placed),
}

async fn place<H: Herdr>(
    runner: &dyn Runner,
    herdr: &H,
    socket: &str,
    snapshot: Option<&Snapshot>,
    run: &Run,
    worker: &Worker,
    intent: &mut PlacementIntent,
) -> Result<PlacementResult> {
    let (operation_id, expected_kind) = match worker.operation.as_ref() {
        Some(WorkerOperation::Start(operation)) => (operation.id, worker.agent.kind.as_str()),
        Some(WorkerOperation::Restart(operation)) => (operation.id, operation.kind.as_str()),
        _ => anyhow::bail!("worker {} has no placement operation", worker.id),
    };
    if run.record()?.coordinator_misplaced {
        return Ok(PlacementResult::Waiting);
    }
    let (session, workspace, cwd, label) = match intent {
        PlacementIntent::Requested {
            session,
            workspace,
            cwd,
            label,
        } => (
            session.clone(),
            workspace.clone(),
            cwd.clone(),
            label.clone(),
        ),
        PlacementIntent::Unknown {
            session,
            workspace,
            cwd,
            label,
        } => {
            if session != socket {
                return Ok(PlacementResult::Waiting);
            }
            if let Some(snapshot) = snapshot.filter(|s| s.skipped == 0) {
                let Some(expected) = std::fs::canonicalize(&cwd).ok() else {
                    return Ok(PlacementResult::Waiting);
                };
                let found: Vec<_> = snapshot
                    .panes
                    .values()
                    .filter(|pane| {
                        pane.id.0 != worker.agent.pane_id
                            && pane.workspace.0 == *workspace
                            && snapshot.tab_label(&pane.tab) == Some(label.as_str())
                            && [pane.cwd.as_deref(), pane.foreground_cwd.as_deref()]
                                .into_iter()
                                .flatten()
                                .any(|actual| {
                                    std::fs::canonicalize(actual)
                                        .is_ok_and(|actual| actual == expected)
                                })
                            && pane_matches_worker(snapshot, pane, worker, expected_kind)
                    })
                    .collect();
                if found.len() == 1 {
                    let pane = found[0];
                    return Ok(PlacementResult::Placed(Placed {
                        workspace: pane.workspace.clone(),
                        tab: pane.tab.clone(),
                        pane: pane.id.clone(),
                        cwd: pane.cwd.clone().unwrap_or_else(|| cwd.clone()),
                    }));
                }
                ensure!(
                    found.len() <= 1,
                    "{} worker {} placement is ambiguous; refusing to create another tab",
                    run.key,
                    worker.id
                );
            }
            return Ok(PlacementResult::Waiting);
        }
        PlacementIntent::Placed(reference) => {
            return Ok(PlacementResult::Placed(Placed {
                workspace: WorkspaceId(reference.workspace.clone()),
                tab: reference.tab.clone(),
                pane: PaneId(reference.pane.clone()),
                cwd: reference.cwd.clone(),
            }));
        }
    };
    ensure!(
        session == socket,
        "worker {} placement belongs to another Herdr session",
        worker.id
    );
    let (repo, branch) = match worker.operation.as_ref() {
        Some(WorkerOperation::Restart(operation)) => {
            (operation.repo_path.as_str(), operation.branch.as_str())
        }
        _ => (worker.repo_path.as_str(), worker.branch.as_str()),
    };
    crate::worktree::verify_checkout(runner, repo, &cwd, branch).await?;
    let unknown = PlacementIntent::Unknown {
        session: session.clone(),
        workspace: workspace.clone(),
        cwd: cwd.clone(),
        label: label.clone(),
    };
    if !save_placement(run, &worker.id, operation_id, unknown.clone()).await? {
        return Ok(PlacementResult::Waiting);
    }
    match herdr
        .tab_create(&WorkspaceId(workspace.clone()), &cwd, &label)
        .await
    {
        Ok(placed) => {
            ensure!(
                placed.workspace.0 == workspace,
                "Herdr placed worker {} in an unexpected workspace",
                worker.id
            );
            ensure!(
                std::fs::canonicalize(&placed.cwd).ok().is_some()
                    && std::fs::canonicalize(&placed.cwd).ok() == std::fs::canonicalize(&cwd).ok(),
                "Herdr placed worker {} at an unexpected path",
                worker.id
            );
            Ok(PlacementResult::Placed(placed))
        }
        Err(HerdrError::NotSent(_)) => {
            *intent = PlacementIntent::Requested {
                session,
                workspace,
                cwd,
                label,
            };
            save_placement(run, &worker.id, operation_id, intent.clone()).await?;
            Ok(PlacementResult::Waiting)
        }
        Err(error @ HerdrError::Api { .. }) => {
            *intent = PlacementIntent::Requested {
                session,
                workspace,
                cwd,
                label,
            };
            save_placement(run, &worker.id, operation_id, intent.clone()).await?;
            Err(error.into())
        }
        Err(HerdrError::OutcomeUnknown(_) | HerdrError::Protocol(_)) => {
            *intent = unknown;
            Ok(PlacementResult::Waiting)
        }
    }
}

fn pane_matches_worker(
    snapshot: &Snapshot,
    pane: &crate::herdr::Pane,
    worker: &Worker,
    expected_kind: &str,
) -> bool {
    snapshot
        .agents
        .iter()
        .filter(|agent| agent.pane == pane.id)
        .all(|agent| {
            agent
                .name
                .as_deref()
                .is_none_or(|name| name.is_empty() || name == worker.agent.agent_name)
                && agent
                    .kind
                    .as_deref()
                    .is_none_or(|kind| kind == expected_kind)
        })
}

fn pane_matches(snapshot: &Snapshot, previous: &PlacementRef, worker: &Worker) -> bool {
    let Some(pane) = snapshot.panes.get(&PaneId(previous.pane.clone())) else {
        return false;
    };
    let expected = std::fs::canonicalize(&previous.cwd).ok();
    pane.workspace.0 == previous.workspace
        && pane.tab == previous.tab
        && expected.is_some()
        && [pane.cwd.as_deref(), pane.foreground_cwd.as_deref()]
            .into_iter()
            .flatten()
            .any(|cwd| std::fs::canonicalize(cwd).ok() == expected)
        && pane_matches_worker(snapshot, pane, worker, &worker.agent.kind)
}

fn placement_ref(placed: &Placed, session: &str) -> PlacementRef {
    PlacementRef {
        session: session.into(),
        workspace: placed.workspace.0.clone(),
        tab: placed.tab.clone(),
        pane: placed.pane.0.clone(),
        cwd: placed.cwd.clone(),
    }
}

async fn prepare_brief(
    runner: &dyn Runner,
    run: &Run,
    worker: &Worker,
    instructions: &[crate::config::Instructions],
    restart: bool,
) -> Result<()> {
    let run = run.clone();
    let worker = worker.clone();
    let instructions = instructions.to_vec();
    let runner = runner.shared();
    crate::commands::exclude_from_git(&*runner, &worker.repo_path).await?;
    tokio::task::spawn_blocking(move || {
        let run_record = run.record()?;
        std::fs::create_dir_all(&worker.brief_dir)
            .with_context(|| format!("could not create {}", worker.brief_dir))?;
        let task = std::fs::read_to_string(worker::task_path(&run, &worker.id))
            .with_context(|| format!("could not read task for worker {}", worker.id))?;
        let brief = worker::compose_brief(&worker::BriefInput {
            issue_key: &run_record.identifier,
            issue_title: &run_record.title,
            issue_url: &run_record.url,
            worker: &worker,
            task: &task,
            restart,
            binary: &crate::coordinator::binary_command()?,
            instructions: &instructions,
        });
        crate::files::write_atomic(
            &Path::new(&worker.brief_dir).join("brief.md"),
            brief.as_bytes(),
        )
    })
    .await
    .context("a worker brief task failed")?
}

async fn save_start(
    run: &Run,
    id: &str,
    operation_id: uuid::Uuid,
    op: worker::StartOperation,
) -> Result<bool> {
    update_active_operation(run, id, operation_id, move |worker| {
        if let Some(WorkerOperation::Start(saved)) = &mut worker.operation
            && saved.id == operation_id
        {
            *saved = op;
        }
    })
    .await
}

async fn save_restart(
    run: &Run,
    id: &str,
    operation_id: uuid::Uuid,
    op: worker::RestartOperation,
) -> Result<bool> {
    update_active_operation(run, id, operation_id, move |worker| {
        if let Some(WorkerOperation::Restart(saved)) = &mut worker.operation
            && saved.id == operation_id
        {
            *saved = op;
        }
    })
    .await
}

async fn save_placement(
    run: &Run,
    id: &str,
    operation_id: uuid::Uuid,
    intent: PlacementIntent,
) -> Result<bool> {
    update_active_operation(run, id, operation_id, move |worker| {
        match &mut worker.operation {
            Some(WorkerOperation::Start(op)) if op.id == operation_id => op.placement = intent,
            Some(WorkerOperation::Restart(op)) if op.id == operation_id => op.placement = intent,
            _ => (),
        }
    })
    .await
}

async fn clear_operation(run: &Run, id: &str, operation_id: uuid::Uuid) -> Result<bool> {
    update_active_operation(run, id, operation_id, move |worker| {
        if worker
            .operation
            .as_ref()
            .is_some_and(|op| operation_id_of(op) == operation_id)
        {
            worker.operation = None;
        }
    })
    .await
}

async fn update_active_operation(
    run: &Run,
    id: &str,
    operation_id: uuid::Uuid,
    change: impl FnOnce(&mut Worker) + Send + 'static,
) -> Result<bool> {
    let run = run.clone();
    let id = id.to_string();
    blocking(move || {
        let lock = run.lock()?;
        let record = run.record()?;
        if record.status != Status::Active
            || record.stopped
            || record.finished
            || record.awaiting_reply.is_some()
        {
            return Ok(false);
        }
        if worker::load(&run, &id)?
            .operation
            .as_ref()
            .is_none_or(|operation| operation_id_of(operation) != operation_id)
        {
            return Ok(false);
        }
        worker::update_held(&run, &lock, &id, change)?;
        Ok(true)
    })
    .await
}

async fn fail_operation(run: &Run, id: &str, operation_id: uuid::Uuid, error: &str) -> Result<()> {
    let error = error.to_string();
    let _ = update_active_operation(run, id, operation_id, move |worker| {
        if worker
            .operation
            .as_ref()
            .is_some_and(|operation| operation_id_of(operation) == operation_id)
        {
            worker.agent.status = crate::run::AgentStatus::Failed;
            worker.agent.prompt_pending = false;
            worker.agent.recovery = crate::run::Recovery::None;
            worker.agent.error = error;
            worker.restarting = false;
        }
    })
    .await?;
    Ok(())
}

fn operation_id_of(operation: &WorkerOperation) -> uuid::Uuid {
    match operation {
        WorkerOperation::Start(op) => op.id,
        WorkerOperation::Restart(op) => op.id,
        WorkerOperation::Cleanup(op) => op.id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{self, WorkerStart};
    use crate::herdr::{Herdr, WorkspaceId};
    use crate::process::fake::{fail, ok};
    use crate::ticker::world::World;

    const KEY: &str = "acme/DATA-1";

    #[tokio::test]
    async fn unknown_tab_creation_survives_ticker_restart_and_ignores_unrelated_tab() {
        let mut world = World::sample();
        let _ = world.running_issue().await;
        world.herdr.delay_panes(4);
        let args = WorkerStart {
            repo: "api".into(),
            profile: "standard".into(),
            title: "Change api".into(),
            task: "Make the change and open a PR.".into(),
        };
        let requested = commands::worker_start(&world.ctx(), &world.session(), KEY, &args)
            .await
            .unwrap();
        for _ in 0..20 {
            let saved = world.worker(KEY, &requested.id);
            if matches!(
                saved.operation,
                Some(WorkerOperation::Start(ref operation))
                    if operation.phase == StartPhase::CreateTab
            ) {
                break;
            }
            world.pass_alone().await;
            world.wait_worker_tasks().await;
        }
        let ready = world.worker(KEY, &requested.id);
        assert!(matches!(
            ready.operation,
            Some(WorkerOperation::Start(ref operation))
                if operation.phase == StartPhase::CreateTab
        ));
        world.wait_worker_tasks().await;
        let unrelated = world
            .herdr
            .tab_create(
                &WorkspaceId(world.record(KEY).coordinator.workspace_id),
                &ready.worktree_path,
                "unrelated user tab",
            )
            .await
            .unwrap();

        let creates_before_worker = world.asked("tab.create");
        world.herdr.next_placement_unknown();
        world.pass_alone().await;
        world.wait_worker_tasks().await;
        assert!(matches!(
            world.worker(KEY, &requested.id).operation,
            Some(WorkerOperation::Start(ref operation))
                if matches!(operation.placement, PlacementIntent::Unknown { .. })
        ));
        assert_eq!(world.asked("tab.create"), creates_before_worker + 1);

        world.restart_ticker().await;
        world.settle().await;
        let placed = world.worker(KEY, &requested.id);
        assert_eq!(world.asked("tab.create"), creates_before_worker + 1);
        assert_ne!(placed.agent.pane_id, unrelated.pane.0);
        assert!(
            world
                .herdr
                .panes()
                .iter()
                .any(|pane| pane.id == unrelated.pane)
        );
    }

    #[tokio::test]
    async fn restart_refuses_a_replaced_checkout_before_creating_a_tab() {
        let mut world = World::sample();
        world.running_issue().await;
        let worker = world.start_worker("api").await;
        let creates = world.asked("tab.create");
        std::fs::remove_dir_all(&worker.worktree_path).unwrap();
        std::fs::create_dir(&worker.worktree_path).unwrap();
        commands::worker_restart(&world.ctx(), &world.session(), KEY, &worker.id, None)
            .await
            .unwrap();
        world.settle().await;
        let failed = world.worker(KEY, &worker.id);
        assert_eq!(failed.agent.status, crate::run::AgentStatus::Failed);
        assert!(failed.operation.is_some());
        assert_eq!(world.asked("tab.create"), creates);
        assert!(
            world
                .herdr
                .panes()
                .iter()
                .any(|pane| pane.id.0 == worker.agent.pane_id)
        );
    }

    #[tokio::test]
    async fn retry_resumes_setup_at_the_failed_hook_cursor() {
        let mut world = World::with(|text| {
            text.replace(
                "post_create = []",
                "post_create = ['echo FIRST_HOOK', 'echo SECOND_HOOK; exit 7']",
            )
        });
        world
            .runner
            .on("FIRST_HOOK", ok(""))
            .on("SECOND_HOOK; exit 7", fail(7, "hook failed"));
        world.running_issue().await;
        let failed = world.start_worker("api").await;
        let Some(WorkerOperation::Start(operation)) = failed.operation else {
            panic!("failed setup operation was not retained")
        };
        assert_eq!(operation.setup_cursor, 1);
        assert_eq!(world.runner.count("FIRST_HOOK"), 1);
        assert_eq!(world.runner.count("SECOND_HOOK"), 1);

        world.runner.on("SECOND_HOOK; exit 7", ok(""));
        commands::worker_restart(&world.ctx(), &world.session(), KEY, &failed.id, None)
            .await
            .unwrap();
        world.settle().await;
        assert_eq!(world.runner.count("FIRST_HOOK"), 1);
        assert_eq!(world.runner.count("SECOND_HOOK"), 2);
        assert!(world.worker(KEY, &failed.id).operation.is_none());
    }
}
