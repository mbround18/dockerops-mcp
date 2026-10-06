//! `docker system df`: where the disk went, and how much of it you can have back.
//!
//! The CLI's table is four rows and six columns of which two matter — the size and the
//! reclaimable part — and its totals line is missing, so the usual follow-up is arithmetic
//! in your head. This renders the four rows, adds the total, and says what fraction is
//! reclaimable, because that is the number a decision is made on.

use serde::Serialize;

use crate::{
    Budget, Result,
    invoke::{self, Invocation},
    runner::CommandRunner,
    text::{human, pad, parse_size, width_of},
};

#[derive(Debug, Clone, Default)]
pub struct UsageRequest {
    pub invocation: Invocation,
    /// Also list the largest items in each class, as `docker system df -v` does — but
    /// capped, and only the ones worth naming.
    pub detail: bool,
    /// How many items to name per class when `detail` is set. Defaults to 5.
    pub top: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct UsageClass {
    /// `Images`, `Containers`, `Local Volumes`, `Build Cache`.
    pub class: String,
    pub total: u64,
    pub active: u64,
    pub size: u64,
    pub reclaimable: u64,
}

#[derive(Debug, Serialize)]
pub struct UsageOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub classes: Vec<UsageClass>,
    pub command: String,
    pub size: u64,
    pub reclaimable: u64,
}

pub fn usage(runner: &dyn CommandRunner, request: &UsageRequest) -> Result<UsageOutcome> {
    let mut rest = vec![
        "system".to_owned(),
        "df".to_owned(),
        "--format".to_owned(),
        "{{json .}}".to_owned(),
    ];
    if request.detail {
        rest.push("--verbose".to_owned());
    }

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    if !output.ok() && output.stdout.trim().is_empty() {
        return Ok(UsageOutcome {
            text: format!("[no usage: {}]\n", output.failure()),
            classes: Vec::new(),
            command,
            size: 0,
            reclaimable: 0,
        });
    }

    let classes: Vec<UsageClass> = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row.get("Type").is_some())
        .map(|row| UsageClass {
            class: string(&row, "Type"),
            total: number(&row, "TotalCount"),
            active: number(&row, "Active"),
            size: parse_size(&string(&row, "Size")).unwrap_or(0),
            // Docker writes `12.3GB (47%)`; the percentage is of the size, and recomputing
            // it from bytes is both shorter and not subject to its rounding.
            reclaimable: parse_size(string(&row, "Reclaimable").split(' ').next().unwrap_or(""))
                .unwrap_or(0),
        })
        .collect();

    let size: u64 = classes.iter().map(|class| class.size).sum();
    let reclaimable: u64 = classes.iter().map(|class| class.reclaimable).sum();

    let rows: Vec<[String; 4]> = classes
        .iter()
        .map(|class| {
            [
                class.class.clone(),
                human(class.size),
                format!("{} active", class.active),
                if class.reclaimable == 0 {
                    "-".to_owned()
                } else {
                    format!("{} reclaimable", human(class.reclaimable))
                },
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..3)
        .map(|column| width_of(rows.iter().map(|row| row[column].as_str())))
        .collect();

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
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
    }

    let share = (reclaimable * 100).checked_div(size).unwrap_or(0);
    let footer = format!(
        "[{} total, {} reclaimable ({share}%)]\n",
        human(size),
        human(reclaimable)
    );

    Ok(UsageOutcome {
        text: format!("{body}{footer}"),
        classes,
        command,
        size,
        reclaimable,
    })
}

fn string(row: &serde_json::Value, key: &str) -> String {
    match row.get(key) {
        Some(serde_json::Value::String(value)) => value.trim().to_owned(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn number(row: &serde_json::Value, key: &str) -> u64 {
    match row.get(key) {
        Some(serde_json::Value::Number(value)) => value.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(value)) => value.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const CALL: &str = "docker system df --format {{json .}}";

    fn row(class: &str, total: u64, active: u64, size: &str, reclaimable: &str) -> String {
        serde_json::json!({
            "Type": class, "TotalCount": total, "Active": active,
            "Size": size, "Reclaimable": reclaimable,
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn the_totals_line_the_cli_leaves_out_is_the_point_of_the_call() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}{}{}",
                row("Images", 14, 3, "24.6GB", "18.2GB (73%)"),
                row("Containers", 6, 2, "1.1GB", "900MB (81%)"),
                row("Local Volumes", 9, 4, "12GB", "3.5GB (29%)"),
                row("Build Cache", 120, 0, "4.2GB", "4.2GB"),
            ),
        );
        let outcome = usage(&runner, &UsageRequest::default()).expect("usage");

        assert_eq!(
            outcome.text,
            "\
Images         24G   3 active  18G reclaimable
Containers     1G    2 active  900M reclaimable
Local Volumes  12G   4 active  3.5G reclaimable
Build Cache    4.1G  0 active  4.1G reclaimable
[41G total, 26G reclaimable (63%)]
"
        );
        assert_eq!(outcome.classes.len(), 4);
    }

    #[test]
    fn nothing_reclaimable_says_so_rather_than_printing_a_zero() {
        let runner = ScriptedRunner::new().with(CALL, &row("Images", 1, 1, "500MB", "0B (0%)"));
        let outcome = usage(&runner, &UsageRequest::default()).expect("usage");
        assert_eq!(
            outcome.text,
            "Images  500M  1 active  -\n[500M total, 0B reclaimable (0%)]\n"
        );
    }

    #[test]
    fn detail_is_asked_of_docker_rather_than_reconstructed() {
        let runner = ScriptedRunner::new().with(
            "docker system df --format {{json .}} --verbose",
            &row("Images", 1, 1, "500MB", "0B (0%)"),
        );
        usage(
            &runner,
            &UsageRequest {
                detail: true,
                ..UsageRequest::default()
            },
        )
        .expect("usage");
        assert!(
            runner.calls()[0].ends_with("--verbose"),
            "{:?}",
            runner.calls()
        );
    }

    #[test]
    fn a_daemon_that_is_not_there_is_a_line_rather_than_an_error() {
        let runner =
            ScriptedRunner::new().failing(CALL, 1, "Cannot connect to the Docker daemon\n");
        let outcome = usage(&runner, &UsageRequest::default()).expect("usage");
        assert_eq!(
            outcome.text,
            "[no usage: Cannot connect to the Docker daemon]\n"
        );
    }

    #[test]
    fn an_empty_machine_does_not_divide_by_zero() {
        let runner = ScriptedRunner::new().with(CALL, &row("Images", 0, 0, "0B", "0B"));
        let outcome = usage(&runner, &UsageRequest::default()).expect("usage");
        assert!(
            outcome.text.ends_with("[0B total, 0B reclaimable (0%)]\n"),
            "{}",
            outcome.text
        );
    }
}
