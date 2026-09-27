//! The ticker: one background process per state folder. It follows the
//! configured Herdr session, reads and writes Linear, and reconciles every
//! run. `ticker start` (the startup hook, and every agent-facing command)
//! keeps one of this version running; `ticker run` is the process itself.

pub mod reconcile;

use std::fs::{File, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, mpsc, watch};

use crate::VERSION;
use crate::config::Config;
use crate::herdr::{self, Link};
use crate::linear::task::{LinearLevel, LinearTask, Links};
use crate::paths::Ctx;

/// The log stays below this size; trimming keeps its newer half.
pub const LOG_CAP: u64 = 1_000_000;
/// The ticker exits once the configured session was unreachable this long.
pub const UNREACHABLE_FOR: SignedDuration = SignedDuration::from_secs(5 * 60);
const STOP_WAIT: Duration = Duration::from_secs(30);
const SUPERVISE_EVERY: Duration = Duration::from_secs(1);
const SOCKET_RETRY: Duration = Duration::from_secs(5);
const EVENT_QUEUE: usize = 256;

pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.lock")
}

pub fn stop_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.stop")
}

pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join("ticker.log")
}

/// What the lock holder writes into the lock file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub started: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    Free,
    Held(Info),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartAction {
    Nothing,
    Spawn,
    StopThenSpawn,
}

/// A ticker of another version, or one that is being stopped, is replaced.
pub fn decide_start(lock: &LockState, version: &str, stopping: bool) -> StartAction {
    match lock {
        LockState::Free => StartAction::Spawn,
        LockState::Held(info) if info.version == version && !stopping => StartAction::Nothing,
        LockState::Held(_) => StartAction::StopThenSpawn,
    }
}

fn read_info(file: &mut File) -> Info {
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_default()
}

/// Probes the lock without taking it for longer than the probe.
pub fn lock_state(state_dir: &Path) -> LockState {
    let Ok(mut file) = File::open(lock_path(state_dir)) else {
        return LockState::Free;
    };
    match file.try_lock_shared() {
        Ok(()) => LockState::Free,
        Err(TryLockError::WouldBlock) => LockState::Held(read_info(&mut file)),
        Err(TryLockError::Error(_)) => LockState::Free,
    }
}

pub fn describe(state_dir: &Path) -> String {
    match lock_state(state_dir) {
        LockState::Free => "ticker not running".into(),
        LockState::Held(info) => format!(
            "ticker {} running since {} (pid {})",
            info.version, info.started, info.pid
        ),
    }
}

/// Starts the ticker detached unless one of this version runs. Without a
/// config there is nothing to run and nothing is created.
pub async fn start(ctx: &Ctx<'_>) -> Result<()> {
    if !Config::path(&ctx.config_dir()).exists() || !ctx.detached_ticker {
        return Ok(());
    }
    let state_dir = ctx.ensure_state_dir()?;
    let stopping = stop_path(&state_dir).exists();
    match decide_start(&lock_state(&state_dir), VERSION, stopping) {
        StartAction::Nothing => Ok(()),
        StartAction::Spawn => spawn(),
        StartAction::StopThenSpawn => {
            stop(&state_dir).await?;
            spawn()
        }
    }
}

