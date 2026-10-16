use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use super::{Harness, execute};
use crate::files::{self, shell_quote, write_json};
use crate::herdr::{Client as HerdrClient, Herdr, PaneId, Snapshot, WorkspaceId};
use crate::linear::api::LinearApi;
use crate::linear::client::Client as LinearClient;
use crate::paths::{self, Env};
use crate::run::{Run, RunRecord, Status};
use crate::worker::{self, CleanupPhase, StartPhase, Worker, WorkerOperation};

struct Observation {
    record: RunRecord,
    workers: Vec<Worker>,
    snapshot: Snapshot,
    evidence: Value,
}

struct Check<'a> {
    harness: &'a Harness,
    env: &'a Env,
    key: &'a str,
    linear: LinearClient,
    herdr: HerdrClient,
    budget: Duration,
}

pub(super) async fn verify(
    harness: &Harness,
    env: &Env,
    key: &str,
    terminal: &str,
    seconds: u64,
    pause_after_workers: bool,
) -> Result<()> {
    crate::run::validate_key(key)?;
    ensure!(
        matches!(terminal, "done" | "canceled"),
        "terminal must be done or canceled"
    );
    ensure!(
        !pause_after_workers || terminal == "done",
        "--pause-after-workers requires --terminal done"
    );
    ensure!(
        (1..=1800).contains(&seconds),
        "timeout must be 1..=1800 seconds per checkpoint"
    );
    let (workspace, issue) = crate::run::split_key(key);
    let config = crate::config::Config::load(&harness.root.join("config/herdr-linear-agent"))?;
    let workspace_config = config
        .workspaces
        .get(workspace)
        .context("workspace is not configured")?;
    let detail = workspace_config
        .teams
        .keys()
        .any(|team| issue.starts_with(&format!("{team}-")));
    ensure!(detail, "issue is outside the configured scratch teams");
    let evidence = harness.root.join("evidence").join(key);
    ensure!(
        !evidence.exists(),
        "use a fresh issue; evidence already exists for {key}"
    );
    fs::create_dir_all(&evidence)?;
    let result = async {
        let linear = timeout(
            Duration::from_secs(seconds),
            LinearClient::production(
                workspace,
                workspace_config,
                &harness.root.join("state/herdr-linear-agent"),
            ),
        )
        .await
        .context("Linear credential readiness timed out")??;
        let check = Check {
            harness,
            env,
            key,
            linear,
            herdr: HerdrClient::new(
                harness
                    .root
                    .join("config/herdr/sessions")
                    .join(&harness.session)
                    .join("herdr.sock"),
            ),
            budget: Duration::from_secs(seconds),
        };
        check.scenario(terminal, pause_after_workers).await
    }
    .await;
    let stop = execute(harness, env, &paths::binary()?, &["ticker", "stop"]).await;
    write_json(
        &evidence.join("result.json"),
        &json!({
            "at": files::now(), "key": key, "terminal": terminal,
            "passed": result.is_ok() && stop.is_ok(),
            "failure": result.as_ref().err().map(|e| format!("{e:#}")),
            "ticker_stop_failure": stop.as_ref().err().map(|e| format!("{e:#}")),
            "scope": "real Linear, Herdr, Codex and persisted records; no PR or merge workflow"
        }),
    )?;
    result?;
    stop?;
    println!("PASS {key} ({terminal}); evidence: {}", evidence.display());
    Ok(())
}

pub(super) fn resume(harness: &Harness, key: &str) -> Result<()> {
    crate::run::validate_key(key)?;
    let evidence = harness.root.join("evidence").join(key);
    let pause_path = evidence.join("inspection-pause.json");
    let pause: Value = serde_json::from_slice(
        &fs::read(&pause_path).context("no worker inspection pause is active")?,
    )?;
    let started: Value = serde_json::from_slice(
        &fs::read(evidence.join("workers-started.json"))
            .context("workers-started checkpoint is missing")?,
    )?;
    ensure!(
        started["passed"] == true,
        "workers-started checkpoint did not pass"
    );
    ensure!(
        !evidence.join("result.json").exists(),
        "verification has already finished"
    );
    ensure!(
        pause["waiting"] == true,
        "worker inspection pause is no longer waiting"
    );
    ensure!(
        pause["deadline_unix_ms"]
            .as_u64()
            .is_some_and(|deadline| deadline > now_ms()),
        "worker inspection pause has expired"
    );
    ensure!(
        !evidence.join("resume-after-workers").exists(),
        "worker inspection pause has already been resumed"
    );
    files::write_atomic(&evidence.join("resume-after-workers"), b"resume\n")?;
    println!("Resuming worker inspection for {key}");
    Ok(())
}

