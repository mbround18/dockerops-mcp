//! How a command line gets built, and the rules about what may go into one.
//!
//! Every tool in this crate ends up here. Two jobs:
//!
//! * **Addressing.** Which `docker` to run, which daemon, which compose project and
//!   files. A caller sets it once per call and every argument is assembled the same way.
//! * **Refusing flag injection.** A value a caller supplies — a container name, a service,
//!   a label filter — is passed to `docker` as an argument, not through a shell, so there
//!   is no quoting to get wrong. There is still one way to turn data into instructions: a
//!   value that begins with `-` arrives as a flag. [`safe_value`] is why `{"targets":
//!   ["--privileged"]}` is a request error instead of a surprise.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    Error, Result,
    runner::{CommandOutput, CommandRunner},
};

/// Used when a caller names no binary of its own.
pub const DEFAULT_DOCKER_BIN: &str = "docker";

/// Which docker, which daemon, which directory, and how long it may take.
#[derive(Debug, Clone, Default)]
pub struct Invocation {
    /// Binary to run. Defaults to `docker`; a caller might name `podman`.
    pub docker_bin: Option<String>,
    /// Daemon to talk to, as `docker -H` takes it (`ssh://host`, `tcp://10.0.0.2:2375`).
    pub host: Option<String>,
    /// Directory to run in. For compose this is how a project is found without `-f`.
    pub cwd: Option<PathBuf>,
    /// Deadline in seconds. Reads default to 30; building or pulling needs more.
    pub timeout_seconds: Option<u64>,
}

impl Invocation {
    pub fn bin(&self) -> &str {
        self.docker_bin.as_deref().unwrap_or(DEFAULT_DOCKER_BIN)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(crate::runner::DEFAULT_TIMEOUT)
    }

    pub fn dir(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Global flags that precede the subcommand.
    pub fn global_args(&self) -> Result<Vec<String>> {
        let mut args = Vec::new();
        if let Some(host) = &self.host {
            safe_value(host, "host")?;
            args.push("-H".to_owned());
            args.push(host.clone());
        }
        Ok(args)
    }
}

/// Which compose project a call is about: the `docker compose …` prefix.
#[derive(Debug, Clone, Default)]
pub struct ComposeScope {
    /// Compose files, in order, as `-f` takes them.
    pub files: Vec<String>,
    /// Project name, as `-p` takes it. Defaults to the directory name, like the CLI.
    pub project: Option<String>,
    /// Profiles to enable, as `--profile` takes them.
    pub profiles: Vec<String>,
    /// Env file, as `--env-file` takes it.
    pub env_file: Option<String>,
}

impl ComposeScope {
    /// `compose -f … -p … --profile …`, ready for a subcommand to be appended.
    pub fn args(&self) -> Result<Vec<String>> {
        let mut args = vec!["compose".to_owned()];
        for file in &self.files {
            safe_value(file, "compose file")?;
            args.push("-f".to_owned());
            args.push(file.clone());
        }
        if let Some(project) = &self.project {
            safe_value(project, "project")?;
            args.push("-p".to_owned());
            args.push(project.clone());
        }
        for profile in &self.profiles {
            safe_value(profile, "profile")?;
            args.push("--profile".to_owned());
            args.push(profile.clone());
        }
        if let Some(env_file) = &self.env_file {
            safe_value(env_file, "env file")?;
            args.push("--env-file".to_owned());
            args.push(env_file.clone());
        }
        Ok(args)
    }

    /// True when nothing about a compose project was named, so a tool should address
    /// containers directly instead.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.project.is_none()
            && self.profiles.is_empty()
            && self.env_file.is_none()
    }
}

