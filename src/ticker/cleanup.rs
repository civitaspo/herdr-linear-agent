//! Closed-run cleanup, with owned tasks and persisted worker cursors.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use tokio::task::JoinHandle;

use super::reconcile::{AgentKey, Deps, blocking};
use crate::herdr::{Herdr, HerdrError, PaneId, Snapshot};
use crate::run::{AgentStatus, Run, Status};
use crate::worker::{
    self, CleanupOperation, CleanupPhase, PlacementIntent, PlacementRef, Worker, WorkerOperation,
};

#[derive(Default)]
pub(super) struct CleanupDriver {
    tasks: BTreeMap<AgentKey, JoinHandle<Result<()>>>,
}

impl Drop for CleanupDriver {
    fn drop(&mut self) {
        for task in self.tasks.values() {
            task.abort();
        }
    }
}

impl CleanupDriver {
    #[cfg(test)]
    pub(super) fn tasks_finished(&self) -> bool {
        self.tasks.values().all(|task| task.is_finished())
    }

    pub(super) fn is_busy(&self) -> bool {
        !self.tasks.is_empty()
    }

    pub(super) async fn shutdown(&mut self) {
        let tasks = std::mem::take(&mut self.tasks);
        for task in tasks.values() {
            task.abort();
        }
        for (_, task) in tasks {
            let _ = task.await;
        }
    }

