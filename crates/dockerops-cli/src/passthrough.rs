//! Any other docker or compose subcommand, under the same budget and the same rules.
//!
//! The two CLIs have something like a hundred and forty subcommands. Describing each as
//! its own MCP tool would cost more context than the shell chains this server exists to
//! replace, because every tool's schema is re-sent on every single request. So the common
//! questions get a curated tool, and everything else — `cp`, `top`, `port`, `history`,
//! `network inspect`, `volume prune`, `buildx bake`, `swarm init` — comes through here.
//!
//! What a caller gives up is the rendering. What it keeps is the budget, the deadline, the
//! flag-injection guard, and two rules that matter more than any of them:
//!
//! * **Nothing may stream.** A subcommand that attaches or follows is refused by name,
//!   with a pointer to the tool or flag that answers the same question and terminates.
//! * **Nothing destroys state by accident.** Every subcommand is classified, and the ones
//!   that throw things away need `confirm` — the agent has to have meant it.

use serde::Serialize;

use crate::{
    Budget, Error, Result,
    invoke::{self, ComposeScope, Invocation, matcher},
    runner::CommandRunner,
    text::plural,
};

pub const DEFAULT_MAX_LINES: usize = 300;

/// Cap on `stdin`. Enough for a SQL script or a config file; an image tarball belongs in a
/// file that `load -i` names.
pub const MAX_STDIN_BYTES: usize = 1024 * 1024;

