//! Child processes (git, `open`) under tokio, behind a trait so tests can
//! answer them.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::time::{Instant, sleep_until};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long output is still read after the child exited.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub timeout: Duration,
    /// Optional combined stdout/stderr byte cap. Output at or below the cap
    /// is retained; exceeding it kills and reaps the child.
    pub max_output_bytes: Option<usize>,
}

impl Cmd {
    pub fn new(program: impl Into<String>, timeout: Duration) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            timeout,
            max_output_bytes: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<S: Into<String>>(mut self, args: impl IntoIterator<Item = S>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        self.env_remove.push(key.as_ref().to_os_string());
        self
    }

    pub fn max_output_bytes(mut self, max: usize) -> Self {
        self.max_output_bytes = Some(max);
        self
    }

    /// The program and its arguments separated by spaces, for tests.
    #[cfg(test)]
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// The exit code; `None` when the child was killed.
    pub code: Option<i32>,
    pub stdout: String,
    /// Original captured stdout bytes when a caller requested a cap. This
    /// preserves binary diff bytes without retaining raw output for ordinary
    /// commands.
    pub capped_stdout_bytes: Option<Vec<u8>>,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }

    /// The first line of standard error, or how the child ended.
    pub fn error_text(&self) -> String {
        if self.timed_out {
            return "timed out".into();
        }
        match self.stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
            Some(line) => line.to_string(),
            None => match self.code {
                Some(code) => format!("exit code {code}"),
                None => "killed by a signal".into(),
            },
        }
    }

    pub fn raw_stdout(&self) -> &[u8] {
        self.capped_stdout_bytes
            .as_deref()
            .unwrap_or(self.stdout.as_bytes())
    }
}

pub trait Runner: Send + Sync {
    fn run<'a>(&'a self, cmd: &'a Cmd) -> BoxFuture<'a, Result<Output>>;

    /// An owned handle for work that must outlive the borrowing command.
    fn shared(&self) -> Arc<dyn Runner>;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run<'a>(&'a self, cmd: &'a Cmd) -> BoxFuture<'a, Result<Output>> {
        Box::pin(run(cmd))
    }

    fn shared(&self) -> Arc<dyn Runner> {
        Arc::new(RealRunner)
    }
}

async fn run(cmd: &Cmd) -> Result<Output> {
    let mut command = tokio::process::Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .envs(cmd.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in &cmd.env_remove {
        command.env_remove(key);
    }
    if let Some(cwd) = &cmd.cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("could not run `{}`", cmd.program))?;
    #[cfg(unix)]
    let mut group = ProcessGroup::new(child.id());
    #[cfg(unix)]
    let result =
        collect_child_inner(child, cmd.timeout, cmd.max_output_bytes, Some(&mut group)).await;
    #[cfg(not(unix))]
    let result = collect_child(child, cmd.timeout, cmd.max_output_bytes).await;
    result
}

