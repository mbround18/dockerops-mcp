//! The outbound port, and its adapters.
//!
//! Nothing in this crate touches `std::process` directly; it talks to [`CommandRunner`],
//! which runs one program and hands back what it said. Tests substitute
//! [`ScriptedRunner`], so every rendering decision below is verifiable without a Docker
//! daemon, a network, or an image pull.
//!
//! Two things the real adapter does that a naive `Command::output()` would not:
//!
//! * **It always has a deadline.** `docker ps` against an unreachable daemon hangs until
//!   the client gives up, and an MCP server that hangs takes the conversation with it. A
//!   timed-out call is a normal outcome here, reported as one line.
//! * **It drains stdout and stderr on their own threads.** `docker logs` can emit
//!   megabytes; polling for exit without reading the pipes deadlocks the moment the
//!   kernel buffer fills.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long a call may take before it is killed. Reads are quick; a `compose up` that
/// builds is not, so callers raise this deliberately.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Captured result of a single process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// The process overran its deadline and was killed. `status` is then meaningless.
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.status == 0 && !self.timed_out
    }

    /// Trimmed stdout, or `None` when the command failed or produced nothing.
    pub fn value(&self) -> Option<String> {
        if !self.ok() {
            return None;
        }
        let trimmed = self.stdout.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Non-empty stdout lines, which is the shape every `--format` call comes back in.
    pub fn stdout_lines(&self) -> impl Iterator<Item = &str> {
        self.stdout
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
    }

    /// The most useful line of a failure. Docker puts the reason last and prefixes it with
    /// `Error response from daemon:`, which is noise once the reason is in hand.
    pub fn failure(&self) -> String {
        if self.timed_out {
            return "timed out".to_owned();
        }
        let detail = self
            .stderr
            .lines()
            .map(str::trim)
            .rev()
            .find(|l| !l.is_empty())
            .or_else(|| self.stdout.lines().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("no output");
        detail
            .trim_start_matches("Error response from daemon: ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_owned()
    }
}

/// The single outbound port of this crate.
pub trait CommandRunner {
    /// Run `program` with `args`, optionally inside `cwd`, killing it after `timeout`.
    /// Returning `Err` means the process could not be spawned at all — typically `docker`
    /// is not installed. A non-zero exit is a successful `Ok`.
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        timeout: Duration,
    ) -> std::io::Result<CommandOutput>;
}

/// Adapter that shells out to the real `docker` on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        timeout: Duration,
    ) -> std::io::Result<CommandOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        // `DOCKER_CLI_HINTS` suppresses the "What's next:" advertisement the CLI appends
        // to many commands — four lines of nothing, on every call. The progress settings
        // keep `compose` and BuildKit from emitting terminal-control spinners into a pipe
        // that is going to be read as text.
        cmd.env("DOCKER_CLI_HINTS", "false")
            .env("BUILDKIT_PROGRESS", "plain")
            .env("COMPOSE_PROGRESS", "plain")
            .env("NO_COLOR", "1");

        tracing::debug!(program, ?args, ?cwd, ?timeout, "running command");
        let mut child = cmd.spawn()?;

        // Drain both pipes concurrently; see the module note about buffer deadlock.
        let mut out = child.stdout.take().expect("stdout piped");
        let mut err = child.stderr.take().expect("stderr piped");
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    tracing::warn!(program, ?args, "deadline reached, killing");
                    let _ = child.kill();
                    timed_out = true;
                    break child.wait()?;
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };

        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        Ok(CommandOutput {
            status: status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out,
        })
    }
}

/// Test adapter: replays canned output keyed by `"program arg arg ..."`.
///
/// A key that was never registered comes back as a failure naming itself, which makes a
/// test that builds the wrong command line fail with the command line it built.
#[derive(Debug, Default)]
pub struct ScriptedRunner {
    replies: HashMap<String, CommandOutput>,
    calls: Mutex<Vec<String>>,
    cwds: Mutex<Vec<Option<PathBuf>>>,
}

impl ScriptedRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register stdout for a command that succeeds.
    pub fn with(self, command: &str, stdout: &str) -> Self {
        self.reply(command, 0, stdout, "", false)
    }

    /// Register a command that fails, with its stderr.
    pub fn failing(self, command: &str, status: i32, stderr: &str) -> Self {
        self.reply(command, status, "", stderr, false)
    }

    /// Register a command that overruns its deadline.
    pub fn timing_out(self, command: &str) -> Self {
        self.reply(command, -1, "", "", true)
    }

    pub fn reply(
        mut self,
        command: &str,
        status: i32,
        stdout: &str,
        stderr: &str,
        timed_out: bool,
    ) -> Self {
        self.replies.insert(
            command.to_owned(),
            CommandOutput {
                status,
                stdout: stdout.to_owned(),
                stderr: stderr.to_owned(),
                timed_out,
            },
        );
        self
    }

    /// Every command line this runner was asked for, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }

    /// The working directories those calls were made in.
    pub fn cwds(&self) -> Vec<Option<PathBuf>> {
        self.cwds.lock().expect("lock").clone()
    }

    fn key(program: &str, args: &[&str]) -> String {
        let mut key = String::from(program);
        for arg in args {
            key.push(' ');
            key.push_str(arg);
        }
        key
    }
}

impl CommandRunner for ScriptedRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        _timeout: Duration,
    ) -> std::io::Result<CommandOutput> {
        let key = Self::key(program, args);
        self.calls.lock().expect("lock").push(key.clone());
        self.cwds
            .lock()
            .expect("lock")
            .push(cwd.map(Path::to_path_buf));
        Ok(self.replies.get(&key).cloned().unwrap_or(CommandOutput {
            status: 127,
            stdout: String::new(),
            stderr: format!("scripted runner has no reply for `{key}`"),
            timed_out: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_reported_without_the_daemon_boilerplate() {
        let out = CommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "Error response from daemon: No such container: web\n".to_owned(),
            timed_out: false,
        };
        assert_eq!(out.failure(), "No such container: web");
    }

    #[test]
    fn a_timeout_outranks_whatever_the_process_managed_to_say() {
        let out = CommandOutput {
            status: -1,
            stdout: "half a line".to_owned(),
            stderr: String::new(),
            timed_out: true,
        };
        assert!(!out.ok());
        assert_eq!(out.failure(), "timed out");
    }

    #[test]
    fn an_unscripted_command_fails_by_naming_itself() {
        let runner = ScriptedRunner::new();
        let out = runner
            .run("docker", &["ps", "-a"], None, DEFAULT_TIMEOUT)
            .expect("spawn");
        assert!(!out.ok());
        assert!(out.stderr.contains("docker ps -a"), "{}", out.stderr);
        assert_eq!(runner.calls(), vec!["docker ps -a"]);
    }

    #[test]
    fn blank_lines_never_reach_a_parser() {
        let out = CommandOutput {
            status: 0,
            stdout: "one\n\ntwo\n\n".to_owned(),
            stderr: String::new(),
            timed_out: false,
        };
        assert_eq!(out.stdout_lines().collect::<Vec<_>>(), vec!["one", "two"]);
    }
}
