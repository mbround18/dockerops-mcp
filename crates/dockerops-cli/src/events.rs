//! A bounded window of the daemon's event stream.
//!
//! `docker events` is a stream with no end: run it without `--until` and it waits for the
//! next container to be created, forever. That makes it unusable from a request/response
//! protocol and awkward even in a terminal, which is why the usual substitute is guessing
//! from `docker ps` and the logs. A window with two ends is both answerable and the thing
//! people actually want: *what happened to this container in the last ten minutes* — the
//! restarts, the OOM kills, the health transitions, in order.

use serde::Serialize;

use crate::{
    Budget, Result,
    invoke::{self, Invocation, safe_value},
    runner::CommandRunner,
    text::{elide, pad, plural, width_of},
};

/// Default window. Long enough to hold the restart you are asking about, short enough
/// that a busy daemon does not fill the budget with image pulls.
pub const DEFAULT_SINCE: &str = "15m";
pub const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct EventsRequest {
    pub invocation: Invocation,
    /// Containers, images, networks or volumes to narrow to. Matched by name or id.
    pub targets: Vec<String>,
    /// Object types to keep: `container`, `image`, `network`, `volume`, `daemon`.
    pub types: Vec<String>,
    /// Actions to keep: `die`, `oom`, `health_status`, `restart`, `kill`.
    pub actions: Vec<String>,
    /// Docker filters, as `key=value`, for anything the fields above do not cover.
    pub filter: Vec<String>,
    /// Start of the window. Relative (`15m`, `2h`) or a timestamp. Defaults to `15m`.
    pub since: Option<String>,
    /// End of the window. Relative or a timestamp; `0s` means now, which is the default.
    ///
    /// There is no way to leave this open. An open-ended `docker events` never returns,
    /// and a tool call that never returns takes the conversation with it.
    pub until: Option<String>,
    /// Fold a run of identical events into one line and a count.
    pub collapse: bool,
    pub limit: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Event {
    /// `container`, `image`, `network`, `volume`, `daemon`.
    pub kind: String,
    /// `die`, `start`, `health_status: unhealthy`, `pull`.
    pub action: String,
    /// The name if the daemon reported one, else the short id.
    pub subject: String,
    /// Unix seconds, as the daemon reported them.
    pub time: i64,
    /// The attributes worth printing: an exit code, an image, a signal.
    pub detail: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EventsOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub events: Vec<Event>,
    pub command: String,
    pub window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn events(runner: &dyn CommandRunner, request: &EventsRequest) -> Result<EventsOutcome> {
    let since = request
        .since
        .clone()
        .unwrap_or_else(|| DEFAULT_SINCE.to_owned());
    // `0s` is "now". Not optional, and not configurable to "never": see `until` above.
    let until = request.until.clone().unwrap_or_else(|| "0s".to_owned());
    safe_value(&since, "since")?;
    safe_value(&until, "until")?;

    let mut rest = vec![
        "events".to_owned(),
        "--since".to_owned(),
        since.clone(),
        "--until".to_owned(),
        until.clone(),
        "--format".to_owned(),
        "{{json .}}".to_owned(),
    ];
    for (key, values) in [
        ("container", &request.targets),
        ("type", &request.types),
        ("event", &request.actions),
    ] {
        for value in values {
            safe_value(value, key)?;
            rest.push("--filter".to_owned());
            rest.push(format!("{key}={value}"));
        }
    }
    for filter in &request.filter {
        invoke::check_filter(filter)?;
        rest.push("--filter".to_owned());
        rest.push(filter.clone());
    }

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));
    let window = format!("{since}..{until}");

    if !output.ok() && output.stdout.trim().is_empty() {
        return Ok(EventsOutcome {
            text: format!("[no events: {}]\n", output.failure()),
            events: Vec::new(),
            command,
            window,
            truncated: None,
        });
    }

    let mut events: Vec<Event> = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(event)
        .collect();

    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    // The window's far end is the interesting one: if more happened than fits, the recent
    // events are the ones worth keeping, so the oldest are dropped.
    let over = events.len().saturating_sub(limit);
    if over > 0 {
        events.drain(..over);
    }

    let rows = rows(&events, request.collapse);
    let width = width_of(rows.iter().map(|(label, _)| label.as_str()));

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut shown = 0;
    for (label, detail) in &rows {
        let line = format!("{}  {detail}\n", pad(label, width));
        if !budget.try_spend(&line) {
            break;
        }
        body.push_str(&line);
        shown += 1;
    }

    let cut = rows.len() - shown + over;
    let truncated = (cut > 0).then(|| {
        if over > 0 && shown == rows.len() {
            format!("{cut} older, over the limit of {limit}")
        } else {
            format!("{cut} not shown, byte budget reached")
        }
    });

    let mut footer = format!("[{} in {window}", plural(events.len(), "event"));
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(EventsOutcome {
        text: format!("{body}{footer}"),
        events,
        command,
        window,
        truncated,
    })
}

