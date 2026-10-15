//! Explicit, credential-free startup check. This shares coordinator priming,
//! arguments and Herdr requests with production, but never sends a prompt.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, timeout};

use crate::config::{Config, Profile};
use crate::herdr::{Client, Herdr, PaneId, Placed, WorkspaceId};
use crate::paths::Env;
use crate::process::{Cmd, RealRunner, Runner};
use crate::run::{Run, RunRecord};
use crate::{agents, coordinator};

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("target").required(true).args(["profile", "kind"])))]
pub struct Options {
    /// An already running, non-default Herdr session dedicated to testing.
    #[arg(long)]
    session: String,
    /// Resolve this manual coordinator profile, including its base profiles.
    #[arg(long)]
    profile: Option<String>,
    /// Start this kind with its default interactive arguments.
    #[arg(long)]
    kind: Option<String>,
    /// Read profiles from this config directory instead of the ordinary config.
    #[arg(long, requires = "profile")]
    config_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(5..=300))]
    timeout_seconds: u64,
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Session {
    name: String,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    running: bool,
    socket_path: PathBuf,
}

fn test_socket(bytes: &[u8], name: &str) -> Result<PathBuf> {
    ensure!(
        !name.trim().is_empty() && name != "default",
        "a non-default test session is required"
    );
    let list: SessionList = serde_json::from_slice(bytes)?;
    let selected = list
        .sessions
        .iter()
        .find(|s| s.name == name)
        .with_context(|| {
            format!("test session `{name}` does not exist; start it explicitly first")
        })?;
    let physical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    ensure!(
        !selected.default
            && !list
                .sessions
                .iter()
                .any(|s| s.default && physical(&s.socket_path) == physical(&selected.socket_path)),
        "the default Herdr session cannot be used for a smoke test"
    );
    ensure!(selected.running, "test session `{name}` is not running");
    Ok(selected.socket_path.clone())
}

/// Positional prompts, headless modes, resume flags and unreviewed switches
/// could invoke a model on startup. Refuse them rather than silently dropping
/// profile arguments. Other kinds need an audited prompt-free launch first.
fn prompt_free(profile: &Profile) -> Result<()> {
    ensure!(
        matches!(
            profile.kind.as_str(),
            "codex" | "claude" | "cursor" | "opencode"
        ),
        "constraint: prompt-free interactive startup is not established for kind `{}`; not executed",
        profile.kind
    );
    for value in [&profile.model, &profile.effort].into_iter().flatten() {
        ensure!(
            !value.is_empty() && !value.starts_with('-'),
            "constraint: model/effort must be a value, not a CLI switch"
        );
    }
    let mut args = profile.args.iter();
    while let Some(flag) = args.next() {
        let takes_value = match profile.kind.as_str() {
            "codex" => matches!(
                flag.as_str(),
                "-s" | "--sandbox" | "-a" | "--ask-for-approval" | "-c" | "--config"
            ),
            "claude" => matches!(flag.as_str(), "--permission-mode"),
            "cursor" => matches!(flag.as_str(), "--allowed-tools" | "--exclude-tools"),
            "opencode" => flag == "--agent",
            _ => false,
        };
        if takes_value {
            let value = args
                .next()
                .with_context(|| format!("constraint: missing value for {flag}"))?;
            ensure!(
                !value.is_empty() && !value.starts_with('-'),
                "constraint: invalid value for {flag}"
            );
            if matches!(flag.as_str(), "-c" | "--config") {
                ensure!(
                    value == "agents.enabled=false"
                        || value
                            .strip_prefix("model_reasoning_effort=")
                            .is_some_and(|v| matches!(
                                v,
                                "minimal" | "low" | "medium" | "high" | "xhigh"
                            )),
                    "constraint: startup config override `{value}` is not audited for prompt-free startup"
                );
            }
        } else {
            let safe = match profile.kind.as_str() {
                "codex" => flag == "--no-alt-screen",
                "claude" => {
                    flag == "--dangerously-skip-permissions" || flag == "--disallowed-tools=Agent"
                }
                "cursor" => flag == "--trust",
                _ => false,
            };
            ensure!(
                safe,
                "constraint: argument `{flag}` is not audited for prompt-free startup; not executed"
            );
        }
    }
    Ok(())
}

