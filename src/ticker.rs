//! The ticker: one background loop per state directory.
//!
//! Everything it does is "check on an interval, compare with last time, act".
//! It exits on request through a stop file, never through signals, and when
//! the configured Herdr session has been unreachable for five minutes.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::files;
use crate::paths::Ctx;
use crate::steps::{self, Memory};

pub const TICK: Duration = Duration::from_secs(15);
const STOP_WAIT: Duration = Duration::from_secs(60);
const UNREACHABLE_EXIT: Duration = Duration::from_secs(300);
const LOG_CAP: u64 = 1_000_000;

fn lock_path(state: &Path) -> PathBuf {
    state.join("ticker.lock")
}

fn stop_path(state: &Path) -> PathBuf {
    state.join("ticker.stop")
}

pub fn log_path(state: &Path) -> PathBuf {
    state.join("ticker.log")
}

/// What the lock holder writes into the lock file, for `ticker status` and
/// `doctor`. The pid is for display only; nothing signals it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub started: String,
}

#[derive(Debug, PartialEq)]
pub enum LockState {
    Free,
    Held(Info),
}

/// Probes the lock without keeping it. The file is never created here.
pub fn lock_state(state: &Path) -> LockState {
    let Ok(mut file) = File::options()
        .read(true)
        .write(true)
        .open(lock_path(state))
    else {
        return LockState::Free;
    };
    match file.try_lock() {
        Ok(()) => LockState::Free,
        Err(_) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            LockState::Held(serde_json::from_str(&text).unwrap_or_default())
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum StartAction {
    Spawn,
    Nothing,
    StopThenSpawn,
}

/// The `ticker start` decision. A healthy ticker of the same version is never
/// replaced; a different version, or a stop in progress, is stopped first so
/// the start never ends with no ticker.
pub fn decide_start(lock: &LockState, my_version: &str, stop_file_exists: bool) -> StartAction {
    match lock {
        LockState::Free => StartAction::Spawn,
        LockState::Held(info) if info.version == my_version && !stop_file_exists => {
            StartAction::Nothing
        }
        LockState::Held(_) => StartAction::StopThenSpawn,
    }
}

/// Spawns the detached loop unless there is nothing to do: without a config
/// file the plugin is installed but not set up, and nothing is created.
pub fn start(ctx: &Ctx) -> Result<()> {
    if !ctx.detached_ticker || !Config::path(&ctx.config_dir()).is_file() {
        return Ok(());
    }
    let state = ctx.ensure_state_dir()?;
    match decide_start(
        &lock_state(&state),
        crate::VERSION,
        stop_path(&state).exists(),
    ) {
        StartAction::Nothing => Ok(()),
        StartAction::Spawn => {
            // A leftover stop file would make the new ticker exit at once.
            let _ = std::fs::remove_file(stop_path(&state));
            spawn()
        }
        StartAction::StopThenSpawn => {
            stop(&state)?;
            spawn()
        }
    }
}

unsafe extern "C" {
    fn setsid() -> i32;
}

/// `ticker run`, detached: null stdio and a new session, so it does not die
/// with the process group of whatever started it (an agent's shell tool or a
/// startup hook).
fn spawn() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(crate::paths::binary()?);
    command
        .args(["ticker", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Pane and plugin variables belong to whoever started us: the ticker finds
    // the configured session's socket itself.
    for key in [
        "HERDR_SOCKET_PATH",
        "HERDR_SESSION",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_PLUGIN_CONTEXT_JSON",
    ] {
        command.env_remove(key);
    }
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    command.spawn().context("could not start the ticker")?;
    Ok(())
}

/// Asks the running ticker to exit and waits for the lock to be released.
pub fn stop(state: &Path) -> Result<()> {
    if lock_state(state) == LockState::Free {
        let _ = std::fs::remove_file(stop_path(state));
        return Ok(());
    }
    std::fs::write(stop_path(state), b"")?;
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        if lock_state(state) == LockState::Free {
            let _ = std::fs::remove_file(stop_path(state));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = std::fs::remove_file(stop_path(state));
    bail!(
        "the ticker did not exit within {} seconds",
        STOP_WAIT.as_secs()
    )
}

/// One line per fact, for `ticker status` and the status action.
pub fn describe(state: &Path) -> String {
    match lock_state(state) {
        LockState::Free => "ticker: not running".to_string(),
        LockState::Held(info) => {
            let mut text = format!(
                "ticker: running (version {}, pid {}, since {})",
                info.version, info.pid, info.started
            );
            if info.version != crate::VERSION {
                text.push_str(&format!(
                    "\n  note: this binary is {}; `ticker start` replaces the running one",
                    crate::VERSION
                ));
            }
            text
        }
    }
}

pub struct Log {
    path: PathBuf,
}

impl Log {
    pub fn new(path: PathBuf) -> Self {
        Log { path }
    }

    pub fn line(&self, text: &str) {
        let Ok(mut file) = File::options().create(true).append(true).open(&self.path) else {
            return;
        };
        let _ = writeln!(file, "{} {}", files::now(), text.replace('\n', " "));
        // Size cap: keep the newer half.
        if file.metadata().map(|m| m.len()).unwrap_or(0) > LOG_CAP
            && let Ok(mut reader) = File::open(&self.path)
        {
            let mut tail = Vec::new();
            if reader.seek(SeekFrom::End(-((LOG_CAP / 2) as i64))).is_ok()
                && reader.read_to_end(&mut tail).is_ok()
            {
                let start = tail.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
                let _ = files::write_atomic(&self.path, &tail[start..]);
            }
        }
    }
}

/// The loop. Exits when another ticker holds the lock, when the stop file
/// appears, or when the configured session has been unreachable for five
/// minutes. It keeps running with no active run: it has to keep reading Linear.
pub fn run(ctx: &Ctx) -> Result<()> {
    let state = ctx.ensure_state_dir()?;
    let mut lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(&state))?;
    if lock.try_lock().is_err() {
        return Ok(());
    }
    let info = Info {
        version: crate::VERSION.to_string(),
        pid: std::process::id(),
        started: files::now(),
    };
    lock.set_len(0)?;
    lock.write_all(serde_json::to_string_pretty(&info)?.as_bytes())?;
    lock.flush()?;

    let log = Log::new(log_path(&state));
    log.line(&format!(
        "ticker {} started (pid {})",
        info.version, info.pid
    ));
    let mut last_reachable = Instant::now();
    let mut memory = Memory::default();
    loop {
        if stop_path(&state).exists() {
            log.line("stop file found; exiting");
            return Ok(());
        }
        match steps::tick(ctx, &mut memory, &log) {
            Ok(()) => last_reachable = Instant::now(),
            Err(error) => {
                log.line(&format!("{error:#}"));
                if last_reachable.elapsed() > UNREACHABLE_EXIT {
                    log.line("the configured Herdr session has been unreachable for five minutes; exiting");
                    return Ok(());
                }
            }
        }
        // Sleep in short slices so a stop request is honoured promptly.
        let wake = Instant::now() + TICK;
        while Instant::now() < wake {
            if stop_path(&state).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use crate::runner::fake::FakeRunner;

    fn held(version: &str) -> LockState {
        LockState::Held(Info {
            version: version.into(),
            ..Info::default()
        })
    }

    #[test]
    fn start_decisions() {
        assert_eq!(
            decide_start(&LockState::Free, "v1", false),
            StartAction::Spawn
        );
        assert_eq!(
            decide_start(&LockState::Free, "v1", true),
            StartAction::Spawn
        );
        assert_eq!(decide_start(&held("v1"), "v1", false), StartAction::Nothing);
        assert_eq!(
            decide_start(&held("v0"), "v1", false),
            StartAction::StopThenSpawn
        );
        // A stop in progress: finish it, then spawn.
        assert_eq!(
            decide_start(&held("v1"), "v1", true),
            StartAction::StopThenSpawn
        );
    }

    #[test]
    fn start_creates_nothing_without_a_config() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: true,
        };
        start(&ctx).unwrap();
        assert!(!env.state_dir().exists());
    }

    #[test]
    fn lock_probe_sees_a_holder_and_its_version() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(lock_state(state.path()), LockState::Free);
        let mut file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path(state.path()))
            .unwrap();
        file.lock().unwrap();
        file.write_all(br#"{"version":"v9","pid":1}"#).unwrap();
        match lock_state(state.path()) {
            LockState::Held(info) => assert_eq!(info.version, "v9"),
            LockState::Free => panic!("lock should be held"),
        }
        drop(file);
        // Another test may fork a child at this instant; until that child execs,
        // it shares the locked descriptor. Real callers poll too (`ticker stop`).
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock_state(state.path()) != LockState::Free && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock_state(state.path()), LockState::Free);
        assert!(describe(state.path()).contains("not running"));
    }

    #[test]
    fn stop_with_a_free_lock_removes_a_stale_stop_file() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(stop_path(state.path()), b"").unwrap();
        stop(state.path()).unwrap();
        assert!(!stop_path(state.path()).exists());
    }

    #[test]
    fn log_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::new(dir.path().join("log"));
        let long = "x".repeat(10_000);
        for _ in 0..150 {
            log.line(&long);
        }
        let size = std::fs::metadata(&log.path).unwrap().len();
        assert!(size <= LOG_CAP, "{size}");
        assert!(size > LOG_CAP / 4);
    }
}
