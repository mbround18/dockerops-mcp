//! One container, however it was described.
//!
//! `docker ps` and `docker compose ps` answer the same question in two different shapes,
//! so both are parsed into [`Container`] and every tool renders that. Three pieces of
//! Docker's presentation are undone here, because each one is pure cost in a context
//! window:
//!
//! * **Status is English.** `Up About an hour (healthy)` is twenty-four characters
//!   carrying `1h` and `healthy`. [`Status::parse`] turns the sentence back into fields.
//! * **Ports are listed twice.** A published port on a dual-stack host appears as
//!   `0.0.0.0:8080->80/tcp, :::8080->80/tcp` — the same mapping, said twice, in 38
//!   characters. [`compress_ports`] renders it `8080→80`.
//! * **Image references are long.** `ghcr.io/acme/api@sha256:9f86d0…` is a line of its
//!   own. The digest is kept to twelve characters, which is still unique enough to
//!   compare against another line of the same output.

use serde::Serialize;

use crate::text::elide;

/// The lifecycle state of a container, as both CLIs name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Created,
    Running,
    Restarting,
    Paused,
    Exited,
    Dead,
    Removing,
    Unknown,
}

impl State {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "created" => Self::Created,
            "running" | "up" => Self::Running,
            "restarting" => Self::Restarting,
            "paused" => Self::Paused,
            "exited" | "stopped" => Self::Exited,
            "dead" => Self::Dead,
            "removing" | "removal in progress" => Self::Removing,
            _ => Self::Unknown,
        }
    }

    /// The word used in rendered output. `up` rather than `running`, because it is the
    /// word in `docker ps` and two characters shorter.
    pub fn word(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "up",
            Self::Restarting => "restarting",
            Self::Paused => "paused",
            Self::Exited => "exited",
            Self::Dead => "dead",
            Self::Removing => "removing",
            Self::Unknown => "?",
        }
    }

    /// Whether this state is one a caller is usually looking *for* when something is
    /// wrong. Used to order output so the interesting lines are not at the bottom.
    pub fn is_healthy_looking(self) -> bool {
        matches!(self, Self::Running | Self::Created)
    }
}

/// What a `Status` string was carrying before it became a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub state: Option<State>,
    pub health: Option<String>,
    pub age: Option<String>,
    pub exit_code: Option<i64>,
}

impl Status {
    /// Parse `Up 3 hours (healthy)`, `Exited (137) 5 minutes ago`, `Restarting (1) 4
    /// seconds ago`, `Created`, `Up 10 seconds (health: starting)`.
    pub fn parse(status: &str) -> Self {
        let status = status.trim();
        if status.is_empty() {
            return Self::default();
        }
        let lower = status.to_ascii_lowercase();

        let state = if lower.starts_with("up") {
            Some(State::Running)
        } else if lower.starts_with("exited") {
            Some(State::Exited)
        } else if lower.starts_with("created") {
            Some(State::Created)
        } else if lower.starts_with("restarting") {
            Some(State::Restarting)
        } else if lower.starts_with("paused") {
            Some(State::Paused)
        } else if lower.starts_with("dead") {
            Some(State::Dead)
        } else if lower.starts_with("removal") {
            Some(State::Removing)
        } else {
            None
        };

        // Everything in parentheses is either a health word or an exit code.
        let mut health = None;
        let mut exit_code = None;
        if let (Some(open), Some(close)) = (status.find('('), status.rfind(')'))
            && open < close
        {
            let inner = status[open + 1..close].trim();
            if let Ok(code) = inner.parse::<i64>() {
                exit_code = Some(code);
            } else {
                let word = inner
                    .strip_prefix("health:")
                    .unwrap_or(inner)
                    .trim()
                    .to_ascii_lowercase();
                if !word.is_empty() {
                    health = Some(word);
                }
            }
        }

        // What is left, once the state word and any parenthesis are gone, is the age.
        let mut rest = status.to_owned();
        if let (Some(open), Some(close)) = (rest.find('('), rest.rfind(')'))
            && open < close
        {
            rest.replace_range(open..=close, "");
        }
        let words: Vec<&str> = rest.split_whitespace().collect();
        let age = (words.len() > 1)
            .then(|| compact_age(&words[1..].join(" ")))
            .flatten();

        Self {
            state,
            health,
            age,
            exit_code,
        }
    }

