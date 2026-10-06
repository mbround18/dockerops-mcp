//! Log tails from several targets at once, filtered the way you were going to filter them
//! anyway.
//!
//! This is the tool the whole server is for. The shell version of this question is
//!
//! ```text
//! docker compose logs api 2>&1 | grep -iE 'error|panic' | tail -40
//! ```
//!
//! which costs a round trip to write, usually another to fix, and cannot ask about two
//! containers without being written twice. Here it is one call with a list of targets,
//! and four behaviours the pipeline does not have:
//!
//! * **It never follows.** There is no `follow` parameter. `docker logs -f` does not
//!   terminate, and a tool call that does not terminate takes the conversation with it.
//!   A caller who wants the next lines calls again with `since`.
//! * **stdout and stderr come back in order.** `docker logs` writes the container's two
//!   streams to its own two streams, so anything capturing them separately gets all the
//!   output and none of the chronology. Timestamps are therefore always requested, used
//!   to merge the streams, and then thrown away unless the caller wanted them — so
//!   correctness here costs the caller nothing.
//! * **Repetition collapses.** A container in a crash loop says the same line hundreds of
//!   times. `… ×147` is the same information.
//! * **The filter is part of the call.** `grep`, `exclude`, `level` and `context` run
//!   before the byte budget, so a budget spent on a thousand lines of healthy startup
//!   noise is a budget spent on the forty lines that matter instead.

use regex::RegexBuilder;
use serde::Serialize;

use crate::{
    Budget, Error, Result,
    invoke::{self, ComposeScope, Invocation, safe_value},
    runner::CommandRunner,
    text::{compact_count, plural},
};

/// Lines asked of Docker per target when the caller names no tail.
pub const DEFAULT_TAIL: usize = 80;

/// Lines rendered per target after filtering, when the caller names no cap.
pub const DEFAULT_MAX_LINES: usize = 200;

/// A convenience over writing the same regex every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Lines that look like a failure: `error`, `fatal`, `panic`, `exception`, a 5xx.
    Error,
    /// The above, plus warnings.
    Warn,
}