    pub(super) async fn advance<H: Herdr + Clone + 'static>(
        &mut self,
        d: &Deps<'_, H>,
        snapshot: Option<&Snapshot>,
        run: &Run,
    ) -> Result<()> {
        let finished: Vec<_> = self
            .tasks
            .iter()
            .filter(|(_, task)| task.is_finished())
            .map(|(key, _)| key.clone())
            .collect();
        for key in finished {
            if let Some(task) = self.tasks.remove(&key) {
                match task.await {
                    Ok(Ok(())) => (),
                    Ok(Err(error)) => d.fail(&key.run, &error),
                    Err(error) if error.is_cancelled() => (),
                    Err(error) => d
                        .log
                        .line(&format!("{} cleanup task failed: {error}", key.run)),
                }
            }
        }
        let owned = run.clone();
        let socket = d.socket.to_string();
        blocking(move || begin_cleanup(&owned, &socket)).await?;
        let record = run.record()?;
        if record.status != Status::Closed || !record.cleanup_pending {
            return Ok(());
        }
        let Some(snapshot) = snapshot.filter(|s| s.skipped == 0) else {
            return Ok(());
        };

        let coordinator_key = AgentKey::coordinator(&run.key);
        if !record.coordinator.pane_id.is_empty() && !self.tasks.contains_key(&coordinator_key) {
            let agent = record.coordinator.clone();
            let reference = agent_reference(&agent, d.socket);
            if reference.session != d.socket
                || !pane_matches(snapshot, &reference)
                || !agent_matches(snapshot, &reference, &agent)
            {
                let error = "cleanup is waiting for the recorded coordinator pane identity in its original Herdr session";
                let run = run.clone();
                blocking(move || {
                    run.update(|r| r.coordinator.error = error.into())?;
                    Ok(())
                })
                .await?;
            } else {
                let herdr = d.herdr.clone();
                let run = run.clone();
                self.tasks.insert(
                    coordinator_key,
                    tokio::spawn(async move {
                        if let Err(error) = close_pane(&herdr, &reference.pane).await {
                            let (run, pane, message) = (
                                run.clone(),
                                reference.pane.clone(),
                                format!("cleanup could not close coordinator: {error:#}"),
                            );
                            blocking(move || {
                                run.update(|r| {
                                    if r.status == Status::Closed && r.coordinator.pane_id == pane {
                                        r.coordinator.error = message;
                                    }
                                })?;
                                Ok(())
                            })
                            .await?;
                            return Err(error);
                        }
                        blocking(move || {
                            run.update(|r| {
                                if r.status == Status::Closed
                                    && r.coordinator.pane_id == reference.pane
                                    && (r.coordinator.herdr_session.is_empty()
                                        || r.coordinator.herdr_session == reference.session)
                                {
                                    r.coordinator.pane_id.clear();
                                    r.coordinator.tab_id.clear();
                                    r.coordinator.error.clear();
                                }
                            })?;
                            Ok(())
                        })
                        .await
                    }),
                );
            }
        }

        for worker in worker::list_checked(run)? {
            let Some(WorkerOperation::Cleanup(operation)) = &worker.operation else {
                continue;
            };
            let key = AgentKey::worker(&run.key, &worker.id);
            if operation.retained_reason.is_some() || self.tasks.contains_key(&key) {
                continue;
            }
            let mut operation = operation.clone();
            if operation.phase == CleanupPhase::ClosePanes {
                match resolve_unknown(snapshot, d.socket, &worker, &mut operation) {
                    Ok(true) => (),
                    Ok(false) => {
                        save_wait(run, &worker.id, operation.id, "cleanup is waiting for an uncertain tab creation; no second tab will be created").await?;
                        continue;
                    }
                    Err(error) => {
                        save_wait(run, &worker.id, operation.id, &error.to_string()).await?;
                        continue;
                    }
                }
                if operation.placements.iter().any(|p| {
                    p.session != d.socket
                        || !pane_matches(snapshot, p)
                        || !agent_matches(snapshot, p, &worker.agent)
                }) {
                    save_wait(run, &worker.id, operation.id, "cleanup is waiting for the recorded worker pane identity in its original Herdr session").await?;
                    continue;
                }
            }
            // Record an adopted uncertain pane before closing it: a restart
            // after close must not lose the only remaining ownership evidence.
            let (owned, worker_id, adopted) = (run.clone(), worker.id.clone(), operation.clone());
            let accepted = blocking(move || {
                let lock = owned.lock()?;
                if owned.record()?.status != Status::Closed { return Ok(false); }
                let mut accepted = false;
                worker::update_held(&owned, &lock, &worker_id, |w| {
                    if matches!(&w.operation, Some(WorkerOperation::Cleanup(op)) if op.id == adopted.id) {
                        w.operation = Some(WorkerOperation::Cleanup(adopted));
                        accepted = true;
                    }
                })?;
                Ok(accepted)
            }).await?;
            if !accepted {
                continue;
            }
            let herdr = d.herdr.clone();
            let runner = d.ctx.runner.shared();
            let run = run.clone();
            self.tasks.insert(
                key,
                tokio::spawn(async move {
                    cleanup_step(&herdr, runner.as_ref(), &run, &worker, operation).await
                }),
            );
        }
        if !self.tasks.keys().any(|k| k.run == run.key) {
            let owned = run.clone();
            blocking(move || {
                let lock = owned.lock()?;
                let record = owned.record()?;
                if record.status == Status::Closed
                    && record.coordinator.pane_id.is_empty()
                    && record.coordinator_placement.is_none()
                    && record.coordinator_orphans.is_empty()
                {
                    let terminal =
                        worker::list_checked(&owned)?
                            .iter()
                            .all(|w| match &w.operation {
                                None => true,
                                Some(WorkerOperation::Cleanup(op)) => {
                                    op.retained_reason.is_some()
                                        && op.placements.is_empty()
                                        && op.placement_intent.is_none()
                                }
                                _ => false,
                            });
                    if terminal {
                        owned.update_held(&lock, |r| r.cleanup_pending = false)?;
                    }
                }
                Ok(())
            })
            .await?;
        }
        Ok(())
    }
}

fn agent_reference(agent: &crate::run::AgentRecord, socket: &str) -> PlacementRef {
    PlacementRef {
        session: if agent.herdr_session.is_empty() {
            socket.into()
        } else {
            agent.herdr_session.clone()
        },
        workspace: agent.workspace_id.clone(),
        tab: agent.tab_id.clone(),
        pane: agent.pane_id.clone(),
        cwd: agent.cwd.clone(),
    }
}

