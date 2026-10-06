//! The compose lifecycle verbs, summarised rather than streamed.
//!
//! `docker compose up` is a progress display: two lines per object, one for the attempt
//! and one for the result, repainted in place in a terminal and emitted in full to a pipe.
//! Bringing up five services costs forty lines to say "five containers started". Worse,
//! without `-d` it attaches and never returns, which from a tool call means the call never
//! returns either.
//!
//! So every verb here is non-attached by construction, the progress stream is reduced to
//! its outcomes grouped by what happened, and the verbs that throw things away do not run
//! without being asked twice.

use serde::Serialize;

use crate::{
    Budget, Error, Result,
    invoke::{self, ComposeScope, Invocation, safe_value},
    runner::CommandRunner,
    text::{pad, plural, width_of},
};

/// The lifecycle verbs, and nothing else: anything exotic belongs in `passthrough`, where
/// a caller has to name the arguments and accept the classification that comes with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Create and start, always detached.
    Up,
    /// Stop and remove containers and networks. Destructive.
    Down,
    Start,
    Stop,
    Restart,
    /// SIGKILL, rather than the orderly stop. Destructive.
    Kill,
    Build,
    Pull,
    /// Create containers without starting them.
    Create,
    /// Remove stopped containers. Destructive.
    Remove,
}

