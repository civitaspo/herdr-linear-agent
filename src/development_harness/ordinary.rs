use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::development_harness::Harness;
use crate::files::{self, sha256_hex};
use crate::paths::Env;
use crate::process::{Cmd, RealRunner, Runner};
use crate::ticker::{self, Info, LockState};

const RECORD: &str = "ordinary.json";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(40);
const START_WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Custody {
    plugin: PathBuf,
    plugin_sha256: String,
    version: String,
    stopped_ticker: Info,
    /// Captured only after the ordinary lock is free, avoiding races with the ticker.
    baseline: Option<TreeHashes>,
    restore_checked: Option<TreeHashes>,
    restored_ticker: Option<Info>,
    status: Status,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Status {
    Prepared,
    Suspended,
    Restored,
}

type TreeHashes = BTreeMap<String, String>;

/// Stop the exact installed plugin after capturing its identity and the
/// ordinary config/run/supervisor files. Repeated calls while suspended are
/// safe and leave the original evidence intact.
pub async fn suspend(harness: &Harness, env: &Env, plugin: &Path) -> Result<String> {
    let path = record_path(harness);
    let plugin = plugin
        .canonicalize()
        .context("cannot resolve the installed ordinary plugin binary")?;
    ensure!(
        plugin.is_absolute(),
        "ordinary plugin path must be absolute"
    );
    ensure!(plugin.is_file(), "ordinary plugin is not a regular file");
    let mut custody = if let Some(existing) = read_custody(&path)? {
        ensure!(
            existing.plugin == plugin,
            "ordinary custody record names another binary"
        );
        match existing.status {
            Status::Suspended => {
                return Ok(format!(
                    "ordinary ticker remains suspended; evidence: {}",
                    path.display()
                ));
            }
            Status::Restored => {
                bail!("ordinary ticker was already restored; use a fresh harness for another check")
            }
            Status::Prepared => existing,
        }
    } else {
        let plugin_bytes = fs::read(&plugin).context("cannot hash ordinary plugin binary")?;
        let plugin_sha256 = sha256_hex(&plugin_bytes);
        let version = command(env, &plugin, ["--version"]).await?;
        let version = version
            .trim()
            .strip_prefix("herdr-linear-agent ")
            .unwrap_or_default()
            .to_owned();
        ensure!(
            !version.is_empty(),
            "ordinary plugin returned an empty version"
        );

        let LockState::Held(stopped_ticker) = ticker::lock_state(&env.state_dir()) else {
            bail!("ordinary ticker is not running; refusing to claim custody");
        };
        ensure!(
            stopped_ticker.version == version,
            "ordinary ticker version {} does not match requested binary {}",
            stopped_ticker.version,
            version
        );

        let custody = Custody {
            plugin: plugin.clone(),
            plugin_sha256,
            version,
            stopped_ticker,
            baseline: None,
            restore_checked: None,
            restored_ticker: None,
            status: Status::Prepared,
            errors: Vec::new(),
        };
        files::write_json(&path, &custody)?;
        custody
    };

    let current_binary_hash = fs::read(&plugin).map(|bytes| sha256_hex(&bytes));
    if !current_binary_hash
        .as_ref()
        .is_ok_and(|hash| hash == &custody.plugin_sha256)
    {
        let error = "ordinary binary changed after custody was prepared; refusing to run a different binary";
        custody.errors.push(error.into());
        files::write_json(&path, &custody)?;
        bail!("{error}; evidence retained in {}", path.display());
    }
    if let LockState::Held(info) = ticker::lock_state(&env.state_dir()) {
        if info.version != custody.version || info.pid != custody.stopped_ticker.pid {
            let error = format!(
                "ordinary ticker identity changed before stop: expected {:?}, observed {:?}",
                custody.stopped_ticker, info
            );
            custody.errors.push(error.clone());
            files::write_json(&path, &custody)?;
            bail!("{error}; evidence retained in {}", path.display());
        }
        if let Err(error) = command(env, &plugin, ["ticker", "stop"]).await {
            custody.errors.push(format!("ticker stop: {error:#}"));
            files::write_json(&path, &custody)?;
            if ticker::lock_state(&env.state_dir()) != LockState::Free {
                return Err(error)
                    .context("could not stop ordinary ticker; custody evidence retained");
            }
        }
    }
    if ticker::lock_state(&env.state_dir()) != LockState::Free {
        let error = format!(
            "ticker stop returned but ordinary lock remains held: {}",
            ticker::describe(&env.state_dir())
        );
        custody.errors.push(error.clone());
        files::write_json(&path, &custody)?;
        bail!("{error}; evidence retained in {}", path.display());
    }

    let stopped = match ordinary_hashes(env) {
        Ok(hashes) => hashes,
        Err(error) => {
            custody
                .errors
                .push(format!("could not capture stopped baseline: {error:#}"));
            files::write_json(&path, &custody)?;
            return Err(error).context("could not capture ordinary stopped-state baseline");
        }
    };
    custody.baseline = Some(stopped);
    custody.status = Status::Suspended;
    files::write_json(&path, &custody)?;
    Ok(format!(
        "ordinary ticker {} (pid {}) stopped; protected-file baseline captured; evidence: {}",
        custody.version,
        custody.stopped_ticker.pid,
        path.display()
    ))
}

/// Check protected ordinary files while stopped and restart the recorded binary
/// even when file hashes differ. Failure details stay in the evidence record.
pub async fn restore(harness: &Harness, env: &Env) -> Result<String> {
    let path = record_path(harness);
    let mut custody = read_custody(&path)?
        .with_context(|| format!("missing ordinary custody record {}", path.display()))?;
    if custody.status == Status::Restored {
        let LockState::Held(info) = ticker::lock_state(&env.state_dir()) else {
            bail!(
                "ordinary ticker was recorded restored but is not running: {}",
                ticker::describe(&env.state_dir())
            );
        };
        ensure!(
            info.version == custody.version,
            "ordinary ticker version changed after restore: {info:?}"
        );
        return Ok(format!(
            "ordinary ticker already restored; evidence: {}",
            path.display()
        ));
    }

    let current_binary_hash = fs::read(&custody.plugin)
        .with_context(|| format!("cannot read recorded plugin {}", custody.plugin.display()))
        .map(|bytes| sha256_hex(&bytes));
    let binary_matches = current_binary_hash
        .as_ref()
        .is_ok_and(|hash| hash == &custody.plugin_sha256);
    let actual = ordinary_hashes(env);
    let mut mismatch = match (&actual, &custody.baseline, binary_matches) {
        (Ok(hashes), Some(baseline), true) if hashes == baseline => None,
        (Ok(hashes), Some(baseline), true) => Some(format!(
            "ordinary protected-file hashes changed while stopped: {}",
            diff_hashes(baseline, hashes)
        )),
        (Ok(_), _, false) => Some(format!(
            "recorded ordinary binary changed or is unreadable; cannot restore from the recorded executable: {}",
            custody.plugin.display()
        )),
        (Ok(_), None, true) => Some("suspension did not record a stopped-state baseline".into()),
        (Err(error), _, _) => Some(format!(
            "could not hash ordinary files while stopped: {error:#}"
        )),
    };
    custody.restore_checked = actual.as_ref().ok().cloned();
    if let Some(error) = &mismatch {
        custody.errors.push(error.clone());
    }
    files::write_json(&path, &custody)?;

    // Never let `ticker start` replace a different lock holder or execute a
    // binary whose contents changed after suspension.
    let decision = restore_action(
        ticker::lock_state(&env.state_dir()),
        &custody.version,
        binary_matches,
    );
    let info = match decision {
        Ok(RestoreAction::AlreadyRunning(info)) => Some(info),
        Err(error) => {
            mismatch.get_or_insert_with(|| error.to_string());
            None
        }
        Ok(RestoreAction::Start) => {
            let start_result = command(env, &custody.plugin, ["ticker", "start"]).await;
            if let Err(error) = start_result {
                custody.errors.push(format!("ticker start: {error:#}"));
                files::write_json(&path, &custody)?;
                return Err(error).context("ordinary ticker restore failed; evidence retained");
            }
            let deadline = tokio::time::Instant::now() + START_WAIT;
            loop {
                match ticker::lock_state(&env.state_dir()) {
                    LockState::Held(info) if info.version == custody.version => break Some(info),
                    LockState::Held(info) => {
                        mismatch.get_or_insert_with(|| {
                            format!("unexpected ticker holds ordinary state: {info:?}")
                        });
                        break None;
                    }
                    LockState::Free if tokio::time::Instant::now() >= deadline => {
                        mismatch.get_or_insert_with(|| {
                            format!(
                                "ordinary ticker did not acquire its lock within {} seconds: {}",
                                START_WAIT.as_secs(),
                                ticker::describe(&env.state_dir())
                            )
                        });
                        break None;
                    }
                    LockState::Free => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        }
    };
    custody.restored_ticker = info.clone();
    if info.is_some() {
        custody.status = Status::Restored;
    }
    if let Some(error) = &mismatch
        && !custody.errors.contains(error)
    {
        custody.errors.push(error.clone());
    }
    files::write_json(&path, &custody)?;
    if let Some(error) = mismatch.take() {
        bail!(
            "{error}; ordinary ticker restore was attempted and evidence is at {}",
            path.display()
        );
    }
    let info = info.context("ordinary ticker lock was not reacquired; restore can be retried")?;
    Ok(format!(
        "restored ordinary ticker {} (pid {}); protected hashes match; evidence: {}",
        info.version,
        info.pid,
        path.display()
    ))
}

async fn command<const N: usize>(env: &Env, plugin: &Path, args: [&str; N]) -> Result<String> {
    let config_home = env
        .config_dir()
        .parent()
        .context("ordinary config folder has no XDG parent")?
        .to_path_buf();
    let state_home = env
        .state_dir()
        .parent()
        .context("ordinary state folder has no XDG parent")?
        .to_path_buf();
    let cmd = Cmd::new(plugin.to_string_lossy(), COMMAND_TIMEOUT)
        .args(args)
        .env("HOME", &env.home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_STATE_HOME", state_home)
        .max_output_bytes(64 * 1024);
    let output = RealRunner.run(&cmd).await?;
    ensure!(
        output.success(),
        "{} failed: {}{}",
        cmd.program,
        output.error_text(),
        if output.stdout.is_empty() {
            String::new()
        } else {
            format!("; {}", output.stdout.trim())
        }
    );
    Ok(output.stdout)
}

fn record_path(harness: &Harness) -> PathBuf {
    harness.root.join(RECORD)
}

fn read_custody(path: &Path) -> Result<Option<Custody>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "unsafe ordinary custody record {}",
        path.display()
    );
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("invalid ordinary custody record {}", path.display())
    })?))
}