/// What a subcommand does to the world. Decided from the argv before anything runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Asks a question. Allowed in `read_only` mode.
    Read,
    /// Changes state in a way that can be undone by changing it back: `start`, `tag`,
    /// `cp`, `pull`, `build`.
    Mutate,
    /// Throws something away: `rm`, `rmi`, `prune`, `kill`, `down`, `volume rm`. Needs
    /// `confirm`.
    Destroy,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Mutate => "mutate",
            Self::Destroy => "destroy",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PassthroughRequest {
    pub invocation: Invocation,
    /// Set to prefix the arguments with `compose -f … -p …`.
    pub scope: ComposeScope,
    /// The subcommand and its arguments, already split: `["volume", "inspect", "ens-data"]`.
    /// No shell is involved, so quoting cannot be got wrong.
    pub args: Vec<String>,
    /// Required when the classification is [`Class::Destroy`].
    pub confirm: bool,
    /// Refuse anything that is not [`Class::Read`]. The server sets this from its own
    /// `--read-only` flag; a caller can also set it per call to be sure.
    pub read_only: bool,
    pub grep: Option<String>,
    pub exclude: Option<String>,
    pub ignore_case: bool,
    /// Keep the first lines rather than the last.
    pub head: bool,
    /// Cap on lines rendered. Defaults to 300.
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
    /// Written to the command's stdin, which is then closed. Without it stdin is empty, so
    /// `-i` reads end-of-file at once instead of waiting.
    pub stdin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PassthroughOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub command: String,
    pub class: Class,
    pub status: i32,
    pub lines_shown: usize,
    pub lines_total: usize,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn passthrough(
    runner: &dyn CommandRunner,
    request: &PassthroughRequest,
) -> Result<PassthroughOutcome> {
    let args = &request.args;
    let Some(first) = args.first() else {
        return Err(Error::request(
            "`args` is empty; send the subcommand and its arguments as a list, \
             e.g. [\"volume\", \"inspect\", \"ens-data\"]",
        ));
    };
    for arg in args {
        if arg.contains(['\n', '\r', '\0']) {
            return Err(Error::request(
                "an argument contains a newline or NUL, which cannot be one argument",
            ));
        }
    }
    if first.starts_with('-') {
        return Err(Error::request(format!(
            "`args` starts with the flag `{first}`; global flags are not passed here — \
             use `host` for `-H` and `cwd` for the working directory"
        )));
    }

    if request
        .stdin
        .as_ref()
        .is_some_and(|input| input.len() > MAX_STDIN_BYTES)
    {
        return Err(Error::request(format!(
            "`stdin` is over {MAX_STDIN_BYTES} bytes; write it to a file and name the file"
        )));
    }

    refuse_if_endless(args)?;
    let class = classify(args);
    let compose = !request.scope.is_empty() || words(args).0 == "compose";
    let terminal = Terminal::of(args, compose);
    if request.stdin.is_some() && terminal.is_some_and(|t| !t.interactive) {
        return Err(Error::request(format!(
            "`stdin` was given but `{}` was not asked for `-i`, so docker would not \
             pass it on; add `-i`",
            phrase(args)
        )));
    }
    // `-t` with `-i` fails outright when stdin is not a terminal, which here it never is.
    // Keeping stdin is what such a call usually wanted, so the terminal is what goes.
    let dropped_tty = terminal.is_some_and(|t| t.interactive && t.tty);
    let tty = terminal.is_some_and(|t| t.tty) && !dropped_tty;
    let args = &if dropped_tty {
        drop_tty(args, compose)
    } else {
        args.clone()
    };

    if request.read_only && class != Class::Read {
        return Err(Error::refused(format!(
            "`{}` is classed as {} and this server is running read-only",
            phrase(args),
            class.name()
        )));
    }
    if class == Class::Destroy && !request.confirm {
        return Err(Error::NeedsConfirmation {
            verb: phrase(args),
            effect: "removes or destroys state, and docker will not bring it back".to_owned(),
            hint: "The read tools say what exists first: `ps`, `images`, `usage`.".to_owned(),
        });
    }

    let mut rest = if request.scope.is_empty() {
        Vec::new()
    } else {
        request.scope.args()?
    };
    rest.extend(args.iter().cloned());

    let line = invoke::command(&request.invocation, rest)?;
    let mut output =
        invoke::run_with_stdin(runner, &request.invocation, &line, request.stdin.as_deref())?;
    let command = format!("{} {}", request.invocation.bin(), line.join(" "));

    // A terminal gets carriage returns and escape sequences that a pipe would not have,
    // and both would be spent from the budget as if they were text.
    if tty {
        output.stdout = strip_terminal(&output.stdout);
        output.stderr = strip_terminal(&output.stderr);
    }

    // Which stream a docker subcommand uses is not worth modelling: `inspect` writes JSON
    // to stdout, `pull` writes progress to stderr, `cp` writes nothing at all.
    let mut lines: Vec<&str> = output.stdout.lines().collect();
    lines.extend(output.stderr.lines());
    let total = lines.len();

    let grep = matcher(request.grep.as_deref(), request.ignore_case, "grep")?;
    let exclude = matcher(request.exclude.as_deref(), request.ignore_case, "exclude")?;
    let mut lines: Vec<&str> = lines
        .into_iter()
        .filter(|line| grep.as_ref().map(|re| re.is_match(line)).unwrap_or(true))
        .filter(|line| {
            !exclude
                .as_ref()
                .map(|re| re.is_match(line))
                .unwrap_or(false)
        })
        .collect();
    let filtered_out = total - lines.len();

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
    for line in &lines {
        let rendered = format!("{line}\n");
        if !budget.try_spend(&rendered) {
            break;
        }
        body.push_str(&rendered);
        shown += 1;
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

    let mut footer = format!("[{}, {}", phrase(args), plural(total, "line"));
    if filtered_out > 0 {
        footer.push_str(&format!(", {filtered_out} filtered"));
    }
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    if dropped_tty {
        footer.push_str(", `-t` dropped: stdin is not a terminal");
    }
    footer.push_str(&if output.timed_out {
        format!(
            ", timed out after {}s]\n",
            request.invocation.timeout().as_secs()
        )
    } else {
        format!(", exit {}]\n", output.status)
    });

    Ok(PassthroughOutcome {
        text: format!("{body}{footer}"),
        command,
        class,
        status: output.status,
        lines_shown: shown,
        lines_total: total,
        timed_out: output.timed_out,
        truncated,
    })
}

/// The first two non-flag words, which is what a classification is about and what an error
/// message should name: `volume rm`, `system prune`, `ps`.
fn phrase(args: &[String]) -> String {
    args.iter()
        .filter(|arg| !arg.starts_with('-'))
        .take(2)
        .cloned()
        .collect::<Vec<String>>()
        .join(" ")
}

fn words(args: &[String]) -> (String, Option<String>) {
    let mut plain = args.iter().filter(|arg| !arg.starts_with('-'));
    let first = plain.next().cloned().unwrap_or_default().to_lowercase();
    (first, plain.next().map(|word| word.to_lowercase()))
}

fn has_flag(args: &[String], long: &str, short: Option<char>) -> bool {
    args.iter().any(|arg| {
        if arg == long {
            return true;
        }
        // `-it` is two short flags in one argument, which is how everybody writes it.
        match short {
            Some(short) => {
                arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains(short)
            }
            None => false,
        }
    })
}

/// Refuse the subcommands and flag combinations that never return.
///
/// This is the hard rule of the server. A streaming call does not time out into a partial
/// answer: it holds the connection until the deadline and then reports nothing, and in a
/// conversation that is indistinguishable from a hang. Every refusal here names the thing
/// that answers the same question and ends.
fn refuse_if_endless(args: &[String]) -> Result<()> {
    let (first, second) = words(args);
    let subcommand = match (first.as_str(), second.as_deref()) {
        ("compose", Some(second)) => second.to_owned(),
        _ => first.clone(),
    };

    let follow = has_flag(args, "--follow", Some('f'));
    match subcommand.as_str() {
        "attach" => Err(Error::refused(
            "`attach` connects to a container's streams and never returns; \
             `logs` is the bounded form of the same question",
        )),
        "wait" => Err(Error::refused(
            "`wait` blocks until a container exits; `ps` reports the state now, and \
             `events` reports the exit when it happens inside a window",
        )),
        "watch" => Err(Error::refused(
            "`compose watch` runs until interrupted; it has no bounded form",
        )),
        "logs" if follow => Err(Error::refused(
            "`--follow` never returns. Drop it: the `logs` tool tails, filters and \
             deduplicates a bounded window, which is what the flag is usually a way to do",
        )),
        "stats" if !args.iter().any(|arg| arg == "--no-stream") => Err(Error::refused(
            "`stats` repaints forever without `--no-stream`; add it, or use the `stats` \
             tool, which always passes it",
        )),
        "events" if !args.iter().any(|arg| arg == "--until") => Err(Error::refused(
            "`events` waits for the next event forever without `--until`; add \
             `--until 0s`, or use the `events` tool, which always bounds the window",
        )),
        "up" if !has_flag(args, "--detach", Some('d')) => Err(Error::refused(
            "`compose up` attaches without `-d` and never returns; add `--detach`, or \
             use the `compose` tool, which always does",
        )),
        "run" if !has_flag(args, "--detach", Some('d')) && !has_flag(args, "--rm", None) => {
            Err(Error::refused(
                "`run` in the foreground lasts as long as the container does; add \
                 `--detach`, or `--rm` with a command that exits",
            ))
        }
        _ => Ok(()),
    }
}

/// What `-i` and `-t` asked for, on the subcommands where that is what they mean.
///
/// Elsewhere the same letters are something else entirely — `build -t` is a tag, `logs -t`
/// timestamps, `stop -t` a timeout, `load -i` a file — and after the image or container
/// they belong to the command being run, as in `exec api ls -lt`. So only the options of
/// `run`, `create`, `exec` and `start` are looked at, and only up to the first argument
/// that is not one.
///
/// Compose is left alone: its `run` and `exec` keep stdin open already and turn the
/// terminal off with `-T`, and they have no `-t` to drop.
#[derive(Debug, Clone, Copy)]
struct Terminal {
    interactive: bool,
    tty: bool,
}

impl Terminal {
    fn of(args: &[String], compose: bool) -> Option<Self> {
        let (verb, options) = terminal_options(args, compose)?;
        let mut terminal = Self {
            interactive: false,
            tty: false,
        };
        for arg in &args[options] {
            match arg.as_str() {
                "--interactive" | "--interactive=true" => terminal.interactive = true,
                "--tty" | "--tty=true" => terminal.tty = true,
                arg if arg.starts_with('-') && !arg.starts_with("--") => {
                    let letters = short_flags(verb, arg);
                    terminal.interactive |= letters.contains('i');
                    terminal.tty |= letters.contains('t');
                }
                _ => {}
            }
        }
        Some(terminal)
    }
}

/// The verb, and the span of `args` holding its options, for the subcommands in
/// [`Terminal`]. The span ends at the first argument that is neither a flag nor the value
/// of one. A value-taking flag this does not know ends it early, which can only miss a
/// `-t` (and docker then says so) — never mistake the container's command for options.
fn terminal_options(
    args: &[String],
    compose: bool,
) -> Option<(&'static str, std::ops::Range<usize>)> {
    if compose {
        return None;
    }
    let at = usize::from(args.first().is_some_and(|arg| arg == "container"));
    let verb = match args.get(at)?.as_str() {
        "run" => "run",
        "create" => "create",
        "exec" => "exec",
        "start" => "start",
        _ => return None,
    };
    let start = at + 1;
    let mut end = start;
    while let Some(arg) = args.get(end) {
        if arg == "-" || arg == "--" || !arg.starts_with('-') {
            break;
        }
        end += 1;
        if takes_value(verb, arg) {
            end += 1;
        }
    }
    Some((verb, start..end.min(args.len())))
}

/// The short and long options of `verb` that take a value.
fn value_flags(verb: &str) -> (&'static str, &'static [&'static str]) {
    match verb {
        "run" | "create" => ("acehlmpuvw", RUN_VALUE_FLAGS),
        "exec" => (
            "euw",
            &["detach-keys", "env", "env-file", "user", "workdir"],
        ),
        _ => ("", &["detach-keys", "checkpoint", "checkpoint-dir"]),
    }
}

/// The boolean letters of a short-flag cluster such as `-dit`: everything before the first
/// letter that takes a value, since the rest of the argument is that value (`-ePATH=/x`).
fn short_flags<'a>(verb: &str, arg: &'a str) -> &'a str {
    let (short, _) = value_flags(verb);
    let letters = &arg[1..];
    match letters.find(|letter| short.contains(letter)) {
        Some(at) => &letters[..at],
        None => letters,
    }
}

/// Whether `arg` consumes the argument after it.
fn takes_value(verb: &str, arg: &str) -> bool {
    let (short, long) = value_flags(verb);
    if let Some(name) = arg.strip_prefix("--") {
        return !name.contains('=') && long.contains(&name);
    }
    let letters = &arg[1..];
    letters
        .find(|letter| short.contains(letter))
        .is_some_and(|at| at == letters.len() - 1)
}

/// The long options of `docker run` and `docker create` that take a value, as their
/// `--help` lists them.
#[rustfmt::skip]
const RUN_VALUE_FLAGS: &[&str] = &[
    "add-host", "annotation", "attach", "blkio-weight", "blkio-weight-device", "cap-add",
    "cap-drop", "cgroup-parent", "cgroupns", "cidfile", "cpu-period", "cpu-quota",
    "cpu-rt-period", "cpu-rt-runtime", "cpu-shares", "cpus", "cpuset-cpus", "cpuset-mems",
    "detach-keys", "device", "device-cgroup-rule", "device-read-bps", "device-read-iops",
    "device-write-bps", "device-write-iops", "dns", "dns-option", "dns-search", "domainname",
    "entrypoint", "env", "env-file", "expose", "gpus", "group-add", "health-cmd",
    "health-interval", "health-retries", "health-start-interval", "health-start-period",
    "health-timeout", "hostname", "ip", "ip6", "ipc", "isolation", "label", "label-file",
    "link", "link-local-ip", "log-driver", "log-opt", "mac-address", "memory",
    "memory-reservation", "memory-swap", "memory-swappiness", "mount", "name", "net",
    "net-alias", "network", "network-alias", "oom-score-adj", "pid", "pids-limit",
    "platform", "publish", "pull", "restart", "runtime", "security-opt", "shm-size",
    "stop-signal", "stop-timeout", "storage-opt", "sysctl", "tmpfs", "ulimit", "umask",
    "user", "userns", "uts", "volume", "volume-driver", "volumes-from", "workdir",
];

/// `args` without the terminal: `--tty` removed, and `t` taken out of short clusters,
/// within the option span only.
fn drop_tty(args: &[String], compose: bool) -> Vec<String> {
    let Some((verb, options)) = terminal_options(args, compose) else {
        return args.to_vec();
    };
    args.iter()
        .enumerate()
        .filter_map(|(at, arg)| {
            if !options.contains(&at) || !arg.starts_with('-') {
                return Some(arg.clone());
            }
            if arg == "--tty" || arg == "--tty=true" {
                return None;
            }
            if arg.starts_with("--") {
                return Some(arg.clone());
            }
            let flags = short_flags(verb, arg);
            let rest = format!("{}{}", flags.replace('t', ""), &arg[1 + flags.len()..]);
            (!rest.is_empty()).then(|| format!("-{rest}"))
        })
        .collect()
}

/// Remove what a terminal adds to text: escape sequences, and carriage returns.
fn strip_terminal(text: &str) -> String {
    static ESCAPES: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-_]|\r")
            .expect("valid")
    });
    ESCAPES.replace_all(text, "").into_owned()
}

