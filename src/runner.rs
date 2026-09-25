//! Every external command (herdr, git, the routing agent) goes through `Runner`.

// Derived from herdr-projects v0.2.11 (https://github.com/eliasstravik/herdr-projects).
// Copyright (c) 2026 Elias Stravik. MIT License; see NOTICE.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub timeout: Duration,
}

impl Cmd {
    pub fn new(program: impl Into<String>, timeout: Duration) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            timeout,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    /// The command as one line; the scripted fake matches on it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn display(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// `None` when the process was killed (timeout or signal).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// stderr when it has text, else stdout, trimmed; for error messages.
    pub fn error_text(&self) -> String {
        if self.timed_out {
            return "timed out".to_string();
        }
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        text.to_string()
    }
}

pub trait Runner {
    /// `Err` means the command could not be spawned at all (for example the
    /// program is missing). A non-zero exit or a timeout is an `Ok(Output)`.
    fn run(&self, cmd: &Cmd) -> Result<Output>;
}

pub struct RealRunner;

const POLL: Duration = Duration::from_millis(20);

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args);
        for key in &cmd.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &cmd.env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("could not run `{}`", cmd.program))?;

        // The readers run on their own threads so a full pipe cannot deadlock
        // against the deadline loop below.
        let stdout_thread = child.stdout.take().map(read_all);
        let stderr_thread = child.stderr.take().map(read_all);

        let deadline = Instant::now() + cmd.timeout;
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
            std::thread::sleep(POLL);
        };

        let stdout = stdout_thread.map(join_text).unwrap_or_default();
        let stderr = stderr_thread.map(join_text).unwrap_or_default();

        Ok(Output {
            code: if timed_out {
                None
            } else {
                status.and_then(|s| s.code())
            },
            stdout,
            stderr,
            timed_out,
        })
    }
}

fn read_all<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn join_text(thread: std::thread::JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&thread.join().unwrap_or_default()).into_owned()
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;

    type Matcher = Box<dyn Fn(&Cmd) -> bool>;
    type Answer = Box<dyn Fn(&Cmd) -> Result<Output>>;

    /// A scripted runner: the last rule whose matcher accepts the command
    /// answers it, so a test can override a fixture's default answer. Every
    /// command is recorded, matched or not.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: RefCell<Vec<(Matcher, Answer)>>,
        pub calls: RefCell<Vec<Cmd>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Answer commands whose display line contains `needle`.
        pub fn on(&self, needle: &str, output: Output) -> &Self {
            let needle = needle.to_string();
            self.rules.borrow_mut().push((
                Box::new(move |cmd| cmd.display().contains(&needle)),
                Box::new(move |_| Ok(output.clone())),
            ));
            self
        }

        pub fn on_fn(
            &self,
            matcher: impl Fn(&Cmd) -> bool + 'static,
            answer: impl Fn(&Cmd) -> Result<Output> + 'static,
        ) -> &Self {
            self.rules
                .borrow_mut()
                .push((Box::new(matcher), Box::new(answer)));
            self
        }

        pub fn count(&self, needle: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|cmd| cmd.display().contains(needle))
                .count()
        }

        /// The display lines of every recorded call containing `needle`.
        pub fn lines(&self, needle: &str) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(Cmd::display)
                .filter(|line| line.contains(needle))
                .collect()
        }
    }

    pub fn ok(stdout: &str) -> Output {
        Output {
            code: Some(0),
            stdout: stdout.to_string(),
            ..Output::default()
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stderr: stderr.to_string(),
            ..Output::default()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.calls.borrow_mut().push(cmd.clone());
            for (matcher, answer) in self.rules.borrow().iter().rev() {
                if matcher(cmd) {
                    return answer(cmd);
                }
            }
            anyhow::bail!("FakeRunner: no rule for `{}`", cmd.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_output_and_exit_code() {
        let out = RealRunner
            .run(
                &Cmd::new("sh", Duration::from_secs(5))
                    .args(["-c", "echo hi; echo err >&2; exit 3"]),
            )
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "err\n");
        assert!(!out.success());
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(
            RealRunner
                .run(&Cmd::new("hla-no-such-program", Duration::from_secs(1)))
                .is_err()
        );
    }

    #[test]
    fn times_out_a_chatty_child() {
        // `yes` fills the pipe far past its buffer; the reader threads keep it
        // drained so the deadline still fires.
        let start = Instant::now();
        let out = RealRunner
            .run(&Cmd::new("yes", Duration::from_millis(300)))
            .unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(out.stdout.len() > 65_536);
    }
}
