//! A resource snapshot that terminates, unlike `docker stats`.
//!
//! `docker stats` is a full-screen live display: it clears the terminal, repaints twice a
//! second, and never returns. `--no-stream` fixes the returning, and leaves seven padded
//! columns of which two are usually the question — what is eating the CPU, and what is
//! about to be killed for memory. So this sorts by CPU, renders memory as a percentage of
//! its limit next to the figure, and keeps the rest only when it is non-zero.

use serde::Serialize;

use crate::{
    Budget, Result,
    invoke::{self, Invocation, safe_value},
    runner::CommandRunner,
    text::{elide, pad, parse_size, plural, width_of},
};

/// What to sort by. Descending in every case: the interesting row is the big one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Cpu,
    Memory,
    Name,
}

#[derive(Debug, Clone, Default)]
pub struct StatsRequest {
    pub invocation: Invocation,
    /// Containers to measure. Empty means every running container.
    pub targets: Vec<String>,
    pub sort: Sort,
    /// Cap on rows. Defaults to 40 — a machine with more containers than that is being
    /// asked the wrong question.
    pub limit: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ContainerStats {
    pub name: String,
    /// CPU as Docker reports it, percent of one core times the cores it may use.
    pub cpu_percent: f64,
    pub memory_usage: String,
    pub memory_percent: f64,
    pub network: String,
    pub block_io: String,
    pub pids: String,
}

#[derive(Debug, Serialize)]
pub struct StatsOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub containers: Vec<ContainerStats>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

const DEFAULT_LIMIT: usize = 40;

pub fn stats(runner: &dyn CommandRunner, request: &StatsRequest) -> Result<StatsOutcome> {
    for target in &request.targets {
        safe_value(target, "target")?;
    }

    let mut rest = vec!["stats".to_owned()];
    // Not negotiable: a streaming call never returns, and this is a request/response
    // protocol. `docker stats` without this flag is how you lose a conversation.
    rest.push("--no-stream".to_owned());
    rest.push("--format".to_owned());
    rest.push("{{json .}}".to_owned());
    rest.extend(request.targets.iter().cloned());

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    if !output.ok() && output.stdout.trim().is_empty() {
        let reason = output.failure();
        return Ok(StatsOutcome {
            text: format!("[no stats: {reason}]\n"),
            containers: Vec::new(),
            command,
            truncated: None,
        });
    }

    let mut containers: Vec<ContainerStats> = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|row| ContainerStats {
            name: string(&row, "Name"),
            cpu_percent: percent(&string(&row, "CPUPerc")),
            memory_usage: string(&row, "MemUsage"),
            memory_percent: percent(&string(&row, "MemPerc")),
            network: string(&row, "NetIO"),
            block_io: string(&row, "BlockIO"),
            pids: string(&row, "PIDs"),
        })
        .collect();

    match request.sort {
        Sort::Cpu => containers.sort_by(|a, b| {
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then_with(|| a.name.cmp(&b.name))
        }),
        Sort::Memory => containers.sort_by(|a, b| {
            memory_bytes(b)
                .cmp(&memory_bytes(a))
                .then_with(|| a.name.cmp(&b.name))
        }),
        Sort::Name => containers.sort_by(|a, b| a.name.cmp(&b.name)),
    }

    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let over = containers.len().saturating_sub(limit);
    containers.truncate(limit);