#[derive(Debug, PartialEq, Eq)]
enum RestoreAction {
    AlreadyRunning(Info),
    Start,
}

fn restore_action(lock: LockState, version: &str, binary_matches: bool) -> Result<RestoreAction> {
    match lock {
        LockState::Held(info) if info.version == version && binary_matches => {
            Ok(RestoreAction::AlreadyRunning(info))
        }
        LockState::Held(info) => {
            bail!("refusing to disturb unexpected ordinary lock holder: {info:?}")
        }
        LockState::Free if binary_matches => Ok(RestoreAction::Start),
        LockState::Free => {
            bail!("refusing to start a changed or unreadable ordinary plugin binary")
        }
    }
}

fn ordinary_hashes(env: &Env) -> Result<TreeHashes> {
    let mut result = BTreeMap::new();
    for (label, root) in [
        ("config", env.config_dir()),
        ("runs", env.state_dir().join("runs")),
        ("supervisor", env.state_dir().join("supervisor")),
    ] {
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&root)?;
            result.insert(
                format!("{label}/@link"),
                sha256_hex(target.to_string_lossy().as_bytes()),
            );
            if let Ok(target) = root.canonicalize() {
                collect_actual(target, Path::new(label), &mut result)?;
            }
        } else {
            collect_actual(root, Path::new(label), &mut result)?;
        }
    }
    Ok(result)
}

