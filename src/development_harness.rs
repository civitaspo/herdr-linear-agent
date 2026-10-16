use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::files::write_atomic;
use crate::paths::{self, Env};
use crate::process::{Cmd, RealRunner, Runner};

mod fixture;
mod lifecycle;
mod ordinary;

const MARKER: &str = ".hla-development-session";
const REMOVED_VARS: &[&str] = &[
    "HERDR_PLUGIN_CONFIG_DIR",
    "HERDR_PLUGIN_STATE_DIR",
    "HERDR_CONFIG_PATH",
    "HERDR_SOCKET_PATH",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
];

#[derive(Subcommand)]
pub enum Command {
    /// Create a new private root under an existing parent. Starts no processes.
    Init {
        root: PathBuf,
        #[arg(long)]
        session: String,
    },
    /// Run this plugin binary in the isolated environment.
    Plugin(Launch),
    /// Run Herdr with the root's dedicated session.
    Herdr(Launch),
    /// Run a command in the isolated environment. Use absolute executable paths.
    Exec(Launch),
    /// Stop the isolated ticker and delete its session, retaining all files.
    Stop { root: PathBuf },
    /// Clone the configured scratch repository twice and write isolated test profiles.
    Fixture {
        root: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        team: String,
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        allowed_user: String,
        #[arg(long, default_value = "gpt-6-luna")]
        model: String,
    },
    /// Run one deterministic post-create or post-remove hook step.
    Hook { root: PathBuf, step: String },
    /// Run one bounded live lifecycle verification against a delegated scratch issue.
    Verify {
        root: PathBuf,
        key: String,
        #[arg(long)]
        terminal: String,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[arg(long)]
        pause_after_workers: bool,
    },
    /// Resume a verifier paused for visual inspection after both workers started.
    Resume { root: PathBuf, key: String },
    /// Record and stop the ordinary ticker before live scratch verification.
    OrdinarySuspend {
        root: PathBuf,
        #[arg(long)]
        plugin: PathBuf,
    },
    /// Verify ordinary state and restart the ticker recorded by ordinary-suspend.
    OrdinaryRestore { root: PathBuf },
}