    /// `up 3h`, `up 3h unhealthy`, `exited (137) 5m`, `created`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(state) = self.state {
            out.push_str(state.word());
        } else {
            out.push('?');
        }
        if let Some(code) = self.exit_code
            && self.state != Some(State::Running)
        {
            out.push_str(&format!(" ({code})"));
        }
        if let Some(age) = &self.age {
            out.push(' ');
            out.push_str(age);
        }
        // `healthy` is the expected case and says nothing; the other words are the news.
        if let Some(health) = &self.health
            && health != "healthy"
        {
            out.push(' ');
            out.push_str(health);
        }
        out
    }
}

/// Docker's English durations, back to something two characters long.
///
/// `About an hour ago` is the CLI being kind to a person reading one line. A caller
/// reading forty lines wants `1h`.
pub fn compact_age(phrase: &str) -> Option<String> {
    let phrase = phrase
        .trim()
        .trim_end_matches("ago")
        .trim()
        .to_ascii_lowercase();
    if phrase.is_empty() {
        return None;
    }
    if phrase.starts_with("less than") {
        return Some("0s".to_owned());
    }
    let words: Vec<&str> = phrase.split_whitespace().collect();
    // `About an hour` / `About a minute` carry no number; the article is the number.
    let (count, unit) = match words.as_slice() {
        [first, article, unit] if *first == "about" && matches!(*article, "a" | "an") => {
            (1u64, *unit)
        }
        [article, unit] if matches!(*article, "a" | "an") => (1, *unit),
        ["about", count, unit] => (count.parse().ok()?, *unit),
        [count, unit] => (count.parse().ok()?, *unit),
        _ => return None,
    };
    let unit = unit.trim_end_matches('s');
    let (scale, suffix) = match unit {
        "second" => (1, "s"),
        "minute" => (1, "m"),
        "hour" => (1, "h"),
        "day" => (1, "d"),
        "week" => (7, "d"),
        "month" => (30, "d"),
        "year" => (365, "d"),
        _ => return None,
    };
    Some(format!("{}{suffix}", count * scale))
}

/// A published or exposed port, deduplicated across address families.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Port {
    /// Host port, absent when the port is exposed but not published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<u16>,
    pub target: u16,
    pub protocol: String,
    /// Host address, kept only when it is not a wildcard — `127.0.0.1` is the news.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

impl Port {
    /// `8080→80`, `5432`, `2456/udp`, `127.0.0.1:8080→80`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(address) = &self.address {
            out.push_str(address);
            out.push(':');
        }
        match self.published {
            Some(published) if published as u32 == self.target as u32 => {
                out.push_str(&published.to_string());
            }
            Some(published) => {
                out.push_str(&format!("{published}→{}", self.target));
            }
            None => out.push_str(&self.target.to_string()),
        }
        if self.protocol != "tcp" {
            out.push('/');
            out.push_str(&self.protocol);
        }
        out
    }

    fn wildcard(address: &str) -> bool {
        matches!(address.trim(), "" | "0.0.0.0" | "::" | "[::]" | "*")
    }
}

