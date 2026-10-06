//! A command in a container, with its output filtered the way a log is.
//!
//! The shell version of this is `docker exec ens-wine-1 steamcmd +app_update 2278520 |
//! grep -v '^Update state'`, and the reason for the `grep` is that steamcmd prints a
//! progress line per percent: four hundred lines of which one — the failure — is the
//! answer. Running the filter here instead means the four hundred lines never cross the
//! wire, and the line that mattered arrives with the exit status attached to it.
//!
//! Nothing here allocates a TTY. `-t` makes a program paint a progress bar in escape
//! codes, and `-i` waits for input that is never coming.

use serde::Serialize;

use crate::{
    Budget, Error, Result,
    invoke::{self, ComposeScope, Invocation, matcher, safe_value},
    runner::CommandRunner,
    text::plural,
};

pub const DEFAULT_MAX_LINES: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct ExecRequest {
    pub invocation: Invocation,
    /// Set to address a compose service rather than a container by name.
    pub scope: ComposeScope,
    /// A container name or id, or a compose service when `scope` is set.
    pub target: String,
    /// The command, already split into arguments. No shell is involved, so there is
    /// nothing to quote and nothing to escape.
    pub argv: Vec<String>,
    /// A shell command line instead of `argv`, run as `sh -c`. This is how a pipeline or a
    /// redirect gets run; it is deliberate, named, and the only way to get a shell.
    pub shell: Option<String>,
    /// `--user`.
    pub user: Option<String>,
    /// `--workdir`.
    pub workdir: Option<String>,
    /// `--env`, as `KEY=value`.
    pub env: Vec<String>,
    /// Keep only lines matching this regex.
    pub grep: Option<String>,
    /// Drop lines matching this regex. Applied after `grep`, which is the order that makes
    /// `grep: "state"`, `exclude: "0x61"` mean what it reads like.
    pub exclude: Option<String>,
    pub ignore_case: bool,
    /// Keep the first lines rather than the last.
    pub head: bool,
    /// Cap on lines rendered. Defaults to 200.
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ExecOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub target: String,
    pub command: String,
    /// The command's own exit status, which is the fact a pipeline throws away.
    pub status: i32,
    pub lines_shown: usize,
    pub lines_total: usize,
    pub filtered_out: usize,
    /// How many of the rendered lines came from stderr.
    pub stderr_lines: usize,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn exec(runner: &dyn CommandRunner, request: &ExecRequest) -> Result<ExecOutcome> {
    safe_value(&request.target, "target")?;
    if request.argv.is_empty() && request.shell.is_none() {
        return Err(Error::request(
            "nothing to run: send `argv` as a list (`[\"ls\", \"-la\", \"/data\"]`) or \
             `shell` as one command line",
        ));
    }
    if !request.argv.is_empty() && request.shell.is_some() {
        return Err(Error::request(
            "`argv` and `shell` both given; pick one — `argv` runs the binary directly, \
             `shell` runs `sh -c` so that pipes and redirects work",
        ));
    }

    let mut rest = if request.scope.is_empty() {
        vec!["exec".to_owned()]
    } else {
        let mut args = request.scope.args()?;
        args.push("exec".to_owned());
        // Compose's exec allocates a TTY unless told not to, and a TTY turns the output
        // into escape codes and the call into a wait.
        args.push("-T".to_owned());
        args
    };

    if let Some(user) = &request.user {
        safe_value(user, "user")?;
        rest.push("--user".to_owned());
        rest.push(user.clone());
    }
    if let Some(workdir) = &request.workdir {
        safe_value(workdir, "workdir")?;
        rest.push("--workdir".to_owned());
        rest.push(workdir.clone());
    }
    for entry in &request.env {
        safe_value(entry, "env entry")?;
        if !entry.contains('=') {
            return Err(Error::request(format!(
                "env entry `{entry}` is not `KEY=value`"
            )));
        }
        rest.push("--env".to_owned());
        rest.push(entry.clone());
    }

    rest.push(request.target.clone());
    match &request.shell {
        Some(line) => {
            if line.contains('\0') {
                return Err(Error::request("shell command contains a NUL byte"));
            }
            rest.push("sh".to_owned());
            rest.push("-c".to_owned());
            rest.push(line.clone());
        }
        None => {
            // Only the first word can be mistaken for a flag by `docker exec`; everything
            // after it belongs to the program being run, where `-la` is the point.
            safe_value(&request.argv[0], "command")?;
            rest.extend(request.argv.iter().cloned());
        }
    }

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    let stdout: Vec<&str> = output.stdout.lines().collect();
    let stderr: Vec<&str> = output.stderr.lines().collect();
    let total = stdout.len() + stderr.len();

    let grep = matcher(request.grep.as_deref(), request.ignore_case, "grep")?;
    let exclude = matcher(request.exclude.as_deref(), request.ignore_case, "exclude")?;
    let keep = |line: &str| {
        grep.as_ref().map(|re| re.is_match(line)).unwrap_or(true)
            && !exclude
                .as_ref()
                .map(|re| re.is_match(line))
                .unwrap_or(false)
    };

    let kept_out: Vec<&str> = stdout.iter().copied().filter(|line| keep(line)).collect();
    let kept_err: Vec<&str> = stderr.iter().copied().filter(|line| keep(line)).collect();
    let surviving = kept_out.len() + kept_err.len();
    let filtered_out = total - surviving;

    // stderr after stdout, rather than interleaved: the two streams have no shared
    // ordering once a pipe has buffered them, and inventing one would be a lie. For a
    // long-running container the tool that *can* order them is `logs`.
    let mut lines: Vec<(&str, bool)> = kept_out
        .iter()
        .map(|line| (*line, false))
        .chain(kept_err.iter().map(|line| (*line, true)))
        .collect();

    let max_lines = request.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let over = lines.len().saturating_sub(max_lines);
    if over > 0 {
        if request.head {
            lines.truncate(max_lines);
        } else {
            lines.drain(..over);
        }
    }

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut shown = 0;
    let mut stderr_lines = 0;
    for (line, is_stderr) in &lines {
        let rendered = format!("{line}\n");
        if !budget.try_spend(&rendered) {
            break;
        }
        body.push_str(&rendered);
        shown += 1;
        if *is_stderr {
            stderr_lines += 1;
        }
    }

    let cut = lines.len() - shown + over;
    let truncated = (cut > 0).then(|| {
        if over > 0 && shown == lines.len() {
            format!(
                "{cut} more, over max_lines {max_lines} ({})",
                if request.head {
                    "first kept"
                } else {
                    "last kept"
                }
            )
        } else {
            format!("{cut} not shown, byte budget reached")
        }
    });

    let mut footer = format!("[{} of {}", plural(shown, "line"), plural(total, "line"));
    if filtered_out > 0 {
        footer.push_str(&format!(", {filtered_out} filtered"));
    }
    if stderr_lines > 0 {
        footer.push_str(&format!(", {stderr_lines} on stderr"));
    }
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str(&if output.timed_out {
        format!(
            ", timed out after {}s]\n",
            request.invocation.timeout().as_secs()
        )
    } else {
        format!(", exit {}]\n", output.status)
    });

    Ok(ExecOutcome {
        text: format!("{body}{footer}"),
        target: request.target.clone(),
        command,
        status: output.status,
        lines_shown: shown,
        lines_total: total,
        filtered_out,
        stderr_lines,
        timed_out: output.timed_out,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    /// The call this tool was written for: four hundred progress lines and one failure.
    fn steamcmd() -> String {
        let mut stdout = String::new();
        for percent in 0..200 {
            stdout.push_str(&format!(
                "Update state (0x61) downloading, progress: {percent}.00 (1000 / 2000)\n"
            ));
        }
        stdout.push_str("Error! App '2278520' state is 0x202 after update job.\n");
        stdout
    }

    #[test]
    fn the_one_line_that_mattered_arrives_with_its_exit_status() {
        let runner = ScriptedRunner::new().reply(
            "docker exec ens-wine-1 steamcmd +app_update 2278520 validate +quit",
            1,
            &steamcmd(),
            "",
            false,
        );
        let outcome = exec(
            &runner,
            &ExecRequest {
                target: "ens-wine-1".to_owned(),
                argv: vec![
                    "steamcmd".to_owned(),
                    "+app_update".to_owned(),
                    "2278520".to_owned(),
                    "validate".to_owned(),
                    "+quit".to_owned(),
                ],
                grep: Some("error|failed".to_owned()),
                ignore_case: true,
                ..ExecRequest::default()
            },
        )
        .expect("exec");

        assert_eq!(
            outcome.text,
            "\
Error! App '2278520' state is 0x202 after update job.
[1 line of 201 lines, 200 filtered, exit 1]
"
        );
        assert_eq!(outcome.status, 1);
    }

    #[test]
    fn exclude_is_the_other_half_of_the_pipeline() {
        let runner = ScriptedRunner::new().reply(
            "docker exec ens-wine-1 steamcmd",
            0,
            "Update state (0x61) downloading\nSuccess! App '2278520' fully installed.\n",
            "",
            false,
        );
        let outcome = exec(
            &runner,
            &ExecRequest {
                target: "ens-wine-1".to_owned(),
                argv: vec!["steamcmd".to_owned()],
                exclude: Some("^Update state".to_owned()),
                ..ExecRequest::default()
            },
        )
        .expect("exec");
        assert_eq!(
            outcome.text,
            "Success! App '2278520' fully installed.\n[1 line of 2 lines, 1 filtered, exit 0]\n"
        );
    }

    #[test]
    fn a_compose_service_is_addressed_without_a_tty() {
        let runner = ScriptedRunner::new().reply(
            "docker compose -p ens exec -T wine ls /home/steam",
            0,
            "enshrouded\n",
            "",
            false,
        );
        let outcome = exec(
            &runner,
            &ExecRequest {
                scope: ComposeScope {
                    project: Some("ens".to_owned()),
                    ..ComposeScope::default()
                },
                target: "wine".to_owned(),
                argv: vec!["ls".to_owned(), "/home/steam".to_owned()],
                ..ExecRequest::default()
            },
        )
        .expect("exec");
        assert_eq!(outcome.text, "enshrouded\n[1 line of 1 line, exit 0]\n");
    }

    #[test]
    fn a_pipeline_needs_a_shell_and_has_to_say_so() {
        let runner = ScriptedRunner::new().reply(
            "docker exec ens-wine-1 sh -c ps aux | grep -c enshrouded",
            0,
            "2\n",
            "",
            false,
        );
        exec(
            &runner,
            &ExecRequest {
                target: "ens-wine-1".to_owned(),
                shell: Some("ps aux | grep -c enshrouded".to_owned()),
                ..ExecRequest::default()
            },
        )
        .expect("exec");
        assert_eq!(
            runner.calls()[0],
            "docker exec ens-wine-1 sh -c ps aux | grep -c enshrouded"
        );
    }

    #[test]
    fn argv_and_shell_together_is_a_request_error() {
        let err = exec(
            &ScriptedRunner::new(),
            &ExecRequest {
                target: "api".to_owned(),
                argv: vec!["ls".to_owned()],
                shell: Some("ls".to_owned()),
                ..ExecRequest::default()
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("pick one"), "{err}");
    }

    #[test]
    fn nothing_to_run_says_what_to_send_instead() {
        let err = exec(
            &ScriptedRunner::new(),
            &ExecRequest {
                target: "api".to_owned(),
                ..ExecRequest::default()
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("argv"), "{err}");
    }

    #[test]
    fn stderr_comes_after_stdout_and_is_counted_separately() {
        let runner = ScriptedRunner::new().reply(
            "docker exec api env",
            2,
            "PATH=/usr/bin\n",
            "env: cannot read /proc\n",
            false,
        );
        let outcome = exec(
            &runner,
            &ExecRequest {
                target: "api".to_owned(),
                argv: vec!["env".to_owned()],
                ..ExecRequest::default()
            },
        )
        .expect("exec");
        assert_eq!(
            outcome.text,
            "\
PATH=/usr/bin
env: cannot read /proc
[2 lines of 2 lines, 1 on stderr, exit 2]
"
        );
    }

    #[test]
    fn a_command_that_overran_its_deadline_says_so_instead_of_an_exit_code() {
        let runner = ScriptedRunner::new().timing_out("docker exec api sleep 999");
        let outcome = exec(
            &runner,
            &ExecRequest {
                target: "api".to_owned(),
                argv: vec!["sleep".to_owned(), "999".to_owned()],
                invocation: Invocation {
                    timeout_seconds: Some(5),
                    ..Invocation::default()
                },
                ..ExecRequest::default()
            },
        )
        .expect("exec");
        assert_eq!(outcome.text, "[0 lines of 0 lines, timed out after 5s]\n");
        assert!(outcome.timed_out);
    }

    #[test]
    fn max_lines_keeps_the_end_by_default() {
        let stdout: String = (0..10).map(|n| format!("line {n}\n")).collect();
        let runner = ScriptedRunner::new().reply("docker exec api cat /log", 0, &stdout, "", false);
        let outcome = exec(
            &runner,
            &ExecRequest {
                target: "api".to_owned(),
                argv: vec!["cat".to_owned(), "/log".to_owned()],
                max_lines: Some(2),
                ..ExecRequest::default()
            },
        )
        .expect("exec");
        assert_eq!(
            outcome.text,
            "line 8\nline 9\n[2 lines of 10 lines, 8 more, over max_lines 2 (last kept), exit 0]\n"
        );
    }
}