    let rows: Vec<[String; 4]> = containers
        .iter()
        .map(|container| {
            [
                elide(&container.name, 40),
                format!("{:.1}%", container.cpu_percent),
                memory(container),
                io(container),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..3)
        .map(|column| width_of(rows.iter().map(|row| row[column].as_str())))
        .collect();

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut shown = 0;
    for row in &rows {
        let line = format!(
            "{}  {}  {}  {}\n",
            pad(&row[0], widths[0]),
            pad(&row[1], widths[1]),
            pad(&row[2], widths[2]),
            row[3]
        );
        if !budget.try_spend(&line) {
            break;
        }
        body.push_str(&line);
        shown += 1;
    }
    containers.truncate(shown);

    let cut = rows.len() - shown + over;
    let truncated = (cut > 0).then(|| {
        if over > 0 && shown == rows.len() {
            format!("{cut} more, over the limit of {limit}")
        } else {
            format!("{cut} not shown, byte budget reached")
        }
    });

    let total_cpu: f64 = containers.iter().map(|c| c.cpu_percent).sum();
    let mut footer = format!(
        "[{}, {total_cpu:.1}% cpu",
        plural(containers.len(), "container")
    );
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(StatsOutcome {
        text: format!("{body}{footer}"),
        containers,
        command,
        truncated,
    })
}

fn string(row: &serde_json::Value, key: &str) -> String {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// `12.34%` → `12.34`. Docker reports `--` for a container it could not measure.
fn percent(value: &str) -> f64 {
    value.trim_end_matches('%').parse().unwrap_or(0.0)
}

/// `1.23GiB / 8GiB` → the used side, in bytes, for sorting.
fn memory_bytes(container: &ContainerStats) -> u64 {
    container
        .memory_usage
        .split('/')
        .next()
        .and_then(|used| parse_size(used.trim()))
        .unwrap_or(0)
}

/// `1.23GiB / 8GiB` and `15.4%` say the same thing twice; the percentage is the one that
/// answers "is this about to be killed", so the limit is dropped and the share kept.
fn memory(container: &ContainerStats) -> String {
    let used = container
        .memory_usage
        .split('/')
        .next()
        .unwrap_or_default()
        .trim();
    if container.memory_percent > 0.0 {
        format!("{used} ({:.0}%)", container.memory_percent)
    } else {
        used.to_owned()
    }
}

/// Network and disk, kept only when something has actually moved.
fn io(container: &ContainerStats) -> String {
    let mut parts = Vec::new();
    for (label, value) in [("net", &container.network), ("blk", &container.block_io)] {
        let value = value.trim();
        if !value.is_empty() && !is_zero(value) {
            parts.push(format!("{label} {value}"));
        }
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join("  ")
    }
}

fn is_zero(value: &str) -> bool {
    value
        .split('/')
        .all(|side| parse_size(side.trim()).unwrap_or(1) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const CALL: &str = "docker stats --no-stream --format {{json .}}";

    fn row(name: &str, cpu: &str, mem: &str, pct: &str, net: &str, blk: &str) -> String {
        serde_json::json!({
            "Name": name, "CPUPerc": cpu, "MemUsage": mem, "MemPerc": pct,
            "NetIO": net, "BlockIO": blk, "PIDs": "12",
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn a_snapshot_is_one_line_per_container_worst_first() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}",
                row(
                    "api",
                    "4.10%",
                    "120MiB / 8GiB",
                    "1.46%",
                    "1.2MB / 800kB",
                    "0B / 0B"
                ),
                row(
                    "wine",
                    "180.20%",
                    "6.1GiB / 8GiB",
                    "76.25%",
                    "0B / 0B",
                    "0B / 0B"
                ),
            ),
        );
        let outcome = stats(&runner, &StatsRequest::default()).expect("stats");

        assert_eq!(
            outcome.text,
            "\
wine  180.2%  6.1GiB (76%)  -
api   4.1%    120MiB (1%)   net 1.2MB / 800kB
[2 containers, 184.3% cpu]
"
        );
    }

    #[test]
    fn it_never_streams_whatever_else_it_is_asked() {
        let runner = ScriptedRunner::new().with(CALL, "");
        stats(&runner, &StatsRequest::default()).expect("stats");
        assert!(
            runner.calls()[0].contains("--no-stream"),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn memory_can_be_the_question_instead_of_cpu() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}",
                row(
                    "busy",
                    "90.00%",
                    "100MiB / 8GiB",
                    "1.22%",
                    "0B / 0B",
                    "0B / 0B"
                ),
                row(
                    "fat",
                    "1.00%",
                    "7GiB / 8GiB",
                    "87.50%",
                    "0B / 0B",
                    "0B / 0B"
                ),
            ),
        );
        let outcome = stats(
            &runner,
            &StatsRequest {
                sort: Sort::Memory,
                ..StatsRequest::default()
            },
        )
        .expect("stats");
        assert!(outcome.text.starts_with("fat "), "{}", outcome.text);
    }

    #[test]
    fn a_daemon_that_refuses_is_a_line_rather_than_an_error() {
        let runner = ScriptedRunner::new().failing(
            CALL,
            1,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock\n",
        );
        let outcome = stats(&runner, &StatsRequest::default()).expect("stats");
        assert_eq!(
            outcome.text,
            "[no stats: Cannot connect to the Docker daemon at unix:///var/run/docker.sock]\n"
        );
    }

    #[test]
    fn an_unmeasurable_container_does_not_poison_the_sort() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}",
                row("stopped", "--", "-- / --", "--", "-- / --", "-- / --"),
                row(
                    "api",
                    "2.00%",
                    "50MiB / 8GiB",
                    "0.61%",
                    "0B / 0B",
                    "0B / 0B"
                ),
            ),
        );
        let outcome = stats(&runner, &StatsRequest::default()).expect("stats");
        assert!(outcome.text.starts_with("api "), "{}", outcome.text);
        assert_eq!(outcome.containers.len(), 2);
    }

    #[test]
    fn a_limit_says_how_many_it_left_out() {
        let rows: String = (0..5)
            .map(|n| {
                row(
                    &format!("c{n}"),
                    "1.00%",
                    "1MiB / 8GiB",
                    "0.01%",
                    "0B / 0B",
                    "0B / 0B",
                )
            })
            .collect();
        let runner = ScriptedRunner::new().with(CALL, &rows);
        let outcome = stats(
            &runner,
            &StatsRequest {
                limit: Some(2),
                ..StatsRequest::default()
            },
        )
        .expect("stats");
        assert!(
            outcome
                .text
                .ends_with("[2 containers, 2.0% cpu, 3 more, over the limit of 2]\n"),
            "{}",
            outcome.text
        );
    }
}