fn push_placement(placements: &mut Vec<PlacementRef>, reference: PlacementRef) {
    if !reference.pane.is_empty()
        && !placements
            .iter()
            .any(|p| p.session == reference.session && p.pane == reference.pane)
    {
        placements.push(reference);
    }
}

fn begin_cleanup(run: &Run, socket: &str) -> Result<()> {
    let lock = run.lock()?;
    let record = run.record()?;
    if record.status != Status::Closed || !record.cleanup_pending {
        return Ok(());
    }
    for worker in worker::list_checked(run)? {
        if worker.cleanup_completed || matches!(worker.operation, Some(WorkerOperation::Cleanup(_)))
        {
            continue;
        }
        let mut placements = Vec::new();
        push_placement(&mut placements, agent_reference(&worker.agent, socket));
        let mut hooks = worker.worktree_hooks.clone().unwrap_or_default();
        let mut intent = None;
        match &worker.operation {
            Some(WorkerOperation::Start(op)) => {
                hooks = op.hooks.clone();
                intent = Some(op.placement.clone());
            }
            Some(WorkerOperation::Restart(op)) => {
                hooks = op.hooks.clone();
                push_placement(&mut placements, op.previous.clone());
                intent = Some(op.placement.clone());
            }
            _ => (),
        }
        if let Some(PlacementIntent::Placed(reference)) = intent.take() {
            push_placement(&mut placements, reference);
        } else {
            intent = match &worker.operation {
                Some(WorkerOperation::Start(op)) => Some(op.placement.clone()),
                Some(WorkerOperation::Restart(op)) => Some(op.placement.clone()),
                _ => None,
            };
            if matches!(intent, Some(PlacementIntent::Requested { .. })) {
                intent = None;
            }
        }
        worker::update_held(run, &lock, &worker.id, |w| {
            w.agent.status = AgentStatus::Stopped;
            w.agent.prompt_pending = false;
            w.restarting = false;
            w.operation = Some(WorkerOperation::Cleanup(CleanupOperation {
                id: uuid::Uuid::new_v4(),
                phase: CleanupPhase::ClosePanes,
                hook_cursor: 0,
                hooks,
                placement: None,
                placements,
                placement_intent: intent,
                retained_reason: None,
            }));
        })?;
    }
    Ok(())
}

