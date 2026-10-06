//! `docker run`, bounded and rendered for agent use.
//!
//! This is for the common "start one container from one image" case that otherwise falls
//! through to passthrough. It keeps the same safety and budget behavior as every other
//! tool and adds one guardrail from practice: foreground runs should be explicitly
//! ephemeral (`--rm`) or detached (`-d`) so they do not leave stopped containers behind
//! by accident.

use serde::Serialize;

use crate::{
    Budget, Error, Result,
    invoke::{self, Invocation, matcher, safe_value},
    runner::CommandRunner,
    text::plural,
};

pub const DEFAULT_MAX_LINES: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    pub invocation: Invocation,
    /// Image reference, e.g. `hello-world`, `postgres:17`, `ghcr.io/acme/api:sha`.
    pub image: String,
    /// Command and arguments after the image.
    pub command: Vec<String>,
    /// `--name`.
    pub name: Option<String>,
    /// `--user`.
    pub user: Option<String>,
    /// `--workdir`.
    pub workdir: Option<String>,
    /// `--env`, as `KEY=value`.
    pub env: Vec<String>,
    /// `--publish`, as Docker accepts it (`8080:80`, `127.0.0.1:5432:5432`).
    pub publish: Vec<String>,
    /// `--volume`, as Docker accepts it (`host:container[:mode]`).
    pub volume: Vec<String>,
    /// `--network`.
    pub network: Option<String>,
    /// `--detach` (`-d`).
    pub detach: bool,
    /// `--rm`.
    pub rm: bool,
    /// Keep only output lines matching this regex.
    pub grep: Option<String>,
    /// Drop output lines matching this regex, applied after `grep`.
    pub exclude: Option<String>,
    pub ignore_case: bool,
    /// Keep the first lines rather than the last.
    pub head: bool,
    /// Cap on rendered lines. Defaults to 200.
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RunOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub image: String,
    pub command: String,
    pub status: i32,
    pub lines_shown: usize,
    pub lines_total: usize,
    pub filtered_out: usize,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn run(runner: &dyn CommandRunner, request: &RunRequest) -> Result<RunOutcome> {
    safe_value(&request.image, "image")?;
    if !request.detach && !request.rm {
        return Err(Error::request(
            "`run` without `detach` or `rm` can hang for long-lived containers and leaves \
             stopped containers behind; set `detach: true` or `rm: true`",
        ));
    }
    let mut rest = vec!["run".to_owned()];
    if request.detach {
        rest.push("--detach".to_owned());
    }
    if request.rm {
        rest.push("--rm".to_owned());
    }
    if let Some(name) = &request.name {
        safe_value(name, "name")?;
        rest.push("--name".to_owned());
        rest.push(name.clone());
    }
    if let Some(user) = &request.user {
        safe_value(user, "user")?;
        rest.push("--user".to_owned());
        rest.push(user.clone());
    }
    if let Some(workdir) = &request.workdir {
        safe_value(workdir, "workdir")?;
        rest.push("--workdir".to_owned());
        rest.push(workdir.clone());
    }
    for entry in &request.env {
        safe_value(entry, "env entry")?;
        if !entry.contains('=') {
            return Err(Error::request(format!(
                "env entry `{entry}` is not `KEY=value`"
            )));
        }
        rest.push("--env".to_owned());
        rest.push(entry.clone());
    }
    for publish in &request.publish {
        safe_value(publish, "publish")?;
        rest.push("--publish".to_owned());
        rest.push(publish.clone());
    }
    for volume in &request.volume {
        safe_value(volume, "volume")?;
        rest.push("--volume".to_owned());
        rest.push(volume.clone());
    }
    if let Some(network) = &request.network {
        safe_value(network, "network")?;
        rest.push("--network".to_owned());
        rest.push(network.clone());
    }
    rest.push(request.image.clone());
    rest.extend(request.command.iter().cloned());

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

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

    let mut footer = format!("[run {}, {}", request.image, plural(total, "line"));
    if filtered_out > 0 {
        footer.push_str(&format!(", {filtered_out} filtered"));
    }
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str(&if output.timed_out {
        format!(
            ", timed out after {}s]\n",
            request.invocation.timeout().as_secs()
        )
    } else {
        format!(", exit {}]\n", output.status)
    });

    let container_id = if request.detach && output.status == 0 {
        output
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(ToOwned::to_owned)
    } else {
        None
    };

    Ok(RunOutcome {
        text: format!("{body}{footer}"),
        image: request.image.clone(),
        command,
        status: output.status,
        lines_shown: shown,
        lines_total: total,
        filtered_out,
        timed_out: output.timed_out,
        container_id,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    #[test]
    fn foreground_runs_must_be_ephemeral_or_detached() {
        let err = run(
            &ScriptedRunner::new(),
            &RunRequest {
                image: "hello-world".to_owned(),
                rm: false,
                detach: false,
                ..RunRequest::default()
            },
        )
        .expect_err("gated");
        assert!(err.to_string().contains("detach"), "{err}");
    }

    #[test]
    fn hello_world_runs_with_rm_and_renders_the_footer() {
        let runner = ScriptedRunner::new().with(
            "docker run --rm hello-world",
            "Hello from Docker!\nThis message shows that your installation appears to be working correctly.\n",
        );
        let outcome = run(
            &runner,
            &RunRequest {
                image: "hello-world".to_owned(),
                rm: true,
                ..RunRequest::default()
            },
        )
        .expect("ran");
        assert_eq!(
            outcome.text,
            "Hello from Docker!\nThis message shows that your installation appears to be working correctly.\n[run hello-world, 2 lines, exit 0]\n"
        );
    }

    #[test]
    fn detached_runs_report_the_container_id() {
        let runner = ScriptedRunner::new().with(
            "docker run --detach --name web nginx:latest",
            "2a6f1d7f0df987654321\n",
        );
        let outcome = run(
            &runner,
            &RunRequest {
                image: "nginx:latest".to_owned(),
                detach: true,
                rm: false,
                name: Some("web".to_owned()),
                ..RunRequest::default()
            },
        )
        .expect("ran");
        assert_eq!(
            outcome.container_id.as_deref(),
            Some("2a6f1d7f0df987654321")
        );
    }
}
