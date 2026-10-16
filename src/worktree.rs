//! Git-owned worker checkouts and the trusted configuration used to prepare them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::config::{self, WorktreeHooks};
use crate::paths::Ctx;
use crate::process::{Cmd, Runner};
use crate::{run, worker::Worker};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_LIMIT: usize = 1024 * 1024;
const LOG_LIMIT: usize = 1024 * 1024;

pub fn path(ctx: &Ctx<'_>, key: &str, id: &str, repo: &str) -> Result<PathBuf> {
    run::validate_key(key)?;
    crate::worker::validate_id(id)?;
    ensure!(config::valid_name(repo), "invalid repository name");
    Ok(ctx
        .state_dir()
        .join("worktrees")
        .join(key)
        .join(format!("{id}-{repo}")))
}

async fn git(runner: &dyn Runner, repo: &str, args: &[&str]) -> Result<crate::process::Output> {
    runner
        .run(
            &Cmd::new("git", GIT_TIMEOUT)
                .args(["-C", repo])
                .args(args.iter().copied())
                .max_output_bytes(OUTPUT_LIMIT),
        )
        .await
}

pub async fn check_git(runner: &dyn Runner) -> Result<()> {
    let out = runner
        .run(
            &Cmd::new("git", GIT_TIMEOUT)
                .arg("--version")
                .max_output_bytes(4096),
        )
        .await?;
    ensure!(out.success(), "git --version failed: {}", out.error_text());
    let version = out
        .stdout
        .trim()
        .strip_prefix("git version ")
        .context("unrecognized Git version")?;
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .context("unrecognized Git version")?;
    let minor = parts
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .context("unrecognized Git version")?;
    ensure!(
        (major, minor) >= (2, 17),
        "Git 2.17 or later is required; found {version}"
    );
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
struct Entry {
    path: PathBuf,
    branch: String,
    locked: bool,
}

fn unquote(value: &str) -> Result<PathBuf> {
    if !value.starts_with('"') {
        return Ok(PathBuf::from(value));
    }
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .context("invalid quoted worktree path")?;
    let mut result = Vec::new();
    let mut bytes = value.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            result.push(byte);
            continue;
        }
        let next = bytes.next().context("invalid worktree path escape")?;
        result.push(match next {
            b'"' | b'\\' => next,
            b'a' => 7,
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'v' => 11,
            b'f' => 12,
            b'r' => 13,
            b'0'..=b'7' => {
                let mut number = u16::from(next - b'0');
                for _ in 0..2 {
                    let n = bytes
                        .next()
                        .filter(|b| (b'0'..=b'7').contains(b))
                        .context("invalid octal worktree path")?;
                    number = number * 8 + u16::from(n - b'0');
                }
                u8::try_from(number).context("invalid octal worktree path")?
            }
            _ => bail!("invalid worktree path escape"),
        });
    }
    use std::os::unix::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_vec(result).into())
}

fn entries(text: &str) -> Result<Vec<Entry>> {
    text.split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .map(|block| {
            let mut entry = Entry::default();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("worktree ") {
                    entry.path = unquote(value)?;
                }
                if let Some(value) = line.strip_prefix("branch refs/heads/") {
                    entry.branch = value.into();
                }
                if line == "locked" || line.starts_with("locked ") {
                    entry.locked = true;
                }
            }
            ensure!(
                !entry.path.as_os_str().is_empty(),
                "Git returned a worktree without a path"
            );
            Ok(entry)
        })
        .collect()
}

async fn list(runner: &dyn Runner, repo: &str) -> Result<Vec<Entry>> {
    let out = git(
        runner,
        repo,
        &[
            "-c",
            "core.quotePath=true",
            "worktree",
            "list",
            "--porcelain",
        ],
    )
    .await?;
    ensure!(
        out.success(),
        "could not list worktrees: {}",
        out.error_text()
    );
    entries(&out.stdout)
}