impl Verb {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "up" => Self::Up,
            "down" => Self::Down,
            "start" => Self::Start,
            "stop" => Self::Stop,
            "restart" => Self::Restart,
            "kill" => Self::Kill,
            "build" => Self::Build,
            "pull" => Self::Pull,
            "create" => Self::Create,
            "rm" | "remove" => Self::Remove,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Kill => "kill",
            Self::Build => "build",
            Self::Pull => "pull",
            Self::Create => "create",
            Self::Remove => "rm",
        }
    }

    pub fn all() -> &'static str {
        "up, down, start, stop, restart, kill, build, pull, create, rm"
    }

    /// What this verb destroys, if anything. A verb with an effect does not run without
    /// `confirm`, because an agent that guessed wrong should cost a round trip rather than
    /// a database.
    pub fn destroys(self) -> Option<&'static str> {
        match self {
            Self::Down => Some("stops and removes this project's containers and networks"),
            Self::Remove => Some("removes this project's stopped containers"),
            Self::Kill => Some("kills these containers without letting them shut down"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComposeRequest {
    pub invocation: Invocation,
    pub scope: ComposeScope,
    pub verb: Verb,
    /// Services to act on. Empty means the whole project, which is what the CLI does.
    pub services: Vec<String>,
    /// Required for a verb that [`Verb::destroys`] something.
    pub confirm: bool,
    /// `up --build`: rebuild images first.
    pub build: bool,
    /// `up --force-recreate`.
    pub force_recreate: bool,
    /// `up --no-deps`, `start --no-deps`: act only on what was named.
    pub no_deps: bool,
    /// `up --remove-orphans`, `down --remove-orphans`.
    pub remove_orphans: bool,
    /// `up --wait`: return once the containers are healthy rather than merely started.
    /// Bounded by the invocation's own deadline, like everything else here.
    pub wait: bool,
    /// `down --volumes`. Named volumes are usually the data, so this is reported
    /// separately in the confirmation and never implied by `down` alone.
    pub volumes: bool,
    /// `stop`/`restart`/`down --timeout`: seconds before SIGKILL.
    pub stop_timeout: Option<u64>,
    /// `build --no-cache`, `pull --ignore-pull-failures` and friends are deliberately not
    /// here; `passthrough` takes them, with the same budget and classification.
    pub max_bytes: Option<usize>,
}

impl Default for ComposeRequest {
    fn default() -> Self {
        Self {
            invocation: Invocation::default(),
            scope: ComposeScope::default(),
            verb: Verb::Up,
            services: Vec::new(),
            confirm: false,
            build: false,
            force_recreate: false,
            no_deps: false,
            remove_orphans: false,
            wait: false,
            volumes: false,
            stop_timeout: None,
            max_bytes: None,
        }
    }
}

/// One thing compose did: `Container ens-db-1 Started`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Step {
    /// `Container`, `Network`, `Volume`, `Image`, `Service`.
    pub object: String,
    pub name: String,
    /// Always a finished state — `Started`, `Removed`, `Healthy`, `Error`.
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct ComposeOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub verb: String,
    pub steps: Vec<Step>,
    pub command: String,
    /// The process's exit status. Zero does not always mean every service is healthy —
    /// that is what `wait` is for.
    pub status: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn compose(runner: &dyn CommandRunner, request: &ComposeRequest) -> Result<ComposeOutcome> {
    for service in &request.services {
        safe_value(service, "service")?;
    }

    if let Some(effect) = request.verb.destroys()
        && !request.confirm
    {
        let mut effect = effect.to_owned();
        if request.volumes {
            effect.push_str(
                ", and `volumes` would delete this project's named volumes \
                             along with whatever data is in them",
            );
        }
        return Err(Error::NeedsConfirmation {
            verb: format!("compose {}", request.verb.name()),
            effect,
            hint: "`compose stop` stops without removing anything, and `ps` says what \
                   would be affected."
                .to_owned(),
        });
    }

    let mut rest = request.scope.args()?;
    rest.push(request.verb.name().to_owned());

    match request.verb {
        Verb::Up => {
            // Not negotiable. An attached `up` streams until the containers exit, and a
            // tool call that does not return is worse than one that fails.
            rest.push("--detach".to_owned());
            if request.wait {
                rest.push("--wait".to_owned());
            }
            if request.build {
                rest.push("--build".to_owned());
            }
            if request.force_recreate {
                rest.push("--force-recreate".to_owned());
            }
            if request.no_deps {
                rest.push("--no-deps".to_owned());
            }
            if request.remove_orphans {
                rest.push("--remove-orphans".to_owned());
            }
        }
        Verb::Down => {
            if request.volumes {
                rest.push("--volumes".to_owned());
            }
            if request.remove_orphans {
                rest.push("--remove-orphans".to_owned());
            }
            if let Some(seconds) = request.stop_timeout {
                rest.push("--timeout".to_owned());
                rest.push(seconds.to_string());
            }
        }
        Verb::Stop | Verb::Restart => {
            if let Some(seconds) = request.stop_timeout {
                rest.push("--timeout".to_owned());
                rest.push(seconds.to_string());
            }
        }
        Verb::Remove => {
            // Without this, `compose rm` asks the terminal for a yes. There is no terminal.
            rest.push("--force".to_owned());
            if request.volumes {
                rest.push("--volumes".to_owned());
            }
        }
        Verb::Start => {
            if request.no_deps {
                rest.push("--no-deps".to_owned());
            }
        }
        Verb::Create | Verb::Build | Verb::Pull | Verb::Kill => {}
    }

    rest.extend(request.services.iter().cloned());

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    // Compose writes its progress to stderr and its few real outputs to stdout, and which
    // one a given version uses for a given line is not worth tracking.
    let combined = format!("{}\n{}", output.stderr, output.stdout);
    let steps = steps(&combined);

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut shown = 0;
    let rows = group(&steps);
    let object_width = width_of(rows.iter().map(|(object, _, _)| object.as_str()));
    let action_width = width_of(rows.iter().map(|(_, action, _)| action.as_str()));
    for (object, action, names) in &rows {
        let line = format!(
            "{}  {}  {}\n",
            pad(object, object_width),
            pad(action, action_width),
            names.join(", ")
        );
        if !budget.try_spend(&line) {
            break;
        }
        body.push_str(&line);
        shown += 1;
    }

    // A failure's own words matter more than the summary, so they get what is left of the
    // budget before the footer.
    let failure = (!output.ok()).then(|| output.failure());
    if let Some(reason) = &failure {
        for line in reason.lines().take(6) {
            let line = format!("{line}\n");
            if !budget.try_spend(&line) {
                break;
            }
            body.push_str(&line);
        }
    }

    let truncated = (rows.len() > shown)
        .then(|| format!("{} not shown, byte budget reached", rows.len() - shown));

    let mut footer = format!("[{} {}", request.verb.name(), plural(steps.len(), "step"));
    if failure.is_some() {
        footer.push_str(&format!(", failed with status {}", output.status));
    }
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(ComposeOutcome {
        text: format!("{body}{footer}"),
        verb: request.verb.name().to_owned(),
        steps,
        command,
        status: output.status,
        failure,
        truncated,
    })
}

/// Pull the finished states out of compose's progress stream.
///
/// A line is ` Container ens-db-1  Started`. Each object gets two or three of them —
/// `Creating`, `Created`, `Starting`, `Started` — and only the last is news, so anything
/// still in progress is dropped on the grounds that its conclusion is in the same output.
fn steps(combined: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for line in combined.lines() {
        let line = line.trim();
        let mut words = line.split_whitespace();
        let (Some(object), Some(name)) = (words.next(), words.next()) else {
            continue;
        };
        if !matches!(
            object,
            "Container" | "Network" | "Volume" | "Image" | "Service"
        ) {
            continue;
        }
        let action: String = words.collect::<Vec<&str>>().join(" ");
        if action.is_empty() || action.ends_with("ing") {
            continue;
        }
        let step = Step {
            object: object.to_owned(),
            name: name.to_owned(),
            action,
        };
        // Compose repeats a line when a layer finishes; the state is idempotent.
        if !steps.contains(&step) {
            steps.push(step);
        }
    }
    steps
}

/// `(object, action, names)` in order of first appearance. Five containers that all
/// started are one line, which is the whole point.
fn group(steps: &[Step]) -> Vec<(String, String, Vec<String>)> {
    let mut rows: Vec<(String, String, Vec<String>)> = Vec::new();
    for step in steps {
        match rows
            .iter_mut()
            .find(|(object, action, _)| object == &step.object && action == &step.action)
        {
            Some((_, _, names)) => names.push(step.name.clone()),
            None => rows.push((
                step.object.clone(),
                step.action.clone(),
                vec![step.name.clone()],
            )),
        }
    }
    for row in &mut rows {
        row.2.sort();
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    fn scope() -> ComposeScope {
        ComposeScope {
            project: Some("ens".to_owned()),
            ..ComposeScope::default()
        }
    }

    const PROGRESS: &str = "\
 Network ens_default  Creating
 Network ens_default  Created
 Container ens-db-1  Creating
 Container ens-db-1  Created
 Container ens-wine-1  Creating
 Container ens-wine-1  Created
 Container ens-db-1  Starting
 Container ens-db-1  Started
 Container ens-wine-1  Starting
 Container ens-wine-1  Started
";

    #[test]
    fn forty_lines_of_progress_become_two() {
        let runner = ScriptedRunner::new().reply(
            "docker compose -p ens up --detach",
            0,
            "",
            PROGRESS,
            false,
        );
        let outcome = compose(
            &runner,
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Up,
                ..ComposeRequest::default()
            },
        )
        .expect("up");

        assert_eq!(
            outcome.text,
            "\
Network    Created  ens_default
Container  Created  ens-db-1, ens-wine-1
Container  Started  ens-db-1, ens-wine-1
[up 5 steps]
"
        );
    }

    #[test]
    fn up_is_detached_whatever_else_it_is_asked() {
        let runner = ScriptedRunner::new().reply(
            "docker compose -p ens up --detach --wait --build",
            0,
            "",
            "",
            false,
        );
        compose(
            &runner,
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Up,
                wait: true,
                build: true,
                ..ComposeRequest::default()
            },
        )
        .expect("up");
        assert!(
            runner.calls()[0].contains("--detach"),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn down_does_not_run_without_being_asked_twice() {
        let runner = ScriptedRunner::new();
        let err = compose(
            &runner,
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Down,
                ..ComposeRequest::default()
            },
        )
        .expect_err("gated");
        let message = err.to_string();
        assert!(message.contains("confirm: true"), "{message}");
        assert!(
            message.contains("removes this project's containers"),
            "{message}"
        );
        assert!(runner.calls().is_empty(), "nothing ran");
    }

    #[test]
    fn the_confirmation_says_out_loud_that_volumes_are_data() {
        let err = compose(
            &ScriptedRunner::new(),
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Down,
                volumes: true,
                ..ComposeRequest::default()
            },
        )
        .expect_err("gated");
        assert!(
            err.to_string().contains("whatever data is in them"),
            "{err}"
        );
    }

    #[test]
    fn a_confirmed_down_runs_and_reports_what_went() {
        let runner = ScriptedRunner::new().reply(
            "docker compose -p ens down --volumes",
            0,
            "",
            "\
 Container ens-wine-1  Stopping
 Container ens-wine-1  Stopped
 Container ens-wine-1  Removing
 Container ens-wine-1  Removed
 Volume ens_ens-data  Removing
 Volume ens_ens-data  Removed
 Network ens_default  Removing
 Network ens_default  Removed
",
            false,
        );
        let outcome = compose(
            &runner,
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Down,
                volumes: true,
                confirm: true,
                ..ComposeRequest::default()
            },
        )
        .expect("down");
        assert_eq!(
            outcome.text,
            "\
Container  Stopped  ens-wine-1
Container  Removed  ens-wine-1
Volume     Removed  ens_ens-data
Network    Removed  ens_default
[down 4 steps]
"
        );
    }

    #[test]
    fn a_service_that_will_not_start_is_the_output_not_an_error() {
        let runner = ScriptedRunner::new().reply(
            "docker compose -p ens up --detach",
            1,
            "",
            "\
 Container ens-wine-1  Starting
Error response from daemon: driver failed programming external connectivity
",
            false,
        );
        let outcome = compose(
            &runner,
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Up,
                ..ComposeRequest::default()
            },
        )
        .expect("up");
        assert_eq!(
            outcome.text,
            "\
driver failed programming external connectivity
[up 0 steps, failed with status 1]
"
        );
        assert_eq!(outcome.status, 1);
    }

    #[test]
    fn rm_never_waits_for_a_terminal_that_is_not_there() {
        let runner =
            ScriptedRunner::new().reply("docker compose -p ens rm --force", 0, "", "", false);
        compose(
            &runner,
            &ComposeRequest {
                scope: scope(),
                verb: Verb::Remove,
                confirm: true,
                ..ComposeRequest::default()
            },
        )
        .expect("rm");
        assert_eq!(runner.calls()[0], "docker compose -p ens rm --force");
    }

    #[test]
    fn a_verb_that_is_not_one_is_not_guessed_at() {
        assert_eq!(Verb::parse("up"), Some(Verb::Up));
        assert_eq!(Verb::parse("RM"), Some(Verb::Remove));
        assert_eq!(Verb::parse("prune"), None);
    }
}