fn failure_kind(text: &str) -> &'static str {
    let text = text.to_ascii_lowercase();
    if [
        "permission denied",
        "operation not permitted",
        "eperm",
        "eacces",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        "execution_permission"
    } else if [
        "command not found",
        "not found",
        "no such file",
        "exit code",
        "exited",
        "failed to spawn",
        "no version is set",
        "does not have a version set",
        "not installed",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        "launch_failure"
    } else if ["timeout", "timed out", "not detected"]
        .iter()
        .any(|s| text.contains(s))
    {
        "detection_timeout"
    } else {
        "launch_failure"
    }
}

async fn pane_text(client: &Client, pane: &PaneId) -> String {
    client
        .call::<Value>(
            "pane.read",
            json!({"pane_id": pane, "source": "recent_unwrapped", "lines": 60}),
        )
        .await
        .ok()
        .and_then(|v| v["read"]["text"].as_str().map(str::to_string))
        .unwrap_or_default()
}

async fn attempt(
    client: &Client,
    record: &RunRecord,
    profile: &Profile,
    limit: Duration,
    found: &Placed,
) -> Result<()> {
    let deadline = Instant::now() + limit;
    println!(
        "workspace={} pane={} role=coordinator kind={}",
        found.workspace, found.pane, profile.kind
    );
    let session = if profile.kind == "claude" {
        uuid::Uuid::new_v4().to_string()
    } else {
        String::new()
    };
    let args = agents::start_args(profile, false, &session);
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        remaining > Duration::from_secs(3),
        "detection_timeout: workspace creation exhausted startup limit"
    );
    let started = client
        .agent_start_with_timeout(
            &record.coordinator.agent_name,
            &profile.kind,
            &found.pane,
            &args,
            remaining,
        )
        .await;
    let mut last_seen = String::new();
    loop {
        let snapshot = client.snapshot().await?;
        if let Some(agent) = snapshot.agents.iter().find(|a| a.pane == found.pane) {
            let seen = format!(
                "snapshot: pane={} kind={} name={} status={} interactive_ready={} launch_pending={}",
                agent.pane,
                agent.kind.as_deref().unwrap_or("unknown"),
                agent.name.as_deref().unwrap_or(""),
                agent.status.as_str(),
                agent.interactive_ready,
                agent.launch_pending
            );
            if seen != last_seen {
                println!("{seen}");
                last_seen = seen;
            }
            if agent.kind.as_deref() == Some(profile.kind.as_str())
                && agent.name.as_deref() == Some(&record.coordinator.agent_name)
                && agent.interactive_ready
                && !agent.launch_pending
                && agent.status.is_idle()
                && started.is_ok()
            {
                println!("PASS: coordinator CLI detected and ready; no prompt sent");
                return Ok(());
            }
        }
        if let Err(error) = &started {
            let text = pane_text(client, &found.pane).await;
            let detail = format!("{error}\n{text}");
            bail!("{}: {}", failure_kind(&detail), detail.trim());
        }
        if Instant::now() >= deadline {
            let text = pane_text(client, &found.pane).await;
            let detail = format!(
                "detection timeout: expected coordinator was not ready in its pane\n{text}"
            );
            bail!("{}: {detail}", failure_kind(&detail));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// Recover by the unique temporary cwd as well as the returned workspace ID:
/// workspace.create may have succeeded even when its response was lost.
async fn cleanup<H: Herdr>(herdr: &H, cwd: &Path, placed: Option<&Placed>) -> Result<()> {
    // Even if the initial snapshot fails, close the workspace whose creation
    // was acknowledged. A later snapshot must still verify removal.
    let initial = herdr.snapshot().await;
    let cwd = cwd.to_string_lossy();
    let mut ids: BTreeSet<WorkspaceId> = placed.into_iter().map(|p| p.workspace.clone()).collect();
    ids.extend(initial.as_ref().ok().into_iter().flat_map(|snapshot| {
        snapshot
            .panes
            .values()
            .filter(|p| p.cwd.as_deref() == Some(&cwd) || p.foreground_cwd.as_deref() == Some(&cwd))
            .map(|p| p.workspace.clone())
    }));
    let mut panes: BTreeSet<_> = initial
        .as_ref()
        .ok()
        .into_iter()
        .flat_map(|snapshot| {
            snapshot
                .panes
                .values()
                .filter(|p| ids.contains(&p.workspace))
                .map(|p| p.id.clone())
        })
        .collect();
    panes.extend(placed.into_iter().map(|p| p.pane.clone()));
    let mut errors = Vec::new();
    for id in &ids {
        // Closing the workspace terminates its PTY/process group, including
        // agents whose startup never reached Herdr's recognized-agent state.
        if let Err(error) = herdr.workspace_close(id).await {
            errors.push(format!("{id}: {error}"));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = herdr.snapshot().await?;
        let mut remaining: Vec<_> = snapshot
            .panes
            .values()
            .filter(|p| {
                ids.contains(&p.workspace)
                    || p.cwd.as_deref() == Some(&cwd)
                    || p.foreground_cwd.as_deref() == Some(&cwd)
            })
            .map(|p| p.id.to_string())
            .collect();
        remaining.extend(
            snapshot
                .agents
                .iter()
                .filter(|a| panes.contains(&a.pane))
                .map(|a| format!("agent:{}", a.pane)),
        );
        if remaining.is_empty() {
            println!("cleanup: test workspace and agent removed");
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "cleanup_failure: remaining panes {remaining:?}; close errors {errors:?}"
        );
        sleep(Duration::from_millis(200)).await;
    }
}

pub async fn run(env: &Env, options: Options) -> Result<()> {
    let (name, profile) = match &options.profile {
        Some(name) => {
            let config = Config::load(options.config_dir.as_deref().unwrap_or(&env.config_dir()))?;
            (name.clone(), config.profile(name)?.clone())
        }
        None => {
            let kind = options
                .kind
                .as_deref()
                .context("profile or kind is required")?;
            (
                kind.to_string(),
                Profile {
                    kind: kind.to_string(),
                    model: None,
                    effort: None,
                    args: Vec::new(),
                    description: String::new(),
                    instructions: Vec::new(),
                    env: Default::default(),
                    timeout_seconds: None,
                },
            )
        }
    };
    prompt_free(&profile)?;
    let runner = RealRunner;
    let list = runner
        .run(&Cmd::new(env.herdr_bin(), Duration::from_secs(5)).args(["session", "list", "--json"]))
        .await?;
    ensure!(
        list.success(),
        "could not list Herdr sessions: {}",
        list.error_text()
    );
    let socket = test_socket(list.stdout.as_bytes(), &options.session)?;
    let client = Client::new(&socket);
    let snapshot = client.snapshot().await?;
    ensure!(
        crate::herdr::version_at_least(&snapshot.version, crate::herdr::MIN_VERSION),
        "Herdr {} is too old",
        snapshot.version
    );
    println!(
        "session={} Herdr={} profile={name}",
        options.session, snapshot.version
    );
    let executable = if profile.kind == "cursor" {
        "agent"
    } else {
        &profile.kind
    };
    match runner
        .run(
            &Cmd::new(executable, Duration::from_secs(5))
                .arg("--version")
                .max_output_bytes(4096),
        )
        .await
    {
        Ok(out) if out.success() => println!("version (caller environment): {}", out.stdout.trim()),
        Ok(out) => println!(
            "version unavailable in caller environment: {}; Herdr launch will still be checked",
            out.error_text()
        ),
        Err(error) => println!(
            "version unavailable in caller environment: {error:#}; Herdr launch will still be checked"
        ),
    }
    let temp = tempfile::Builder::new()
        .prefix("hla-agent-smoke-")
        .tempdir()?;
    let mut record = RunRecord {
        workspace: "smoke".into(),
        identifier: "SMOKE-1".into(),
        issue_id: uuid::Uuid::new_v4().to_string(),
        title: "Agent startup smoke test".into(),
        ..RunRecord::default()
    };
    record.coordinator = coordinator::pending_record(&record, &name, &profile.kind);
    let run = Run::create(temp.path(), record.clone())?;
    coordinator::write_priming(
        &run,
        &record,
        &coordinator::binary_command()?,
        &profile.instructions,
    )?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let limit = Duration::from_secs(options.timeout_seconds);
    // Await workspace creation before entering the cancellable launch phase:
    // cleanup must not race an outstanding workspace.create request.
    let placement = client
        .workspace_create(
            &run.canonical_dir().to_string_lossy(),
            &coordinator::workspace_label(&record),
        )
        .await;
    let result = match &placement {
        Ok(placed) => tokio::select! {
            result = timeout(limit + Duration::from_secs(5), attempt(&client, &record, &profile, limit, placed)) =>
                result.unwrap_or_else(|_| Err(anyhow::anyhow!("detection_timeout: startup deadline exceeded"))),
            _ = interrupt.recv() => Err(anyhow::anyhow!("interrupted")),
            _ = terminate.recv() => Err(anyhow::anyhow!("terminated")),
        },
        Err(error) => Err(anyhow::anyhow!("launch_failure: {error}")),
    };
    let cleaned = cleanup(&client, &run.canonical_dir(), placement.as_ref().ok()).await;
    if let Err(error) = cleaned {
        let path = temp.keep();
        bail!(
            "{}; cleanup failed: {error:#}; retained temporary files at {}",
            result
                .err()
                .map_or_else(|| "startup passed".into(), |e| format!("{e:#}")),
            path.display()
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{FakeHerdr, Herdr};

    #[test]
    fn defaults_and_default_socket_aliases_are_rejected() {
        let list = br#"{"sessions":[{"name":"default","default":true,"running":true,"socket_path":"/default.sock"},{"name":"alias","running":true,"socket_path":"/default.sock"},{"name":"smoke","running":true,"socket_path":"/smoke.sock"},{"name":"stopped","socket_path":"/off.sock"}]}"#;
        for name in ["default", "", "alias", "stopped", "missing"] {
            assert!(test_socket(list, name).is_err(), "{name}");
        }
        assert_eq!(
            test_socket(list, "smoke").unwrap(),
            Path::new("/smoke.sock")
        );
    }

    #[test]
    fn profile_prompts_and_unknown_modes_are_refused_before_launch() {
        let mut p = crate::config::tests::sample()
            .profile("deep")
            .unwrap()
            .clone();
        assert!(prompt_free(&p).is_ok());
        for args in [
            vec!["exec", "hello"],
            vec!["--resume", "last"],
            vec!["-c", "startup_prompt=hello"],
            vec!["-s"],
        ] {
            p.args = args.into_iter().map(str::to_string).collect();
            assert!(prompt_free(&p).is_err(), "{:?}", p.args);
        }
        p.kind = "gemini".into();
        p.args.clear();
        assert!(prompt_free(&p).is_err());
        assert_eq!(
            failure_kind("timeout; zsh: command not found: codex"),
            "launch_failure"
        );
        assert_eq!(
            failure_kind("timeout; EPERM opening log"),
            "execution_permission"
        );
        assert_eq!(
            failure_kind("agent not detected: timeout"),
            "detection_timeout"
        );
    }

    #[tokio::test]
    async fn cleanup_recovers_a_workspace_after_a_lost_creation_response() {
        let temp = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(temp.path());
        let cwd = Path::new("/tmp/unique-smoke/run");
        let placed = herdr
            .workspace_create(cwd.to_str().unwrap(), "smoke")
            .await
            .unwrap();
        let unrelated = herdr.workspace_create("/other", "keep").await.unwrap();
        cleanup(&herdr, cwd, None).await.unwrap();
        let snapshot = herdr.snapshot().await.unwrap();
        assert!(!snapshot.panes.contains_key(&placed.pane));
        assert!(snapshot.panes.contains_key(&unrelated.pane));
    }

    #[tokio::test]
    async fn startup_checks_the_exact_agent_without_sending_a_prompt() {
        let server = crate::herdr::FakeHerdrServer::start().await;
        let pane = PaneId("w1:p1".into());
        let placed = Placed {
            pane: pane.clone(),
            workspace: WorkspaceId("w1".into()),
            tab: "w1:t1".into(),
            cwd: "/tmp/smoke".into(),
            worktree_path: None,
        };
        let profile = crate::config::tests::sample()
            .profile("deep")
            .unwrap()
            .clone();
        let mut record = RunRecord::default();
        record.coordinator.agent_name = "smoke-coordinator".into();
        server.answer("agent.start", json!({}));
        server.set_snapshot(
            vec![],
            vec![json!({
                "pane_id": pane, "terminal_id": "term", "workspace_id": "w1", "tab_id": "w1:t1",
                "focused": false, "revision": 0, "agent": profile.kind,
                "name": record.coordinator.agent_name, "agent_status": "idle",
                "interactive_ready": true, "launch_pending": false
            })],
        );
        attempt(
            &server.client(),
            &record,
            &profile,
            Duration::from_secs(5),
            &placed,
        )
        .await
        .unwrap();
        let requests = server.requests();
        assert_eq!(requests[0]["method"], "agent.start");
        assert_eq!(requests[0]["params"]["pane_id"], "w1:p1");
        assert_eq!(
            requests[0]["params"]["args"],
            json!(agents::start_args(&profile, false, ""))
        );
        assert!(requests.iter().all(|r| r["method"] != "agent.prompt"));

        server.fail("agent.start", "launch_failed", "startup failed");
        server.answer(
            "pane.read",
            json!({"type": "pane_read", "read": {"text": "EPERM opening log"}}),
        );
        let error = attempt(
            &server.client(),
            &record,
            &profile,
            Duration::from_secs(5),
            &placed,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().starts_with("execution_permission:"));
        assert!(error.to_string().contains("EPERM opening log"));
    }
}
