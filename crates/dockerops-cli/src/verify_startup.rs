//! End-to-end cold-volume startup verification for one or more images.
//!
//! This wraps the repetitive shell flow:
//! create isolated storage, run container, poll readiness, capture logs, clean up.
//! The outcome is one bounded call with per-image pass/fail evidence.

use std::{
    path::Path,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    Budget, Error, Result,
    invoke::{self, Invocation, safe_value},
    runner::CommandRunner,
    text::plural,
};

pub const DEFAULT_INITIAL_DELAY_SECONDS: u64 = 30;
pub const DEFAULT_INTERVAL_SECONDS: u64 = 10;
pub const DEFAULT_MAX_ATTEMPTS: usize = 120;
pub const DEFAULT_LOG_TAIL_LINES: usize = 80;

#[derive(Debug, Clone, Default)]
pub struct VerifyStartupRequest {
    pub invocation: Invocation,
    pub images: Vec<String>,
    /// `KEY=value` entries passed as `docker run --env`.
    pub env: Vec<String>,
    /// `host:container` entries passed as `docker run --publish`.
    pub publish: Vec<String>,
    /// Container path where the cold volume is mounted.
    pub volume_target: String,
    /// Readiness URL (for example `http://127.0.0.1:3000/ready`).
    pub probe_url: String,
    pub initial_delay_seconds: Option<u64>,
    pub interval_seconds: Option<u64>,
    pub max_attempts: Option<usize>,
    pub log_tail_lines: Option<usize>,
    /// Remove test containers and volumes even when verification fails.
    pub cleanup: bool,
    /// Delay between images when testing several in one call.
    pub pause_between_images_seconds: Option<u64>,
    /// Optional deterministic id suffix for names/volumes. Useful in CI/tests.
    pub run_id: Option<String>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyImageResult {
    pub image: String,
    pub container: String,
    pub volume: String,
    pub ready: bool,
    pub attempts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub container_exited: bool,
    pub logs_tail: String,
    pub cleaned_up: bool,
}

#[derive(Debug, Serialize)]
pub struct VerifyStartupOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    pub text: String,
    pub command: String,
    pub results: Vec<VerifyImageResult>,
    pub passed: usize,
    pub failed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

pub fn verify_startup(
    runner: &dyn CommandRunner,
    request: &VerifyStartupRequest,
) -> Result<VerifyStartupOutcome> {
    if request.images.is_empty() {
        return Err(Error::request(
            "name at least one image in `images`; this tool verifies startup per image",
        ));
    }
    for image in &request.images {
        safe_value(image, "image")?;
    }
    for entry in &request.env {
        safe_value(entry, "env entry")?;
        if !entry.contains('=') {
            return Err(Error::request(format!(
                "env entry `{entry}` is not `KEY=value`"
            )));
        }
    }
    for publish in &request.publish {
        safe_value(publish, "publish")?;
    }
    safe_value(&request.volume_target, "volume_target")?;
    safe_value(&request.probe_url, "probe_url")?;

    let initial_delay = request
        .initial_delay_seconds
        .unwrap_or(DEFAULT_INITIAL_DELAY_SECONDS);
    let interval = request.interval_seconds.unwrap_or(DEFAULT_INTERVAL_SECONDS);
    let max_attempts = request.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
    let log_tail = request.log_tail_lines.unwrap_or(DEFAULT_LOG_TAIL_LINES);
    let pause_between = request.pause_between_images_seconds.unwrap_or(5);
    let run_id = request
        .run_id
        .clone()
        .unwrap_or_else(|| unix_seconds().to_string());

    let mut results = Vec::new();
    for (index, image) in request.images.iter().enumerate() {
        let slug = slug(image);
        let container = format!("verify-{slug}-{run_id}");
        let volume = format!("verify-{slug}-{run_id}-vol");
        let mut cleaned_up = false;
        let mut ready = false;
        let mut attempts = 0usize;
        let mut probe_output = None;
        let mut failure = None;
        let mut container_exited = false;

        let created_volume = docker(
            runner,
            &request.invocation,
            vec!["volume", "create", &volume],
        )?;
        if !created_volume.ok() {
            results.push(VerifyImageResult {
                image: image.clone(),
                container,
                volume,
                ready: false,
                attempts,
                probe_output,
                failure: Some(format!(
                    "could not create volume: {}",
                    created_volume.failure()
                )),
                container_exited,
                logs_tail: String::new(),
                cleaned_up,
            });
            continue;
        }

        let mut run_args: Vec<String> = vec![
            "run".to_owned(),
            "--detach".to_owned(),
            "--name".to_owned(),
            container.clone(),
        ];
        for entry in &request.env {
            run_args.push("--env".to_owned());
            run_args.push(entry.clone());
        }
        for publish in &request.publish {
            run_args.push("--publish".to_owned());
            run_args.push(publish.clone());
        }
        run_args.push("--volume".to_owned());
        run_args.push(format!("{}:{}", volume, request.volume_target));
        run_args.push(image.clone());

        let run = docker_owned(runner, &request.invocation, run_args)?;
        if !run.ok() {
            failure = Some(format!("container did not start: {}", run.failure()));
        } else {
            if initial_delay > 0 {
                thread::sleep(Duration::from_secs(initial_delay));
            }
            for attempt in 1..=max_attempts {
                attempts = attempt;
                let probe = probe(
                    runner,
                    request.invocation.dir(),
                    &request.probe_url,
                    request.invocation.timeout(),
                )
                .map_err(|err| {
                    Error::request(format!(
                        "`curl` could not be run for readiness probe: {err}"
                    ))
                })?;
                if probe.ok() {
                    ready = true;
                    probe_output = probe.value();
                    break;
                }

                let ps = docker(
                    runner,
                    &request.invocation,
                    vec![
                        "ps",
                        "--filter",
                        &format!("name={container}"),
                        "--format",
                        "{{.Names}}",
                    ],
                )?;
                let still_running = ps.stdout_lines().any(|line| line == container);
                if !still_running {
                    container_exited = true;
                    failure = Some("container exited before readiness probe succeeded".to_owned());
                    break;
                }
                if interval > 0 {
                    thread::sleep(Duration::from_secs(interval));
                }
            }
            if !ready && failure.is_none() {
                failure = Some(format!(
                    "probe did not succeed after {max_attempts} {}",
                    plural(max_attempts, "attempt")
                ));
            }
        }

        let logs = docker_owned(
            runner,
            &request.invocation,
            vec![
                "logs".to_owned(),
                "--tail".to_owned(),
                log_tail.to_string(),
                container.clone(),
            ],
        )?;
        let logs_tail = if logs.stdout.trim().is_empty() {
            logs.stderr.trim().to_owned()
        } else {
            logs.stdout.trim_end().to_owned()
        };

        if request.cleanup {
            let _ = docker_owned(
                runner,
                &request.invocation,
                vec!["rm".to_owned(), "-f".to_owned(), container.clone()],
            )?;
            let _ = docker_owned(
                runner,
                &request.invocation,
                vec![
                    "volume".to_owned(),
                    "rm".to_owned(),
                    "-f".to_owned(),
                    volume.clone(),
                ],
            )?;
            cleaned_up = true;
        }

        results.push(VerifyImageResult {
            image: image.clone(),
            container,
            volume,
            ready,
            attempts,
            probe_output,
            failure,
            container_exited,
            logs_tail,
            cleaned_up,
        });

        if index + 1 < request.images.len() && pause_between > 0 {
            thread::sleep(Duration::from_secs(pause_between));
        }
    }

    let passed = results.iter().filter(|r| r.ready).count();
    let failed = results.len().saturating_sub(passed);
    let command = format!(
        "{} verify_startup images={}",
        request.invocation.bin(),
        request.images.join(",")
    );

    let mut budget = Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut cut = 0usize;
    for result in &results {
        let status = if result.ready { "READY" } else { "FAILED" };
        let mut block = format!(
            "=== TESTING {} ===\nvolume={} name={}\n{} on {}",
            result.image, result.volume, result.container, status, result.image
        );
        if result.ready {
            block.push_str(&format!(
                " after {} {}",
                result.attempts,
                plural(result.attempts, "attempt")
            ));
            if let Some(probe) = &result.probe_output {
                block.push_str(&format!("\nprobe: {probe}"));
            }
        } else if let Some(failure) = &result.failure {
            block.push_str(&format!("\nreason: {failure}"));
        }
        if !result.logs_tail.is_empty() {
            block.push_str("\n--- logs tail ---\n");
            block.push_str(&result.logs_tail);
        }
        block.push('\n');
        block.push_str(&format!("=== DONE {} ===\n\n", result.image));

        if budget.try_spend(&block) {
            body.push_str(&block);
        } else {
            cut += 1;
        }
    }
    let truncated =
        (cut > 0).then(|| format!("{cut} image results not shown, byte budget reached"));
    body.push_str(&format!(
        "[{}, {}, {}",
        plural(results.len(), "image"),
        plural(passed, "ready"),
        plural(failed, "failed")
    ));
    if let Some(note) = &truncated {
        body.push_str(&format!(", {note}"));
    }
    body.push_str("]\n");

    Ok(VerifyStartupOutcome {
        text: body,
        command,
        results,
        passed,
        failed,
        truncated,
    })
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn slug(image: &str) -> String {
    let tail = image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image);
    let mut out = String::with_capacity(tail.len());
    for ch in tail.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

fn probe(
    runner: &dyn CommandRunner,
    cwd: Option<&Path>,
    url: &str,
    timeout: Duration,
) -> std::io::Result<crate::runner::CommandOutput> {
    runner.run("curl", &["-fsS", url], cwd, timeout)
}

fn docker(
    runner: &dyn CommandRunner,
    invocation: &Invocation,
    rest: Vec<&str>,
) -> Result<crate::runner::CommandOutput> {
    let owned: Vec<String> = rest.into_iter().map(ToOwned::to_owned).collect();
    docker_owned(runner, invocation, owned)
}

fn docker_owned(
    runner: &dyn CommandRunner,
    invocation: &Invocation,
    rest: Vec<String>,
) -> Result<crate::runner::CommandOutput> {
    let args = invoke::command(invocation, rest)?;
    invoke::run(runner, invocation, &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    #[test]
    fn one_image_ready_flow_includes_cleanup() {
        let runner = ScriptedRunner::new()
            .with("docker volume create verify-enshrouded-wine-test-run1-vol", "verify-enshrouded-wine-test-run1-vol\n")
            .with("docker run --detach --name verify-enshrouded-wine-test-run1 --env NAME=Verify --publish 3000:3000 --volume verify-enshrouded-wine-test-run1-vol:/home/steam/enshrouded enshrouded-wine-test", "abc123\n")
            .with("curl -fsS http://127.0.0.1:3000/ready", "{\"ready\":true}\n")
            .with("docker logs --tail 80 verify-enshrouded-wine-test-run1", "boot ok\nready ok\n")
            .with("docker rm -f verify-enshrouded-wine-test-run1", "verify-enshrouded-wine-test-run1\n")
            .with("docker volume rm -f verify-enshrouded-wine-test-run1-vol", "verify-enshrouded-wine-test-run1-vol\n");

        let outcome = verify_startup(
            &runner,
            &VerifyStartupRequest {
                images: vec!["enshrouded-wine-test".to_owned()],
                env: vec!["NAME=Verify".to_owned()],
                publish: vec!["3000:3000".to_owned()],
                volume_target: "/home/steam/enshrouded".to_owned(),
                probe_url: "http://127.0.0.1:3000/ready".to_owned(),
                run_id: Some("run1".to_owned()),
                initial_delay_seconds: Some(0),
                interval_seconds: Some(0),
                max_attempts: Some(1),
                cleanup: true,
                ..VerifyStartupRequest::default()
            },
        )
        .expect("verify");

        assert_eq!(outcome.passed, 1);
        assert_eq!(outcome.failed, 0);
        assert!(outcome.text.contains("READY on enshrouded-wine-test"));
        assert!(outcome.text.contains("boot ok"));
        assert_eq!(runner.calls().len(), 6, "{:?}", runner.calls());
    }

    #[test]
    fn container_exit_before_ready_is_reported() {
        let runner = ScriptedRunner::new()
            .with("docker volume create verify-enshrouded-proton-test-run2-vol", "verify-enshrouded-proton-test-run2-vol\n")
            .with("docker run --detach --name verify-enshrouded-proton-test-run2 --volume verify-enshrouded-proton-test-run2-vol:/data enshrouded-proton-test", "def456\n")
            .failing("curl -fsS http://127.0.0.1:3000/ready", 22, "curl: (22) The requested URL returned error: 503\n")
            .with("docker ps --filter name=verify-enshrouded-proton-test-run2 --format {{.Names}}", "")
            .with("docker logs --tail 80 verify-enshrouded-proton-test-run2", "panic: failed init\n")
            .with("docker rm -f verify-enshrouded-proton-test-run2", "verify-enshrouded-proton-test-run2\n")
            .with("docker volume rm -f verify-enshrouded-proton-test-run2-vol", "verify-enshrouded-proton-test-run2-vol\n");

        let outcome = verify_startup(
            &runner,
            &VerifyStartupRequest {
                images: vec!["enshrouded-proton-test".to_owned()],
                volume_target: "/data".to_owned(),
                probe_url: "http://127.0.0.1:3000/ready".to_owned(),
                run_id: Some("run2".to_owned()),
                initial_delay_seconds: Some(0),
                interval_seconds: Some(0),
                max_attempts: Some(2),
                cleanup: true,
                ..VerifyStartupRequest::default()
            },
        )
        .expect("verify");

        assert_eq!(outcome.passed, 0);
        assert_eq!(outcome.failed, 1);
        assert!(outcome.results[0].container_exited);
        assert!(
            outcome
                .text
                .contains("container exited before readiness probe succeeded"),
            "{}",
            outcome.text
        );
    }
}
