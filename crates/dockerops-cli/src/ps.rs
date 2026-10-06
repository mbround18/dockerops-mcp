//! What is running — the question asked most often, and the one `docker ps` answers most
//! expensively.
//!
//! The CLI's own output spends a header row and seven space-padded columns on each
//! container, most of it `CONTAINER ID` (twelve hex characters nobody types), `COMMAND`
//! (truncated to uselessness), and `CREATED` next to `STATUS`, which say the same thing
//! twice. What a caller actually needs is: what is it called, is it up, what image, how
//! long, and what is published. That is one line of about sixty characters.
//!
//! Containers and compose services are the same question, so one tool answers both: name
//! a `project` or `files` and the call goes through `docker compose ps`, otherwise through
//! `docker ps`. Either way the output is identical, because both are parsed into
//! [`crate::container::Container`].

use serde::Serialize;

use crate::{
    Budget, Error, Result,
    container::{Container, State, parse_json_stream, render_ports},
    invoke::{self, ComposeScope, Invocation, safe_value},
    runner::CommandRunner,
    text::{pad, plural, width_of},
};

/// Containers rendered before the listing admits it is cutting the rest.
pub const DEFAULT_LIMIT: usize = 200;

/// How to order the listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Alphabetical, which is stable across calls and so comparable between them.
    #[default]
    Name,
    /// Anything not simply running first: what you want when something is wrong.
    State,
    /// Whatever order the CLI returned, which is newest first.
    Given,
}

#[derive(Debug, Clone, Default)]
pub struct PsRequest {
    pub invocation: Invocation,
    /// Naming a project or compose file routes the call through `docker compose ps`.
    pub scope: ComposeScope,
    /// Include containers that are not running.
    pub all: bool,
    /// Keep only these compose services or container names.
    pub services: Vec<String>,
    /// `docker ps --filter` expressions, e.g. `status=exited`, `label=env=prod`,
    /// `health=unhealthy`, `name=api`.
    pub filter: Vec<String>,
    pub sort: Sort,
    pub limit: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct PsOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub containers: Vec<Container>,
    /// The command that produced this, so a caller can reproduce it in a terminal.
    pub command: String,
    pub total: usize,
    pub running: usize,
    pub stopped: usize,
    /// Set when the listing is shorter than what was found, and says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn ps(runner: &dyn CommandRunner, request: &PsRequest) -> Result<PsOutcome> {
    for service in &request.services {
        safe_value(service, "service")?;
    }
    for filter in &request.filter {
        safe_value(filter, "filter")?;
        if !filter.contains('=') {
            return Err(Error::request(format!(
                "filter `{filter}` is not a `key=value` expression; docker takes \
                 `status=exited`, `health=unhealthy`, `name=api`, `label=env=prod`"
            )));
        }
    }

    let compose = !request.scope.is_empty();
    let mut rest: Vec<String> = if compose {
        request.scope.args()?
    } else {
        Vec::new()
    };
    rest.push("ps".to_owned());
    if request.all {
        rest.push("--all".to_owned());
    }
    for filter in &request.filter {
        rest.push("--filter".to_owned());
        rest.push(filter.clone());
    }
    // `docker ps` renders one JSON object per line; compose takes `json` and may answer
    // with either that or an array. `parse_json_stream` covers both.
    rest.push("--format".to_owned());
    rest.push(if compose { "json" } else { "{{json .}}" }.to_owned());
    if compose {
        rest.extend(request.services.iter().cloned());
    }

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    if !output.ok() {
        // A daemon that is down, a compose file that is not there: one line, not an
        // error, so a batch of questions about an environment still returns the rest.
        let text = format!("{}\n[no listing: {}]\n", command, output.failure());
        return Ok(PsOutcome {
            text,
            containers: Vec::new(),
            command,
            total: 0,
            running: 0,
            stopped: 0,
            truncated: Some(output.failure()),
        });
    }

    let mut containers: Vec<Container> = parse_json_stream(&output.stdout)
        .iter()
        .filter_map(|value| {
            if compose {
                Container::from_compose_ps(value)
            } else {
                Container::from_docker_ps(value)
            }
        })
        .collect();

    // On the compose path the CLI did this already, by positional argument.
    if !compose && !request.services.is_empty() {
        containers.retain(|container| {
            request.services.iter().any(|wanted| {
                container.service.as_deref() == Some(wanted.as_str())
                    || container.name == *wanted
                    || container.name.contains(wanted.as_str())
            })
        });
    }

