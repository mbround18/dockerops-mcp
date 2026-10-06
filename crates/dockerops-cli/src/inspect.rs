//! The fields of `docker inspect` you asked for, not the document.
//!
//! `docker inspect` on one container is roughly four hundred lines of JSON — the full
//! config, the full host config, every default the daemon filled in — and the question it
//! was asked is almost always one of half a dozen: is it healthy, what is it mounting,
//! what image is it actually running, why did it exit. So this tool takes dotted field
//! paths, or one of the named presets that are those half-dozen questions, and renders
//! only what they select.
//!
//! The selection happens here rather than in `--format`, for two reasons: a Go template
//! is a thing to get wrong on the first attempt, and one call can inspect every target at
//! once while still reporting per-target failures separately.

use serde::Serialize;
use serde_json::Value;

use crate::{
    Budget, Error, Result,
    invoke::{self, Invocation, safe_value},
    runner::CommandRunner,
    text::{elide, plural},
};

/// What `docker inspect` should look for. Docker resolves a bare name against containers
/// first, then images, which is occasionally the wrong guess.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    /// Let Docker decide, as the CLI does.
    #[default]
    Any,
    Container,
    Image,
    Network,
    Volume,
}

impl Kind {
    fn flag(self) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::Container => Some("container"),
            Self::Image => Some("image"),
            Self::Network => Some("network"),
            Self::Volume => Some("volume"),
        }
    }
}

/// A named set of fields: the question, rather than the paths that answer it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// State, health, exit code, restart count — why is it not working.
    Health,
    /// Environment variables.
    Env,
    /// Volumes and bind mounts, as `source -> destination`.
    Mounts,
    /// Published ports.
    Ports,
    /// Networks, addresses and aliases.
    Network,
    /// What is actually running: image reference, resolved digest, created date.
    Image,
    /// Labels, including the compose project and service.
    Labels,
    /// Restart policy and what it has been doing.
    Restart,
    /// Entrypoint, command, working directory, user.
    Entrypoint,
    /// Memory and CPU limits.
    Limits,
    /// A short digest: the fields worth seeing before deciding what to ask next.
    Summary,
}

