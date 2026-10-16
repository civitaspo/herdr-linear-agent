use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::json;

use super::{Harness, execute};
use crate::files::{self, shell_quote};
use crate::paths::Env;

const HOOK_WAIT: Duration = Duration::from_secs(300);
const HOOK_EVENTS: [&str; 4] = ["first", "wait-enter", "ready", "removed"];
const FIXTURE_REPOSITORIES: [&str; 2] = ["api", "web"];

#[derive(Serialize)]
struct Profile<'a> {
    kind: &'static str,
    model: &'a str,
    effort: &'static str,
    args: Vec<String>,
    description: &'static str,
}

#[expect(
    clippy::too_many_arguments,
    reason = "these inputs mirror the explicit fixture command options"
)]
pub(super) async fn prepare(
    harness: &Harness,
    env: &Env,
    repository: &str,
    workspace: &str,
    team: &str,
    client_id: &str,
    allowed_user: &str,
    model: &str,
) -> Result<()> {
    ensure!(
        !repository.trim().is_empty()
            && !repository.starts_with('-')
            && !repository.chars().any(char::is_whitespace)
            && (repository.starts_with("https://")
                || repository.starts_with("ssh://")
                || repository.starts_with("git@")
                || repository.starts_with("file://")),
        "repository must be an explicit HTTPS, SSH, or local file clone URL"
    );
    ensure!(
        !(repository.starts_with("https://")
            && repository
                .split_once("//")
                .and_then(|(_, rest)| rest.split('/').next())
                .is_some_and(|authority| authority.contains('@'))),
        "repository URL must not contain embedded credentials"
    );
    ensure!(
        crate::config::valid_name(workspace),
        "invalid Linear workspace name `{workspace}`"
    );
    ensure!(
        crate::run::valid_team_key(team),
        "invalid Linear team key `{team}`"
    );
    ensure!(!client_id.trim().is_empty(), "Linear client ID is required");
    ensure!(
        !allowed_user.trim().is_empty(),
        "allowed Linear user ID is required"
    );
    ensure!(!model.trim().is_empty(), "agent model is required");

    let root = harness
        .root
        .canonicalize()
        .context("cannot resolve harness root")?;
    let repos_dir = root.join("repositories");
    fs::create_dir(&repos_dir).context("cannot create fixture repositories directory")?;
    for name in FIXTURE_REPOSITORIES {
        let path = repos_dir.join(name);
        ensure!(
            fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "fixture checkout already exists: {}",
            path.display()
        );
        execute(
            harness,
            env,
            Path::new("git"),
            &[
                "clone",
                "--",
                repository,
                path.to_str().context("fixture path is not UTF-8")?,
            ],
        )
        .await
        .with_context(|| format!("could not clone {repository} into {}", path.display()))?;

        let env_fixture = path.join(".env.integration");
        let modules = path.join("node_modules");
        ensure!(
            fs::symlink_metadata(&env_fixture)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "clone contains reserved fixture path {}",
            env_fixture.display()
        );
        ensure!(
            fs::symlink_metadata(&modules)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "clone contains reserved fixture path {}",
            modules.display()
        );
        files::write_atomic(&env_fixture, format!("fixture={name}\n").as_bytes())?;
        fs::create_dir(&modules)?;
        files::write_atomic(&modules.join("fixture.txt"), format!("{name}\n").as_bytes())?;

        let excludes = path.join(".git/info/exclude");
        let mut text = fs::read_to_string(&excludes)
            .with_context(|| format!("could not read {}", excludes.display()))?;
        for pattern in [".env.integration", "node_modules", ".setup-ready"] {
            if !text.lines().any(|line| line == pattern) {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(pattern);
                text.push('\n');
            }
        }
        files::write_atomic(&excludes, text.as_bytes())?;
    }

    let binary = crate::paths::binary()?;
    let hook = |step: &str| {
        format!(
            "{} --development-mode development harness hook {} {}",
            shell_quote(&binary.to_string_lossy()),
            shell_quote(&root.to_string_lossy()),
            step
        )
    };
    let repos = FIXTURE_REPOSITORIES
        .into_iter()
        .map(|name| {
            (
                name.to_string(),
                json!({
                    "path": root.join("repositories").join(name).to_string_lossy(),
                    "base": "main",
                    "description": format!("Issue 140 live lifecycle fixture {name}"),
                    "worktree": {
                        "copy": [".env.integration"],
                        "symlink": ["node_modules"],
                        "post_create": [hook("first"), hook("wait")],
                        "post_create_timeout_seconds": 300,
                        "post_remove": [hook("remove")],
                        "post_remove_timeout_seconds": 30
                    }
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let config = json!({
        "development_mode": true,
        "herdr": {"session": harness.session},
        "limits": {"max_runs": 1, "max_workers_per_run": 2, "max_agents": 3},
        "workspaces": {
            (workspace): {
                "client_id": client_id,
                "teams": {
                    (team): {
                        "allowed_user_ids": [allowed_user],
                        "allowed_delegator_ids": [allowed_user],
                        "routing": "testing"
                    }
                }
            }
        },
        "routing": {
            "testing": {
                "coordinators": ["coordinator"],
                "workers": ["worker"],
                "postmortems": ["postmortem"]
            }
        },
        "repositories": repos
    });
    write_toml(&root.join("config/herdr-linear-agent/config.toml"), &config)?;
    fs::create_dir_all(root.join("config/mise"))?;
    write_toml(
        &root.join("config/mise/config.toml"),
        &json!({"tools": {"codex": "0.160.0"}}),
    )?;

    let add_dir = root.to_string_lossy().into_owned();
    for (name, description) in [
        ("coordinator", "Read-only issue 140 lifecycle verification"),
        ("worker", "Read-only issue 140 checkout verification"),
        (
            "postmortem",
            "Read-only review of observed lifecycle evidence",
        ),
    ] {
        let profile = Profile {
            kind: "codex",
            model,
            effort: "low",
            args: vec![
                "-s".into(),
                "workspace-write".into(),
                "--add-dir".into(),
                add_dir.clone(),
                "-a".into(),
                "never".into(),
                "-c".into(),
                "agents.enabled=false".into(),
            ],
            description,
        };
        let dir = root.join("config/herdr-linear-agent/profiles").join(name);
        fs::create_dir_all(&dir)?;
        write_toml(&dir.join("config.toml"), &profile)?;
        files::write_atomic(&dir.join("instructions.md"), instructions(name).as_bytes())?;
    }
    files::write_json(
        &root.join("fixture-manifest.json"),
        &json!({
            "linear": {"workspace": workspace, "team": team},
            "session": harness.session,
            "model": model,
            "agent_cli": {"name": "codex", "version": "0.160.0"},
            "repository_paths": {
                "api": root.join("repositories/api"),
                "web": root.join("repositories/web")
            },
            "worker_checkout_pattern": root.join("state/herdr-linear-agent/worktrees/<workspace>/<TEAM-N>/<worker>-<repository>"),
            "hook_release_file": root.join("release-hooks"),
            "setup_hook_timeout_seconds": HOOK_WAIT.as_secs(),
            "evidence_directory": root.join("evidence"),
            "issue_cases": [
                {
                    "name": "setup-restart-worker-restart-done",
                    "title": "Verify issue 140 setup recovery and shared workspace",
                    "instructions": "Read-only lifecycle check. Start exactly two workers, one in api and one in web. The operator will stop and restart the ticker while setup hooks are blocked, release setup, restart one worker, then mark the issue Done after both reports. Verify your own assigned work and report observed results. Do not modify code, commit, push, or create a PR."
                },
                {
                    "name": "cancel-during-setup",
                    "title": "Verify issue 140 cancellation during worker setup",
                    "instructions": "Read-only lifecycle check. Start exactly two workers, one in api and one in web. The operator will cancel the issue while both setup hooks are blocked. Do not modify code, commit, push, or create a PR."
                }
            ],
            "prerequisites": [
                "Create disposable issues in the configured scratch Linear team and delegate them to this app user.",
                "Keep the Herdr session named in this manifest running while the isolated ticker runs.",
                "The verifier creates release-hooks only after it records the ticker-restart observation.",
                "The repository base branch must be main; fixture files are private ignored paths in each clone."
            ]
        }),
    )?;
    Ok(())
}

fn write_toml(path: &Path, value: &impl Serialize) -> Result<()> {
    let text =
        toml::to_string(value).with_context(|| format!("could not encode {}", path.display()))?;
    files::write_atomic(path, format!("{text}\n").as_bytes())
}

fn instructions(profile: &str) -> &'static str {
    match profile {
        "coordinator" => {
            "This is a read-only integration check. Start exactly two workers with profile worker, one in api and one in web. Ask them to verify their checkout, branch, copied .env.integration, .setup-ready marker, the node_modules directory symlink, and the regular fixture.txt file reached through it, then report actual findings. Do not make code changes, commits, pushes, PRs, extra workers, or Linear state changes. After starting both workers, end your turn. The operator will test ticker restart, worker restart, and terminal-state cleanup. On report notifications, summarize their actual results and end your turn. Stay within this scratch issue and the two configured repositories. Do not invoke native subagents, poll, or sleep. All messages must be English.\n"
        }
        "worker" => {
            "This is an explicitly read-only integration check. Verify your actual checkout path and branch with Git. Read .env.integration and .setup-ready, and resolve node_modules/fixture.txt. Report the actual commands and results to the normal worker report using ## Report and ## Next. Do not change tracked files, commit, push, open PRs, or call GitHub. Stay in your assigned checkout. Do not invoke native subagents, poll, or sleep. All messages must be English.\n"
        }
        _ => {
            "Review only the saved run evidence for this read-only lifecycle check. Report observations and limits. Do not create issues, labels, code, commits, or PRs. All messages must be English.\n"
        }
    }
}

pub(super) fn hook(root: &Path, step: &str) -> Result<()> {
    ensure!(
        matches!(step, "first" | "wait" | "remove"),
        "unknown fixture hook `{step}`"
    );
    let root = root.canonicalize().context("cannot resolve fixture root")?;
    let marker =
        fs::symlink_metadata(root.join(super::MARKER)).context("fixture root marker is missing")?;
    ensure!(
        marker.is_file() && !marker.file_type().is_symlink(),
        "fixture root marker is unsafe"
    );
    let context = HookContext::from_process(&root)?;
    match step {
        "first" => {
            ensure!(
                context.cwd == context.worktree,
                "setup hook cwd is not its worker checkout"
            );
            ensure!(context.worktree.is_dir(), "worker checkout is missing");
            ensure!(
                context.worktree.join(".env.integration").is_file(),
                "copied fixture file is missing"
            );
            ensure!(
                fs::symlink_metadata(context.worktree.join("node_modules"))?
                    .file_type()
                    .is_symlink(),
                "node_modules is not a symlink"
            );
            append_event(&root, &context, "first")
        }
        "wait" => {
            ensure!(
                context.cwd == context.worktree,
                "setup hook cwd is not its worker checkout"
            );
            ensure!(context.worktree.is_dir(), "worker checkout is missing");
            ensure!(
                fs::symlink_metadata(context.worktree.join(".setup-ready"))
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "setup marker already exists before the release gate"
            );
            append_event(&root, &context, "wait-enter")?;
            wait_for_release(&root, HOOK_WAIT)?;
            files::write_atomic(&context.worktree.join(".setup-ready"), b"ready\n")?;
            append_event(&root, &context, "ready")
        }
        "remove" => {
            ensure!(
                context.cwd == context.repo_path,
                "remove hook cwd is not its main checkout"
            );
            ensure!(
                fs::symlink_metadata(&context.worktree)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "worker checkout still exists after removal"
            );
            append_event(&root, &context, "removed")
        }
        _ => unreachable!(),
    }
}

#[derive(Debug)]
struct HookContext {
    run_key: String,
    worker_id: String,
    repo_path: PathBuf,
    worktree: PathBuf,
    cwd: PathBuf,
}

impl HookContext {
    fn from_process(root: &Path) -> Result<Self> {
        let env = Env::from_process()?;
        ensure!(
            env.config_dir().starts_with(root.join("config"))
                && env.state_dir().starts_with(root.join("state")),
            "hook XDG paths are outside the fixture root"
        );
        let value = |name: &str| -> Result<String> {
            std::env::var(name).with_context(|| format!("missing {name} hook environment"))
        };
        for name in ["HLA_BRANCH", "HLA_BASE"] {
            let branch = value(name)?;
            ensure!(
                !branch.is_empty()
                    && !branch.starts_with('-')
                    && !branch.chars().any(char::is_control),
                "invalid {name} hook environment"
            );
        }
        Self::new(
            root,
            &value("HLA_RUN_KEY")?,
            &value("HLA_WORKER_ID")?,
            &value("HLA_REPO")?,
            &value("HLA_REPO_PATH")?,
            &value("HLA_WORKTREE")?,
            &std::env::current_dir()?,
        )
    }

    fn new(
        root: &Path,
        run_key: &str,
        worker_id: &str,
        repo: &str,
        repo_path: &str,
        worktree: &str,
        cwd: &Path,
    ) -> Result<Self> {
        crate::run::validate_key(run_key)?;
        crate::worker::validate_id(worker_id)?;
        ensure!(
            FIXTURE_REPOSITORIES.contains(&repo),
            "unknown fixture repository `{repo}`"
        );
        let root = root.canonicalize().context("cannot resolve fixture root")?;
        let repo_path = PathBuf::from(repo_path)
            .canonicalize()
            .context("cannot resolve fixture repository path")?;
        ensure!(
            repo_path == root.join("repositories").join(repo),
            "repository path is outside the fixture layout"
        );
        let worktree = PathBuf::from(worktree);
        let expected_worktree = root
            .join("state/herdr-linear-agent/worktrees")
            .join(run_key)
            .join(format!("{worker_id}-{repo}"));
        ensure!(
            worktree == expected_worktree,
            "worker checkout path is outside the fixture layout"
        );
        let cwd = cwd
            .canonicalize()
            .context("cannot resolve hook working directory")?;
        ensure!(
            cwd == repo_path || cwd == worktree,
            "hook cwd is outside the fixture repository and checkout"
        );
        Ok(Self {
            run_key: run_key.into(),
            worker_id: worker_id.into(),
            repo_path,
            worktree,
            cwd,
        })
    }
}

fn event_path(root: &Path, context: &HookContext) -> PathBuf {
    root.join("hooks")
        .join(&context.run_key)
        .join(format!("{}.log", context.worker_id))
}

fn append_event(root: &Path, context: &HookContext, event: &str) -> Result<()> {
    ensure!(HOOK_EVENTS.contains(&event), "invalid hook event `{event}`");
    let path = event_path(root, context);
    let hooks = root.join("hooks");
    let workspace = hooks.join(crate::run::split_key(&context.run_key).0);
    for dir in [&hooks, &workspace, path.parent().unwrap()] {
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", dir.display())),
        }
        let metadata = fs::symlink_metadata(dir)?;
        ensure!(
            metadata.is_dir() && !metadata.is_symlink(),
            "unsafe hook log directory: {}",
            dir.display()
        );
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "unsafe hook log file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{event}").with_context(|| format!("could not append {}", path.display()))
}

pub(super) fn hook_counts(root: &Path, key: &str, worker: &str) -> Result<[usize; 3]> {
    crate::run::validate_key(key)?;
    crate::worker::validate_id(worker)?;
    let path = root.join("hooks").join(key).join(format!("{worker}.log"));
    let mut counts = [0; 3];
    match fs::read_to_string(&path) {
        Ok(text) => {
            for line in text.lines() {
                match line {
                    "first" => counts[0] += 1,
                    "wait-enter" => counts[1] += 1,
                    "ready" => {}
                    "removed" => counts[2] += 1,
                    _ => bail!("unknown event `{line}` in {}", path.display()),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    }
    Ok(counts)
}

fn wait_for_release(root: &Path, timeout: Duration) -> Result<()> {
    let release = root.join("release-hooks");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match fs::symlink_metadata(&release) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                return Ok(());
            }
            Ok(_) => bail!(
                "hook release marker is not a regular file: {}",
                release.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect {}", release.display()));
            }
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            bail!(
                "timed out after {} seconds waiting for {}; observed marker absent",
                timeout.as_secs_f64(),
                release.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100).min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_context_accepts_only_expected_fixture_paths() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let repo = root_path.join("repositories/api");
        fs::create_dir_all(&repo).unwrap();
        let worktree =
            root_path.join("state/herdr-linear-agent/worktrees/civitaspo/THLA-37/w1-api");
        fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        fs::create_dir(&worktree).unwrap();
        let context = HookContext::new(
            &root_path,
            "civitaspo/THLA-37",
            "w1",
            "api",
            repo.to_str().unwrap(),
            worktree.to_str().unwrap(),
            &worktree,
        )
        .unwrap();
        assert_eq!(context.cwd, worktree.canonicalize().unwrap());
        assert!(
            HookContext::new(
                &root_path,
                "civitaspo/THLA-37",
                "w1",
                "api",
                "/outside/repository",
                worktree.to_str().unwrap(),
                &worktree,
            )
            .is_err()
        );
    }

    #[test]
    fn events_are_counted_per_worker_and_wait_reports_missing_release() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let repo = root_path.join("repositories/api");
        fs::create_dir_all(&repo).unwrap();
        let worktree =
            root_path.join("state/herdr-linear-agent/worktrees/civitaspo/THLA-37/w1-api");
        fs::create_dir_all(&worktree).unwrap();
        let context = HookContext::new(
            &root_path,
            "civitaspo/THLA-37",
            "w1",
            "api",
            repo.to_str().unwrap(),
            worktree.to_str().unwrap(),
            &worktree,
        )
        .unwrap();
        append_event(&root_path, &context, "first").unwrap();
        append_event(&root_path, &context, "wait-enter").unwrap();
        assert_eq!(
            hook_counts(&root_path, "civitaspo/THLA-37", "w1").unwrap(),
            [1, 1, 0]
        );
        let error = wait_for_release(&root_path, Duration::from_millis(2)).unwrap_err();
        assert!(error.to_string().contains("observed marker absent"));
        files::write_atomic(&root_path.join("release-hooks"), b"release\n").unwrap();
        wait_for_release(&root_path, Duration::from_secs(1)).unwrap();
    }

    #[tokio::test]
    async fn prepare_creates_two_isolated_clones_and_loadable_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("harness");
        let home = temp.path().join("ordinary");
        let env = Env::for_test(&home, &[]);
        let harness = Harness::init(&env, &root, "fixture-test".into()).unwrap();
        let bare = temp.path().join("scratch.git");
        let output = std::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(&bare)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        prepare(
            &harness,
            &env,
            &format!("file://{}", bare.display()),
            "scratch",
            "THLA",
            "test-client-id",
            "test-user-id",
            "test-model",
        )
        .await
        .unwrap();

        let config = crate::config::Config::load(&root.join("config/herdr-linear-agent")).unwrap();
        assert!(config.repositories.contains_key("api"));
        assert!(config.repositories.contains_key("web"));
        for name in FIXTURE_REPOSITORIES {
            let checkout = root.join("repositories").join(name);
            assert!(checkout.join(".env.integration").is_file());
            assert_eq!(
                fs::read_to_string(checkout.join("node_modules/fixture.txt")).unwrap(),
                format!("{name}\n")
            );
            assert!(
                fs::read_to_string(checkout.join(".git/info/exclude"))
                    .unwrap()
                    .lines()
                    .any(|line| line == ".setup-ready")
            );
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("fixture-manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["issue_cases"].as_array().unwrap().len(), 2);
        assert!(root.join("config/mise/config.toml").is_file());
    }
}