impl Check<'_> {
    fn evidence(&self, phase: &str) -> std::path::PathBuf {
        self.harness
            .root
            .join("evidence")
            .join(self.key)
            .join(format!("{phase}.json"))
    }

    async fn observe(&self) -> Result<Observation> {
        let dir = self
            .harness
            .root
            .join("state/herdr-linear-agent/runs")
            .join(self.key);
        let run = Run::load(
            &self.harness.root.join("state/herdr-linear-agent/runs"),
            self.key,
        )?;
        // Atomic record files are read without Run::record's compatibility repair.
        // No file lock is held across external I/O.
        let record: RunRecord = serde_json::from_slice(&fs::read(dir.join(".state/run.json"))?)?;
        let (workspace, identifier) = crate::run::split_key(self.key);
        ensure!(
            record.workspace == workspace && record.identifier == identifier,
            "saved run identity does not match requested key"
        );
        let workers = worker::list_checked(&run)?;
        let raw: Value = self.herdr.call("session.snapshot", json!({})).await?;
        let snapshot = Snapshot::from_value(raw.clone())?;
        ensure!(
            snapshot.skipped == 0,
            "Herdr snapshot skipped {} malformed entries",
            snapshot.skipped
        );
        let issue = self.linear.issue(&record.issue_id).await?;
        ensure!(
            issue.identifier == record.identifier && issue.team.key == record.team_key,
            "Linear identity does not match saved run"
        );
        let evidence = json!({"at": files::now(), "record": record, "workers": workers,
            "snapshot": raw, "linear": {"id": issue.id, "identifier": issue.identifier,
            "team": issue.team.key, "state": issue.state, "delegate_id": issue.delegate_id},
            "ticker": crate::ticker::describe(&self.harness.root.join("state/herdr-linear-agent"))});
        Ok(Observation {
            record,
            workers,
            snapshot,
            evidence,
        })
    }

    async fn wait(
        &self,
        phase: &str,
        predicate: impl Fn(&Observation) -> Result<()>,
    ) -> Result<Observation> {
        wait_for(
            &self.evidence(phase),
            &self.evidence("latest"),
            phase,
            self.budget,
            || async {
                let o = self.observe().await?;
                let evidence = o.evidence.clone();
                Ok((o, evidence))
            },
            predicate,
        )
        .await
    }

    async fn plugin(&self, args: &[&str]) -> Result<()> {
        execute(self.harness, self.env, &paths::binary()?, args).await?;
        if args == ["ticker", "start"] {
            let state = self.harness.root.join("state/herdr-linear-agent");
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match crate::ticker::lock_state(&state) {
                    crate::ticker::LockState::Held(info) => {
                        ensure!(
                            info.version == crate::VERSION,
                            "unexpected ticker version: {info:?}"
                        );
                        write_json(&self.evidence("ticker-ready"), &info)?;
                        break;
                    }
                    crate::ticker::LockState::Free => ensure!(
                        Instant::now() < deadline,
                        "ticker did not acquire its lock: {}",
                        crate::ticker::describe(&state)
                    ),
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    async fn restart_ticker(&self) -> Result<()> {
        self.plugin(&["ticker", "stop"]).await?;
        self.plugin(&["ticker", "start"]).await
    }

    async fn scenario(&self, terminal: &str, pause_after_workers: bool) -> Result<()> {
        let release = self.harness.root.join("release-hooks");
        if release.exists() {
            fs::remove_file(&release)?;
        }
        let baseline = self.herdr.snapshot().await?;
        ensure!(
            baseline.skipped == 0 && baseline.agents.is_empty(),
            "start with no existing agents in the scratch session"
        );
        let baseline_panes: Vec<_> = baseline.panes.keys().cloned().collect();
        self.plugin(&["ticker", "start"]).await?;
        let blocked = self
            .wait("setup-blocked", |o| {
                blocked_setup(o)?;
                ensure!(
                    o.snapshot.panes.len() == baseline_panes.len() + 1
                        && o.snapshot.agents.len() == 1,
                    "unexpected panes or agents during setup"
                );
                for w in &o.workers {
                    ensure!(
                        super::fixture::hook_counts(&self.harness.root, self.key, &w.id)?
                            == [1, 1, 0],
                        "waiting for both blocking hooks to enter"
                    );
                }
                let mut repositories: Vec<_> = o
                    .workers
                    .iter()
                    .map(|w| (w.repo.clone(), w.repo_path.clone()))
                    .collect();
                repositories.sort();
                ensure!(
                    repositories
                        == vec![
                            (
                                "api".into(),
                                self.harness
                                    .root
                                    .join("repositories/api")
                                    .to_string_lossy()
                                    .into_owned()
                            ),
                            (
                                "web".into(),
                                self.harness
                                    .root
                                    .join("repositories/web")
                                    .to_string_lossy()
                                    .into_owned()
                            )
                        ],
                    "expected exactly one api and one web worker in the fixture clones"
                );
                Ok(())
            })
            .await?;
        for w in &blocked.workers {
            self.checkout(w).await?;
        }
        if terminal == "canceled" {
            println!("ACTION: set {} to Canceled in Linear now", self.key);
            let closed = self
                .wait("canceled-cleanup", |o| {
                    terminal_cleanup(o, "canceled")?;
                    ensure!(
                        o.workers
                            .iter()
                            .all(|w| w.agent.pane_id.is_empty() && w.report_hash.is_empty()),
                        "a canceled setup unexpectedly launched a worker"
                    );
                    Ok(())
                })
                .await?;
            self.cleanup_checks(&closed, &baseline_panes).await?;
            return Ok(());
        }
        self.plugin(&["ticker", "stop"]).await?;
        let stopped = self.observe().await?;
        blocked_setup(&stopped)?;
        ensure!(
            stopped
                .workers
                .iter()
                .map(|w| &w.operation)
                .eq(blocked.workers.iter().map(|w| &w.operation)),
            "setup operations changed while stopping"
        );
        write_json(&self.evidence("stopped-setup"), &stopped.evidence)?;
        self.plugin(&["ticker", "start"]).await?;
        self.wait("setup-resumed", |o| {
            blocked_setup(o)?;
            for w in &o.workers {
                let counts = super::fixture::hook_counts(&self.harness.root, self.key, &w.id)?;
                ensure!(
                    counts[0] == 1 && counts[1] == 2,
                    "expected first hook once and interrupted hook twice: {} {counts:?}",
                    w.id
                );
            }
            Ok(())
        })
        .await?;
        files::write_atomic(&release, b"release\n")?;
        println!("ACTION: inspect scratch panes and accept Codex trust dialogs if shown");
        if pause_after_workers {
            let workers = self.wait("workers-started", active_workers).await?;
            self.pause_for_inspection(&workers).await?;
        }
        let started = self
            .wait("workers-reported", |o| {
                active_workers(o)?;
                for w in &o.workers {
                    let report = fs::read(
                        self.harness
                            .root
                            .join("state/herdr-linear-agent/runs")
                            .join(self.key)
                            .join(format!("workers/{}.md", w.id)),
                    )?;
                    ensure!(
                        !w.report_hash.is_empty() && files::sha256_hex(&report) == w.report_hash,
                        "{} report hash does not match persisted copy",
                        w.id
                    );
                    let report = String::from_utf8(report)?;
                    ensure!(
                        !worker::report_section(&report).is_empty(),
                        "{} report has no Report section",
                        w.id
                    );
                }
                Ok(())
            })
            .await?;
        for w in &started.workers {
            self.checkout(w).await?;
            ensure!(
                Path::new(&w.worktree_path).join(".setup-ready").is_file(),
                "setup-ready marker missing"
            );
        }
        let workspace = WorkspaceId(started.record.coordinator.workspace_id.clone());
        let user = self
            .herdr
            .tab_create(
                &workspace,
                self.harness.root.to_str().context("non UTF-8 root")?,
                "operator user tab",
            )
            .await?;
        let first = started.workers.first().context("missing worker")?;
        self.plugin(&["worker", "restart", self.key, &first.id])
            .await?;
        let restarted = self
            .wait("worker-restarted", |o| {
                active_workers(o)?;
                let w = o
                    .workers
                    .iter()
                    .find(|w| w.id == first.id)
                    .context("worker disappeared")?;
                ensure!(
                    w.restarts == first.restarts + 1 && w.agent.pane_id != first.agent.pane_id,
                    "replacement pane not recorded"
                );
                ensure!(
                    !o.snapshot
                        .panes
                        .contains_key(&PaneId(first.agent.pane_id.clone())),
                    "old worker pane still exists"
                );
                ensure!(
                    w.worktree_path == first.worktree_path && w.branch == first.branch,
                    "worker restart changed checkout identity"
                );
                ensure!(
                    o.snapshot.panes.contains_key(&user.pane),
                    "operator pane disappeared"
                );
                for w in &o.workers {
                    ensure!(
                        super::fixture::hook_counts(&self.harness.root, self.key, &w.id)?
                            == [1, 2, 0],
                        "restart repeated setup hooks"
                    );
                }
                Ok(())
            })
            .await?;
        for w in &restarted.workers {
            let status = execute(
                self.harness,
                self.env,
                Path::new("git"),
                &["-C", &w.worktree_path, "status", "--porcelain"],
            )
            .await?;
            ensure!(
                status.trim().is_empty(),
                "checkout {} is unexpectedly dirty: {status}",
                w.id
            );
        }
        let dirty = Path::new(&first.worktree_path).join("retain-untracked-proof.txt");
        ensure!(!dirty.exists(), "dirty fixture already exists");
        files::write_atomic(&dirty, b"harness disposable cleanup proof\n")?;
        println!("ACTION: set {} to Done in Linear now", self.key);
        let retained = self.wait("dirty-cleanup-retained", |o| {
            ensure!(o.record.status == Status::Closed && !o.record.cleanup_pending, "run cleanup not settled");
            ensure!(o.evidence["linear"]["state"]["type"] == "completed", "Linear is not Done");
            for w in &o.workers {
                if w.id == first.id {
                    ensure!(matches!(&w.operation,Some(WorkerOperation::Cleanup(op)) if op.phase == CleanupPhase::RemoveCheckout && op.retained_reason.is_some()), "dirty checkout was not retained with a reason");
                    ensure!(dirty.is_file(), "dirty fixture disappeared");
                } else { ensure!(w.cleanup_completed && path_absent(Path::new(&w.worktree_path)), "clean checkout remains"); }
                ensure!(!o.snapshot.panes.contains_key(&PaneId(w.agent.pane_id.clone())), "owned worker pane remains");
            }
            ensure!(o.snapshot.panes.contains_key(&user.pane), "operator pane disappeared");
            Ok(())
        }).await?;
        self.restart_ticker().await?;
        self.wait("retained-after-restart", |o| {
            let before = retained.workers.iter().find(|w| w.id == first.id).unwrap();
            let after = o
                .workers
                .iter()
                .find(|w| w.id == first.id)
                .context("retained worker disappeared")?;
            ensure!(
                after.operation == before.operation && dirty.is_file(),
                "retained cleanup state did not survive restart"
            );
            Ok(())
        })
        .await?;
        ensure!(
            fs::read(&dirty)? == b"harness disposable cleanup proof\n",
            "dirty fixture changed; refusing removal"
        );
        fs::remove_file(dirty)?;
        self.plugin(&["ticker", "stop"]).await?;
        self.plugin(&["worker", "retry-cleanup", self.key, &first.id])
            .await?;
        let cleaned = self
            .wait("completed-cleanup", |o| terminal_cleanup(o, "completed"))
            .await?;
        let mut preserved = baseline_panes;
        preserved.push(user.pane);
        self.cleanup_checks(&cleaned, &preserved).await
    }

    async fn pause_for_inspection(&self, workers: &Observation) -> Result<()> {
        let evidence = self.harness.root.join("evidence").join(self.key);
        let pause_path = evidence.join("inspection-pause.json");
        let resume_path = evidence.join("resume-after-workers");
        ensure!(
            !resume_path.exists(),
            "stale worker inspection resume signal"
        );
        let deadline_unix_ms = now_ms() + self.budget.as_millis() as u64;
        write_json(
            &pause_path,
            &json!({
                "waiting": true,
                "session": self.harness.session,
                "deadline_unix_ms": deadline_unix_ms,
                "worker_tabs": workers.workers.iter().map(|w| &w.agent.tab_id).collect::<Vec<_>>(),
            }),
        )?;
        println!(
            "PAUSED after both workers started in Herdr session {}. Evidence: {}. Deadline: {} unix ms.",
            self.harness.session,
            evidence.display(),
            deadline_unix_ms
        );
        println!(
            "Inspect the tabs, then run `{} --development-mode development harness resume {} {}` within {}s.",
            shell_quote(&paths::binary()?.display().to_string()),
            shell_quote(&self.harness.root.display().to_string()),
            shell_quote(self.key),
            self.budget.as_secs()
        );
        self.wait("inspection-resumed", |o| {
            active_workers(o)?;
            ensure!(
                resume_path.is_file(),
                "waiting for explicit worker inspection resume"
            );
            Ok(())
        })
        .await?;
        write_json(
            &pause_path,
            &json!({"waiting":false,"resumed":true,"deadline_unix_ms":deadline_unix_ms}),
        )?;
        fs::remove_file(resume_path)?;
        Ok(())
    }

    async fn checkout(&self, w: &Worker) -> Result<()> {
        ensure!(
            Path::new(&w.worktree_path)
                .starts_with(self.harness.root.join("state/herdr-linear-agent/worktrees")),
            "checkout escaped harness"
        );
        let branch = execute(
            self.harness,
            self.env,
            Path::new("git"),
            &["-C", &w.worktree_path, "branch", "--show-current"],
        )
        .await?;
        ensure!(
            branch.trim() == w.branch,
            "actual branch does not match worker record"
        );
        ensure!(
            fs::read(Path::new(&w.repo_path).join(".env.integration"))?
                == fs::read(Path::new(&w.worktree_path).join(".env.integration"))?,
            "copied fixture mismatch"
        );
        ensure!(
            fs::read_link(Path::new(&w.worktree_path).join("node_modules"))?
                == Path::new(&w.repo_path).join("node_modules"),
            "symlink fixture mismatch"
        );
        Ok(())
    }

    async fn cleanup_checks(&self, o: &Observation, preserved: &[PaneId]) -> Result<()> {
        ensure!(
            preserved.iter().all(|p| o.snapshot.panes.contains_key(p)),
            "unrelated user pane was removed"
        );
        ensure!(
            o.snapshot.panes.len() == preserved.len(),
            "owned or unexpected panes remain after cleanup"
        );
        for w in &o.workers {
            let counts = super::fixture::hook_counts(&self.harness.root, self.key, &w.id)?;
            ensure!(
                counts[0] == 1 && counts[2] == 1,
                "{} removal hook count mismatch: {counts:?}",
                w.id
            );
            let registrations = execute(
                self.harness,
                self.env,
                Path::new("git"),
                &["-C", &w.repo_path, "worktree", "list", "--porcelain"],
            )
            .await?;
            ensure!(
                registrations
                    .lines()
                    .filter(|l| l.starts_with("worktree "))
                    .count()
                    == 1,
                "Git retains a linked worktree: {registrations}"
            );
            if !w.report_hash.is_empty() {
                let report = fs::read(
                    self.harness
                        .root
                        .join("state/herdr-linear-agent/runs")
                        .join(self.key)
                        .join(format!("workers/{}.md", w.id)),
                )?;
                ensure!(
                    files::sha256_hex(&report) == w.report_hash,
                    "report lost during cleanup"
                );
            }
        }
        write_json(
            &self.evidence("cleanup-cross-check"),
            &json!({"passed":true,"preserved_panes":preserved,"observation":o.evidence}),
        )
    }
}

fn path_absent(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn blocked_setup(o: &Observation) -> Result<()> {
    ensure!(
        o.record.status == Status::Active && o.workers.len() == 2,
        "expected an active run with two workers"
    );
    for w in &o.workers {
        ensure!(
            matches!(&w.operation,Some(WorkerOperation::Start(op)) if op.phase == StartPhase::Setup && op.setup_cursor == 3),
            "{} has not reached blocked setup",
            w.id
        );
        ensure!(
            w.agent.pane_id.is_empty(),
            "worker pane exists before setup completed"
        );
    }
    ensure!(
        o.snapshot
            .panes
            .contains_key(&PaneId(o.record.coordinator.pane_id.clone())),
        "coordinator pane missing"
    );
    Ok(())
}

fn active_workers(o: &Observation) -> Result<()> {
    ensure!(
        o.record.status == Status::Active && o.workers.len() == 2,
        "expected two active workers"
    );
    for w in &o.workers {
        ensure!(
            w.operation.is_none() && !w.agent.pane_id.is_empty(),
            "{} has not completed placement",
            w.id
        );
        let pane = o
            .snapshot
            .panes
            .get(&PaneId(w.agent.pane_id.clone()))
            .context("saved worker pane is absent from Herdr")?;
        ensure!(
            pane.workspace.0 == o.record.coordinator.workspace_id
                && pane.tab == w.agent.tab_id
                && pane.cwd.as_deref() == Some(&w.worktree_path),
            "saved placement does not match Herdr"
        );
        ensure!(
            o.snapshot
                .agents
                .iter()
                .any(|a| a.pane.0 == w.agent.pane_id
                    && a.name.as_deref() == Some(&w.agent.agent_name)),
            "worker agent missing from Herdr"
        );
        let tab = o
            .snapshot
            .tabs
            .get(&w.agent.tab_id)
            .context("worker tab missing")?;
        ensure!(
            tab.workspace_id == o.record.coordinator.workspace_id && tab.label.starts_with(&w.id),
            "worker tab ownership or label mismatch"
        );
    }
    Ok(())
}

fn terminal_cleanup(o: &Observation, state: &str) -> Result<()> {
    ensure!(
        o.evidence["linear"]["state"]["type"] == state,
        "Linear terminal state not observed"
    );
    ensure!(
        o.record.status == Status::Closed && !o.record.cleanup_pending && o.workers.len() == 2,
        "saved run is not closed and settled"
    );
    for w in &o.workers {
        ensure!(
            w.cleanup_completed
                && w.operation.is_none()
                && path_absent(Path::new(&w.worktree_path)),
            "{} cleanup incomplete",
            w.id
        );
    }
    Ok(())
}

async fn wait_for<T, F, Fut>(
    path: &Path,
    latest: &Path,
    phase: &str,
    budget: Duration,
    mut observe: F,
    predicate: impl Fn(&T) -> Result<()>,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(T, Value)>>,
{
    println!("CHECKPOINT {phase}: waiting up to {}s", budget.as_secs());
    let deadline = Instant::now() + budget;
    let mut last = json!({});
    loop {
        let attempt = timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(30)),
            observe(),
        )
        .await;
        let reason = match attempt {
            Ok(Ok(observation)) => {
                let (observation, evidence) = observation;
                last = evidence;
                match predicate(&observation) {
                    Ok(()) => {
                        write_json(path, &json!({"passed":true,"observation":last}))?;
                        println!("PASS {phase}");
                        return Ok(observation);
                    }
                    Err(error) => format!("{error:#}"),
                }
            }
            Ok(Err(error)) => format!("observation failed: {error:#}"),
            Err(_) => "observation exceeded its deadline".into(),
        };
        write_json(
            latest,
            &json!({"phase":phase,"expected_failure":reason,"observation":last}),
        )?;
        if Instant::now() >= deadline {
            write_json(
                path,
                &json!({"passed":false,"reason":reason,"observation":last}),
            )?;
            bail!(
                "checkpoint {phase} timed out: {reason}; observed state: {}",
                path.display()
            );
        }
        sleep(Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::AgentRecord;

    #[test]
    fn dangling_checkout_symlink_is_not_a_removed_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("checkout");
        assert!(path_absent(&path));
        std::os::unix::fs::symlink(temp.path().join("missing"), &path).unwrap();
        assert!(!path_absent(&path));
    }

    #[tokio::test]
    async fn deadline_retains_the_failed_condition_and_last_observation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failed.json");
        let result = wait_for(
            &path,
            &temp.path().join("latest.json"),
            "worker-report",
            Duration::from_millis(10),
            || async { Ok(((), json!({"workers":[{"id":"w1","report_hash":""}]}))) },
            |_| bail!("w1 report has not arrived"),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("w1 report has not arrived")
        );
        let evidence: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(evidence["passed"], false);
        assert_eq!(evidence["observation"]["workers"][0]["id"], "w1");
    }

    #[tokio::test]
    async fn hung_observation_is_bounded_and_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("failed.json");
        let result: Result<()> = wait_for(
            &path,
            &temp.path().join("latest.json"),
            "snapshot",
            Duration::from_millis(10),
            std::future::pending::<Result<((), Value)>>,
            |_| Ok(()),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("observation exceeded its deadline")
        );
        assert!(path.is_file());
    }

    #[test]
    fn resume_requires_a_live_worker_inspection_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let evidence = temp.path().join("evidence/civitaspo/THLA-1");
        let harness = Harness {
            root: temp.path().to_path_buf(),
            session: "inspection-test".into(),
        };
        assert!(resume(&harness, "civitaspo/THLA-1").is_err());
        assert!(resume(&harness, "invalid").is_err());
        fs::create_dir_all(&evidence).unwrap();
        write_json(
            &evidence.join("workers-started.json"),
            &json!({"passed":true}),
        )
        .unwrap();

        write_json(
            &evidence.join("inspection-pause.json"),
            &json!({"waiting":true,"deadline_unix_ms":now_ms() + 60_000}),
        )
        .unwrap();
        resume(&harness, "civitaspo/THLA-1").unwrap();
        assert!(evidence.join("resume-after-workers").is_file());
        assert!(resume(&harness, "civitaspo/THLA-1").is_err());

        fs::remove_file(evidence.join("resume-after-workers")).unwrap();
        write_json(
            &evidence.join("inspection-pause.json"),
            &json!({"waiting":true,"deadline_unix_ms":now_ms().saturating_sub(1)}),
        )
        .unwrap();
        assert!(resume(&harness, "civitaspo/THLA-1").is_err());
        write_json(
            &evidence.join("inspection-pause.json"),
            &json!({"waiting":false,"deadline_unix_ms":now_ms() + 60_000}),
        )
        .unwrap();
        write_json(&evidence.join("result.json"), &json!({"passed":true})).unwrap();
        assert!(resume(&harness, "civitaspo/THLA-1").is_err());
    }

    #[test]
    fn saved_worker_placement_cannot_pass_without_matching_herdr_agent() {
        let mut workers = vec![];
        for (id, pane, repo) in [("w1", "w1:p2", "api"), ("w2", "w1:p3", "web")] {
            workers.push(Worker {
                id: id.into(),
                worktree_path: format!("/scratch/{repo}"),
                agent: AgentRecord {
                    pane_id: pane.into(),
                    tab_id: format!("tab-{id}"),
                    agent_name: id.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        let snapshot = Snapshot::from_value(json!({"snapshot":{"version":"test","protocol":1,"agents":[],"panes":[
            {"pane_id":"w1:p2","workspace_id":"w1","tab_id":"tab-w1","terminal_id":"t2","cwd":"/scratch/api"},
            {"pane_id":"w1:p3","workspace_id":"w1","tab_id":"tab-w2","terminal_id":"t3","cwd":"/scratch/web"}
        ]}})).unwrap();
        let o = Observation {
            record: RunRecord {
                coordinator: AgentRecord {
                    workspace_id: "w1".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            workers,
            snapshot,
            evidence: json!({}),
        };
        assert!(
            active_workers(&o)
                .unwrap_err()
                .to_string()
                .contains("agent missing")
        );
    }
}
