use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};

use crate::files::write_atomic;
use crate::paths::{self, Env};

const MARKER: &str = ".hla-dev-session";
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
}

#[derive(Args)]
pub struct Launch {
    root: PathBuf,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
}

struct Harness {
    root: PathBuf,
    session: String,
}

pub fn run(env: &Env, command: Command) -> Result<()> {
    match command {
        Command::Init { root, session } => {
            let harness = Harness::init(env, &root, session)?;
            println!(
                "Created {}; configure scratch teams and profiles in config/herdr-linear-agent",
                harness.root.display()
            );
            Ok(())
        }
        Command::Stop { root } => Harness::open(env, &root, false)?.stop(env, &paths::binary()?),
        Command::Plugin(launch) => {
            let harness = Harness::open(env, &launch.root, true)?;
            Err(harness
                .command(env, paths::binary()?)
                .args(launch.args)
                .exec())
            .context("could not execute isolated plugin")
        }
        Command::Herdr(launch) => {
            let harness = Harness::open(env, &launch.root, true)?;
            Err(harness
                .command(env, env.herdr_bin())
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
            Err(harness.command(env, program).args(args).exec())
                .context("could not execute isolated command")
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
            format!("[herdr]\nsession = \"{session}\"\n").as_bytes(),
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

    fn command(&self, env: &Env, program: impl AsRef<OsStr>) -> Process {
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

    fn stop(&self, env: &Env, plugin: &Path) -> Result<()> {
        let mut commands = [
            self.command(env, plugin),
            self.command(env, env.herdr_bin()),
            self.command(env, env.herdr_bin()),
        ];
        commands[0].args(["ticker", "stop"]);
        commands[1].args(["session", "stop", &self.session, "--json"]);
        commands[2].args(["session", "delete", &self.session, "--json"]);
        let mut failed = false;
        for mut child in commands {
            match child.status() {
                Ok(status) if status.success() => {}
                result => {
                    eprintln!(
                        "harness cleanup failed for {}: {result:?}",
                        child.get_program().to_string_lossy()
                    );
                    failed = true;
                }
            }
        }
        ensure!(
            !failed,
            "harness cleanup failed; retained files at {}",
            self.root.display()
        );
        Ok(())
    }
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
        assert_eq!(child.get_current_dir(), Some(harness.root.as_path()));
        let vars: std::collections::HashMap<_, _> = child.get_envs().collect();
        assert_eq!(
            vars[OsStr::new("XDG_CONFIG_HOME")],
            Some(harness.root.join("config").as_os_str())
        );
        assert_eq!(
            vars[OsStr::new("XDG_STATE_HOME")],
            Some(harness.root.join("state").as_os_str())
        );
        assert_eq!(
            vars[OsStr::new("GH_CONFIG_DIR")],
            Some(env.home.join(".config/gh").as_os_str())
        );
        for var in REMOVED_VARS {
            assert_eq!(vars[OsStr::new(var)], None);
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

    #[test]
    fn stop_attempts_every_cleanup_after_a_failure_and_retains_evidence() {
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
        assert!(harness.stop(&env, &fake).is_err());
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "ticker stop\nsession stop scratch-check --json\nsession delete scratch-check --json\n"
        );
        assert!(root.join(MARKER).is_file());
        assert!(!env.home.exists());
    }
}
