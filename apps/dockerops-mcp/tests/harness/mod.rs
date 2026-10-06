//! An MCP client, and a Docker that is a shell script.
//!
//! These tests drive the built binary over its own stdio, which means they need a
//! `docker` to talk to — and must not need a daemon, a network, or anything on the
//! machine running them. So `Fake::new()` writes a `docker` script into a tempdir and puts
//! that tempdir first on the server's `PATH`. The script answers from canned replies
//! keyed by its own arguments, and records every call, so a test can assert both the
//! rendered output and the command that produced it.

#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

/// A fake `docker` on a PATH of its own.
pub struct Fake {
    dir: tempfile::TempDir,
    replies: Vec<(String, String, String, i32)>,
}

impl Fake {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            replies: Vec::new(),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Reply to a call whose arguments contain `when` — a substring of the whole argument
    /// line, so a test names the part it cares about.
    pub fn reply(mut self, when: &str, stdout: &str) -> Self {
        self.replies
            .push((when.to_owned(), stdout.to_owned(), String::new(), 0));
        self
    }

    /// Reply on stderr with a non-zero status, the way a missing container does.
    pub fn fail(mut self, when: &str, stderr: &str) -> Self {
        self.replies
            .push((when.to_owned(), String::new(), stderr.to_owned(), 1));
        self
    }

    /// Reply on both streams at once, to test that they are merged in timestamp order.
    pub fn reply_both(mut self, when: &str, stdout: &str, stderr: &str) -> Self {
        self.replies
            .push((when.to_owned(), stdout.to_owned(), stderr.to_owned(), 0));
        self
    }

    /// Write the script and start the server with it on PATH.
    pub fn server(&self) -> Server {
        let mut script = String::from(
            "#!/usr/bin/env bash\nargs=\"$*\"\nprintf '%s\\n' \"$args\" >> \"$(dirname \"$0\")/calls\"\n",
        );
        for (when, stdout, stderr, status) in &self.replies {
            script.push_str(&format!(
                "if [[ \"$args\" == *{when}* ]]; then printf '%s' {stdout}; printf '%s' {stderr} >&2; exit {status}; fi\n",
                when = shell_quote(when),
                stdout = shell_quote(stdout),
                stderr = shell_quote(stderr),
            ));
        }
        script.push_str("printf 'unscripted: %s\\n' \"$args\" >&2\nexit 127\n");

        let docker = self.dir.path().join("docker");
        std::fs::write(&docker, script).expect("write docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755))
                .expect("chmod docker");
        }

        Server::start(self.dir.path())
    }

    /// Every argument line the fake was called with, in order.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The server, spoken to the way a client speaks to it.
pub struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    pub instructions: String,
}

impl Server {
    fn start(path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dockerops-mcp"))
            .current_dir(path)
            // The fake comes first, so `docker` is the script and nothing reaches a daemon.
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    path.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dockerops-mcp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            instructions: String::new(),
        };

        let hello = server.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "harness", "version": "0"},
            }),
        );
        server.instructions = hello["instructions"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        server.notify("notifications/initialized");
        server
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }));
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read");
            assert!(
                read > 0,
                "the server closed stdout before answering {method}"
            );
            let message: serde_json::Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                // Anything that is not a JSON-RPC message on stdout would break a real
                // client; the protocol owns this stream, and logs belong on stderr.
                Err(err) => panic!("non-protocol output on stdout: {line:?} ({err})"),
            };
            if message["id"] == serde_json::json!(id) {
                assert!(
                    message.get("error").is_none(),
                    "{method} failed: {}",
                    message["error"]
                );
                return message["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str) {
        self.send(serde_json::json!({"jsonrpc": "2.0", "method": method}));
    }

    fn send(&mut self, value: serde_json::Value) {
        writeln!(self.stdin, "{value}").expect("write");
        self.stdin.flush().expect("flush");
    }

    pub fn tools(&mut self) -> serde_json::Value {
        self.request("tools/list", serde_json::json!({}))
    }

    pub fn call(&mut self, tool: &str, arguments: serde_json::Value) -> ToolResult {
        ToolResult(self.request(
            "tools/call",
            serde_json::json!({"name": tool, "arguments": arguments}),
        ))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct ToolResult(serde_json::Value);

impl ToolResult {
    pub fn is_error(&self) -> bool {
        self.0["isError"].as_bool().unwrap_or(false)
    }

    /// The text content block — the half most hosts render.
    pub fn text(&self) -> String {
        self.0["content"]
            .as_array()
            .expect("content")
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect()
    }

    /// The structured half — the one Claude Code renders.
    pub fn structured(&self) -> &serde_json::Value {
        &self.0["structuredContent"]
    }
}

/// A docker `logs --timestamps` line, so a test does not spell out nanoseconds.
pub fn stamped(time: &str, text: &str) -> String {
    format!("2026-10-06T{time}.000000000Z {text}\n")
}

/// Where a fake lives, for a test that needs the directory itself.
pub fn dir_of(fake: &Fake) -> PathBuf {
    fake.path().to_path_buf()
}