#[cfg(unix)]
struct ProcessGroup {
    id: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessGroup {
    fn new(id: Option<u32>) -> Self {
        ProcessGroup {
            id: id.map(|id| id as libc::pid_t),
        }
    }

    fn kill(&mut self) {
        if let Some(id) = self.id.take() {
            unsafe {
                libc::kill(-id, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Drains a child's output without retaining more than `max_output_bytes`.
/// This only owns the direct child; callers that need descendant cleanup
/// must also own and kill the child's process group.
pub(crate) async fn collect_child(
    child: tokio::process::Child,
    timeout: Duration,
    max_output_bytes: Option<usize>,
) -> Result<Output> {
    collect_child_inner(child, timeout, max_output_bytes, None).await
}

async fn collect_child_inner(
    mut child: tokio::process::Child,
    timeout: Duration,
    max_output_bytes: Option<usize>,
    #[cfg(unix)] mut group: Option<&mut ProcessGroup>,
    #[cfg(not(unix))] _group: Option<&mut ()>,
) -> Result<Output> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if stdout.is_none() && stderr.is_none() {
        anyhow::bail!("child has no output pipes");
    }
    let mut deadline = Instant::now() + timeout;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut out_buf, mut err_buf) = ([0u8; 8192], [0u8; 8192]);
    let (mut out_open, mut err_open) = (stdout.is_some(), stderr.is_some());
    let mut status = None;
    let mut exceeded = false;
    // Both pipes are drained while waiting, so a chatty child cannot fill a
    // pipe and stall before its deadline.
    while status.is_none() || out_open || err_open {
        tokio::select! {
            read = async { stdout.as_mut().expect("open stdout").read(&mut out_buf).await }, if out_open => match read {
                Ok(n) if n > 0 => {
                    if max_output_bytes.is_some_and(|max| {
                        out.len().saturating_add(err.len()).saturating_add(n) > max
                    }) {
                        exceeded = true;
                    } else {
                        out.extend_from_slice(&out_buf[..n]);
                    }
                }
                _ => out_open = false,
            },
            read = async { stderr.as_mut().expect("open stderr").read(&mut err_buf).await }, if err_open => match read {
                Ok(n) if n > 0 => {
                    if max_output_bytes.is_some_and(|max| {
                        out.len().saturating_add(err.len()).saturating_add(n) > max
                    }) {
                        exceeded = true;
                    } else {
                        err.extend_from_slice(&err_buf[..n]);
                    }
                }
                _ => err_open = false,
            },
            waited = child.wait(), if status.is_none() => {
                status = Some(waited?);
                // A process the child left behind may keep the pipes open.
                deadline = deadline.min(Instant::now() + EXIT_DRAIN);
            }
            () = sleep_until(deadline), if status.is_some() => {
                #[cfg(unix)]
                if let Some(group) = group.as_deref_mut() { group.kill(); }
                break
            },
            () = sleep_until(deadline), if status.is_none() => {
                #[cfg(unix)]
                if let Some(group) = group.as_deref_mut() { group.kill(); }
                let _ = child.start_kill();
                let _ = child.wait().await;
                let (stdout, capped_stdout_bytes) = captured_stdout(out, max_output_bytes.is_some());
                return Ok(Output {
                    code: None,
                    stdout,
                    capped_stdout_bytes,
                    stderr: String::from_utf8_lossy(&err).into_owned(),
                    timed_out: true,
                });
            }
        }
        if exceeded {
            #[cfg(unix)]
            if let Some(group) = group.as_deref_mut() {
                group.kill();
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            anyhow::bail!("child output exceeded the configured byte limit");
        }
    }
    let (stdout, capped_stdout_bytes) = captured_stdout(out, max_output_bytes.is_some());
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout,
        capped_stdout_bytes,
        stderr: String::from_utf8_lossy(&err).into_owned(),
        timed_out: false,
    })
}

fn captured_stdout(bytes: Vec<u8>, keep_raw: bool) -> (String, Option<Vec<u8>>) {
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (text, keep_raw.then_some(bytes))
}

#[cfg(test)]
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Answers each command with the last rule whose needle its display line
    /// contains, so a test can override a default. Every command is recorded.
    #[derive(Clone, Default)]
    pub struct FakeRunner {
        rules: Arc<Mutex<Vec<(String, Output)>>>,
        pub calls: Arc<Mutex<Vec<Cmd>>>,
        real_git: bool,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_real_git(mut self) -> Self {
            self.real_git = true;
            self
        }

        pub fn on(&self, needle: &str, output: Output) -> &Self {
            self.rules.lock().unwrap().push((needle.into(), output));
            self
        }

        pub fn lines(&self, needle: &str) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(Cmd::display)
                .filter(|line| line.contains(needle))
                .collect()
        }

        pub fn count(&self, needle: &str) -> usize {
            self.lines(needle).len()
        }
    }

    pub fn ok(stdout: &str) -> Output {
        Output {
            code: Some(0),
            stdout: stdout.into(),
            ..Output::default()
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stderr: stderr.into(),
            ..Output::default()
        }
    }

    impl Runner for FakeRunner {
        fn run<'a>(&'a self, cmd: &'a Cmd) -> BoxFuture<'a, Result<Output>> {
            self.calls.lock().unwrap().push(cmd.clone());
            let line = cmd.display();
            let answer = self
                .rules
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(needle, _)| line.contains(needle.as_str()))
                .map(|(_, output)| output.clone());
            Box::pin(async move {
                match answer {
                    Some(output) => Ok(output),
                    None if self.real_git && cmd.program == "git" => super::run(cmd).await,
                    None => Err(anyhow::anyhow!("FakeRunner: no rule for `{line}`")),
                }
            })
        }

        fn shared(&self) -> Arc<dyn Runner> {
            Arc::new(self.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upper bound for work the runner must finish promptly; far above the
    /// deadlines under test, so a slow machine does not flake.
    const PROMPT: std::time::Duration = Duration::from_secs(5);

    fn sh(script: &str, timeout: Duration) -> Cmd {
        Cmd::new("sh", timeout).args(["-c", script])
    }

    async fn timed(cmd: Cmd) -> (Result<Output>, Duration) {
        let started = std::time::Instant::now();
        let result = RealRunner.run(&cmd).await;
        (result, started.elapsed())
    }

    #[tokio::test]
    async fn direct_child_collection_without_a_process_group_still_kills_on_timeout_or_cap() {
        for (program, args, cap) in [
            ("sleep", vec!["30"], None),
            ("yes", vec!["flood"], Some(64)),
        ] {
            let child = tokio::process::Command::new(program)
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                collect_child(child, Duration::from_millis(30), cap),
            )
            .await
            .expect("collector failed to kill its direct child");
            if cap.is_some() {
                assert!(result.unwrap_err().to_string().contains("output exceeded"));
            } else {
                assert!(result.unwrap().timed_out);
            }
        }
    }

    #[tokio::test]
    async fn runner_removes_inherited_environment_keys() {
        let output = RealRunner
            .run(
                &Cmd::new("sh", Duration::from_secs(2))
                    .args(["-c", "test -z \"${SHOULD_BE_REMOVED+x}\""])
                    .env("SHOULD_BE_REMOVED", "present")
                    .env_remove("SHOULD_BE_REMOVED"),
            )
            .await
            .unwrap();
        assert!(output.success(), "{}", output.stderr);
    }

    #[tokio::test]
    async fn both_streams_and_the_exit_status_are_captured() {
        let script = "printf 'on stdout'; printf 'on stderr' >&2; exit 3";
        let (result, _) = timed(sh(script, PROMPT)).await;
        let output = result.unwrap();
        assert_eq!(
            output,
            Output {
                code: Some(3),
                stdout: "on stdout".into(),
                capped_stdout_bytes: None,
                stderr: "on stderr".into(),
                timed_out: false,
            }
        );
        assert!(!output.success());
        assert_eq!(output.error_text(), "on stderr");

        let (result, _) = timed(sh("true", PROMPT)).await;
        assert!(result.unwrap().success());
    }

    #[tokio::test]
    async fn command_receives_its_working_directory_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let output = RealRunner
            .run(
                &sh("printf '%s:%s' \"$HLA_TEST_VALUE\" \"$PWD\"", PROMPT)
                    .cwd(dir.path())
                    .env("HLA_TEST_VALUE", "value"),
            )
            .await
            .unwrap();
        assert!(output.success());
        assert_eq!(
            output.stdout,
            format!("value:{}", dir.path().canonicalize().unwrap().display())
        );
    }

    #[tokio::test]
    async fn combined_output_cap_accepts_the_limit_and_rejects_the_next_byte() {
        let exact = sh("printf '1234'; printf '5678' >&2", PROMPT).max_output_bytes(8);
        let output = RealRunner.run(&exact).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.len() + output.stderr.len(), 8);

        let over = sh("printf '1234'; printf '5678' >&2", PROMPT).max_output_bytes(7);
        let error = RealRunner.run(&over).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("child output exceeded the configured byte limit"),
            "{error:#}"
        );