/// Reject a caller-supplied value that would arrive as a flag, or that cannot be one
/// argument. The error names the value, because a caller that sent `-f` meant something.
pub fn safe_value(value: &str, what: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::InvalidRequest {
            detail: format!("an empty {what} cannot be used; omit it instead"),
        });
    }
    if value.starts_with('-') {
        return Err(Error::InvalidRequest {
            detail: format!(
                "{what} `{value}` starts with `-`, which docker would read as a flag; \
                 names and filters are values, not options"
            ),
        });
    }
    if value.contains(['\n', '\r', '\0']) {
        return Err(Error::InvalidRequest {
            detail: format!("{what} contains a newline or NUL, which cannot be one argument"),
        });
    }
    Ok(())
}

/// Run an assembled command line, turning "docker is not installed" into an error a
/// caller can act on rather than an I/O code.
/// Docker's filters are all `key=value`. Checking that here means every tool refuses a
/// malformed one the same way, before it reaches a CLI that would answer with its usage.
pub fn check_filter(filter: &str) -> Result<()> {
    safe_value(filter, "filter")?;
    if !filter.contains('=') {
        return Err(Error::request(format!(
            "filter `{filter}` is not a `key=value` expression; docker takes \
             `status=exited`, `health=unhealthy`, `name=api`, `label=env=prod`"
        )));
    }
    Ok(())
}

/// Compile a caller's filter, with a message that says which parameter was wrong.
pub fn matcher(
    pattern: Option<&str>,
    ignore_case: bool,
    what: &str,
) -> Result<Option<regex::Regex>> {
    pattern
        .map(|pattern| {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|err| Error::request(format!("{what} `{pattern}` is not a regex: {err}")))
        })
        .transpose()
}

pub fn run(
    runner: &dyn CommandRunner,
    invocation: &Invocation,
    args: &[String],
) -> Result<CommandOutput> {
    run_with_stdin(runner, invocation, args, None)
}

/// [`run`], with `stdin` handed to the process.
pub fn run_with_stdin(
    runner: &dyn CommandRunner,
    invocation: &Invocation,
    args: &[String],
    stdin: Option<&str>,
) -> Result<CommandOutput> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    runner
        .run_with_stdin(
            invocation.bin(),
            &borrowed,
            invocation.dir(),
            invocation.timeout(),
            stdin,
        )
        .map_err(|err| Error::DockerMissing {
            bin: invocation.bin().to_owned(),
            detail: err.to_string(),
        })
}

/// Build a full command line: global flags, then the subcommand and its arguments.
pub fn command(invocation: &Invocation, rest: Vec<String>) -> Result<Vec<String>> {
    let mut args = invocation.global_args()?;
    args.extend(rest);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_would_arrive_as_a_flag_is_refused() {
        let err = safe_value("--privileged", "target").expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("--privileged"), "{message}");
        assert!(message.contains("docker would read as a flag"), "{message}");
        assert!(safe_value("web", "target").is_ok());
        assert!(safe_value("my-project_1", "project").is_ok());
    }

    #[test]
    fn an_empty_or_multiline_value_is_refused() {
        assert!(safe_value("", "target").is_err());
        assert!(safe_value("web\nrm -rf", "target").is_err());
    }

    #[test]
    fn the_compose_prefix_is_assembled_in_the_order_the_cli_wants() {
        let scope = ComposeScope {
            files: vec!["docker-compose.yml".to_owned(), "override.yml".to_owned()],
            project: Some("ens".to_owned()),
            profiles: vec!["debug".to_owned()],
            env_file: None,
        };
        assert_eq!(
            scope.args().expect("valid"),
            vec![
                "compose",
                "-f",
                "docker-compose.yml",
                "-f",
                "override.yml",
                "-p",
                "ens",
                "--profile",
                "debug"
            ]
        );
        assert!(!scope.is_empty());
        assert!(ComposeScope::default().is_empty());
    }

    #[test]
    fn the_host_flag_precedes_the_subcommand() {
        let invocation = Invocation {
            host: Some("ssh://box".to_owned()),
            ..Invocation::default()
        };
        assert_eq!(
            command(&invocation, vec!["ps".to_owned()]).expect("valid"),
            vec!["-H", "ssh://box", "ps"]
        );
    }
}