fn event(row: serde_json::Value) -> Event {
    let kind = string(&row, "Type");
    // The daemon writes the action in `Action`, and older versions in `status`; and for a
    // health transition it is `health_status: healthy`, which is two facts in one field.
    let action = {
        let action = string(&row, "Action");
        if action.is_empty() {
            string(&row, "status")
        } else {
            action
        }
    };
    let id = string(&row, "id");
    let attributes = row
        .get("Actor")
        .and_then(|actor| actor.get("Attributes"))
        .and_then(serde_json::Value::as_object);
    let name = attributes
        .and_then(|map| map.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // Compose writes four labels onto every container it starts; printing them would cost
    // more than the event. These are the attributes that explain one.
    let detail = attributes
        .map(|map| {
            ["exitCode", "signal", "image", "exitcode", "health_status"]
                .iter()
                .filter_map(|key| {
                    map.get(*key)
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(|value| match *key {
                            "exitCode" | "exitcode" => format!("exit {value}"),
                            "signal" => format!("signal {value}"),
                            other => format!("{other} {value}"),
                        })
                })
                .collect()
        })
        .unwrap_or_default();

    Event {
        kind,
        action,
        subject: if name.is_empty() {
            id.chars().take(12).collect()
        } else {
            name
        },
        time: row
            .get("time")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        detail,
    }
}

/// `(left column, right column)` per line, with runs folded when asked.
///
/// A crash loop is the reason `collapse` exists: twenty `die`/`start` pairs are one fact
/// and a number, and the number is the interesting half.
fn rows(events: &[Event], collapse: bool) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut repeats: Vec<usize> = Vec::new();
    for event in events {
        let left = format!("{} {}", clock(event.time), event.kind);
        let mut right = format!("{} {}", event.action, event.subject);
        if !event.detail.is_empty() {
            right.push_str(&format!(" ({})", event.detail.join(", ")));
        }
        let right = elide(&right, 110);

        // Only the right column is compared: two events differ by their timestamp by
        // definition, and collapsing on the timestamp would collapse nothing.
        if collapse && rows.last().map(|(_, last)| last == &right).unwrap_or(false) {
            *repeats.last_mut().expect("a row has a count") += 1;
            continue;
        }
        rows.push((left, right));
        repeats.push(1);
    }
    if collapse {
        for (row, count) in rows.iter_mut().zip(repeats) {
            if count > 1 {
                row.1.push_str(&format!(" \u{d7}{count}"));
            }
        }
    }
    rows
}

