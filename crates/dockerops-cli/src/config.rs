//! A compose project resolved: what the file *means*, not what it says.
//!
//! `docker compose config` answers an important question — what did the overrides,
//! profiles, `.env` interpolation and `extends` actually produce — and answers it with the
//! whole merged document, several hundred lines of YAML for a handful of services. The
//! facts people go looking for are the same few every time: what image is each service
//! really on, what does it publish, what does it mount, what does it wait for. So that is
//! what this renders, with the rest available a field at a time through the other modes.

use serde::Serialize;
use serde_json::Value;

use crate::{
    Budget, Error, Result,
    invoke::{self, ComposeScope, Invocation, safe_value},
    runner::CommandRunner,
    text::{elide, pad, plural, width_of},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// One line per service: image, ports, mounts, dependencies.
    #[default]
    Summary,
    /// Service names only — the cheapest possible answer to "what is in this project".
    Services,
    /// The image each service resolves to, which is what `compose config --images` says.
    Images,
    /// Declared volumes and networks, and which services use them.
    Resources,
    /// Profiles, and which services belong to each.
    Profiles,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigRequest {
    pub invocation: Invocation,
    pub scope: ComposeScope,
    /// Limit to these services.
    pub services: Vec<String>,
    pub mode: Mode,
    /// Leave `${VAR}` unresolved (`--no-interpolate`), to see the file's own intent rather
    /// than this machine's environment.
    pub raw: bool,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Service {
    pub name: String,
    pub image: String,
    pub ports: Vec<String>,
    pub mounts: Vec<String>,
    pub depends_on: Vec<String>,
    pub profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ConfigOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub services: Vec<Service>,
    pub volumes: Vec<String>,
    pub networks: Vec<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn config(runner: &dyn CommandRunner, request: &ConfigRequest) -> Result<ConfigOutcome> {
    for service in &request.services {
        safe_value(service, "service")?;
    }

    let mut rest = request.scope.args()?;
    rest.push("config".to_owned());
    rest.push("--format".to_owned());
    rest.push("json".to_owned());
    if request.raw {
        rest.push("--no-interpolate".to_owned());
    }
    rest.extend(request.services.iter().cloned());

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    if !output.ok() && output.stdout.trim().is_empty() {
        // A compose file that does not parse is the single most useful thing this tool can
        // report, and it belongs in the output rather than behind an error.
        return Ok(ConfigOutcome {
            text: format!("[no config: {}]\n", output.failure()),
            services: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
            command,
            truncated: None,
        });
    }

    let document: Value = serde_json::from_str(output.stdout.trim()).map_err(|err| {
        Error::request(format!(
            "docker compose config did not return JSON ({err}); its output began: {}",
            elide(output.stdout.trim(), 120)
        ))
    })?;

    let mut services: Vec<Service> = document
        .get("services")
        .and_then(Value::as_object)
        .map(|map| map.iter().map(|(name, body)| service(name, body)).collect())
        .unwrap_or_default();
    services.sort_by(|a, b| a.name.cmp(&b.name));

    let volumes = names(&document, "volumes");
    let networks = names(&document, "networks");

    let mut budget = Budget::new(request.max_bytes);
    let body = match request.mode {
        Mode::Summary => summary(&services, &mut budget),
        Mode::Services => lines(services.iter().map(|s| s.name.clone()), &mut budget),
        Mode::Images => two_columns(
            services.iter().map(|s| (s.name.clone(), s.image.clone())),
            &mut budget,
        ),
        Mode::Resources => resources(&services, &volumes, &networks, &mut budget),
        Mode::Profiles => profiles(&services, &mut budget),
    };

    let truncated = (budget.skipped() > 0)
        .then(|| format!("{} not shown, byte budget reached", budget.skipped()));
    let mut footer = format!(
        "[{}, {}, {}",
        plural(services.len(), "service"),
        plural(volumes.len(), "volume"),
        plural(networks.len(), "network")
    );
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(ConfigOutcome {
        text: format!("{body}{footer}"),
        services,
        volumes,
        networks,
        command,
        truncated,
    })
}

fn service(name: &str, body: &Value) -> Service {
    Service {
        name: name.to_owned(),
        image: body
            .get("image")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                // A service that builds has no image until it is built; saying where it is
                // built from is more use than an empty column.
                body.get("build").map(|build| {
                    let context = build.get("context").and_then(Value::as_str).unwrap_or(".");
                    format!("build {context}")
                })
            })
            .unwrap_or_else(|| "-".to_owned()),
        ports: body
            .get("ports")
            .and_then(Value::as_array)
            .map(|ports| ports.iter().map(port).collect())
            .unwrap_or_default(),
        mounts: body
            .get("volumes")
            .and_then(Value::as_array)
            .map(|mounts| mounts.iter().map(mount).collect())
            .unwrap_or_default(),
        depends_on: body
            .get("depends_on")
            .map(|depends| match depends {
                Value::Object(map) => map.keys().cloned().collect(),
                Value::Array(list) => list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default(),
        profiles: body
            .get("profiles")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        restart: body
            .get("restart")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// `{published: "8080", target: 80}` → `8080→80`, the way `ps` renders a port.
fn port(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            let published = map.get("published").map(render_scalar).unwrap_or_default();
            let target = map.get("target").map(render_scalar).unwrap_or_default();
            let protocol = map.get("protocol").and_then(Value::as_str).unwrap_or("tcp");
            let mut rendered = if published.is_empty() {
                target.clone()
            } else {
                format!("{published}\u{2192}{target}")
            };
            if protocol != "tcp" {
                rendered.push_str(&format!("/{protocol}"));
            }
            rendered
        }
        other => render_scalar(other),
    }
}

/// `{source: "/srv/ens", target: "/data"}` → `/srv/ens→/data`.
fn mount(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            let source = map
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("anonymous");
            let target = map.get("target").and_then(Value::as_str).unwrap_or("?");
            format!("{source}\u{2192}{target}")
        }
        other => render_scalar(other),
    }
}

fn render_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

fn names(document: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = document
        .get(key)
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// One line per service, with each fact labelled and omitted when absent.
fn summary(services: &[Service], budget: &mut Budget) -> String {
    let width = width_of(services.iter().map(|service| service.name.as_str()));
    let image_width = width_of(services.iter().map(|service| service.image.as_str()));
    let mut body = String::new();
    for service in services {
        let mut line = format!(
            "{}  {}",
            pad(&service.name, width),
            pad(&service.image, image_width)
        );
        for (label, values) in [
            ("ports", &service.ports),
            ("mounts", &service.mounts),
            ("needs", &service.depends_on),
        ] {
            if !values.is_empty() {
                line.push_str(&format!("  {label} {}", values.join(",")));
            }
        }
        if let Some(restart) = &service.restart
            && restart != "no"
        {
            line.push_str(&format!("  restart {restart}"));
        }
        // A service with no ports, mounts or dependencies would otherwise carry the image
        // column's padding to the end of the line.
        let line = format!("{}\n", line.trim_end());
        if budget.try_spend(&line) {
            body.push_str(&line);
        } else {
            budget.note_skipped();
        }
    }
    body
}

fn lines(values: impl Iterator<Item = String>, budget: &mut Budget) -> String {
    let mut body = String::new();
    for value in values {
        let line = format!("{value}\n");
        if budget.try_spend(&line) {
            body.push_str(&line);
        } else {
            budget.note_skipped();
        }
    }
    body
}

fn two_columns(pairs: impl Iterator<Item = (String, String)>, budget: &mut Budget) -> String {
    let pairs: Vec<(String, String)> = pairs.collect();
    let width = width_of(pairs.iter().map(|(left, _)| left.as_str()));
    let mut body = String::new();
    for (left, right) in pairs {
        let line = format!("{}  {right}\n", pad(&left, width));
        if budget.try_spend(&line) {
            body.push_str(&line);
        } else {
            budget.note_skipped();
        }
    }
    body
}

/// Declared volumes and networks are only interesting next to what uses them.
fn resources(
    services: &[Service],
    volumes: &[String],
    networks: &[String],
    budget: &mut Budget,
) -> String {
    let mut body = String::new();
    for volume in volumes {
        let users: Vec<&str> = services
            .iter()
            .filter(|service| {
                service
                    .mounts
                    .iter()
                    .any(|mount| mount.split('\u{2192}').next() == Some(volume.as_str()))
            })
            .map(|service| service.name.as_str())
            .collect();
        let line = format!(
            "volume {volume}  {}\n",
            if users.is_empty() {
                "unused".to_owned()
            } else {
                users.join(",")
            }
        );
        if budget.try_spend(&line) {
            body.push_str(&line);
        } else {
            budget.note_skipped();
        }
    }
    for network in networks {
        let line = format!("network {network}\n");
        if budget.try_spend(&line) {
            body.push_str(&line);
        } else {
            budget.note_skipped();
        }
    }
    body
}

/// Which services a profile would bring up — the thing that decides whether a `--profile`
/// flag was needed.
fn profiles(services: &[Service], budget: &mut Budget) -> String {
    let mut named: Vec<(String, Vec<&str>)> = Vec::new();
    let mut always: Vec<&str> = Vec::new();
    for service in services {
        if service.profiles.is_empty() {
            always.push(&service.name);
            continue;
        }
        for profile in &service.profiles {
            match named.iter_mut().find(|(name, _)| name == profile) {
                Some((_, members)) => members.push(&service.name),
                None => named.push((profile.clone(), vec![&service.name])),
            }
        }
    }
    named.sort_by(|a, b| a.0.cmp(&b.0));

    let mut body = String::new();
    if !always.is_empty() {
        let line = format!("(always)  {}\n", always.join(","));
        if budget.try_spend(&line) {
            body.push_str(&line);
        }
    }
    let width = width_of(named.iter().map(|(name, _)| name.as_str()));
    for (profile, members) in named {
        let line = format!("{}  {}\n", pad(&profile, width), members.join(","));
        if budget.try_spend(&line) {
            body.push_str(&line);
        } else {
            budget.note_skipped();
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const CALL: &str = "docker compose -p ens config --format json";

    fn project() -> String {
        serde_json::json!({
            "name": "ens",
            "services": {
                "wine": {
                    "image": "ghcr.io/acme/ens:1.4",
                    "restart": "unless-stopped",
                    "ports": [
                        {"published": "15636", "target": 15636, "protocol": "udp"},
                        {"published": "8080", "target": 80, "protocol": "tcp"},
                    ],
                    "volumes": [{"source": "ens-data", "target": "/home/steam/ens"}],
                    "depends_on": {"db": {"condition": "service_healthy"}},
                },
                "db": {
                    "image": "postgres:17",
                    "volumes": [{"source": "pgdata", "target": "/var/lib/postgresql/data"}],
                },
                "backup": {
                    "build": {"context": "./backup"},
                    "profiles": ["ops"],
                },
            },
            "volumes": {"ens-data": {}, "pgdata": {}, "spare": {}},
            "networks": {"default": {}},
        })
        .to_string()
    }

    fn scope() -> ComposeScope {
        ComposeScope {
            project: Some("ens".to_owned()),
            ..ComposeScope::default()
        }
    }

    #[test]
    fn a_resolved_project_is_one_line_per_service() {
        let runner = ScriptedRunner::new().with(CALL, &project());
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                ..ConfigRequest::default()
            },
        )
        .expect("config");

        assert_eq!(
            outcome.text,
            "\
backup  build ./backup
db      postgres:17           mounts pgdata→/var/lib/postgresql/data
wine    ghcr.io/acme/ens:1.4  ports 15636→15636/udp,8080→80  mounts ens-data→/home/steam/ens  needs db  restart unless-stopped
[3 services, 3 volumes, 1 network]
"
        );
    }

    #[test]
    fn the_cheapest_question_is_which_services_exist() {
        let runner = ScriptedRunner::new().with(CALL, &project());
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                mode: Mode::Services,
                ..ConfigRequest::default()
            },
        )
        .expect("config");
        assert_eq!(
            outcome.text,
            "backup\ndb\nwine\n[3 services, 3 volumes, 1 network]\n"
        );
    }

    #[test]
    fn images_says_what_each_service_actually_resolves_to() {
        let runner = ScriptedRunner::new().with(CALL, &project());
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                mode: Mode::Images,
                ..ConfigRequest::default()
            },
        )
        .expect("config");
        assert_eq!(
            outcome.text,
            "\
backup  build ./backup
db      postgres:17
wine    ghcr.io/acme/ens:1.4
[3 services, 3 volumes, 1 network]
"
        );
    }

    #[test]
    fn a_declared_volume_nothing_mounts_is_worth_saying_out_loud() {
        let runner = ScriptedRunner::new().with(CALL, &project());
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                mode: Mode::Resources,
                ..ConfigRequest::default()
            },
        )
        .expect("config");
        assert_eq!(
            outcome.text,
            "\
volume ens-data  wine
volume pgdata  db
volume spare  unused
network default
[3 services, 3 volumes, 1 network]
"
        );
    }

    #[test]
    fn profiles_say_which_services_a_flag_would_bring_up() {
        let runner = ScriptedRunner::new().with(CALL, &project());
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                mode: Mode::Profiles,
                ..ConfigRequest::default()
            },
        )
        .expect("config");
        assert_eq!(
            outcome.text,
            "(always)  db,wine\nops  backup\n[3 services, 3 volumes, 1 network]\n"
        );
    }

    #[test]
    fn a_compose_file_that_does_not_parse_is_the_answer_not_an_error() {
        let runner = ScriptedRunner::new().failing(
            CALL,
            1,
            "services.wine.ports: invalid port specification: \"808o:80\"\n",
        );
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                ..ConfigRequest::default()
            },
        )
        .expect("config");
        assert_eq!(
            outcome.text,
            "[no config: services.wine.ports: invalid port specification: \"808o:80\"]\n"
        );
    }

    #[test]
    fn raw_mode_leaves_the_interpolation_alone() {
        let runner = ScriptedRunner::new().with(
            "docker compose -p ens config --format json --no-interpolate",
            &serde_json::json!({"services": {"api": {"image": "${REGISTRY}/api:${TAG}"}}})
                .to_string(),
        );
        let outcome = config(
            &runner,
            &ConfigRequest {
                scope: scope(),
                raw: true,
                ..ConfigRequest::default()
            },
        )
        .expect("config");
        assert!(
            outcome.text.contains("${REGISTRY}/api:${TAG}"),
            "{}",
            outcome.text
        );
    }
}