        let flood = Cmd::new("yes", PROMPT).arg("flood").max_output_bytes(64);
        let (result, took) = timed(flood).await;
        assert!(result.unwrap_err().to_string().contains("output exceeded"));
        assert!(took < PROMPT, "took {took:?}");
    }

    #[tokio::test]
    async fn capped_stdout_preserves_original_binary_bytes() {
        let output = RealRunner
            .run(&sh("printf '\\377'", PROMPT).max_output_bytes(8))
            .await
            .unwrap();
        assert_eq!(output.raw_stdout(), [0xff]);
        assert!(output.stdout.contains('�'));
    }

    #[test]
    fn error_text_prefers_stderr_then_says_how_the_child_ended() {
        let ended = |code, stderr: &str, timed_out| Output {
            code,
            stderr: stderr.into(),
            timed_out,
            ..Output::default()
        };
        let cases = [
            (
                ended(
                    Some(128),
                    "\n  fatal: not a git repository  \nhint\n",
                    false,
                ),
                "fatal: not a git repository",
            ),
            (ended(Some(2), "", false), "exit code 2"),
            (ended(None, "", false), "killed by a signal"),
            (ended(None, "partial output", true), "timed out"),
        ];
        for (output, expected) in cases {
            assert_eq!(output.error_text(), expected, "{output:?}");
        }
        let fetch = Cmd::new("git", PROMPT)
            .args(["-C", "/src/api", "fetch"])
            .arg("origin");
        assert_eq!(fetch.display(), "git -C /src/api fetch origin");
    }

    #[tokio::test]
    async fn a_background_grandchild_holding_the_pipes_does_not_delay_the_result() {
        let script = "sleep 30 & echo launched";
        let (result, took) = timed(sh(script, Duration::from_secs(20))).await;
        let output = result.unwrap();
        assert_eq!(output.code, Some(0));
        assert_eq!(output.stdout, "launched\n");
        assert!(!output.timed_out);
        assert!(took < PROMPT, "took {took:?}");
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_an_error() {
        let missing = Cmd::new("/nonexistent/herdr-linear-agent-no-such-program", PROMPT);
        let (result, _) = timed(missing).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("could not run"), "{error}");
    }

    #[tokio::test]
    async fn a_child_past_its_deadline_is_killed_even_while_it_floods_stdout() {
        let deadline = Duration::from_millis(300);
        for cmd in [
            Cmd::new("yes", deadline).arg("flood"),
            sh("sleep 30", deadline),
        ] {
            let shown = cmd.display();
            let (result, took) = timed(cmd).await;
            let output = result.unwrap();
            assert!(output.timed_out, "{shown}");
            assert_eq!(output.code, None, "{shown}");
            assert!(!output.success(), "{shown}");
            assert!(took < PROMPT, "{shown} took {took:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_a_descendant_before_it_can_write() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late-write");
        let cmd = sh(
            "(sleep 0.4; printf touched > \"$HLA_MARKER\") & wait",
            Duration::from_millis(100),
        )
        .env("HLA_MARKER", &marker);
        let output = RealRunner.run(&cmd).await.unwrap();
        assert!(output.timed_out);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "a descendant outlived the command timeout"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_running_command_kills_its_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("started");
        let marker = dir.path().join("late-write");
        let cmd = sh(
            "printf started > \"$HLA_STARTED\"; (sleep 0.5; printf touched > \"$HLA_MARKER\") & wait",
            Duration::from_secs(10),
        )
        .env("HLA_STARTED", &started)
        .env("HLA_MARKER", &marker);
        let task = tokio::spawn(async move { RealRunner.run(&cmd).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("command did not start");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert!(
            !marker.exists(),
            "a descendant outlived command cancellation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_limit_kills_a_descendant_before_it_can_write() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late-write");
        let cmd = sh(
            "(sleep 0.4; printf touched > \"$HLA_MARKER\") & printf too-much; wait",
            PROMPT,
        )
        .env("HLA_MARKER", &marker)
        .max_output_bytes(2);
        let error = RealRunner.run(&cmd).await.unwrap_err();
        assert!(error.to_string().contains("output exceeded"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!marker.exists(), "a descendant outlived the output limit");
    }
}