/// `HH:MM:SS` in UTC from unix seconds, without a calendar crate.
///
/// The date is deliberately left out: an event stream a caller asked for in minutes is
/// read by time of day, and the window is already in the footer.
fn clock(time: i64) -> String {
    let seconds = time.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn string(row: &serde_json::Value, key: &str) -> String {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const CALL: &str = "docker events --since 15m --until 0s --format {{json .}}";

    fn row(kind: &str, action: &str, name: &str, time: i64, extra: serde_json::Value) -> String {
        let mut attributes = serde_json::json!({"name": name});
        if let (Some(target), Some(source)) = (attributes.as_object_mut(), extra.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        serde_json::json!({
            "Type": kind,
            "Action": action,
            "id": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "time": time,
            "Actor": {"ID": "9f86d081884c", "Attributes": attributes},
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn a_window_of_the_stream_is_one_line_per_event() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}{}",
                row(
                    "container",
                    "die",
                    "ens-wine-1",
                    1_700_000_000,
                    serde_json::json!({"exitCode": "137"})
                ),
                row(
                    "container",
                    "start",
                    "ens-wine-1",
                    1_700_000_004,
                    serde_json::json!({})
                ),
                row(
                    "network",
                    "connect",
                    "ens_default",
                    1_700_000_004,
                    serde_json::json!({})
                ),
            ),
        );
        let outcome = events(&runner, &EventsRequest::default()).expect("events");

        assert_eq!(
            outcome.text,
            "\
22:13:20 container  die ens-wine-1 (exit 137)
22:13:24 container  start ens-wine-1
22:13:24 network    connect ens_default
[3 events in 15m..0s]
"
        );
    }

    #[test]
    fn the_window_always_has_a_far_end_so_the_call_returns() {
        let runner = ScriptedRunner::new().with(CALL, "");
        events(&runner, &EventsRequest::default()).expect("events");
        assert!(
            runner.calls()[0].contains("--until 0s"),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_crash_loop_is_one_line_and_a_count() {
        let mut stdout = String::new();
        for n in 0..6 {
            stdout.push_str(&row(
                "container",
                "die",
                "ens-wine-1",
                1_700_000_000 + n * 10,
                serde_json::json!({"exitCode": "137"}),
            ));
        }
        let runner = ScriptedRunner::new().with(CALL, &stdout);
        let outcome = events(
            &runner,
            &EventsRequest {
                collapse: true,
                ..EventsRequest::default()
            },
        )
        .expect("events");
        assert_eq!(
            outcome.text,
            "22:13:20 container  die ens-wine-1 (exit 137) \u{d7}6\n[6 events in 15m..0s]\n"
        );
    }

    #[test]
    fn narrowing_is_expressed_in_docker_s_own_filters() {
        let runner = ScriptedRunner::new().with(
            "docker events --since 2h --until 0s --format {{json .}} \
--filter container=ens-wine-1 --filter type=container --filter event=die",
            "",
        );
        events(
            &runner,
            &EventsRequest {
                targets: vec!["ens-wine-1".to_owned()],
                types: vec!["container".to_owned()],
                actions: vec!["die".to_owned()],
                since: Some("2h".to_owned()),
                ..EventsRequest::default()
            },
        )
        .expect("events");
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn an_unnamed_object_falls_back_to_its_short_id() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &row("image", "pull", "", 1_700_000_000, serde_json::json!({})),
        );
        let outcome = events(&runner, &EventsRequest::default()).expect("events");
        assert_eq!(
            outcome.text,
            "22:13:20 image  pull 9f86d081884c\n[1 event in 15m..0s]\n"
        );
    }

    #[test]
    fn a_window_that_overflows_keeps_the_recent_end() {
        let mut stdout = String::new();
        for n in 0..5 {
            stdout.push_str(&row(
                "container",
                "start",
                &format!("c{n}"),
                1_700_000_000 + n,
                serde_json::json!({}),
            ));
        }
        let runner = ScriptedRunner::new().with(CALL, &stdout);
        let outcome = events(
            &runner,
            &EventsRequest {
                limit: Some(2),
                ..EventsRequest::default()
            },
        )
        .expect("events");
        assert!(outcome.text.contains("start c3"), "{}", outcome.text);
        assert!(outcome.text.contains("start c4"), "{}", outcome.text);
        assert!(!outcome.text.contains("start c0"), "{}", outcome.text);
        assert!(
            outcome
                .text
                .ends_with("[2 events in 15m..0s, 3 older, over the limit of 2]\n"),
            "{}",
            outcome.text
        );
    }

    #[test]
    fn a_daemon_that_is_not_there_is_a_line_rather_than_an_error() {
        let runner =
            ScriptedRunner::new().failing(CALL, 1, "Cannot connect to the Docker daemon\n");
        let outcome = events(&runner, &EventsRequest::default()).expect("events");
        assert_eq!(
            outcome.text,
            "[no events: Cannot connect to the Docker daemon]\n"
        );
    }
}