impl Level {
    fn pattern(self) -> &'static str {
        match self {
            Self::Error => {
                r"\b(err|error|errors|fatal|panic|panicked|exception|critical|segfault|traceback)\b|\b5\d\d\b"
            }
            Self::Warn => {
                r"\b(err|error|errors|fatal|panic|panicked|exception|critical|segfault|traceback|warn|warning|deprecated)\b|\b[45]\d\d\b"
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogsRequest {
    pub invocation: Invocation,
    /// Naming a project or compose file routes the call through `docker compose logs`.
    pub scope: ComposeScope,
    /// Containers or compose services. Empty, with a compose scope, means the whole
    /// project — one call, with each line attributed to the service that wrote it.
    pub targets: Vec<String>,
    /// Lines to ask Docker for, per target, before filtering. Defaults to 80.
    pub tail: Option<usize>,
    /// Only lines since this point: `10m`, `2h`, `2026-10-06T12:00:00`.
    pub since: Option<String>,
    /// Only lines before this point, same syntax.
    pub until: Option<String>,
    /// Keep only lines matching any of these regexes.
    pub grep: Vec<String>,
    /// Drop lines matching any of these regexes, applied after `grep`.
    pub exclude: Vec<String>,
    /// A built-in pattern for the usual question.
    pub level: Option<Level>,
    /// Lines of context either side of a kept line.
    pub context: Option<usize>,
    /// Case-insensitive matching for `grep` and `exclude`. On by default: logs are prose.
    pub ignore_case: Option<bool>,
    /// Keep the timestamp on each line, as `HH:MM:SS`.
    pub timestamps: bool,
    /// Fold runs of identical lines into one with a `×N` count. On by default.
    pub collapse: Option<bool>,
    /// Render the start of the window rather than the end — the first lines of a startup
    /// rather than the last lines of a failure.
    pub head: bool,
    /// Cap on rendered lines per target, after filtering. Defaults to 200.
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

/// One target's logs.
#[derive(Debug, Serialize)]
pub struct TargetLogs {
    pub target: String,
    /// `ok`, or why there are no lines.
    pub status: String,
    /// Lines Docker returned, before filtering.
    pub available: usize,
    /// Lines rendered.
    pub shown: usize,
    /// Lines the filter removed.
    pub filtered_out: usize,
    /// Lines folded away by `collapse`.
    pub collapsed: usize,
    /// How many of the rendered lines the container wrote to stderr. A service that logs
    /// everything there looks broken in `docker logs` and is not.
    pub stderr_lines: usize,
}

#[derive(Debug, Serialize)]
pub struct LogsOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub targets: Vec<TargetLogs>,
    /// The commands that produced this, so a caller can reproduce them in a terminal.
    pub commands: Vec<String>,
    pub lines_shown: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// One log line, before rendering.
#[derive(Debug, Clone)]
struct Line {
    /// RFC3339 timestamp as Docker wrote it, used to merge the two streams.
    stamp: Option<String>,
    text: String,
    /// True when the container wrote this to stderr.
    stderr: bool,
}

pub fn logs(runner: &dyn CommandRunner, request: &LogsRequest) -> Result<LogsOutcome> {
    for target in &request.targets {
        safe_value(target, "target")?;
    }
    if let Some(since) = &request.since {
        safe_value(since, "since")?;
    }
    if let Some(until) = &request.until {
        safe_value(until, "until")?;
    }
    if request.targets.is_empty() && request.scope.is_empty() {
        return Err(Error::request(
            "name at least one container in `targets`, or a compose `project`/`files` to \
             read a whole project's logs",
        ));
    }

    let ignore_case = request.ignore_case.unwrap_or(true);
    let keep = compile(&patterns(request), ignore_case, "grep")?;
    let drop = compile(&request.exclude, ignore_case, "exclude")?;
    let tail = request.tail.unwrap_or(DEFAULT_TAIL);
    let max_lines = request.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let collapse = request.collapse.unwrap_or(true);
    let compose = !request.scope.is_empty();

    // One call per named target keeps every line attributable without parsing compose's
    // `service  |` prefix. With no targets there is one call for the whole project, and
    // the prefix is how each line is attributed.
    let labels: Vec<String> = if request.targets.is_empty() {
        vec![
            request
                .scope
                .project
                .clone()
                .unwrap_or_else(|| "project".to_owned()),
        ]
    } else {
        request.targets.clone()
    };

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut commands = Vec::new();
    let mut summaries = Vec::new();
    let mut lines_shown = 0;
    let mut cut = 0;

    for (index, label) in labels.iter().enumerate() {
        let mut rest: Vec<String> = if compose {
            request.scope.args()?
        } else {
            Vec::new()
        };
        rest.push("logs".to_owned());
        // Always timestamped: see the module note about merging the two streams.
        rest.push("--timestamps".to_owned());
        rest.push("--tail".to_owned());
        rest.push(tail.to_string());
        if let Some(since) = &request.since {
            rest.push("--since".to_owned());
            rest.push(since.clone());
        }
        if let Some(until) = &request.until {
            rest.push("--until".to_owned());
            rest.push(until.clone());
        }
        if compose {
            rest.push("--no-color".to_owned());
            if !request.targets.is_empty() {
                rest.push("--no-log-prefix".to_owned());
                rest.push(label.clone());
            }
        } else {
            rest.push(label.clone());
        }

        let args = invoke::command(&request.invocation, rest)?;
        let output = invoke::run(runner, &request.invocation, &args)?;
        commands.push(format!("{} {}", request.invocation.bin(), args.join(" ")));

        if !output.ok() && output.stdout.trim().is_empty() {
            let status = output.failure();
            let header = format!("#{} {label} — {status}\n", index + 1);
            if budget.try_spend(&header) {
                body.push_str(&header);
            } else {
                cut += 1;
            }
            summaries.push(TargetLogs {
                target: label.clone(),
                status,
                available: 0,
                shown: 0,
                filtered_out: 0,
                collapsed: 0,
                stderr_lines: 0,
            });
            continue;
        }

        let merged = merge(parse(&output.stdout, false), parse(&output.stderr, true));
        let available = merged.len();

        let (kept, filtered_out) = filter(&merged, &keep, &drop, request.context.unwrap_or(0));
        let (mut rendered, collapsed) = if collapse {
            fold(&kept)
        } else {
            (kept.iter().map(|line| (line.clone(), 1usize)).collect(), 0)
        };

        // The window is the end of the log unless the caller asked for its beginning.
        let over = rendered.len().saturating_sub(max_lines);
        if over > 0 {
            if request.head {
                rendered.truncate(max_lines);
            } else {
                rendered.drain(..over);
            }
        }

        let header = header_for(
            index + 1,
            label,
            rendered.len(),
            available,
            tail,
            filtered_out,
            over,
        );
        if !budget.try_spend(&header) {
            cut += 1;
            summaries.push(TargetLogs {
                target: label.clone(),
                status: "not shown, byte budget reached".to_owned(),
                available,
                shown: 0,
                filtered_out,
                collapsed,
                stderr_lines: 0,
            });
            continue;
        }
        body.push_str(&header);

        let mut shown = 0;
        for (line, repeats) in &rendered {
            let text = render_line(line, *repeats, request.timestamps);
            if !budget.try_spend(&text) {
                cut += rendered.len() - shown;
                break;
            }
            body.push_str(&text);
            shown += 1;
        }
        lines_shown += shown;
        summaries.push(TargetLogs {
            target: label.clone(),
            status: "ok".to_owned(),
            available,
            shown,
            filtered_out,
            collapsed,
            stderr_lines: rendered
                .iter()
                .take(shown)
                .filter(|(line, _)| line.stderr)
                .count(),
        });
    }

    let truncated = (cut > 0).then(|| format!("{cut} not shown, byte budget reached"));
    let mut footer = format!(
        "[{}, {}",
        plural(summaries.len(), "target"),
        plural(lines_shown, "line")
    );
    let filtered: usize = summaries.iter().map(|s| s.filtered_out).sum();
    if filtered > 0 {
        footer.push_str(&format!(", {} filtered out", compact_count(filtered)));
    }
    let collapsed: usize = summaries.iter().map(|s| s.collapsed).sum();
    if collapsed > 0 {
        footer.push_str(&format!(", {} repeats folded", compact_count(collapsed)));
    }
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(LogsOutcome {
        text: format!("{body}{footer}"),
        targets: summaries,
        commands,
        lines_shown,
        truncated,
    })
}

/// `grep` plus whatever `level` adds.
fn patterns(request: &LogsRequest) -> Vec<String> {
    let mut patterns = request.grep.clone();
    if let Some(level) = request.level {
        patterns.push(level.pattern().to_owned());
    }
    patterns
}

fn compile(patterns: &[String], ignore_case: bool, what: &str) -> Result<Vec<regex::Regex>> {
    patterns
        .iter()
        .map(|pattern| {
            RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|err| {
                    Error::request(format!("{what} pattern `{pattern}` is not a regex: {err}"))
                })
        })
        .collect()
}

/// Split Docker's timestamped output into lines.
fn parse(stream: &str, stderr: bool) -> Vec<Line> {
    stream
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (stamp, text) = split_stamp(line);
            Line {
                stamp,
                text,
                stderr,
            }
        })
        .collect()
}

/// `2026-10-06T12:04:11.123456789Z message` -> `(stamp, "message")`.
///
/// Compose prefixes each line with the service that wrote it when logging a whole
/// project - `web-1  | 2026-...Z message`. The timestamp sits between the prefix and the
/// message, so removing it means putting the two back together: the prefix is the only
/// thing attributing the line, and it is kept as `web-1 | message`.
fn split_stamp(line: &str) -> (Option<String>, String) {
    let (prefix, rest) = match line.split_once("| ") {
        Some((prefix, rest)) if looks_like_stamp(first_word(rest)) => (Some(prefix.trim()), rest),
        _ => (None, line),
    };
    let Some((candidate, text)) = rest.split_once(' ') else {
        return (None, line.to_owned());
    };
    if !looks_like_stamp(candidate) {
        return (None, line.to_owned());
    }
    let text = match prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix} | {text}"),
        _ => text.to_owned(),
    };
    (Some(candidate.to_owned()), text)
}