fn collect_actual(path: PathBuf, relative: &Path, result: &mut TreeHashes) -> Result<()> {
    if excluded(relative) {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&path)?;
        result.insert(
            relative.display().to_string(),
            sha256_hex(target.to_string_lossy().as_bytes()),
        );
    } else if metadata.is_dir() {
        for entry in
            fs::read_dir(&path).with_context(|| format!("cannot list {}", path.display()))?
        {
            let entry = entry?;
            collect_actual(entry.path(), &relative.join(entry.file_name()), result)?;
        }
    } else if metadata.is_file() {
        result.insert(
            relative.display().to_string(),
            sha256_hex(&fs::read(&path)?),
        );
    }
    Ok(())
}

fn excluded(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "outbox")
        || path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == "lock"
                || name == "ticker.log"
                || name.ends_with(".lock")
                || name.ends_with(".sock")
                || name.ends_with(".log")
                || name.ends_with("-wal")
                || name.ends_with("-shm")
                || name.ends_with(".wal")
                || name.ends_with(".shm")
        })
}

fn diff_hashes(before: &TreeHashes, after: &TreeHashes) -> String {
    let mut changes = Vec::new();
    for (path, hash) in before {
        match after.get(path) {
            None => changes.push(format!("removed {path}")),
            Some(current) if current != hash => changes.push(format!("changed {path}")),
            _ => {}
        }
    }
    for path in after.keys().filter(|path| !before.contains_key(*path)) {
        changes.push(format!("added {path}"));
    }
    if changes.is_empty() {
        "no file differences".into()
    } else {
        changes.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_config_runs_and_supervisor_files_and_detects_additions() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = home.join(".config/herdr-linear-agent");
        let state = home.join(".local/state/herdr-linear-agent");
        fs::create_dir_all(config.join("profiles")).unwrap();
        fs::create_dir_all(state.join("runs/acme/ISSUE-1")).unwrap();
        fs::create_dir_all(state.join("supervisor")).unwrap();
        fs::write(config.join("profiles/default.toml"), "profile").unwrap();
        fs::write(state.join("runs/acme/ISSUE-1/run.json"), "run").unwrap();
        fs::write(state.join("supervisor/receipt.json"), "receipt").unwrap();
        fs::write(state.join("ticker.log"), "ignored").unwrap();
        fs::write(state.join("lock"), "ignored").unwrap();
        fs::write(state.join("supervisor.sqlite-wal"), "ignored").unwrap();
        fs::create_dir_all(state.join("runs/acme/ISSUE-1/outbox")).unwrap();
        fs::write(
            state.join("runs/acme/ISSUE-1/outbox/pending.json"),
            "ignored",
        )
        .unwrap();
        let env = Env::for_test(&home, &[]);

        let before = ordinary_hashes(&env).unwrap();
        assert!(before.contains_key("config/profiles/default.toml"));
        assert!(before.contains_key("runs/acme/ISSUE-1/run.json"));
        assert!(before.contains_key("supervisor/receipt.json"));
        assert!(!before.keys().any(|path| {
            path.contains("ticker.log")
                || path.contains("outbox")
                || path.ends_with("/lock")
                || path.ends_with("-wal")
        }));

        fs::write(state.join("runs/acme/ISSUE-1/run.json"), "changed").unwrap();
        fs::write(state.join("runs/acme/ISSUE-1/new.json"), "added").unwrap();
        let after = ordinary_hashes(&env).unwrap();
        let differences = diff_hashes(&before, &after);
        assert!(differences.contains("changed runs/acme/ISSUE-1/run.json"));
        assert!(differences.contains("added runs/acme/ISSUE-1/new.json"));
    }

    #[cfg(unix)]
    #[test]
    fn config_directory_symlink_hashes_link_and_contents() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let real_config = temp.path().join("immutable-config");
        fs::create_dir_all(&real_config).unwrap();
        fs::write(real_config.join("config.toml"), "configured").unwrap();
        fs::create_dir_all(home.join(".config")).unwrap();
        symlink(&real_config, home.join(".config/herdr-linear-agent")).unwrap();
        let env = Env::for_test(&home, &[]);

        let before = ordinary_hashes(&env).unwrap();
        assert!(before.contains_key("config/@link"));
        assert!(before.contains_key("config/config.toml"));
        fs::write(real_config.join("config.toml"), "changed").unwrap();
        assert_ne!(before, ordinary_hashes(&env).unwrap());
    }

    #[test]
    fn restore_never_starts_over_an_unexpected_lock_or_changed_binary() {
        let info = Info {
            version: "v1".into(),
            pid: 7,
            started: "now".into(),
        };
        assert_eq!(
            restore_action(LockState::Free, "v1", true).unwrap(),
            RestoreAction::Start
        );
        assert!(restore_action(LockState::Free, "v1", false).is_err());
        assert!(restore_action(LockState::Held(info.clone()), "v2", true).is_err());
        assert!(restore_action(LockState::Held(info.clone()), "v1", false).is_err());
        assert_eq!(
            restore_action(LockState::Held(info.clone()), "v1", true).unwrap(),
            RestoreAction::AlreadyRunning(info)
        );
    }

    #[test]
    fn malformed_custody_record_is_not_treated_as_absent_or_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(RECORD);
        fs::write(&path, "partial json").unwrap();
        assert!(read_custody(&path).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "partial json");
    }
}