fn same_path(a: &str, b: &str) -> bool {
    a == b
        || Path::new(a)
            .canonicalize()
            .ok()
            .zip(Path::new(b).canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

fn pane_matches(snapshot: &Snapshot, reference: &PlacementRef) -> bool {
    snapshot
        .panes
        .get(&PaneId(reference.pane.clone()))
        .is_none_or(|p| {
            p.workspace.0 == reference.workspace
                && p.tab == reference.tab
                && [p.cwd.as_deref(), p.foreground_cwd.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|cwd| same_path(cwd, &reference.cwd))
        })
}

fn agent_matches(
    snapshot: &Snapshot,
    reference: &PlacementRef,
    agent: &crate::run::AgentRecord,
) -> bool {
    snapshot
        .agents
        .iter()
        .filter(|a| a.pane.0 == reference.pane)
        .all(|a| {
            a.kind.as_deref().is_none_or(|kind| kind == agent.kind)
                && a.name
                    .as_deref()
                    .is_none_or(|name| name.is_empty() || name == agent.agent_name)
        })
}

fn resolve_unknown(
    snapshot: &Snapshot,
    socket: &str,
    worker: &Worker,
    operation: &mut CleanupOperation,
) -> Result<bool> {
    if let Some(reference) = operation.placement.take() {
        push_placement(&mut operation.placements, reference);
    }
    let Some(PlacementIntent::Unknown {
        session,
        workspace,
        cwd,
        label,
    }) = &operation.placement_intent
    else {
        return Ok(true);
    };
    ensure!(
        session == socket,
        "uncertain placement belongs to another Herdr session"
    );
    let expected_cwd = Path::new(cwd)
        .canonicalize()
        .context("cannot resolve uncertain placement directory; checkout retained")?;
    let candidates: Vec<_> = snapshot
        .panes
        .values()
        .filter(|p| {
            p.workspace.0 == *workspace
                && snapshot.tab_label(&p.tab) == Some(label.as_str())
                && !operation
                    .placements
                    .iter()
                    .any(|known| known.session == session.as_str() && known.pane == p.id.0)
                && [p.cwd.as_deref(), p.foreground_cwd.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|path| {
                        Path::new(path)
                            .canonicalize()
                            .is_ok_and(|actual| actual == expected_cwd)
                    })
                && snapshot.agents.iter().filter(|a| a.pane == p.id).all(|a| {
                    a.kind
                        .as_deref()
                        .is_none_or(|kind| kind == worker.agent.kind)
                        && a.name
                            .as_deref()
                            .is_none_or(|name| name.is_empty() || name == worker.agent.agent_name)
                })
        })
        .collect();
    ensure!(
        candidates.len() <= 1,
        "uncertain worker placement is ambiguous; checkout retained"
    );
    let Some(pane) = candidates.first() else {
        return Ok(false);
    };
    push_placement(
        &mut operation.placements,
        PlacementRef {
            session: session.clone(),
            workspace: workspace.clone(),
            tab: pane.tab.clone(),
            pane: pane.id.0.clone(),
            cwd: cwd.clone(),
        },
    );
    operation.placement_intent = None;
    Ok(true)
}

async fn close_pane<H: Herdr>(herdr: &H, pane: &str) -> Result<()> {
    match herdr.pane_close(&PaneId(pane.into())).await {
        Ok(()) => Ok(()),
        Err(HerdrError::Api { code, .. }) if code == "pane_not_found" => Ok(()),
        other => other.map_err(Into::into),
    }
}

async fn save_wait(run: &Run, id: &str, operation_id: uuid::Uuid, error: &str) -> Result<()> {
    let (run, id, error) = (run.clone(), id.to_string(), error.to_string());
    blocking(move || {
        worker::update(&run, &id, |w| {
            if matches!(&w.operation, Some(WorkerOperation::Cleanup(op)) if op.id == operation_id)
                && w.agent.error != error
            {
                w.agent.error = error;
            }
        })?;
        Ok(())
    })
    .await
}

async fn cleanup_step<H: Herdr>(
    herdr: &H,
    runner: &dyn crate::process::Runner,
    run: &Run,
    worker: &Worker,
    mut operation: CleanupOperation,
) -> Result<()> {
    let id = operation.id;
    let (owned, worker_id) = (run.clone(), worker.id.clone());
    let current = blocking(move || {
        let _lock = owned.lock()?;
        Ok(owned.record()?.status == Status::Closed && matches!(worker::load(&owned, &worker_id)?.operation, Some(WorkerOperation::Cleanup(op)) if op.id == id && op.retained_reason.is_none()))
    }).await?;
    if !current {
        return Ok(());
    }
    let result: Result<bool> = async {
        match operation.phase {
            CleanupPhase::ClosePanes => {
                let (run, worker) = (run.clone(), worker.clone());
                blocking(move || {
                    worker::copy_report_home(&run, &worker)?;
                    Ok(())
                })
                .await?;
                for reference in &operation.placements {
                    close_pane(herdr, &reference.pane).await?;
                }
                operation.placements.clear();
                operation.placement = None;
                operation.phase = CleanupPhase::CopyReport;
            }
            CleanupPhase::CopyReport => {
                let (run, worker) = (run.clone(), worker.clone());
                blocking(move || {
                    worker::copy_report_home(&run, &worker)?;
                    Ok(())
                })
                .await?;
                operation.phase = CleanupPhase::RemoveCheckout;
            }
            CleanupPhase::RemoveCheckout => {
                if !worker.worktree_path.is_empty() {
                    crate::worktree::remove_checkout(
                        runner,
                        &worker.repo_path,
                        &worker.worktree_path,
                        &worker.branch,
                    )
                    .await?;
                }
                operation.phase = CleanupPhase::PostRemove;
            }
            CleanupPhase::PostRemove => {
                if operation.hook_cursor >= crate::worktree::remove_steps(&operation.hooks) {
                    return Ok(true);
                }
                let vars = crate::worktree::HookVars::new(&run.key, worker);
                let log = run
                    .workers_dir()
                    .join(format!("{}.{}.cleanup.log", worker.id, operation.id));
                crate::worktree::post_remove_step(
                    runner,
                    &operation.hooks,
                    &vars,
                    operation.hook_cursor,
                    &log,
                )
                .await?;
                operation.hook_cursor += 1;
                return Ok(operation.hook_cursor >= crate::worktree::remove_steps(&operation.hooks));
            }
        }
        Ok(false)
    }
    .await;
    let done = match result {
        Ok(done) => done,
        Err(error) => {
            operation.retained_reason = Some(format!("{error:#}"));
            false
        }
    };
    let (run, worker_id) = (run.clone(), worker.id.clone());
    blocking(move || {
        let lock = run.lock()?;
        if run.record()?.status != Status::Closed {
            return Ok(());
        }
        worker::update_held(&run, &lock, &worker_id, |w| {
            if matches!(&w.operation, Some(WorkerOperation::Cleanup(op)) if op.id == id) {
                w.agent.error = operation.retained_reason.clone().unwrap_or_default();
                if done {
                    w.operation = None;
                    w.cleanup_completed = true;
                } else {
                    w.operation = Some(WorkerOperation::Cleanup(operation));
                }
            }
        })?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorktreeHooks;
    use crate::herdr::FakeHerdr;
    use crate::process::fake::FakeRunner;
    use crate::run::RunRecord;

    fn fixture(root: &Path) -> (Run, Worker) {
        let run = Run::create(
            &root.join("runs"),
            RunRecord {
                workspace: "acme".into(),
                identifier: "DATA-1".into(),
                status: Status::Closed,
                cleanup_pending: true,
                ..RunRecord::default()
            },
        )
        .unwrap();
        let worker = worker::allocate(
            &run,
            |_| Ok(()),
            |w| {
                w.brief_dir = root.join("checkout/brief").to_string_lossy().into_owned();
            },
        )
        .unwrap();
        (run, worker)
    }

    #[tokio::test]
    async fn cleanup_keeps_the_checkout_when_the_final_report_cannot_be_saved() {
        let root = tempfile::tempdir().unwrap();
        let (run, worker) = fixture(root.path());
        std::fs::create_dir_all(&worker.brief_dir).unwrap();
        let report = Path::new(&worker.brief_dir).join("report.md");
        std::fs::write(&report, "the only report").unwrap();
        std::fs::create_dir(run.workers_dir().join("w1.md")).unwrap();
        begin_cleanup(&run, "test").unwrap();
        let Some(WorkerOperation::Cleanup(mut operation)) =
            worker::load(&run, "w1").unwrap().operation
        else {
            panic!("missing cleanup")
        };
        operation.phase = CleanupPhase::CopyReport;
        let runner = FakeRunner::default();
        cleanup_step(
            &FakeHerdr::new(root.path()),
            &runner,
            &run,
            &worker,
            operation,
        )
        .await
        .unwrap();
        let saved = worker::load(&run, "w1").unwrap();
        let Some(WorkerOperation::Cleanup(operation)) = saved.operation else {
            panic!("lost cleanup")
        };
        assert_eq!(operation.phase, CleanupPhase::CopyReport);
        assert!(operation.retained_reason.is_some());
        assert_eq!(std::fs::read_to_string(report).unwrap(), "the only report");
        assert_eq!(runner.count("worktree remove"), 0);
    }

    #[tokio::test]
    async fn a_report_saved_before_close_survives_external_checkout_removal() {
        let root = tempfile::tempdir().unwrap();
        let (run, worker) = fixture(root.path());
        std::fs::create_dir_all(&worker.brief_dir).unwrap();
        std::fs::write(
            Path::new(&worker.brief_dir).join("report.md"),
            "final report",
        )
        .unwrap();
        begin_cleanup(&run, "test").unwrap();
        let Some(WorkerOperation::Cleanup(operation)) = worker::load(&run, "w1").unwrap().operation
        else {
            unreachable!()
        };
        let runner = FakeRunner::default();
        let herdr = FakeHerdr::new(root.path());
        cleanup_step(&herdr, &runner, &run, &worker, operation)
            .await
            .unwrap();
        std::fs::remove_dir_all(&worker.brief_dir).unwrap();
        let Some(WorkerOperation::Cleanup(operation)) = worker::load(&run, "w1").unwrap().operation
        else {
            unreachable!()
        };
        assert_eq!(operation.phase, CleanupPhase::CopyReport);
        cleanup_step(&herdr, &runner, &run, &worker, operation)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(worker::home_report_path(&run, "w1")).unwrap(),
            "final report"
        );
    }

    #[tokio::test]
    async fn completed_cleanup_is_not_recreated_and_stale_steps_cannot_replace_a_retry() {
        let root = tempfile::tempdir().unwrap();
        let (run, worker) = fixture(root.path());
        begin_cleanup(&run, "test").unwrap();
        let Some(WorkerOperation::Cleanup(mut operation)) =
            worker::load(&run, "w1").unwrap().operation
        else {
            panic!("missing cleanup")
        };
        let stale = operation.clone();
        operation.id = uuid::Uuid::new_v4();
        operation.phase = CleanupPhase::PostRemove;
        operation.hooks = WorktreeHooks::default();
        worker::update(&run, "w1", |w| {
            w.operation = Some(WorkerOperation::Cleanup(operation.clone()))
        })
        .unwrap();
        let runner = FakeRunner::default();
        let herdr = FakeHerdr::new(root.path());
        cleanup_step(&herdr, &runner, &run, &worker, stale)
            .await
            .unwrap();
        assert_eq!(
            worker::load(&run, "w1").unwrap().operation,
            Some(WorkerOperation::Cleanup(operation.clone()))
        );
        cleanup_step(&herdr, &runner, &run, &worker, operation)
            .await
            .unwrap();
        begin_cleanup(&run, "test").unwrap();
        let saved = worker::load(&run, "w1").unwrap();
        assert!(saved.cleanup_completed);
        assert!(saved.operation.is_none());
    }
    #[tokio::test]
    async fn uncertain_cleanup_does_not_adopt_an_unrelated_tab_in_the_same_directory() {
        let root = tempfile::tempdir().unwrap();
        let (run, worker) = fixture(root.path());
        let checkout = root.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let herdr = FakeHerdr::new(root.path());
        let user = herdr
            .workspace_create(checkout.to_str().unwrap(), "user tab")
            .await
            .unwrap();
        begin_cleanup(&run, "test").unwrap();
        let Some(WorkerOperation::Cleanup(mut operation)) =
            worker::load(&run, "w1").unwrap().operation
        else {
            unreachable!()
        };
        operation.placement_intent = Some(PlacementIntent::Unknown {
            session: "test".into(),
            workspace: user.workspace.0.clone(),
            cwd: checkout.to_string_lossy().into_owned(),
            label: "managed tab".into(),
        });
        assert!(
            !resolve_unknown(
                &herdr.snapshot().await.unwrap(),
                "test",
                &worker,
                &mut operation
            )
            .unwrap()
        );
        assert!(operation.placements.is_empty());
        let managed = herdr
            .tab_create(&user.workspace, checkout.to_str().unwrap(), "managed tab")
            .await
            .unwrap();
        assert!(
            resolve_unknown(
                &herdr.snapshot().await.unwrap(),
                "test",
                &worker,
                &mut operation
            )
            .unwrap()
        );
        assert_eq!(operation.placements.len(), 1);
        assert_eq!(operation.placements[0].pane, managed.pane.0);
        assert_ne!(operation.placements[0].pane, user.pane.0);
    }
}