fn same_path(a: &Path, b: &Path) -> bool {
    fn resolve(path: &Path) -> std::io::Result<PathBuf> {
        match path.canonicalize() {
            Ok(path) => Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or(e)?;
                Ok(resolve(parent)?.join(path.file_name().unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }
    a == b
        || resolve(a)
            .ok()
            .zip(resolve(b).ok())
            .is_some_and(|(a, b)| a == b)
}

async fn verify_common(runner: &dyn Runner, repo: &str, checkout: &str) -> Result<()> {
    let mut common = Vec::new();
    for cwd in [repo, checkout] {
        let out = git(runner, cwd, &["rev-parse", "--git-common-dir"]).await?;
        ensure!(
            out.success(),
            "cannot verify checkout identity: {}",
            out.error_text()
        );
        let p = Path::new(out.stdout.trim());
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(cwd).join(p)
        };
        common.push(
            p.canonicalize()
                .context("cannot resolve Git common directory")?,
        );
    }
    ensure!(
        common[0] == common[1],
        "checkout belongs to a different repository"
    );
    Ok(())
}

pub async fn verify_checkout(
    runner: &dyn Runner,
    repo: &str,
    checkout: &str,
    branch: &str,
) -> Result<()> {
    let registered = list(runner, repo).await?;
    let entry = registered
        .iter()
        .find(|entry| same_path(&entry.path, Path::new(checkout)))
        .context("checkout is not registered with the expected repository")?;
    ensure!(
        entry.branch == branch && !entry.locked,
        "checkout identity changed or worktree is locked: {checkout}"
    );
    ensure!(
        Path::new(checkout).is_dir(),
        "checkout is missing: {checkout}"
    );
    verify_common(runner, repo, checkout).await
}

pub async fn create(
    runner: &dyn Runner,
    repo: &str,
    branch: &str,
    base: &str,
    checkout: &str,
) -> Result<bool> {
    check_git(runner).await?;
    ensure!(!base.starts_with(['-', '+']), "base must be a branch name");
    let valid_base = git(
        runner,
        repo,
        &["check-ref-format", &format!("refs/heads/{base}")],
    )
    .await?;
    ensure!(valid_base.success(), "base must be a branch name: {base}");
    let wanted = Path::new(checkout);
    let registered = list(runner, repo).await?;
    if let Some(entry) = registered.iter().find(|e| same_path(&e.path, wanted)) {
        ensure!(
            entry.branch == branch && !entry.locked,
            "checkout identity changed or worktree is locked: {checkout}"
        );
        if wanted.is_dir() {
            verify_common(runner, repo, checkout).await?;
            return Ok(false);
        }
        ensure!(
            std::fs::symlink_metadata(wanted)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "registered checkout is not a directory or cannot be inspected: {checkout}"
        );
        // Only the worker's exact missing registration may be repaired.
        let out = git(runner, repo, &["worktree", "remove", "--", checkout]).await?;
        ensure!(
            out.success(),
            "cannot repair missing checkout {checkout}: {}",
            out.error_text()
        );
    }
    ensure!(
        !registered
            .iter()
            .any(|e| e.branch == branch && !same_path(&e.path, wanted)),
        "worker branch is checked out at another path"
    );
    ensure!(
        std::fs::symlink_metadata(wanted).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "unregistered checkout path already exists or cannot be inspected: {checkout}"
    );
    let out = git(
        runner,
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await?;
    ensure!(
        !out.timed_out && matches!(out.code, Some(0 | 1)),
        "cannot inspect worker branch: {}",
        out.error_text()
    );
    let exists = out.success();
    if !exists {
        let out = git(runner, repo, &["fetch", "origin", base]).await?;
        ensure!(out.success(), "git fetch failed: {}", out.error_text());
    }
    std::fs::create_dir_all(wanted.parent().context("checkout has no parent")?)?;
    let remote = format!("origin/{base}");
    let args = if exists {
        vec!["worktree", "add", "--", checkout, branch]
    } else {
        vec![
            "worktree",
            "add",
            "--no-track",
            "-b",
            branch,
            "--",
            checkout,
            &remote,
        ]
    };
    let out = git(runner, repo, &args).await?;
    ensure!(
        out.success(),
        "git worktree add failed: {}",
        out.error_text()
    );
    verify_common(runner, repo, checkout).await?;
    Ok(true)
}

pub async fn remove_checkout(
    runner: &dyn Runner,
    repo: &str,
    checkout: &str,
    branch: &str,
) -> Result<()> {
    let registered = list(runner, repo).await?;
    let Some(entry) = registered
        .iter()
        .find(|e| same_path(&e.path, Path::new(checkout)))
    else {
        ensure!(
            std::fs::symlink_metadata(checkout)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "unregistered path retained: {checkout}"
        );
        return Ok(());
    };
    ensure!(
        entry.branch == branch && !entry.locked,
        "checkout identity changed or worktree is locked: {checkout}"
    );
    match std::fs::symlink_metadata(checkout) {
        Ok(_) => {
            verify_common(runner, repo, checkout).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("cannot inspect registered checkout"),
    }
    let out = git(runner, repo, &["worktree", "remove", "--", checkout]).await?;
    ensure!(
        out.success(),
        "checkout retained at {checkout}: {}",
        out.error_text()
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub struct HookVars {
    pub run_key: String,
    pub worker_id: String,
    pub repo: String,
    pub repo_path: String,
    pub worktree: String,
    pub branch: String,
    pub base: String,
}

impl HookVars {
    pub fn new(key: &str, worker: &Worker) -> Self {
        Self {
            run_key: key.into(),
            worker_id: worker.id.clone(),
            repo: worker.repo.clone(),
            repo_path: worker.repo_path.clone(),
            worktree: worker.worktree_path.clone(),
            branch: worker.branch.clone(),
            base: worker.base.clone(),
        }
    }

    fn command(&self, script: &str, seconds: u64, cwd: &str) -> Cmd {
        let mut cmd = Cmd::new("sh", Duration::from_secs(seconds))
            .args(["-c", script])
            .cwd(cwd)
            .max_output_bytes(64 * 1024);
        for (key, value) in [
            ("HLA_RUN_KEY", &self.run_key),
            ("HLA_WORKER_ID", &self.worker_id),
            ("HLA_REPO", &self.repo),
            ("HLA_REPO_PATH", &self.repo_path),
            ("HLA_WORKTREE", &self.worktree),
            ("HLA_BRANCH", &self.branch),
            ("HLA_BASE", &self.base),
        ] {
            cmd = cmd.env(key, value);
        }
        cmd
    }
}

pub fn setup_steps(hooks: &WorktreeHooks) -> usize {
    hooks.copy.len() + hooks.symlink.len() + hooks.post_create.len()
}
pub fn remove_steps(hooks: &WorktreeHooks) -> usize {
    hooks.post_remove.len()
}

fn log(path: &Path, text: &str) -> Result<()> {
    let size = std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    if size >= LOG_LIMIT {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let bytes = text.as_bytes();
    file.write_all(&bytes[..bytes.len().min(LOG_LIMIT - size)])?;
    Ok(())
}

fn prepare_file(vars: &HookVars, entry: &str, symlink: bool) -> Result<()> {
    let repo = Path::new(&vars.repo_path).canonicalize()?;
    let checkout = Path::new(&vars.worktree).canonicalize()?;
    let target = checkout.join(entry);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let source = repo.join(entry);
    let source = match source.canonicalize() {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        source.starts_with(&repo),
        "source escapes repository: {entry}"
    );
    let metadata = source.metadata()?;
    ensure!(
        metadata.is_file() || symlink && metadata.is_dir(),
        "copy requires a regular file; symlink accepts files or directories: {entry}"
    );
    let parent = target.parent().context("destination has no parent")?;
    let relative = parent.strip_prefix(&checkout)?;
    let mut ancestor = checkout.clone();
    for component in relative.components() {
        ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "invalid destination: {entry}"
        );
        ancestor.push(component);
        match std::fs::create_dir(&ancestor) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        ensure!(
            ancestor.canonicalize()?.starts_with(&checkout),
            "destination escapes checkout: {entry}"
        );
    }
    if symlink {
        match std::os::unix::fs::symlink(source, &target) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    } else {
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let mut input = std::fs::File::open(source)?;
        std::io::copy(&mut input, temporary.as_file_mut())?;
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(target) {
            Ok(_) => (),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.error.into()),
        }
    }
    Ok(())
}

pub async fn setup_step(
    runner: &dyn Runner,
    hooks: &WorktreeHooks,
    vars: &HookVars,
    cursor: usize,
    log_path: &Path,
) -> Result<bool> {
    log(log_path, &format!("setup step {cursor}\n"))?;
    if cursor < hooks.copy.len() {
        prepare_file(vars, &hooks.copy[cursor], false)?;
    } else if cursor < hooks.copy.len() + hooks.symlink.len() {
        prepare_file(vars, &hooks.symlink[cursor - hooks.copy.len()], true)?;
    } else if let Some(script) = hooks
        .post_create
        .get(cursor - hooks.copy.len() - hooks.symlink.len())
    {
        let out = runner
            .run(&vars.command(script, hooks.post_create_timeout_seconds, &vars.worktree))
            .await?;
        log(log_path, &format!("{}{}\n", out.stdout, out.stderr))?;
        ensure!(
            out.success(),
            "worktree setup failed: {}; see {}",
            out.error_text(),
            log_path.display()
        );
    }
    Ok(cursor.saturating_add(1) >= setup_steps(hooks))
}

pub async fn post_remove_step(
    runner: &dyn Runner,
    hooks: &WorktreeHooks,
    vars: &HookVars,
    cursor: usize,
    log_path: &Path,
) -> Result<bool> {
    if let Some(script) = hooks.post_remove.get(cursor) {
        log(log_path, &format!("post_remove step {cursor}\n"))?;
        let out = runner
            .run(&vars.command(script, hooks.post_remove_timeout_seconds, &vars.repo_path))
            .await?;
        log(log_path, &format!("{}{}\n", out.stdout, out.stderr))?;
        ensure!(
            out.success(),
            "post_remove failed: {}; see {}",
            out.error_text(),
            log_path.display()
        );
    }
    Ok(cursor.saturating_add(1) >= remove_steps(hooks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::RealRunner;

    fn git_sync(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git_sync(root.path(), &["init", "-b", "main"]);
        git_sync(root.path(), &["config", "core.hooksPath", "/dev/null"]);
        git_sync(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "Test fixture",
            ],
        );
        git_sync(
            root.path(),
            &["remote", "add", "origin", root.path().to_str().unwrap()],
        );
        root
    }

    #[tokio::test]
    async fn real_git_reuses_owned_checkouts_preserves_branches_and_refuses_dirty_removal() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let checkout = state.path().join("check out 日本語");
        let (repo_path, path) = (repo.path().to_str().unwrap(), checkout.to_str().unwrap());
        assert!(
            create(&RealRunner, repo_path, "worker", "main", path)
                .await
                .unwrap()
        );
        assert!(
            !create(&RealRunner, repo_path, "worker", "main", path)
                .await
                .unwrap()
        );
        std::fs::write(checkout.join("untracked"), "keep").unwrap();
        assert!(
            remove_checkout(&RealRunner, repo_path, path, "worker")
                .await
                .is_err()
        );
        assert!(checkout.join("untracked").exists());
        std::fs::remove_file(checkout.join("untracked")).unwrap();
        remove_checkout(&RealRunner, repo_path, path, "worker")
            .await
            .unwrap();
        remove_checkout(&RealRunner, repo_path, path, "worker")
            .await
            .unwrap();
        assert_eq!(
            git_sync(repo.path(), &["branch", "--list", "worker"]).trim(),
            "worker"
        );
        assert!(
            create(&RealRunner, repo_path, "worker", "main", path)
                .await
                .unwrap()
        );
        assert!(
            create(
                &RealRunner,
                repo_path,
                "worker",
                "main",
                state.path().join("other").to_str().unwrap()
            )
            .await
            .is_err()
        );
        std::fs::remove_dir_all(&checkout).unwrap();
        assert!(
            create(&RealRunner, repo_path, "worker", "main", path)
                .await
                .unwrap()
        );
        assert!(checkout.is_dir());
    }

    #[tokio::test]
    async fn checkout_verification_rejects_replaced_paths_and_changed_branches() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let checkout = state.path().join("worker");
        let source = repo.path().to_str().unwrap();
        let path = checkout.to_str().unwrap();
        create(&RealRunner, source, "worker", "main", path)
            .await
            .unwrap();
        verify_checkout(&RealRunner, source, path, "worker")
            .await
            .unwrap();
        git_sync(&checkout, &["checkout", "-b", "other"]);
        assert!(
            verify_checkout(&RealRunner, source, path, "worker")
                .await
                .is_err()
        );
        git_sync(&checkout, &["checkout", "worker"]);
        let foreign = repository();
        assert!(
            verify_checkout(
                &RealRunner,
                foreign.path().to_str().unwrap(),
                path,
                "worker"
            )
            .await
            .is_err()
        );
        std::fs::remove_dir_all(&checkout).unwrap();
        std::fs::create_dir(&checkout).unwrap();
        assert!(
            verify_checkout(&RealRunner, source, path, "worker")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn setup_is_atomic_non_overwriting_and_hooks_keep_their_cwd_and_environment() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let checkout = root.path().join("checkout");
        std::fs::create_dir_all(repo.join("nested")).unwrap();
        std::fs::create_dir_all(repo.join("node_modules")).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(repo.join("nested/.env"), "original").unwrap();
        let worker = Worker {
            id: "w1".into(),
            repo: "api".into(),
            repo_path: repo.to_str().unwrap().into(),
            worktree_path: checkout.to_str().unwrap().into(),
            ..Worker::default()
        };
        let vars = HookVars::new("acme/DATA-1", &worker);
        let hooks = WorktreeHooks {
            copy: vec!["nested/.env".into()],
            symlink: vec!["node_modules".into()],
            post_create: vec![
                "test \"$HLA_RUN_KEY\" = acme/DATA-1 && printf ready > hook-ran".into(),
            ],
            post_remove: vec![
                "test \"$HLA_WORKER_ID\" = w1 && printf removed > hook-removed".into(),
            ],
            ..WorktreeHooks::default()
        };
        let log = root.path().join("setup.log");
        for cursor in 0..setup_steps(&hooks) {
            assert_eq!(
                setup_step(&RealRunner, &hooks, &vars, cursor, &log)
                    .await
                    .unwrap(),
                cursor + 1 == setup_steps(&hooks)
            );
        }
        assert_eq!(
            std::fs::read_to_string(checkout.join("nested/.env")).unwrap(),
            "original"
        );
        std::fs::write(repo.join("nested/.env"), "changed").unwrap();
        setup_step(&RealRunner, &hooks, &vars, 0, &log)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(checkout.join("nested/.env")).unwrap(),
            "original"
        );
        assert_eq!(
            checkout.join("node_modules").canonicalize().unwrap(),
            repo.join("node_modules").canonicalize().unwrap()
        );
        assert!(checkout.join("hook-ran").is_file());
        std::fs::remove_dir_all(&checkout).unwrap();
        assert!(
            post_remove_step(&RealRunner, &hooks, &vars, 0, &log)
                .await
                .unwrap()
        );
        assert!(repo.join("hook-removed").is_file());
    }

    #[tokio::test]
    async fn fetch_refspecs_are_rejected_before_any_checkout_is_created() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let checkout = state.path().join("worker");
        for base in [
            "main:refs/heads/overwrite",
            "+main",
            "-main",
            "bad..branch",
            "HEAD~1",
        ] {
            assert!(
                create(
                    &RealRunner,
                    repo.path().to_str().unwrap(),
                    "worker",
                    base,
                    checkout.to_str().unwrap()
                )
                .await
                .is_err(),
                "accepted {base}"
            );
            assert!(!checkout.exists());
        }
    }

    #[tokio::test]
    async fn git_inspection_errors_do_not_become_missing_checkouts_or_branches() {
        use crate::process::fake::{FakeRunner, fail, ok};
        let runner = FakeRunner::default();
        runner.on("git --version", ok("git version 2.54.0\n"));
        runner.on("worktree list", fail(128, "repository is unavailable"));
        assert!(
            create(&runner, "/repo", "worker", "main", "/checkout")
                .await
                .is_err()
        );
        assert!(
            remove_checkout(&runner, "/repo", "/checkout", "worker")
                .await
                .is_err()
        );
        assert_eq!(runner.count("worktree add"), 0);
        assert_eq!(runner.count("worktree remove"), 0);
        runner.on("worktree list", ok(""));
        runner.on("rev-parse", fail(128, "cannot read references"));
        assert!(
            create(&runner, "/repo", "worker", "main", "/checkout")
                .await
                .is_err()
        );
        assert_eq!(runner.count("fetch"), 0);
        assert_eq!(runner.count("worktree add"), 0);
        runner.on("git --version", ok("git version 2.16.6\n"));
        assert!(
            check_git(&runner)
                .await
                .unwrap_err()
                .to_string()
                .contains("2.16.6")
        );
        runner.on("git --version", ok("git version 2.39.5 (Apple Git-154)\n"));
        check_git(&runner).await.unwrap();
    }

    #[tokio::test]
    async fn failed_hooks_and_unregistered_dangling_checkouts_are_not_treated_as_success() {
        let repo = repository();
        let state = tempfile::tempdir().unwrap();
        let checkout = state.path().join("checkout");
        std::os::unix::fs::symlink(state.path().join("missing"), &checkout).unwrap();
        let path = checkout.to_str().unwrap();
        let source = repo.path().to_str().unwrap();
        assert!(
            create(&RealRunner, source, "worker", "main", path)
                .await
                .is_err()
        );
        assert!(
            remove_checkout(&RealRunner, source, path, "worker")
                .await
                .is_err()
        );
        std::fs::remove_file(&checkout).unwrap();
        std::fs::create_dir(&checkout).unwrap();
        let vars = HookVars::new(
            "acme/DATA-1",
            &Worker {
                repo_path: source.into(),
                worktree_path: path.into(),
                ..Worker::default()
            },
        );
        let hooks = WorktreeHooks {
            post_create: vec![
                "printf failure >&2; exit 7".into(),
                "touch should-not-run".into(),
            ],
            ..WorktreeHooks::default()
        };
        let log = state.path().join("setup.log");
        assert!(
            setup_step(&RealRunner, &hooks, &vars, 0, &log)
                .await
                .unwrap_err()
                .to_string()
                .contains("failure")
        );
        assert!(!checkout.join("should-not-run").exists());
        assert!(std::fs::read_to_string(log).unwrap().contains("failure"));
    }

    #[test]
    fn quoted_paths_and_destination_escape_are_checked() {
        assert_eq!(
            unquote("\"/tmp/a\\n\\\"b\"").unwrap(),
            PathBuf::from("/tmp/a\n\"b")
        );
        assert!(unquote("\"/tmp/\\999\"").is_err());
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let checkout = root.path().join("checkout");
        let outside = root.path().join("outside");
        for p in [&repo, &checkout, &outside] {
            std::fs::create_dir(p).unwrap();
        }
        std::fs::create_dir(repo.join("nested")).unwrap();
        std::fs::write(repo.join("nested/file"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, checkout.join("nested")).unwrap();
        let vars = HookVars::new(
            "acme/DATA-1",
            &Worker {
                repo_path: repo.to_str().unwrap().into(),
                worktree_path: checkout.to_str().unwrap().into(),
                ..Worker::default()
            },
        );
        assert!(prepare_file(&vars, "nested/file", false).is_err());
        assert!(!outside.join("file").exists());
        std::fs::remove_file(checkout.join("nested")).unwrap();
        assert!(prepare_file(&vars, "nested", false).is_err());
        std::os::unix::fs::symlink(repo.join("absent"), checkout.join("kept")).unwrap();
        prepare_file(&vars, "kept", false).unwrap();
        assert!(
            std::fs::symlink_metadata(checkout.join("kept"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