/// `<binary> ticker run` in a new session with null stdio. It is not waited
/// for: Herdr runs at most 32 plugin commands at a time, so the hook that
/// starts it must return at once.
fn spawn() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new(crate::paths::binary()?);
    command
        .args(["ticker", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the
    // parent, as `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("could not start the ticker")?;
    Ok(())
}

/// Asks the running ticker to exit and waits until the lock is free. With a
/// free lock, a stale stop file is removed.
pub async fn stop(state_dir: &Path) -> Result<()> {
    let stop = stop_path(state_dir);
    if lock_state(state_dir) != LockState::Free {
        std::fs::write(&stop, b"")
            .with_context(|| format!("could not write {}", stop.display()))?;
        let deadline = tokio::time::Instant::now() + STOP_WAIT;
        // A child forked at the same instant may share the descriptor for a
        // moment, so the lock is polled.
        while lock_state(state_dir) != LockState::Free {
            if tokio::time::Instant::now() >= deadline {
                bail!("the ticker did not stop within {} s", STOP_WAIT.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let _ = std::fs::remove_file(&stop);
    Ok(())
}

/// Takes the ticker lock and writes this process's info into it. A lock
/// held by another process is retried for a moment, since a probe holds it
/// briefly.
async fn acquire(state_dir: &Path) -> Result<File> {
    let path = lock_path(state_dir);
    let mut file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    for _ in 0..20 {
        match file.try_lock() {
            Ok(()) => {
                let info = Info {
                    version: VERSION.into(),
                    pid: std::process::id(),
                    started: Timestamp::now().to_string(),
                };
                file.set_len(0)?;
                file.rewind()?;
                file.write_all(serde_json::to_string(&info)?.as_bytes())?;
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    bail!("another ticker runs for {}", state_dir.display())
}

/// The ticker's log: one line per event, capped at [`LOG_CAP`].
pub struct Log {
    pub path: PathBuf,
    write: Mutex<()>,
}

impl Log {
    pub fn new(path: PathBuf) -> Log {
        Log {
            path,
            write: Mutex::new(()),
        }
    }

    pub fn line(&self, text: &str) {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let text: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(LOG_CAP as usize / 8)
            .collect();
        let line = format!("{} {text}\n", Timestamp::now());
        let size = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if size + line.len() as u64 > LOG_CAP {
            self.trim();
        }
        if let Ok(mut file) = File::options().create(true).append(true).open(&self.path) {
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Drops the older half of the file at a line boundary.
    fn trim(&self) {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return;
        };
        let half = bytes.len() / 2;
        let start = bytes[half..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| half + i + 1);
        let _ = crate::files::write_atomic(&self.path, &bytes[start..]);
    }
}

/// Whether the session has been down long enough to give up.
pub fn unreachable(link: &Link, now: Timestamp) -> bool {
    !link.connected && now.duration_since(link.since) >= UNREACHABLE_FOR
}

/// `ticker run`: holds the lock until asked to stop or until the configured
/// session has been unreachable for five minutes.
pub async fn run(ctx: &Ctx<'_>) -> Result<()> {
    let config = Config::load(&ctx.config_dir())?;
    let state_dir = ctx.ensure_state_dir()?;
    let _lock = acquire(&state_dir).await?;
    let _ = std::fs::remove_file(stop_path(&state_dir));
    let log = Arc::new(Log::new(log_path(&state_dir)));
    log.line(&format!(
        "ticker {VERSION} started (pid {})",
        std::process::id()
    ));
    match serve(ctx, &config, &state_dir, &log).await {
        Ok(reason) => {
            log.line(&format!("ticker stopped: {reason}"));
            Ok(())
        }
        Err(error) => {
            log.line(&format!("ticker failed: {error:#}"));
            Err(error)
        }
    }
}

async fn serve(ctx: &Ctx<'_>, config: &Config, state_dir: &Path, log: &Arc<Log>) -> Result<String> {
    let Some(socket) = find_socket(ctx, config, state_dir, log).await else {
        return Ok(stop_reason(state_dir).unwrap_or_else(|| unreachable_reason().into()));
    };
    let client = herdr::Client::new(&socket);
    let link = herdr::wake(client.clone());
    let (queries_tx, queries_rx) = watch::channel(Vec::new());
    let (level_tx, level_rx) = watch::channel(LinearLevel::default());
    let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
    let linear_wake = Arc::new(Notify::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let linear = linear(
        config,
        state_dir,
        Links {
            queries: queries_rx,
            level: level_tx,
            events: events_tx,
            wake: linear_wake.clone(),
        },
        log.clone(),
    );
    let reconciler = reconcile::run(reconcile::Inputs {
        ctx,
        config,
        herdr: client,
        socket: socket.to_string_lossy().into_owned(),
        log: log.clone(),
        link: link.clone(),
        level: level_rx,
        events: events_rx,
        queries: queries_tx,
        linear_wake,
        shutdown: shutdown_rx,
    });
    tokio::pin!(linear, reconciler);
    let reason = tokio::select! {
        result = &mut reconciler => return result.map(|()| "the reconciler ended".into()),
        reason = supervise(state_dir, link) => reason,
        () = &mut linear => "the Linear task ended".into(),
    };
    let _ = shutdown_tx.send(true);
    reconciler.await?;
    Ok(reason)
}

fn unreachable_reason() -> &'static str {
    "the configured Herdr session was unreachable for 5 minutes"
}

fn stop_reason(state_dir: &Path) -> Option<String> {
    stop_path(state_dir)
        .exists()
        .then(|| "asked to stop".to_string())
}

/// The configured session's socket. `herdr session list` fails while Herdr
/// is down, so it is retried until the ticker gives up.
async fn find_socket(
    ctx: &Ctx<'_>,
    config: &Config,
    state_dir: &Path,
    log: &Log,
) -> Option<PathBuf> {
    let since = Timestamp::now();
    let mut logged = false;
    loop {
        match herdr::session_socket(&ctx.env.herdr_bin(), config.herdr.session.as_deref()).await {
            Ok(socket) => return Some(socket),
            Err(error) if !logged => {
                log.line(&format!("Herdr is not reachable yet: {error:#}"));
                logged = true;
            }
            Err(_) => {}
        }
        if stop_reason(state_dir).is_some()
            || Timestamp::now().duration_since(since) >= UNREACHABLE_FOR
        {
            return None;
        }
        tokio::time::sleep(SOCKET_RETRY).await;
    }
}

/// Returns why the ticker should exit.
async fn supervise(state_dir: &Path, link: watch::Receiver<Link>) -> String {
    let mut every = tokio::time::interval(SUPERVISE_EVERY);
    loop {
        every.tick().await;
        if let Some(reason) = stop_reason(state_dir) {
            return reason;
        }
        if unreachable(&link.borrow(), Timestamp::now()) {
            return unreachable_reason().into();
        }
    }
}

/// Builds the Linear client from the stored credential, retrying on the
/// intake interval, and runs the Linear task on it.
async fn linear(config: &Config, state_dir: &Path, links: Links, log: Arc<Log>) {
    let lock = state_dir.join("credentials.lock");
    let retry = Duration::from_secs(config.linear.intake_interval_seconds);
    let mut logged = false;
    let client = loop {
        let built = crate::linear::client::Client::production(
            config.linear.client_id.clone(),
            config.linear.callback_port,
            lock.clone(),
        )
        .await;
        match built {
            Ok(client) => break client,
            Err(error) => {
                if !logged {
                    log.line(&format!(
                        "Linear is not available: {error}; run the login action"
                    ));
                    logged = true;
                }
                tokio::select! {
                    () = tokio::time::sleep(retry) => {}
                    () = links.events.closed() => return,
                }
            }
        }
    };
    LinearTask::new(&config.linear)
        .run(&client, links, Timestamp::now, |line| log.line(line))
        .await;
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::paths::Env;
    use crate::process::fake::FakeRunner;

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

    #[tokio::test]
    async fn start_creates_nothing_without_a_config() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx {
            env: &env,
            runner: &runner,
            detached_ticker: true,
        };
        start(&ctx).await.unwrap();
        assert!(!env.state_dir().exists());
    }

    fn wait_until_free(state_dir: &Path) {
        // Another test may fork a child at this instant; until that child
        // execs, it shares the locked descriptor.
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock_state(state_dir) != LockState::Free && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
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
        assert!(describe(state.path()).starts_with("ticker v9 running since"));
        drop(file);
        wait_until_free(state.path());
        assert_eq!(lock_state(state.path()), LockState::Free);
        assert!(describe(state.path()).contains("not running"));
    }

    #[tokio::test]
    async fn stop_with_a_free_lock_removes_a_stale_stop_file() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(stop_path(state.path()), b"").unwrap();
        stop(state.path()).await.unwrap();
        assert!(!stop_path(state.path()).exists());
    }

    #[tokio::test]
    async fn one_ticker_holds_the_lock_with_its_version() {
        let state = tempfile::tempdir().unwrap();
        let lock = acquire(state.path()).await.unwrap();
        match lock_state(state.path()) {
            LockState::Held(info) => {
                assert_eq!(
                    (info.version.as_str(), info.pid),
                    (VERSION, std::process::id())
                );
                assert!(!info.started.is_empty());
            }
            LockState::Free => panic!("lock should be held"),
        }
        assert_eq!(
            decide_start(&lock_state(state.path()), VERSION, false),
            StartAction::Nothing
        );
        let error = acquire(state.path()).await.unwrap_err().to_string();
        assert!(error.starts_with("another ticker runs"), "{error}");
        drop(lock);
        wait_until_free(state.path());
        drop(acquire(state.path()).await.unwrap());
    }

    #[tokio::test]
    async fn a_ticker_of_another_version_is_stopped_before_the_new_one_starts() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().to_path_buf();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        // The old ticker: holds the lock until it sees the stop file.
        let old = std::thread::spawn(move || {
            let mut file = File::options()
                .create(true)
                .write(true)
                .truncate(false)
                .open(lock_path(&dir))
                .unwrap();
            file.lock().unwrap();
            file.write_all(br#"{"version":"0.0.1+old","pid":1}"#)
                .unwrap();
            held_tx.send(()).unwrap();
            while !stop_path(&dir).exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(file);
        });
        held_rx.recv().unwrap();
        let lock = lock_state(state.path());
        assert_eq!(
            decide_start(&lock, VERSION, false),
            StartAction::StopThenSpawn
        );
        stop(state.path()).await.unwrap();
        old.join().unwrap();
        assert_eq!(lock_state(state.path()), LockState::Free);
        assert!(
            !stop_path(state.path()).exists(),
            "the stop file is removed"
        );
        assert_eq!(
            decide_start(&lock_state(state.path()), VERSION, false),
            StartAction::Spawn
        );
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

    #[test]
    fn the_session_counts_as_lost_after_five_minutes_down() {
        let since: Timestamp = "2026-09-25T12:00:00Z".parse().unwrap();
        let link = |connected| Link {
            connected,
            since,
            wakes: 0,
            last_error: None,
        };
        let later = |secs| since + SignedDuration::from_secs(secs);
        assert!(!unreachable(&link(false), later(299)));
        assert!(unreachable(&link(false), later(300)));
        assert!(!unreachable(&link(true), later(3600)));
    }
}