fn first_word(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

fn looks_like_stamp(candidate: &str) -> bool {
    candidate.len() >= 20
        && candidate.as_bytes()[4] == b'-'
        && candidate.contains('T')
        && (candidate.ends_with('Z') || candidate.contains('+'))
}

/// Merge two chronological streams into one. Lines without a timestamp stay with the
/// stream they came from, in order.
fn merge(out: Vec<Line>, err: Vec<Line>) -> Vec<Line> {
    if err.is_empty() {
        return out;
    }
    if out.is_empty() {
        return err;
    }
    let mut merged = Vec::with_capacity(out.len() + err.len());
    let mut a = out.into_iter().peekable();
    let mut b = err.into_iter().peekable();
    loop {
        match (a.peek(), b.peek()) {
            (Some(left), Some(right)) => {
                let take_left = match (&left.stamp, &right.stamp) {
                    (Some(left), Some(right)) => left <= right,
                    // Without a timestamp there is nothing to order by, so stdout first
                    // keeps the common case (a container that logs to stdout) intact.
                    _ => true,
                };
                if take_left {
                    merged.push(a.next().expect("peeked"));
                } else {
                    merged.push(b.next().expect("peeked"));
                }
            }
            (Some(_), None) => merged.push(a.next().expect("peeked")),
            (None, Some(_)) => merged.push(b.next().expect("peeked")),
            (None, None) => break,
        }
    }
    merged
}

/// Apply `grep`/`exclude` with context, returning the kept lines and how many were
/// dropped.
fn filter(
    lines: &[Line],
    keep: &[regex::Regex],
    drop: &[regex::Regex],
    context: usize,
) -> (Vec<Line>, usize) {
    let surviving: Vec<bool> = lines
        .iter()
        .map(|line| !drop.iter().any(|re| re.is_match(&line.text)))
        .collect();

    if keep.is_empty() {
        let kept: Vec<Line> = lines
            .iter()
            .zip(&surviving)
            .filter(|(_, alive)| **alive)
            .map(|(line, _)| line.clone())
            .collect();
        let dropped = lines.len() - kept.len();
        return (kept, dropped);
    }

    let mut wanted = vec![false; lines.len()];
    for (index, line) in lines.iter().enumerate() {
        if surviving[index] && keep.iter().any(|re| re.is_match(&line.text)) {
            let from = index.saturating_sub(context);
            let to = (index + context).min(lines.len() - 1);
            // Context never resurrects a line the caller excluded.
            wanted[from..=to].copy_from_slice(&surviving[from..=to]);
        }
    }
    let kept: Vec<Line> = lines
        .iter()
        .zip(&wanted)
        .filter(|(_, want)| **want)
        .map(|(line, _)| line.clone())
        .collect();
    let dropped = lines.len() - kept.len();
    (kept, dropped)
}

/// Fold runs of identical messages, returning each line with its repeat count and the
/// number of lines folded away.
fn fold(lines: &[Line]) -> (Vec<(Line, usize)>, usize) {
    let mut folded: Vec<(Line, usize)> = Vec::new();
    let mut collapsed = 0;
    for line in lines {
        match folded.last_mut() {
            Some((previous, count)) if previous.text == line.text => {
                *count += 1;
                collapsed += 1;
            }
            _ => folded.push((line.clone(), 1)),
        }
    }
    (folded, collapsed)
}

fn render_line(line: &Line, repeats: usize, timestamps: bool) -> String {
    let mut out = String::new();
    if timestamps
        && let Some(stamp) = &line.stamp
        && let Some(time) = clock(stamp)
    {
        out.push_str(&time);
        out.push(' ');
    }
    out.push_str(line.text.trim_end());
    if repeats > 1 {
        out.push_str(&format!(" ×{repeats}"));
    }
    out.push('\n');
    out
}

/// `2026-10-06T12:04:11.123456789Z` → `12:04:11`. The date is the same for every line in
/// a tail, and the nanoseconds have never answered a question.
fn clock(stamp: &str) -> Option<String> {
    let time = stamp.split('T').nth(1)?;
    let hhmmss: String = time.chars().take(8).collect();
    (hhmmss.len() == 8).then_some(hhmmss)
}

#[allow(clippy::too_many_arguments)]
fn header_for(
    index: usize,
    label: &str,
    shown: usize,
    available: usize,
    tail: usize,
    filtered_out: usize,
    over: usize,
) -> String {
    let mut notes = Vec::new();
    if filtered_out > 0 {
        notes.push(format!("{} filtered", compact_count(filtered_out)));
    }
    if over > 0 {
        notes.push(format!("{} over the line cap", compact_count(over)));
    }
    if available >= tail {
        // Docker gave back everything that was asked for, so there is probably more.
        notes.push(format!("tail {tail}"));
    }
    let note = if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join(", "))
    };
    format!("#{index} {label} {shown}/{available}{note}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const BASE: &str = "docker logs --timestamps --tail 80";

    fn stamped(lines: &[(&str, &str)]) -> String {
        lines
            .iter()
            .map(|(time, text)| format!("2026-10-06T{time}.000000000Z {text}\n"))
            .collect()
    }

    #[test]
    fn a_tail_is_rendered_without_the_timestamps_that_fetched_it() {
        let runner = ScriptedRunner::new().with(
            &format!("{BASE} api"),
            &stamped(&[("12:04:11", "listening on :8080"), ("12:04:12", "ready")]),
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect("read");

        assert_eq!(
            outcome.text,
            "#1 api 2/2\nlistening on :8080\nready\n[1 target, 2 lines]\n"
        );
        assert_eq!(outcome.lines_shown, 2);
    }

    #[test]
    fn timestamps_are_available_but_cost_only_the_time_of_day() {
        let runner =
            ScriptedRunner::new().with(&format!("{BASE} api"), &stamped(&[("12:04:11", "ready")]));
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned()],
                timestamps: true,
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert!(
            outcome.text.contains("12:04:11 ready\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn the_two_streams_come_back_in_the_order_they_were_written() {
        // The whole reason timestamps are always requested: stderr arrives on its own
        // pipe, and concatenating would put the error after a line it preceded.
        let runner = ScriptedRunner::new().reply(
            &format!("{BASE} api"),
            0,
            &stamped(&[("12:04:11", "starting"), ("12:04:14", "still here")]),
            &stamped(&[("12:04:12", "cannot reach db")]),
            false,
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(
            outcome.text,
            "#1 api 3/3\nstarting\ncannot reach db\nstill here\n[1 target, 3 lines]\n"
        );
    }

    #[test]
    fn a_crash_loop_folds_into_one_line_and_a_count() {
        let repeated: String = stamped(&[("12:04:11", "steamcmd failed (0x202)")]).repeat(4);
        let runner = ScriptedRunner::new().with(&format!("{BASE} wine"), &repeated);
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["wine".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(
            outcome.text,
            "#1 wine 1/4\nsteamcmd failed (0x202) ×4\n[1 target, 1 line, 3 repeats folded]\n"
        );
        assert_eq!(outcome.targets[0].collapsed, 3);
    }

    #[test]
    fn a_level_filter_keeps_the_failure_and_says_what_it_dropped() {
        let runner = ScriptedRunner::new().with(
            &format!("{BASE} wine"),
            &stamped(&[
                ("12:04:10", "Downloading update"),
                ("12:04:11", "ERROR! Failed to install app 1203420"),
                ("12:04:12", "Shutting down"),
            ]),
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["wine".to_owned()],
                level: Some(Level::Error),
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(
            outcome.text,
            "#1 wine 1/3 (2 filtered)\nERROR! Failed to install app 1203420\n\
             [1 target, 1 line, 2 filtered out]\n"
        );
    }

    #[test]
    fn context_shows_what_led_up_to_a_match_but_never_an_excluded_line() {
        let runner = ScriptedRunner::new().with(
            &format!("{BASE} wine"),
            &stamped(&[
                ("12:04:09", "noise"),
                ("12:04:10", "verifying install"),
                ("12:04:11", "ERROR! disk full"),
                ("12:04:12", "retrying"),
            ]),
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["wine".to_owned()],
                grep: vec!["error".to_owned()],
                exclude: vec!["retrying".to_owned()],
                context: Some(1),
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(
            outcome.text,
            "#1 wine 2/4 (2 filtered)\nverifying install\nERROR! disk full\n\
             [1 target, 2 lines, 2 filtered out]\n"
        );
    }

    #[test]
    fn several_targets_are_one_call_each_and_one_response() {
        let runner = ScriptedRunner::new()
            .with(&format!("{BASE} api"), &stamped(&[("12:04:11", "ready")]))
            .with(
                &format!("{BASE} db"),
                &stamped(&[("12:04:12", "listening")]),
            );
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned(), "db".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(
            outcome.text,
            "#1 api 1/1\nready\n#2 db 1/1\nlistening\n[2 targets, 2 lines]\n"
        );
        assert_eq!(outcome.commands.len(), 2);
    }

    #[test]
    fn one_missing_container_does_not_cost_the_other_target_its_logs() {
        let runner = ScriptedRunner::new()
            .failing(
                &format!("{BASE} gone"),
                1,
                "Error response from daemon: No such container: gone\n",
            )
            .with(&format!("{BASE} api"), &stamped(&[("12:04:11", "ready")]));
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["gone".to_owned(), "api".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(
            outcome.text,
            "#1 gone — No such container: gone\n#2 api 1/1\nready\n[2 targets, 1 line]\n"
        );
        assert_eq!(outcome.targets[0].status, "No such container: gone");
    }

    #[test]
    fn the_tail_is_the_end_of_the_window_unless_the_start_was_asked_for() {
        let many: String = (0..5)
            .map(|n| format!("2026-10-06T12:04:1{n}.000000000Z line{n}\n"))
            .collect();
        let runner = ScriptedRunner::new().with(&format!("{BASE} api"), &many);
        let request = LogsRequest {
            targets: vec!["api".to_owned()],
            max_lines: Some(2),
            ..LogsRequest::default()
        };

        let tail = logs(&runner, &request).expect("read");
        assert!(tail.text.contains("line3\nline4\n"), "{}", tail.text);

        let head = logs(
            &runner,
            &LogsRequest {
                head: true,
                ..request.clone()
            },
        )
        .expect("read");
        assert!(head.text.contains("line0\nline1\n"), "{}", head.text);
    }

    #[test]
    fn a_compose_project_is_one_call_with_the_service_prefixes_kept() {
        let runner = ScriptedRunner::new().with(
            "docker compose -p ens logs --timestamps --tail 80 --no-color",
            "ens-wine-1  | 2026-10-06T12:04:11.000000000Z booting\n",
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                scope: ComposeScope {
                    project: Some("ens".to_owned()),
                    ..ComposeScope::default()
                },
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert!(outcome.text.starts_with("#1 ens 1/1\n"), "{}", outcome.text);
        assert!(outcome.text.contains("booting"), "{}", outcome.text);
    }

    #[test]
    fn a_named_service_drops_the_prefix_that_would_repeat_its_name() {
        let runner = ScriptedRunner::new().with(
            "docker compose -p ens logs --timestamps --tail 80 --no-color --no-log-prefix wine",
            &stamped(&[("12:04:11", "booting")]),
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                scope: ComposeScope {
                    project: Some("ens".to_owned()),
                    ..ComposeScope::default()
                },
                targets: vec!["wine".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert_eq!(outcome.text, "#1 wine 1/1\nbooting\n[1 target, 1 line]\n");
    }

    #[test]
    fn nothing_to_read_from_is_a_request_error_that_says_what_to_send() {
        let err = logs(&ScriptedRunner::new(), &LogsRequest::default()).expect_err("refused");
        assert!(
            err.to_string().contains("name at least one container"),
            "{err}"
        );
    }

    #[test]
    fn a_broken_pattern_is_refused_before_anything_runs() {
        let runner = ScriptedRunner::new();
        let err = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned()],
                grep: vec!["(unclosed".to_owned()],
                ..LogsRequest::default()
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("is not a regex"), "{err}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn the_header_admits_when_the_tail_was_the_limit() {
        let five: String = (0..5)
            .map(|n| format!("2026-10-06T12:04:1{n}.000000000Z line{n}\n"))
            .collect();
        let runner = ScriptedRunner::new().with("docker logs --timestamps --tail 5 api", &five);
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned()],
                tail: Some(5),
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert!(
            outcome.text.starts_with("#1 api 5/5 (tail 5)\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn since_and_until_are_passed_through_and_checked() {
        let runner = ScriptedRunner::new().with(
            "docker logs --timestamps --tail 80 --since 10m --until 2026-10-06T13:00:00 api",
            &stamped(&[("12:50:00", "still up")]),
        );
        let outcome = logs(
            &runner,
            &LogsRequest {
                targets: vec!["api".to_owned()],
                since: Some("10m".to_owned()),
                until: Some("2026-10-06T13:00:00".to_owned()),
                ..LogsRequest::default()
            },
        )
        .expect("read");
        assert!(outcome.text.contains("still up"), "{}", outcome.text);
    }
}