#[derive(Args)]
pub struct Launch {
    root: PathBuf,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

pub(super) struct Harness {
    root: PathBuf,
    session: String,
}

pub async fn run(env: &Env, command: Command) -> Result<()> {
    match command {
        Command::Init { root, session } => {
            let harness = Harness::init(env, &root, session)?;
            println!(
                "Created {}; configure scratch teams and profiles in config/herdr-linear-agent",
                harness.root.display()
            );
            Ok(())
        }
        Command::Stop { root } => {
            Harness::open(env, &root, false)?
                .stop(env, &paths::binary()?)
                .await
        }
        Command::Plugin(launch) => {
            let harness = Harness::open(env, &launch.root, true)?;
            Err(harness
                .raw_command(env, paths::binary()?)
                .args(launch.args)
                .exec())
            .context("could not execute isolated plugin")
        }
        Command::Herdr(launch) => {
            let harness = Harness::open(env, &launch.root, true)?;
            Err(harness
                .raw_command(env, env.herdr_bin())
                .args(["--session", &harness.session])
                .args(launch.args)
                .exec())
            .context("could not execute isolated Herdr")
        }
        Command::Exec(launch) => {
            let harness = Harness::open(env, &launch.root, true)?;
            let (program, args) = launch
                .args
                .split_first()
                .context("exec requires a command")?;
            Err(harness.raw_command(env, program).args(args).exec())
                .context("could not execute isolated command")
        }
        Command::Fixture {
            root,
            repository,
            workspace,
            team,
            client_id,
            allowed_user,
            model,
        } => {
            let harness = Harness::open(env, &root, true)?;
            crate::development_harness::fixture::prepare(
                &harness,
                env,
                &repository,
                &workspace,
                &team,
                &client_id,
                &allowed_user,
                &model,
            )
            .await
        }
        Command::Hook { root, step } => {
            // Hooks already inherit isolated XDG paths; their own validation
            // checks the marker and exact fixture paths without ordinary-path
            // overlap checks against that same isolated environment.
            crate::development_harness::fixture::hook(&root, &step)
        }
        Command::Verify {
            root,
            key,
            terminal,
            timeout_seconds,
            pause_after_workers,
        } => {
            let harness = Harness::open(env, &root, true)?;
            crate::development_harness::lifecycle::verify(
                &harness,
                env,
                &key,
                &terminal,
                timeout_seconds,
                pause_after_workers,
            )
            .await
        }
        Command::Resume { root, key } => {
            let harness = Harness::open(env, &root, true)?;
            crate::development_harness::lifecycle::resume(&harness, &key)
        }
        Command::OrdinarySuspend { root, plugin } => {
            let harness = Harness::open(env, &root, true)?;
            crate::development_harness::ordinary::suspend(&harness, env, &plugin)
                .await
                .map(|message| println!("{message}"))
        }
        Command::OrdinaryRestore { root } => {
            let harness = Harness::open(env, &root, false)?;
            crate::development_harness::ordinary::restore(&harness, env)
                .await
                .map(|message| println!("{message}"))
        }
    }
}

impl Harness {
    fn init(env: &Env, root: &Path, session: String) -> Result<Self> {
        valid_session(&session)?;
        let root = resolve(root)?;
        protect_ordinary(env, &root)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .with_context(|| {
                format!(
                    "cannot create new harness {}; its parent must exist",
                    root.display()
                )
            })?;
        for name in [
            "config/herdr-linear-agent",
            "config/herdr",
            "state/herdr-linear-agent",
        ] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join(name))?;
        }
        write_atomic(&root.join(MARKER), format!("{session}\n").as_bytes())?;
        write_atomic(
            &root.join("config/herdr-linear-agent/config.toml"),
            format!("development_mode = true\n\n[herdr]\nsession = \"{session}\"\n").as_bytes(),
        )?;
        let directory = root.join("worktrees");
        let directory = directory.to_str().context("root must be UTF-8")?;
        let mut config = format!(
            "[worktrees]\ndirectory = {}\n",
            toml::Value::String(directory.into())
        );
        if env.var("HERDR_ENV") == Some("1") {
            config.push_str("\n[experimental]\nallow_nested = true\n");
        }
        write_atomic(&root.join("config/herdr/config.toml"), config.as_bytes())?;
        Ok(Self { root, session })
    }

    fn open(env: &Env, root: &Path, check_config: bool) -> Result<Self> {
        let root = root.canonicalize().context("cannot open harness root")?;
        protect_ordinary(env, &root)?;
        for name in [
            "config",
            "state",
            "config/herdr",
            "config/herdr-linear-agent",
            "state/herdr-linear-agent",
        ] {
            let metadata = fs::symlink_metadata(root.join(name))?;
            ensure!(metadata.is_dir(), "unsafe harness directory: {name}");
        }
        ensure!(
            fs::symlink_metadata(root.join(MARKER))?.is_file(),
            "unsafe harness marker"
        );
        for name in [
            "config/herdr/config.toml",
            "config/herdr-linear-agent/config.toml",
            "worktrees",
        ] {
            match fs::symlink_metadata(root.join(name)) {
                Ok(metadata) => ensure!(!metadata.is_symlink(), "unsafe harness symlink: {name}"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let session = fs::read_to_string(root.join(MARKER))?.trim().to_string();
        valid_session(&session)?;
        if check_config {
            let plugin: toml::Value = toml::from_str(&fs::read_to_string(
                root.join("config/herdr-linear-agent/config.toml"),
            )?)?;
            ensure!(
                plugin
                    .get("herdr")
                    .and_then(|h| h.get("session"))
                    .and_then(toml::Value::as_str)
                    == Some(&session),
                "config must explicitly name the harness session"
            );
            let herdr: toml::Value =
                toml::from_str(&fs::read_to_string(root.join("config/herdr/config.toml"))?)?;
            let worktrees = herdr
                .get("worktrees")
                .and_then(|w| w.get("directory"))
                .and_then(toml::Value::as_str);
            ensure!(
                worktrees.map(Path::new) == Some(root.join("worktrees").as_path()),
                "Herdr worktrees must remain inside the harness"
            );
        }
        Ok(Self { root, session })
    }

    fn raw_command(&self, env: &Env, program: impl AsRef<OsStr>) -> Process {
        let mut child = Process::new(program);
        child
            .current_dir(&self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"));
        let gh_config = env
            .var("GH_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| env.config_dir().parent().unwrap().join("gh"));
        child.env("GH_CONFIG_DIR", gh_config);
        for var in REMOVED_VARS {
            child.env_remove(var);
        }
        child
    }

    fn command(&self, env: &Env, program: impl AsRef<OsStr>) -> Cmd {
        let raw = self.raw_command(env, program);
        let mut child = Cmd::new(raw.get_program().to_string_lossy(), Duration::from_secs(60))
            .cwd(self.root.clone())
            .max_output_bytes(1024 * 1024);
        for arg in raw.get_args() {
            child = child.arg(arg.to_string_lossy());
        }
        for (key, value) in raw.get_envs() {
            match value {
                Some(value) => child = child.env(key, value),
                None => child = child.env_remove(key),
            }
        }
        child
    }

    async fn stop(&self, env: &Env, plugin: &Path) -> Result<()> {
        #[derive(Debug, Serialize)]
        struct StopResult {
            commands: Vec<(String, String)>,
            ticker_free: bool,
            ticker_observation: String,
            session_absent: bool,
            session_observation: String,
        }
        let mut results = Vec::new();
        for (program, args) in [
            (
                plugin.to_string_lossy().into_owned(),
                vec!["ticker", "stop"],
            ),
            (
                env.herdr_bin(),
                vec!["session", "stop", &self.session, "--json"],
            ),
            (
                env.herdr_bin(),
                vec!["session", "delete", &self.session, "--json"],
            ),
        ] {
            let label = format!("{program} {}", args.join(" "));
            let result = execute(self, env, Path::new(&program), &args).await;
            results.push((label, result.unwrap_or_else(|e| format!("ERROR: {e:#}"))));
        }
        let ticker_free = matches!(
            crate::ticker::lock_state(&self.root.join("state/herdr-linear-agent")),
            crate::ticker::LockState::Free
        );
        let ticker_observation =
            crate::ticker::describe(&self.root.join("state/herdr-linear-agent"));
        let list = execute(
            self,
            env,
            Path::new(&env.herdr_bin()),
            &["session", "list", "--json"],
        )
        .await;
        let (session_absent, session_observation) = match list {
            Ok(text) => (
                session_absent(&text, &self.session),
                format!("session list output: {}", text.trim()),
            ),
            Err(error) => (false, format!("session list failed: {error:#}")),
        };
        let evidence = StopResult {
            commands: results,
            ticker_free,
            ticker_observation,
            session_absent,
            session_observation,
        };
        write_atomic(
            &self.root.join("stop-result.json"),
            &serde_json::to_vec_pretty(&evidence)?,
        )?;
        ensure!(
            ticker_free && session_absent,
            "harness cleanup incomplete; observed {evidence:?}; files retained at {}",
            self.root.display()
        );
        Ok(())
    }
}

pub(super) async fn execute(
    harness: &Harness,
    env: &Env,
    program: &Path,
    args: &[&str],
) -> Result<String> {
    let cmd = harness.command(env, program).args(args.iter().copied());
    let output = RealRunner.run(&cmd).await?;
    ensure!(
        output.success(),
        "{} failed: {} (stdout: {}; stderr: {})",
        std::iter::once(cmd.program.as_str())
            .chain(cmd.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        output.error_text(),
        output.stdout.trim(),
        output.stderr.trim()
    );
    Ok(output.stdout)
}

fn session_absent(list: &str, session: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(list)
        .ok()
        .and_then(|v| {
            v.get("sessions")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .is_some_and(|sessions| {
            !sessions
                .iter()
                .any(|s| s.get("name").and_then(serde_json::Value::as_str) == Some(session))
        })
}

fn valid_session(session: &str) -> Result<()> {
    ensure!(
        !session.is_empty()
            && session != "default"
            && !session.starts_with('-')
            && session
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c)),
        "use a non-default test session containing only a-z, 0-9, _ or -"
    );
    Ok(())
}

fn protect_ordinary(env: &Env, root: &Path) -> Result<()> {
    for protected in [env.config_dir(), env.state_dir()] {
        let protected = resolve(&protected)?;
        ensure!(
            !root.starts_with(&protected) && !protected.starts_with(root),
            "harness root overlaps ordinary plugin configuration or state"
        );
    }
    Ok(())
}

fn resolve(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("path has no existing ancestor")?;
            Ok(resolve(parent)?.join(path.file_name().context("path has no file name")?))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[tokio::test]
    async fn hooks_use_their_own_validation_when_xdg_already_points_inside_root() {
        let temp = tempfile::tempdir().unwrap();
        let ordinary = Env::for_test(&temp.path().join("ordinary"), &[]);
        let root = temp.path().join("scratch");
        Harness::init(&ordinary, &root, "scratch-check".into()).unwrap();
        let isolated = Env::for_test(
            &ordinary.home,
            &[
                ("XDG_CONFIG_HOME", root.join("config").to_str().unwrap()),
                ("XDG_STATE_HOME", root.join("state").to_str().unwrap()),
            ],
        );
        let error = run(
            &isolated,
            Command::Hook {
                root,
                step: "invalid".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("unknown fixture hook"));
    }

    #[test]
    fn isolated_commands_keep_the_original_environment_contract() {
        let temp = tempfile::tempdir().unwrap();
        let env = Env::for_test(&temp.path().join("ordinary"), &[("HERDR_ENV", "1")]);
        let root = temp.path().join("scratch");
        Harness::init(&env, &root, "scratch-check".into()).unwrap();
        let harness = Harness::open(&env, &root, true).unwrap();
        for name in ["", "state/herdr-linear-agent", "config/herdr"] {
            assert_eq!(
                fs::metadata(root.join(name)).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let child = harness.command(&env, "/bin/pwd");
        assert_eq!(child.cwd.as_deref(), Some(harness.root.as_path()));
        let vars: std::collections::HashMap<OsString, OsString> = child
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(
            vars.get(OsStr::new("XDG_CONFIG_HOME")),
            Some(&harness.root.join("config").into_os_string())
        );
        assert_eq!(
            vars.get(OsStr::new("XDG_STATE_HOME")),
            Some(&harness.root.join("state").into_os_string())
        );
        assert_eq!(
            vars.get(OsStr::new("GH_CONFIG_DIR")),
            Some(&env.home.join(".config/gh").into_os_string())
        );
        for var in REMOVED_VARS {
            assert!(child.env_remove.contains(&OsString::from(var)));
        }
        assert!(!vars.contains_key(OsStr::new("HOME")));
        let herdr: toml::Value =
            toml::from_str(&fs::read_to_string(root.join("config/herdr/config.toml")).unwrap())
                .unwrap();
        assert_eq!(herdr["experimental"]["allow_nested"].as_bool(), Some(true));
        assert!(Harness::init(&env, &root, "scratch-check".into()).is_err());
        assert!(root.join(MARKER).is_file());
    }

    #[test]
    fn ordinary_paths_and_invalid_sessions_are_rejected_before_creation() {
        let temp = tempfile::tempdir().unwrap();
        let env = Env::for_test(&temp.path().join("ordinary"), &[]);
        let root = temp.path().join("scratch");
        for session in ["", "default", "--all", "bad/name", "CAPS", "has space"] {
            assert!(Harness::init(&env, &root, session.into()).is_err());
            assert!(!root.exists());
        }
        for protected in [env.config_dir(), env.state_dir()] {
            assert!(Harness::init(&env, &protected.join("test"), "scratch".into()).is_err());
            assert!(!protected.exists());
        }
        assert!(Harness::init(&env, &env.home, "scratch".into()).is_err());
        fs::create_dir(&env.home).unwrap();
        let alias = temp.path().join("alias");
        symlink(&env.home, &alias).unwrap();
        assert!(
            Harness::init(
                &env,
                &alias.join(".config/herdr-linear-agent/test"),
                "scratch".into()
            )
            .is_err()
        );
    }

    #[test]
    fn config_damage_blocks_launch_but_allows_cleanup_and_symlinks_block_both() {
        let temp = tempfile::tempdir().unwrap();
        let env = Env::for_test(&temp.path().join("ordinary"), &[]);
        let root = temp.path().join("scratch");
        Harness::init(&env, &root, "scratch-check".into()).unwrap();
        let herdr = root.join("config/herdr/config.toml");
        let original = fs::read(&herdr).unwrap();
        write_atomic(&herdr, b"[worktrees]\ndirectory = '/outside'\n").unwrap();
        assert!(Harness::open(&env, &root, true).is_err());
        assert!(Harness::open(&env, &root, false).is_ok());
        write_atomic(&herdr, &original).unwrap();
        write_atomic(
            &root.join("config/herdr-linear-agent/config.toml"),
            b"[herdr]\nsession = 'default'\n",
        )
        .unwrap();
        assert!(Harness::open(&env, &root, true).is_err());
        assert!(Harness::open(&env, &root, false).is_ok());
        let state = root.join("state/herdr-linear-agent");
        fs::remove_dir(&state).unwrap();
        symlink(&env.home, &state).unwrap();
        assert!(Harness::open(&env, &root, true).is_err());
        assert!(Harness::open(&env, &root, false).is_err());
        assert!(!env.home.exists());
    }

    #[tokio::test]
    async fn stop_attempts_every_cleanup_after_a_failure_and_retains_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("fake");
        let log = temp.path().join("calls");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$1\" in ticker) exit 7 ;; esac\n",
                crate::files::shell_quote(log.to_str().unwrap())
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let env = Env::for_test(
            &temp.path().join("ordinary"),
            &[("HERDR_BIN_PATH", fake.to_str().unwrap())],
        );
        let root = temp.path().join("scratch");
        Harness::init(&env, &root, "scratch-check".into()).unwrap();
        write_atomic(
            &root.join("config/herdr-linear-agent/config.toml"),
            b"broken [\n",
        )
        .unwrap();
        let harness = Harness::open(&env, &root, false).unwrap();
        assert!(harness.stop(&env, &fake).await.is_err());
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "ticker stop\nsession stop scratch-check --json\nsession delete scratch-check --json\nsession list --json\n"
        );
        assert!(root.join(MARKER).is_file());
        assert!(!env.home.exists());
    }
}