/// Classify a subcommand from its words.
///
/// Both CLIs are regular about this: the verb is the last word of the phrase, and a verb
/// means the same thing wherever it appears — `rm` removes whether the noun is a
/// container, a volume or a network. So the table is over verbs, with the handful of
/// irregular top-level subcommands named outright. Anything unrecognised is `Mutate`,
/// which is the safe guess: it costs a caller nothing but a `read_only` refusal, whereas
/// guessing `Read` would let something through unasked.
fn classify(args: &[String]) -> Class {
    let (first, second) = words(args);
    let verb = match (first.as_str(), second.as_deref()) {
        // `compose` and the object groups put the verb second.
        (
            "compose" | "container" | "image" | "volume" | "network" | "system" | "builder"
            | "buildx" | "context" | "plugin" | "secret" | "config" | "node" | "service" | "stack"
            | "swarm" | "trust" | "manifest" | "checkpoint",
            Some(second),
        ) => second.to_owned(),
        _ => first.clone(),
    };

    match verb.as_str() {
        "ps" | "ls" | "images" | "inspect" | "logs" | "stats" | "events" | "top" | "port"
        | "diff" | "history" | "version" | "info" | "df" | "search" | "config" | "show"
        | "list" | "get" | "export" | "save" | "bake-print" | "help" => Class::Read,

        "rm" | "rmi" | "remove" | "prune" | "kill" | "down" | "delete" | "leave" | "destroy"
        | "uninstall" | "disable" => Class::Destroy,

        _ => Class::Mutate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    fn request(args: &[&str]) -> PassthroughRequest {
        PassthroughRequest {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            ..PassthroughRequest::default()
        }
    }

    #[test]
    fn the_long_tail_of_the_cli_runs_with_the_budget_attached() {
        let runner = ScriptedRunner::new().with(
            "docker volume inspect ens-data --format {{.Mountpoint}}",
            "/var/lib/docker/volumes/ens-data/_data\n",
        );
        let outcome = passthrough(
            &runner,
            &request(&[
                "volume",
                "inspect",
                "ens-data",
                "--format",
                "{{.Mountpoint}}",
            ]),
        )
        .expect("ran");
        assert_eq!(
            outcome.text,
            "/var/lib/docker/volumes/ens-data/_data\n[volume inspect, 1 line, exit 0]\n"
        );
        assert_eq!(outcome.class, Class::Read);
    }

    #[test]
    fn a_verb_that_destroys_needs_confirming_wherever_the_noun_came_from() {
        for args in [
            vec!["volume", "rm", "ens-data"],
            vec!["system", "prune", "--all"],
            vec!["rmi", "postgres:17"],
            vec!["network", "rm", "ens_default"],
            vec!["compose", "down"],
        ] {
            let runner = ScriptedRunner::new();
            let err = passthrough(&runner, &request(&args)).expect_err("gated");
            assert!(
                err.to_string().contains("confirm: true"),
                "{args:?} was not gated: {err}"
            );
            assert!(runner.calls().is_empty(), "{args:?} ran anyway");
        }
    }

    #[test]
    fn confirming_it_runs_it() {
        let runner = ScriptedRunner::new().with("docker volume rm ens-data", "ens-data\n");
        let outcome = passthrough(
            &runner,
            &PassthroughRequest {
                confirm: true,
                ..request(&["volume", "rm", "ens-data"])
            },
        )
        .expect("ran");
        assert_eq!(outcome.class, Class::Destroy);
        assert_eq!(outcome.text, "ens-data\n[volume rm, 1 line, exit 0]\n");
    }

    #[test]
    fn read_only_lets_questions_through_and_nothing_else() {
        let runner = ScriptedRunner::new().with("docker image history postgres:17", "");
        passthrough(
            &runner,
            &PassthroughRequest {
                read_only: true,
                ..request(&["image", "history", "postgres:17"])
            },
        )
        .expect("read");

        let err = passthrough(
            &ScriptedRunner::new(),
            &PassthroughRequest {
                read_only: true,
                confirm: true,
                ..request(&["start", "api"])
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    #[test]
    fn an_unknown_subcommand_is_assumed_to_change_something() {
        assert_eq!(
            classify(&["something-new".to_owned()]),
            Class::Mutate,
            "an unrecognised verb must not be guessed as a read"
        );
    }

    #[test]
    fn everything_that_never_returns_is_refused_by_name() {
        for (args, expected) in [
            (vec!["attach", "api"], "logs"),
            (vec!["wait", "api"], "events"),
            (vec!["logs", "-f", "api"], "--follow"),
            (vec!["stats"], "--no-stream"),
            (vec!["events"], "--until"),
            (vec!["compose", "up"], "--detach"),
            (vec!["compose", "watch"], "bounded"),
            (vec!["run", "nginx"], "--detach"),
        ] {
            let runner = ScriptedRunner::new();
            let err = passthrough(&runner, &request(&args)).expect_err("refused");
            assert!(
                err.to_string().contains(expected),
                "{args:?} refusal did not mention {expected}: {err}"
            );
            assert!(runner.calls().is_empty(), "{args:?} ran anyway");
        }
    }

    #[test]
    fn the_bounded_forms_of_those_commands_are_allowed() {
        for args in [
            vec!["stats", "--no-stream"],
            vec!["events", "--until", "0s"],
            vec!["compose", "up", "-d"],
            vec!["run", "--rm", "alpine", "true"],
        ] {
            let runner = ScriptedRunner::new().with(&format!("docker {}", args.join(" ")), "");
            passthrough(
                &runner,
                &PassthroughRequest {
                    confirm: true,
                    ..request(&args)
                },
            )
            .unwrap_or_else(|err| panic!("{args:?} was refused: {err}"));
        }
    }

    #[test]
    fn a_global_flag_is_a_parameter_rather_than_an_argument() {
        let err = passthrough(&ScriptedRunner::new(), &request(&["-H", "ssh://box", "ps"]))
            .expect_err("refused");
        assert!(err.to_string().contains("use `host`"), "{err}");
    }

    #[test]
    fn a_compose_scope_prefixes_the_arguments_the_cli_way() {
        let runner = ScriptedRunner::new().with("docker compose -p ens top", "");
        passthrough(
            &runner,
            &PassthroughRequest {
                scope: ComposeScope {
                    project: Some("ens".to_owned()),
                    ..ComposeScope::default()
                },
                ..request(&["top"])
            },
        )
        .expect("ran");
        assert_eq!(runner.calls()[0], "docker compose -p ens top");
    }

    #[test]
    fn output_is_filtered_and_capped_like_everything_else() {
        let stdout: String = (0..50).map(|n| format!("layer {n}\n")).collect();
        let runner = ScriptedRunner::new().with("docker image history postgres:17", &stdout);
        let outcome = passthrough(
            &runner,
            &PassthroughRequest {
                grep: Some("layer [1-9]$".to_owned()),
                max_lines: Some(3),
                ..request(&["image", "history", "postgres:17"])
            },
        )
        .expect("ran");
        assert_eq!(
            outcome.text,
            "layer 7\nlayer 8\nlayer 9\n\
             [image history, 50 lines, 41 filtered, 6 more, over max_lines 3 (last kept), exit 0]\n"
        );
    }

    #[test]
    fn a_failure_keeps_its_own_words_and_its_status() {
        let runner = ScriptedRunner::new().failing(
            "docker volume inspect nope",
            1,
            "Error response from daemon: get nope: no such volume\n",
        );
        let outcome = passthrough(&runner, &request(&["volume", "inspect", "nope"])).expect("ran");
        assert_eq!(
            outcome.text,
            "Error response from daemon: get nope: no such volume\n\
             [volume inspect, 1 line, exit 1]\n"
        );
        assert_eq!(outcome.status, 1);
    }

    #[test]
    fn the_letters_i_and_t_mean_something_else_outside_run_exec_create_and_start() {
        for args in [
            vec!["build", "-t", "myimg", "."],
            vec!["logs", "-t", "api"],
            vec!["stop", "-t", "5", "api"],
            vec!["load", "-i", "image.tar"],
            vec!["exec", "api", "ls", "-lt"],
            vec!["run", "--rm", "-e", "A=1", "alpine", "ls", "-it"],
        ] {
            let key = format!("docker {}", args.join(" "));
            let runner = ScriptedRunner::new().with(&key, "");
            let outcome = passthrough(
                &runner,
                &PassthroughRequest {
                    confirm: true,
                    ..request(&args)
                },
            )
            .unwrap_or_else(|err| panic!("{args:?} was refused: {err}"));
            assert_eq!(runner.calls(), vec![key], "{args:?} was rewritten");
            assert!(!outcome.text.contains("dropped"), "{}", outcome.text);
        }
    }

    #[test]
    fn interactive_input_is_written_to_stdin() {
        let runner = ScriptedRunner::new().with("docker exec -i db psql -U app", "INSERT 0 1\n");
        let outcome = passthrough(
            &runner,
            &PassthroughRequest {
                stdin: Some("insert into t values (1);\n".to_owned()),
                ..request(&["exec", "-i", "db", "psql", "-U", "app"])
            },
        )
        .expect("ran");
        assert_eq!(outcome.text, "INSERT 0 1\n[exec db, 1 line, exit 0]\n");
        assert_eq!(
            runner.stdins(),
            vec![Some("insert into t values (1);\n".to_owned())]
        );
    }

    #[test]
    fn stdin_that_docker_would_not_pass_on_is_refused() {
        let runner = ScriptedRunner::new();
        let err = passthrough(
            &runner,
            &PassthroughRequest {
                stdin: Some("select 1;".to_owned()),
                ..request(&["exec", "db", "psql"])
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("add `-i`"), "{err}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn stdin_goes_to_subcommands_that_read_it_without_i() {
        let runner = ScriptedRunner::new().with("docker build -t img -", "");
        passthrough(
            &runner,
            &PassthroughRequest {
                stdin: Some("FROM alpine\n".to_owned()),
                ..request(&["build", "-t", "img", "-"])
            },
        )
        .expect("ran");
        assert_eq!(runner.stdins(), vec![Some("FROM alpine\n".to_owned())]);
    }

    #[test]
    fn oversized_stdin_is_refused() {
        let err = passthrough(
            &ScriptedRunner::new(),
            &PassthroughRequest {
                stdin: Some("x".repeat(MAX_STDIN_BYTES + 1)),
                ..request(&["build", "-"])
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("stdin"), "{err}");
    }

    #[test]
    fn a_terminal_without_input_is_kept_and_its_escapes_stripped() {
        let runner = ScriptedRunner::new().with(
            "docker run --rm -t alpine ls",
            "\x1b[1;34mbin\x1b[0m\r\ndev\r\n",
        );
        let outcome =
            passthrough(&runner, &request(&["run", "--rm", "-t", "alpine", "ls"])).expect("ran");
        assert_eq!(outcome.text, "bin\ndev\n[run alpine, 2 lines, exit 0]\n");
    }

    #[test]
    fn a_terminal_with_input_is_dropped_and_the_input_kept() {
        for (args, sent) in [
            (
                vec!["exec", "-it", "api", "ls", "-lt"],
                "docker exec -i api ls -lt",
            ),
            (vec!["exec", "-ti", "api", "sh"], "docker exec -i api sh"),
            (
                vec!["run", "--rm", "-i", "-t", "-e", "A=1", "alpine", "sh"],
                "docker run --rm -i -e A=1 alpine sh",
            ),
            (
                vec![
                    "container",
                    "run",
                    "--rm",
                    "--interactive",
                    "--tty",
                    "alpine",
                ],
                "docker container run --rm --interactive alpine",
            ),
            (vec!["start", "-ait", "api"], "docker start -ai api"),
            (
                vec!["run", "-dit", "--name", "box", "alpine"],
                "docker run -di --name box alpine",
            ),
        ] {
            let runner = ScriptedRunner::new().with(sent, "");
            let outcome = passthrough(&runner, &request(&args))
                .unwrap_or_else(|err| panic!("{args:?} was refused: {err}"));
            assert_eq!(runner.calls(), vec![sent.to_owned()], "{args:?}");
            assert!(
                outcome.text.contains("`-t` dropped"),
                "{args:?}: {}",
                outcome.text
            );
        }
    }

    #[test]
    fn a_value_flag_ends_a_cluster() {
        // `-e` takes the rest of its argument, so the `t` and `i` there are its value.
        let runner = ScriptedRunner::new().with("docker exec -eTI=it api env", "");
        passthrough(&runner, &request(&["exec", "-eTI=it", "api", "env"])).expect("ran");
        assert_eq!(runner.calls(), vec!["docker exec -eTI=it api env"]);
    }

    #[test]
    fn compose_run_and_exec_are_left_alone() {
        let runner = ScriptedRunner::new().with("docker compose exec -T api ls -lt", "");
        passthrough(
            &runner,
            &request(&["compose", "exec", "-T", "api", "ls", "-lt"]),
        )
        .expect("ran");
        assert_eq!(runner.calls(), vec!["docker compose exec -T api ls -lt"]);
    }

    #[test]
    fn an_empty_request_says_what_to_send() {
        let err = passthrough(&ScriptedRunner::new(), &request(&[])).expect_err("refused");
        assert!(err.to_string().contains("volume"), "{err}");
    }
}