    match request.sort {
        Sort::Name => containers.sort_by(|a, b| a.name.cmp(&b.name)),
        Sort::State => containers.sort_by(|a, b| {
            a.state
                .is_healthy_looking()
                .cmp(&b.state.is_healthy_looking())
                .then(a.name.cmp(&b.name))
        }),
        Sort::Given => {}
    }

    let total = containers.len();
    let running = containers
        .iter()
        .filter(|c| c.state == State::Running)
        .count();
    let stopped = total - running;

    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let mut cut = total.saturating_sub(limit);
    containers.truncate(limit);

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();

    // Columns are padded only where a caller reads down the page; the last column is
    // never padded, because trailing spaces are bytes that say nothing.
    let name_width = width_of(containers.iter().map(|c| c.name.as_str()));
    let status_width = width_of(containers.iter().map(|c| c.status.as_str()));
    let image_width = width_of(containers.iter().map(|c| c.image.as_str()));

    let mut shown = 0;
    for container in &containers {
        let line = format!(
            "{}  {}  {}  {}\n",
            pad(&container.name, name_width),
            pad(&container.status, status_width),
            pad(&container.image, image_width),
            render_ports(&container.ports),
        );
        if !budget.try_spend(&line) {
            cut += containers.len() - shown;
            break;
        }
        body.push_str(&line);
        shown += 1;
    }
    containers.truncate(shown);

    let truncated = (cut > 0).then(|| {
        if budget.skipped() > 0 {
            format!("{cut} not shown, byte budget reached")
        } else {
            format!("{cut} not shown, limit {limit}")
        }
    });

    let mut footer = if total == 0 {
        let scope = if compose {
            " in this project"
        } else {
            " running"
        };
        format!(
            "[no containers{scope}{}]",
            if request.all {
                ""
            } else {
                "; pass `all` to include stopped ones"
            }
        )
    } else {
        let mut parts = Vec::new();
        if running > 0 {
            parts.push(format!("{running} up"));
        }
        if stopped > 0 {
            parts.push(format!("{stopped} stopped"));
        }
        format!("[{}: {}", plural(total, "container"), parts.join(", "))
    };
    if total > 0 {
        if let Some(note) = &truncated {
            footer.push_str(&format!(", {note}"));
        }
        footer.push(']');
    }
    footer.push('\n');

    Ok(PsOutcome {
        text: format!("{body}{footer}"),
        containers,
        command,
        total,
        running,
        stopped,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const PS_JSON: &str = r#"{"ID":"aaaabbbbcccc1111","Names":"ens-wine-1","Image":"ghcr.io/acme/ens:1.4","State":"running","Status":"Up 3 hours (unhealthy)","Ports":"0.0.0.0:15636->15636/udp, :::15636->15636/udp","Labels":"com.docker.compose.project=ens,com.docker.compose.service=wine"}
{"ID":"ddddeeeeffff2222","Names":"ens-db-1","Image":"postgres:17","State":"running","Status":"Up 3 hours (healthy)","Ports":"127.0.0.1:5432->5432/tcp","Labels":"com.docker.compose.project=ens,com.docker.compose.service=db"}
{"ID":"99998888777733","Names":"ens-coldtest","Image":"ghcr.io/acme/ens:1.4","State":"exited","Status":"Exited (137) 5 minutes ago","Ports":"","Labels":""}"#;

    fn runner() -> ScriptedRunner {
        ScriptedRunner::new().with("docker ps --all --format {{json .}}", PS_JSON)
    }

    #[test]
    fn one_line_per_container_and_a_footer_that_counts_them() {
        let outcome = ps(
            &runner(),
            &PsRequest {
                all: true,
                ..PsRequest::default()
            },
        )
        .expect("listed");

        assert_eq!(
            outcome.text,
            "ens-coldtest  exited (137) 5m  ghcr.io/acme/ens:1.4  -\n\
             ens-db-1      up 3h            postgres:17           127.0.0.1:5432\n\
             ens-wine-1    up 3h unhealthy  ghcr.io/acme/ens:1.4  15636/udp\n\
             [3 containers: 2 up, 1 stopped]\n"
        );
        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.running, 2);
        assert_eq!(outcome.stopped, 1);
        assert!(outcome.truncated.is_none());
    }

    #[test]
    fn the_structured_copy_carries_the_text_and_the_command() {
        // The lesson from the sibling server: a host that renders only the structured
        // half must still see the output.
        let outcome = ps(
            &runner(),
            &PsRequest {
                all: true,
                ..PsRequest::default()
            },
        )
        .expect("listed");
        let value = serde_json::to_value(&outcome).expect("serialized");
        assert_eq!(value["text"].as_str(), Some(outcome.text.as_str()));
        assert_eq!(
            value["command"].as_str(),
            Some("docker ps --all --format {{json .}}")
        );
        assert_eq!(value["containers"][0]["service"].as_str(), None);
        assert_eq!(value["containers"][1]["service"].as_str(), Some("db"));
    }

    #[test]
    fn sorting_by_state_puts_the_problem_first() {
        let outcome = ps(
            &runner(),
            &PsRequest {
                all: true,
                sort: Sort::State,
                ..PsRequest::default()
            },
        )
        .expect("listed");
        let names: Vec<&str> = outcome.containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["ens-coldtest", "ens-db-1", "ens-wine-1"]);
    }