/// Parse and deduplicate the `Ports` column of `docker ps`.
///
/// `0.0.0.0:8080->80/tcp, :::8080->80/tcp, 2456/udp` becomes `8080→80, 2456/udp`: the
/// dual-stack pair is one mapping said twice, and saying it once is most of this
/// function's reason for existing.
pub fn compress_ports(column: &str) -> Vec<Port> {
    let mut ports: Vec<Port> = Vec::new();
    for entry in column.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (host, container) = match entry.split_once("->") {
            Some((host, container)) => (Some(host.trim()), container.trim()),
            None => (None, entry),
        };
        let (target, protocol) = match container.split_once('/') {
            Some((port, protocol)) => (port.trim(), protocol.trim()),
            None => (container, "tcp"),
        };
        let Ok(target) = target.parse::<u16>() else {
            continue;
        };

        let (address, published) = match host {
            None => (None, None),
            Some(host) => {
                // `1.2.3.4:8080`, `[::]:8080`, or a bare `8080`.
                let (address, port) = match host.rsplit_once(':') {
                    Some((address, port)) => (address, port),
                    None => ("", host),
                };
                let Ok(port) = port.parse::<u16>() else {
                    continue;
                };
                let address = (!Port::wildcard(address)).then(|| address.to_owned());
                (address, Some(port))
            }
        };

        let port = Port {
            published,
            target,
            protocol: protocol.to_ascii_lowercase(),
            address,
        };
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

/// `8080→80, 5432, 2456/udp`, or `-` when nothing is bound.
pub fn render_ports(ports: &[Port]) -> String {
    if ports.is_empty() {
        return "-".to_owned();
    }
    ports
        .iter()
        .map(Port::render)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A container, normalised out of whichever CLI described it.
#[derive(Debug, Clone, Serialize)]
pub struct Container {
    pub name: String,
    pub id: String,
    pub image: String,
    pub state: State,
    /// The rendered status — `up 3h unhealthy` — which is what a caller reads.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub ports: Vec<Port>,
    /// Compose service name, from the project labels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// Length of image digest kept: enough to tell two builds apart, not a whole line.
const DIGEST_KEPT: usize = 12;

/// Shorten an image reference without losing what identifies it.
pub fn short_image(image: &str) -> String {
    let image = image.trim();
    if let Some((repo, digest)) = image.split_once("@sha256:") {
        let kept: String = digest.chars().take(DIGEST_KEPT).collect();
        return format!("{repo}@{kept}");
    }
    elide(image, 48)
}

impl Container {
    /// Parse one line of `docker ps --format '{{json .}}'`.
    pub fn from_docker_ps(value: &serde_json::Value) -> Option<Self> {
        let get = |key: &str| -> Option<&str> { value.get(key)?.as_str() };
        let name = get("Names").unwrap_or_default().trim();
        // A container can have several names; the first is the one everything addresses.
        let name = name.split(',').next().unwrap_or(name).trim();
        if name.is_empty() {
            return None;
        }
        let status_text = get("Status").unwrap_or_default();
        let parsed = Status::parse(status_text);
        let state = get("State")
            .map(State::parse)
            .or(parsed.state)
            .unwrap_or(State::Unknown);
        let labels = get("Labels").unwrap_or_default();

        Some(Self {
            name: name.to_owned(),
            id: get("ID").unwrap_or_default().chars().take(12).collect(),
            image: short_image(get("Image").unwrap_or_default()),
            state,
            status: Status {
                state: Some(state),
                ..parsed.clone()
            }
            .render(),
            health: parsed.health,
            age: parsed.age,
            exit_code: parsed.exit_code,
            ports: compress_ports(get("Ports").unwrap_or_default()),
            service: label(labels, "com.docker.compose.service"),
            project: label(labels, "com.docker.compose.project"),
        })
    }

    /// Parse one entry of `docker compose ps --format json`.
    pub fn from_compose_ps(value: &serde_json::Value) -> Option<Self> {
        let get = |key: &str| -> Option<&str> { value.get(key)?.as_str() };
        let name = get("Name").unwrap_or_default().trim();
        if name.is_empty() {
            return None;
        }
        let state = State::parse(get("State").unwrap_or_default());
        let parsed = Status::parse(get("Status").unwrap_or_default());
        let health = get("Health")
            .map(str::to_ascii_lowercase)
            .filter(|h| !h.is_empty())
            .or(parsed.health);
        let exit_code = value
            .get("ExitCode")
            .and_then(serde_json::Value::as_i64)
            .filter(|code| *code != 0 || state == State::Exited)
            .or(parsed.exit_code);

        Some(Self {
            name: name.to_owned(),
            id: get("ID").unwrap_or_default().chars().take(12).collect(),
            image: short_image(get("Image").unwrap_or_default()),
            state,
            status: Status {
                state: Some(state),
                health: health.clone(),
                age: parsed.age.clone(),
                exit_code,
            }
            .render(),
            health,
            age: parsed.age,
            exit_code,
            ports: publishers(value.get("Publishers")),
            service: get("Service").map(str::to_owned),
            project: get("Project").map(str::to_owned),
        })
    }

    /// How this container is addressed on a command line.
    pub fn target(&self) -> &str {
        &self.name
    }
}

/// Read one label out of the comma-separated `Labels` column.
fn label(labels: &str, key: &str) -> Option<String> {
    labels
        .split(',')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.to_owned())
        .filter(|value| !value.is_empty())
}

/// Compose reports ports as structured `Publishers`, which needs the same deduplication
/// as the text column: one mapping per address family.
fn publishers(value: Option<&serde_json::Value>) -> Vec<Port> {
    let Some(entries) = value.and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut ports: Vec<Port> = Vec::new();
    for entry in entries {
        let target = entry
            .get("TargetPort")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default() as u16;
        if target == 0 {
            continue;
        }
        let published = entry
            .get("PublishedPort")
            .and_then(serde_json::Value::as_u64)
            .filter(|port| *port != 0)
            .map(|port| port as u16);
        let address = entry
            .get("URL")
            .and_then(serde_json::Value::as_str)
            .filter(|url| !Port::wildcard(url))
            .map(str::to_owned);
        let protocol = entry
            .get("Protocol")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("tcp")
            .to_ascii_lowercase();
        let port = Port {
            published,
            target,
            protocol,
            address,
        };
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

/// Parse either shape compose has shipped: a JSON array, or one JSON object per line.
///
/// Compose changed this between versions and both are still in the wild, so neither is
/// "the" format and a caller should never have to care which one it has.
pub fn parse_json_stream(stdout: &str) -> Vec<serde_json::Value> {
    let trimmed = stdout.trim_start();
    if trimmed.starts_with('[')
        && let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(trimmed)
    {
        return entries;
    }
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_english_status_becomes_fields_again() {
        let status = Status::parse("Up About an hour (healthy)");
        assert_eq!(status.state, Some(State::Running));
        assert_eq!(status.health.as_deref(), Some("healthy"));
        assert_eq!(status.age.as_deref(), Some("1h"));
        assert_eq!(status.render(), "up 1h");

        let status = Status::parse("Up 10 seconds (health: starting)");
        assert_eq!(status.health.as_deref(), Some("starting"));
        assert_eq!(status.render(), "up 10s starting");

        let status = Status::parse("Exited (137) 5 minutes ago");
        assert_eq!(status.state, Some(State::Exited));
        assert_eq!(status.exit_code, Some(137));
        assert_eq!(status.render(), "exited (137) 5m");

        assert_eq!(Status::parse("Created").render(), "created");
        assert_eq!(
            Status::parse("Restarting (1) 4 seconds ago").render(),
            "restarting (1) 4s"
        );
    }

    #[test]
    fn healthy_is_the_expected_case_and_says_nothing() {
        // The word earns its bytes only when it is not what everyone assumed.
        assert_eq!(Status::parse("Up 2 hours (healthy)").render(), "up 2h");
        assert_eq!(
            Status::parse("Up 2 hours (unhealthy)").render(),
            "up 2h unhealthy"
        );
    }

    #[test]
    fn every_duration_the_cli_writes_in_words_has_a_short_form() {
        assert_eq!(compact_age("Less than a second ago").as_deref(), Some("0s"));
        assert_eq!(compact_age("About a minute").as_deref(), Some("1m"));
        assert_eq!(compact_age("About an hour").as_deref(), Some("1h"));
        assert_eq!(compact_age("3 hours").as_deref(), Some("3h"));
        assert_eq!(compact_age("2 weeks ago").as_deref(), Some("14d"));
        assert_eq!(compact_age("5 months").as_deref(), Some("150d"));
        assert_eq!(compact_age("gibberish"), None);
    }

    #[test]
    fn a_dual_stack_mapping_is_one_mapping() {
        let ports = compress_ports("0.0.0.0:8080->80/tcp, :::8080->80/tcp, 2456/udp");
        assert_eq!(ports.len(), 2);
        assert_eq!(render_ports(&ports), "8080→80, 2456/udp");
    }

    #[test]
    fn a_port_bound_to_one_address_keeps_the_address() {
        let ports = compress_ports("127.0.0.1:5432->5432/tcp");
        assert_eq!(render_ports(&ports), "127.0.0.1:5432");
        assert_eq!(render_ports(&compress_ports("")), "-");
        assert_eq!(render_ports(&compress_ports("0.0.0.0:80->80/tcp")), "80");
    }

    #[test]
    fn a_digest_is_shortened_but_a_tag_is_left_alone() {
        assert_eq!(short_image("nginx:1.27"), "nginx:1.27");
        assert_eq!(
            short_image("ghcr.io/acme/api@sha256:9f86d081884c7d65"),
            "ghcr.io/acme/api@9f86d081884c"
        );
    }

    #[test]
    fn docker_ps_json_becomes_a_container() {
        let value = serde_json::json!({
            "ID": "3f1a2b3c4d5e6f7a8b9c",
            "Names": "ens-wine-1",
            "Image": "ghcr.io/acme/ens:1.4",
            "State": "running",
            "Status": "Up 3 hours (unhealthy)",
            "Ports": "0.0.0.0:15636->15636/udp, :::15636->15636/udp",
            "Labels": "com.docker.compose.project=ens,com.docker.compose.service=wine",
        });
        let container = Container::from_docker_ps(&value).expect("parsed");
        assert_eq!(container.id, "3f1a2b3c4d5e");
        assert_eq!(container.status, "up 3h unhealthy");
        assert_eq!(container.service.as_deref(), Some("wine"));
        assert_eq!(container.project.as_deref(), Some("ens"));
        assert_eq!(render_ports(&container.ports), "15636/udp");
    }

    #[test]
    fn compose_ps_json_becomes_the_same_container() {
        let value = serde_json::json!({
            "ID": "3f1a2b3c4d5e6f7a8b9c",
            "Name": "ens-wine-1",
            "Project": "ens",
            "Service": "wine",
            "Image": "ghcr.io/acme/ens:1.4",
            "State": "exited",
            "Status": "Exited (137) 5 minutes ago",
            "Health": "",
            "ExitCode": 137,
            "Publishers": [
                {"URL": "0.0.0.0", "TargetPort": 80, "PublishedPort": 8080, "Protocol": "tcp"},
                {"URL": "::", "TargetPort": 80, "PublishedPort": 8080, "Protocol": "tcp"}
            ]
        });
        let container = Container::from_compose_ps(&value).expect("parsed");
        assert_eq!(container.state, State::Exited);
        assert_eq!(container.status, "exited (137) 5m");
        assert_eq!(render_ports(&container.ports), "8080→80");
    }

    #[test]
    fn both_shapes_compose_has_shipped_parse() {
        let array = parse_json_stream("[{\"Name\":\"a\"},{\"Name\":\"b\"}]");
        let lines = parse_json_stream("{\"Name\":\"a\"}\n\n{\"Name\":\"b\"}\n");
        assert_eq!(array.len(), 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(array, lines);
        assert!(parse_json_stream("").is_empty());
        assert!(parse_json_stream("not json").is_empty());
    }
}