impl Preset {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "health" => Self::Health,
            "env" => Self::Env,
            "mounts" => Self::Mounts,
            "ports" => Self::Ports,
            "network" | "networks" => Self::Network,
            "image" => Self::Image,
            "labels" => Self::Labels,
            "restart" => Self::Restart,
            "entrypoint" | "command" => Self::Entrypoint,
            "limits" | "resources" => Self::Limits,
            "summary" => Self::Summary,
            _ => return None,
        })
    }

    fn fields(self) -> &'static [&'static str] {
        match self {
            Self::Health => &[
                "State.Status",
                "State.Health.Status",
                "State.Health.FailingStreak",
                "State.Health.Log.*.Output",
                "State.ExitCode",
                "State.Error",
                "State.OOMKilled",
                "RestartCount",
            ],
            Self::Env => &["Config.Env"],
            Self::Mounts => &["Mounts.*.Source", "Mounts.*.Destination", "Mounts.*.RW"],
            Self::Ports => &["NetworkSettings.Ports", "HostConfig.PortBindings"],
            Self::Network => &[
                "HostConfig.NetworkMode",
                "NetworkSettings.Networks.*.IPAddress",
                "NetworkSettings.Networks.*.Aliases",
            ],
            Self::Image => &["Config.Image", "Image", "Created"],
            Self::Labels => &["Config.Labels"],
            Self::Restart => &[
                "HostConfig.RestartPolicy.Name",
                "HostConfig.RestartPolicy.MaximumRetryCount",
                "RestartCount",
                "State.Status",
                "State.ExitCode",
                "State.Error",
            ],
            Self::Entrypoint => &[
                "Config.Entrypoint",
                "Config.Cmd",
                "Config.WorkingDir",
                "Config.User",
            ],
            Self::Limits => &[
                "HostConfig.Memory",
                "HostConfig.NanoCpus",
                "HostConfig.CpuShares",
                "HostConfig.PidsLimit",
            ],
            Self::Summary => &[
                "State.Status",
                "State.Health.Status",
                "Config.Image",
                "RestartCount",
                "HostConfig.RestartPolicy.Name",
                "Mounts.*.Destination",
            ],
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InspectRequest {
    pub invocation: Invocation,
    /// Containers, images, networks or volumes. Several in one call.
    pub targets: Vec<String>,
    pub kind: Kind,
    /// Dotted paths into the inspect document: `State.Health.Status`,
    /// `Mounts.*.Destination`, `Config.Env`. `*` means every element of an array or every
    /// key of an object.
    pub fields: Vec<String>,
    /// A named field set, used when `fields` is empty. Defaults to `summary`.
    pub preset: Option<Preset>,
    /// List the keys at each path instead of their values — how to find out what a
    /// document holds without fetching it.
    pub keys: bool,
    /// Cap on the characters of any one rendered value. Defaults to 240.
    pub max_value: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct InspectTarget {
    pub target: String,
    /// `ok`, or why nothing was rendered.
    pub status: String,
    /// The fields that resolved, as path and rendered value.
    pub fields: Vec<(String, String)>,
    /// Paths that matched nothing. A misspelt path is worth saying out loud, because the
    /// alternative is a caller concluding a field is empty.
    pub missing: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct InspectOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub targets: Vec<InspectTarget>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

const DEFAULT_MAX_VALUE: usize = 240;

pub fn inspect(runner: &dyn CommandRunner, request: &InspectRequest) -> Result<InspectOutcome> {
    if request.targets.is_empty() {
        return Err(Error::request(
            "name at least one container, image, network or volume in `targets`",
        ));
    }
    for target in &request.targets {
        safe_value(target, "target")?;
    }

    let paths: Vec<String> = if request.fields.is_empty() {
        request
            .preset
            .unwrap_or(Preset::Summary)
            .fields()
            .iter()
            .map(|field| (*field).to_owned())
            .collect()
    } else {
        request.fields.clone()
    };

    let mut rest = vec!["inspect".to_owned()];
    if let Some(flag) = request.kind.flag() {
        rest.push("--type".to_owned());
        rest.push(flag.to_owned());
    }
    // One document per line, so a batch is one call and still parses target by target.
    rest.push("--format".to_owned());
    rest.push("{{json .}}".to_owned());
    rest.extend(request.targets.iter().cloned());

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    // Docker writes a document per resolved target to stdout and a line per unresolved
    // one to stderr, and does not say which is which. Documents carry their own name, so
    // matching them up by name is how a batch keeps its order.
    let documents: Vec<Value> = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    let mut budget = Budget::new(request.max_bytes);
    let max_value = request.max_value.unwrap_or(DEFAULT_MAX_VALUE);
    let mut body = String::new();
    let mut targets = Vec::new();
    let mut cut = 0;

    for (index, target) in request.targets.iter().enumerate() {
        let document = documents
            .iter()
            .find(|document| names(document).iter().any(|name| matches(name, target)))
            .or_else(|| {
                (request.targets.len() == 1)
                    .then(|| documents.first())
                    .flatten()
            });

        let Some(document) = document else {
            let status = if output.ok() {
                format!("no such {}", described(request.kind))
            } else {
                output.failure()
            };
            let line = format!("#{} {target} — {status}\n", index + 1);
            if budget.try_spend(&line) {
                body.push_str(&line);
            } else {
                cut += 1;
            }
            targets.push(InspectTarget {
                target: target.clone(),
                status,
                fields: Vec::new(),
                missing: Vec::new(),
            });
            continue;
        };

        let mut fields = Vec::new();
        let mut missing = Vec::new();
        for path in &paths {
            let found = select(document, path);
            if found.is_empty() {
                missing.push(path.clone());
                continue;
            }
            for (resolved, value) in found {
                let rendered = if request.keys {
                    render_keys(value)
                } else {
                    render(value, max_value)
                };
                // A field that is present but empty is a fact; a field that renders to
                // nothing at all is noise, and the preset that asked for it was a guess.
                if rendered.is_empty() {
                    continue;
                }
                fields.push((resolved, rendered));
            }
        }

        let header = format!("#{} {target}\n", index + 1);
        if !budget.try_spend(&header) {
            cut += 1;
            targets.push(InspectTarget {
                target: target.clone(),
                status: "not shown, byte budget reached".to_owned(),
                fields: Vec::new(),
                missing,
            });
            continue;
        }
        body.push_str(&header);

        let mut shown = Vec::new();
        for (path, value) in fields {
            let line = format!("{path}: {value}\n");
            if !budget.try_spend(&line) {
                cut += 1;
                break;
            }
            body.push_str(&line);
            shown.push((path, value));
        }
        if !missing.is_empty() {
            let line = format!("[not present: {}]\n", missing.join(", "));
            if budget.try_spend(&line) {
                body.push_str(&line);
            }
        }
        targets.push(InspectTarget {
            target: target.clone(),
            status: "ok".to_owned(),
            fields: shown,
            missing,
        });
    }

    let truncated = (cut > 0).then(|| format!("{cut} not shown, byte budget reached"));
    let fields: usize = targets.iter().map(|target| target.fields.len()).sum();
    let mut footer = format!(
        "[{}, {}",
        plural(targets.len(), "target"),
        plural(fields, "field")
    );
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(InspectOutcome {
        text: format!("{body}{footer}"),
        targets,
        command,
        truncated,
    })
}

fn described(kind: Kind) -> &'static str {
    kind.flag().unwrap_or("container, image, network or volume")
}

/// The names a document answers to: `Name`, `Id`, and image tags.
fn names(document: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["Name", "Id", "ID"] {
        if let Some(value) = document.get(key).and_then(Value::as_str) {
            names.push(value.trim_start_matches('/').to_owned());
        }
    }
    if let Some(tags) = document.get("RepoTags").and_then(Value::as_array) {
        names.extend(tags.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    names
}

/// A target matches a name outright, or as the 12-character prefix of an id.
fn matches(name: &str, target: &str) -> bool {
    name == target
        || name.trim_start_matches("sha256:").starts_with(target) && target.len() >= 8
        || name.split(':').next() == Some(target)
}

/// Resolve a dotted path, returning each match with the concrete path that reached it —
/// so a `*` in the request comes back as the key or index it matched.
fn select<'a>(document: &'a Value, path: &str) -> Vec<(String, &'a Value)> {
    let mut found: Vec<(String, &Value)> = vec![(String::new(), document)];
    for segment in path.split('.') {
        let mut next = Vec::new();
        for (resolved, value) in found {
            match (segment, value) {
                ("*", Value::Array(items)) => next.extend(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| (join(&resolved, &index.to_string()), item)),
                ),
                ("*", Value::Object(map)) => {
                    next.extend(map.iter().map(|(key, item)| (join(&resolved, key), item)))
                }
                (segment, Value::Array(items)) => {
                    if let Some(item) = segment.parse::<usize>().ok().and_then(|i| items.get(i)) {
                        next.push((join(&resolved, segment), item));
                    }
                }
                (segment, Value::Object(map)) => {
                    if let Some(item) = map.get(segment) {
                        next.push((join(&resolved, segment), item));
                    }
                }
                _ => {}
            }
        }
        found = next;
    }
    found
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .collect()
}

fn join(resolved: &str, segment: &str) -> String {
    if resolved.is_empty() {
        segment.to_owned()
    } else {
        format!("{resolved}.{segment}")
    }
}

/// A value as the shortest text that still says what it is.
fn render(value: &Value, max: usize) -> String {
    let rendered = match value {
        Value::Null => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_owned(),
        Value::Array(items) => items
            .iter()
            .map(|item| render(item, max))
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| {
                let item = render(item, max);
                if item.is_empty() {
                    key.clone()
                } else {
                    format!("{key}={item}")
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    elide(&rendered, max)
}

/// The shape of a value rather than its contents.
fn render_keys(value: &Value) -> String {
    match value {
        Value::Object(map) => map.keys().cloned().collect::<Vec<_>>().join(", "),
        Value::Array(items) => plural(items.len(), "item"),
        other => render(other, DEFAULT_MAX_VALUE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const CALL: &str = "docker inspect --format {{json .}}";

    fn container() -> String {
        serde_json::json!({
            "Name": "/ens-wine-1",
            "Id": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "Created": "2026-10-06T09:00:00Z",
            "RestartCount": 3,
            "State": {
                "Status": "running",
                "ExitCode": 0,
                "Error": "",
                "OOMKilled": false,
                "Health": {
                    "Status": "unhealthy",
                    "FailingStreak": 7,
                    "Log": [{"Output": "steamcmd not responding\n"}],
                },
            },
            "Config": {
                "Image": "ghcr.io/acme/ens:1.4",
                "Env": ["PUID=1000", "DISPLAY=:1"],
                "Labels": {"com.docker.compose.service": "wine"},
            },
            "HostConfig": {
                "RestartPolicy": {"Name": "unless-stopped", "MaximumRetryCount": 0},
                "Memory": 0,
            },
            "Mounts": [
                {"Source": "/srv/ens", "Destination": "/home/steam/ens", "RW": true},
            ],
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn a_health_question_costs_six_lines_rather_than_four_hundred() {
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-wine-1"), &container());
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-wine-1".to_owned()],
                preset: Some(Preset::Health),
                ..InspectRequest::default()
            },
        )
        .expect("inspect");

        assert_eq!(
            outcome.text,
            "\
#1 ens-wine-1
State.Status: running
State.Health.Status: unhealthy
State.Health.FailingStreak: 7
State.Health.Log.0.Output: steamcmd not responding
State.ExitCode: 0
State.OOMKilled: false
RestartCount: 3
[1 target, 7 fields]
"
        );
        // `State.Error` is present and empty. Rendering it would spend a line on nothing,
        // and calling it missing would be a lie, so it is dropped without comment.
        assert!(outcome.targets[0].missing.is_empty());
    }

    #[test]
    fn a_dotted_path_asks_for_exactly_one_field() {
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-wine-1"), &container());
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-wine-1".to_owned()],
                fields: vec!["Config.Image".to_owned()],
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "#1 ens-wine-1\nConfig.Image: ghcr.io/acme/ens:1.4\n[1 target, 1 field]\n"
        );
    }

    #[test]
    fn a_star_comes_back_as_the_keys_it_matched() {
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-wine-1"), &container());
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-wine-1".to_owned()],
                preset: Some(Preset::Mounts),
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "\
#1 ens-wine-1
Mounts.0.Source: /srv/ens
Mounts.0.Destination: /home/steam/ens
Mounts.0.RW: true
[1 target, 3 fields]
"
        );
    }

    #[test]
    fn an_object_renders_as_pairs_and_an_array_as_a_list() {
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-wine-1"), &container());
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-wine-1".to_owned()],
                fields: vec!["Config.Env".to_owned(), "Config.Labels".to_owned()],
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "\
#1 ens-wine-1
Config.Env: PUID=1000, DISPLAY=:1
Config.Labels: com.docker.compose.service=wine
[1 target, 2 fields]
"
        );
    }

    #[test]
    fn keys_mode_says_what_a_document_holds_without_fetching_it() {
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-wine-1"), &container());
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-wine-1".to_owned()],
                fields: vec!["State".to_owned()],
                keys: true,
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "#1 ens-wine-1\nState: Error, ExitCode, Health, OOMKilled, Status\n[1 target, 1 field]\n"
        );
    }

    #[test]
    fn a_misspelt_path_is_reported_rather_than_read_as_empty() {
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-wine-1"), &container());
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-wine-1".to_owned()],
                fields: vec!["State.Healthy".to_owned()],
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "#1 ens-wine-1\n[not present: State.Healthy]\n[1 target, 0 fields]\n"
        );
        assert_eq!(outcome.targets[0].missing, vec!["State.Healthy"]);
    }

    #[test]
    fn one_batch_inspects_several_targets_and_keeps_their_order() {
        let mut both = container();
        both.push_str(
            &(serde_json::json!({"Name": "/ens-db-1", "State": {"Status": "exited"}}).to_string()
                + "\n"),
        );
        let runner = ScriptedRunner::new().with(&format!("{CALL} ens-db-1 ens-wine-1"), &both);
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens-db-1".to_owned(), "ens-wine-1".to_owned()],
                fields: vec!["State.Status".to_owned()],
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "\
#1 ens-db-1
State.Status: exited
#2 ens-wine-1
State.Status: running
[2 targets, 2 fields]
"
        );
    }

    #[test]
    fn a_missing_target_is_one_line_next_to_the_ones_that_resolved() {
        let runner = ScriptedRunner::new().reply(
            &format!("{CALL} gone ens-wine-1"),
            1,
            &container(),
            "Error response from daemon: No such object: gone\n",
            false,
        );
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["gone".to_owned(), "ens-wine-1".to_owned()],
                fields: vec!["State.Status".to_owned()],
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "\
#1 gone — No such object: gone
#2 ens-wine-1
State.Status: running
[2 targets, 1 field]
"
        );
    }

    #[test]
    fn a_type_is_passed_through_so_a_name_is_not_resolved_as_the_wrong_thing() {
        let runner = ScriptedRunner::new().with(
            "docker inspect --type image --format {{json .}} ens",
            &(serde_json::json!({"Id": "sha256:abc", "RepoTags": ["ens:1.4"]}).to_string() + "\n"),
        );
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["ens".to_owned()],
                kind: Kind::Image,
                fields: vec!["RepoTags".to_owned()],
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert_eq!(
            outcome.text,
            "#1 ens\nRepoTags: ens:1.4\n[1 target, 1 field]\n"
        );
    }

    #[test]
    fn a_long_value_is_elided_rather_than_sent_whole() {
        let long = "x".repeat(400);
        let runner = ScriptedRunner::new().with(
            &format!("{CALL} api"),
            &(serde_json::json!({"Name": "/api", "Config": {"Image": long}}).to_string() + "\n"),
        );
        let outcome = inspect(
            &runner,
            &InspectRequest {
                targets: vec!["api".to_owned()],
                fields: vec!["Config.Image".to_owned()],
                max_value: Some(20),
                ..InspectRequest::default()
            },
        )
        .expect("inspect");
        assert!(
            outcome.text.lines().nth(1).unwrap().len() < 50,
            "{}",
            outcome.text
        );
    }

    #[test]
    fn nothing_to_inspect_is_a_request_error() {
        let err = inspect(&ScriptedRunner::new(), &InspectRequest::default()).expect_err("refused");
        assert!(err.to_string().contains("name at least one"), "{err}");
    }

    #[test]
    fn a_preset_name_is_recognised_or_it_is_not() {
        assert_eq!(Preset::parse("health"), Some(Preset::Health));
        assert_eq!(Preset::parse("networks"), Some(Preset::Network));
        assert_eq!(Preset::parse("vibes"), None);
    }
}