    #[test]
    fn naming_a_service_narrows_a_plain_docker_listing() {
        let outcome = ps(
            &runner(),
            &PsRequest {
                all: true,
                services: vec!["db".to_owned()],
                ..PsRequest::default()
            },
        )
        .expect("listed");
        assert_eq!(outcome.containers.len(), 1);
        assert_eq!(outcome.containers[0].name, "ens-db-1");
    }

    #[test]
    fn a_compose_scope_asks_compose_and_passes_services_positionally() {
        let compose_json = r#"[{"Name":"ens-wine-1","Project":"ens","Service":"wine","Image":"ghcr.io/acme/ens:1.4","State":"running","Status":"Up 3 hours","Health":"","ExitCode":0,"Publishers":[{"URL":"0.0.0.0","TargetPort":80,"PublishedPort":8080,"Protocol":"tcp"},{"URL":"::","TargetPort":80,"PublishedPort":8080,"Protocol":"tcp"}]}]"#;
        let runner = ScriptedRunner::new().with(
            "docker compose -f docker-compose.yml -p ens ps --format json wine",
            compose_json,
        );
        let outcome = ps(
            &runner,
            &PsRequest {
                scope: ComposeScope {
                    files: vec!["docker-compose.yml".to_owned()],
                    project: Some("ens".to_owned()),
                    ..ComposeScope::default()
                },
                services: vec!["wine".to_owned()],
                ..PsRequest::default()
            },
        )
        .expect("listed");

        assert_eq!(
            outcome.text,
            "ens-wine-1  up 3h  ghcr.io/acme/ens:1.4  8080→80\n[1 container: 1 up]\n"
        );
        assert_eq!(outcome.containers[0].service.as_deref(), Some("wine"));
    }

    #[test]
    fn a_daemon_that_is_down_is_one_line_rather_than_a_failed_call() {
        let runner = ScriptedRunner::new().failing(
            "docker ps --format {{json .}}",
            1,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.\n",
        );
        let outcome = ps(&runner, &PsRequest::default()).expect("still an answer");
        assert!(
            outcome.text.ends_with(
                "[no listing: Cannot connect to the Docker daemon at unix:///var/run/docker.sock.]\n"
            ),
            "{}",
            outcome.text
        );
        assert_eq!(outcome.total, 0);
        assert!(outcome.truncated.is_some());
    }

    #[test]
    fn an_empty_listing_says_what_would_have_widened_it() {
        let runner = ScriptedRunner::new().with("docker ps --format {{json .}}", "");
        let outcome = ps(&runner, &PsRequest::default()).expect("listed");
        assert_eq!(
            outcome.text,
            "[no containers running; pass `all` to include stopped ones]\n"
        );
    }

    #[test]
    fn the_limit_and_the_budget_both_report_what_they_cut() {
        let outcome = ps(
            &runner(),
            &PsRequest {
                all: true,
                limit: Some(1),
                ..PsRequest::default()
            },
        )
        .expect("listed");
        assert!(
            outcome.text.contains("2 not shown, limit 1"),
            "{}",
            outcome.text
        );

        let outcome = ps(
            &runner(),
            &PsRequest {
                all: true,
                max_bytes: Some(60),
                ..PsRequest::default()
            },
        )
        .expect("listed");
        assert!(
            outcome.text.contains("byte budget reached"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn a_filter_that_is_not_a_key_value_expression_is_refused_with_examples() {
        let err = ps(
            &runner(),
            &PsRequest {
                filter: vec!["exited".to_owned()],
                ..PsRequest::default()
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("status=exited"), "{err}");
    }

    #[test]
    fn a_name_that_would_arrive_as_a_flag_never_reaches_docker() {
        let runner = runner();
        let err = ps(
            &runner,
            &PsRequest {
                services: vec!["--privileged".to_owned()],
                ..PsRequest::default()
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("read as a flag"), "{err}");
        assert!(runner.calls().is_empty(), "nothing should have been run");
    }
}
