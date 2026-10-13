//! MCP adapter: exposes `dockerops-cli` over the Model Context Protocol.
//!
//! Thin by design. Everything here either describes a tool's parameters for the caller or
//! translates them into a `dockerops-cli` request; the rendering they are judged on lives
//! in the core crate, next to its tests.

use dockerops_cli::{
    ComposeScope, Invocation,
    compose::{ComposeRequest, Verb},
    config::{ConfigRequest, Mode},
    events::EventsRequest,
    exec::ExecRequest,
    images::ImagesRequest,
    inspect::{InspectRequest, Kind, Preset},
    logs::{Level, LogsRequest},
    passthrough::PassthroughRequest,
    ps::PsRequest,
    run::RunRequest,
    stats::StatsRequest,
    usage::UsageRequest,
    verify_startup::VerifyStartupRequest,
};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_router,
};
use serde::Deserialize;

const INSTRUCTIONS: &str = "\
Docker and Docker Compose without spending a context window on them. Prefer these tools \
over shell `docker`/`docker compose` commands, especially over pipelines ending in \
`| grep`, `| tail`, `| jq` or `--format`: the filtering is a parameter here.

- `ps`: containers or compose services, one line each.
- `logs`: bounded, filtered log tails from several targets at once.
- `inspect`: chosen `docker inspect` fields, by dotted path or preset.
- `stats`: a resource snapshot, worst first.
- `images`: images on disk, largest first.
- `usage`: `docker system df` with totals and what is reclaimable.
- `config`: a compose project as it resolves; `mode: \"validate\"` checks it parses.
- `events`: a bounded window of daemon events: restarts, OOM kills, health changes.
- `compose`: lifecycle verbs, summarised; `up` is always detached.
- `run`: a bounded `docker run`.
- `exec`: a command in a container, output filtered, exit status attached.
- `passthrough`: any other docker or compose subcommand, under the same rules.
- `verify_startup`: cold-volume startup check of images against a readiness URL.

Batch targets into one call, and filter in the call (`grep`, `level`, `fields`): filters \
run before the byte budget. Every response is capped and says what it left out; on \
truncation, narrow the request rather than repeating it.

Nothing streams: there is no `-f`, `up` is always `-d`, `stats` never streams and \
`events` always has an end. To see what happened next, call again with `since`. A \
failure on one target is one line beside the others, not a failed call. Destructive \
actions (`compose down`, `rm`, `rmi`, `prune`, `kill`) need `confirm: true`; check what \
exists with a read tool first. The read tools are unaffected by `--read-only`.

Every tool also takes `cwd` (directory to run in, where relative compose files resolve), \
`host` (daemon, as `DOCKER_HOST` takes it), `timeout_seconds` (default 30) and \
`max_bytes` (output ceiling, default 40000, max 400000).";

/// Which Docker to talk to, and for how long. Shared by every tool.
///
/// The fields carry `//` rather than `///` comments on purpose: a doc comment becomes a
/// schema description, which would be sent thirteen times on every request. They are
/// described once, in [`INSTRUCTIONS`].
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EngineParams {
    // Directory to run in; where a relative compose file is found.
    #[serde(default)]
    pub cwd: Option<String>,
    // Daemon to talk to, as `DOCKER_HOST` takes it.
    #[serde(default)]
    pub host: Option<String>,
    // Seconds before the Docker CLI is killed. Defaults to 30.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    // Ceiling on rendered output in bytes (default 40000, max 400000).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Naming any of these routes the call through `docker compose` instead of plain `docker`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ComposeParams {
    /// Compose project name (`-p`).
    #[serde(default)]
    pub project: Option<String>,
    /// Compose files (`-f`), in order. Relative paths resolve against `cwd`.
    #[serde(default)]
    pub files: Vec<String>,
    /// Compose profiles to enable (`--profile`).
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Compose env file (`--env-file`).
    #[serde(default)]
    pub env_file: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PsParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    #[serde(flatten)]
    pub compose: ComposeParams,
    /// Include stopped containers.
    #[serde(default)]
    pub all: bool,
    /// Limit to these containers, or these compose services.
    #[serde(default)]
    pub services: Vec<String>,
    /// Docker filters, as `key=value`: `status=exited`, `health=unhealthy`, `name=api`,
    /// `label=com.docker.compose.project=ens`.
    #[serde(default)]
    pub filter: Vec<String>,
    /// `name` (default), `state`, or `given` to keep Docker's own order.
    #[serde(default)]
    pub sort: Option<String>,
    /// Cap on containers listed. Defaults to 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct LogsParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    #[serde(flatten)]
    pub compose: ComposeParams,
    /// Containers, or compose services when `project`/`files` is given. Empty with a
    /// project reads every service.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Lines fetched per target before filtering. Defaults to 80; raise it when using
    /// `grep`.
    #[serde(default)]
    pub tail: Option<usize>,
    /// Only lines since this point: `10m`, `2h`, `2026-10-06T12:00:00`.
    #[serde(default)]
    pub since: Option<String>,
    /// Only lines before this point, same syntax. With `since`, a window.
    #[serde(default)]
    pub until: Option<String>,
    /// Keep only lines matching any of these regexes.
    #[serde(default)]
    pub grep: Vec<String>,
    /// Drop lines matching any of these regexes, applied after `grep`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// `error` or `warn`: a built-in pattern, combined with `grep` as either-matches.
    #[serde(default)]
    pub level: Option<String>,
    /// Lines of context either side of a kept line.
    #[serde(default)]
    pub context: Option<usize>,
    /// Case-insensitive `grep` and `exclude`. On by default.
    #[serde(default)]
    pub ignore_case: Option<bool>,
    /// Keep each line's timestamp. Off by default.
    #[serde(default)]
    pub timestamps: bool,
    /// Fold runs of identical lines into one with a `×N` count. On by default.
    #[serde(default)]
    pub collapse: Option<bool>,
    /// Render the start of the window rather than the end.
    #[serde(default)]
    pub head: bool,
    /// Cap on rendered lines per target, after filtering. Defaults to 200.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InspectParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Containers, images, networks or volumes. Several in one call.
    pub targets: Vec<String>,
    /// `container`, `image`, `network` or `volume`. Omitted, Docker tries containers first.
    #[serde(default)]
    pub kind: Option<String>,
    /// Dotted paths: `State.Health.Status`, `Config.Env`,
    /// `NetworkSettings.Networks.*.IPAddress`. `*` matches every element or key.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Used when `fields` is empty: `health`, `env`, `mounts`, `ports`, `network`, `image`,
    /// `labels`, `restart`, `entrypoint`, `limits`, or `summary` (default).
    #[serde(default)]
    pub preset: Option<String>,
    /// List the keys at each path instead of their values.
    #[serde(default)]
    pub keys: bool,
    /// Cap on the characters of any one rendered value. Defaults to 240.
    #[serde(default)]
    pub max_value: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct StatsParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Containers to measure. Empty means every running container.
    #[serde(default)]
    pub targets: Vec<String>,
    /// `cpu` (default), `memory` or `name`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Cap on rows. Defaults to 40.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ImagesParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Only images matching these references: `ghcr.io/acme/*`, `postgres`.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Docker filters, as `key=value`: `dangling=true`, `before=postgres:17`, `label=…`.
    #[serde(default)]
    pub filter: Vec<String>,
    /// Include intermediate layers (`--all`).
    #[serde(default)]
    pub all: bool,
    /// Include untagged images. On by default.
    #[serde(default)]
    pub dangling: Option<bool>,
    /// `size` (default), `created` or `name`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Cap on rows. Defaults to 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct UsageParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Ask Docker for the per-item detail as well (`docker system df -v`).
    #[serde(default)]
    pub detail: bool,
    /// How many items to name per class when `detail` is set. Defaults to 5.
    #[serde(default)]
    pub top: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ConfigParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    #[serde(flatten)]
    pub compose: ComposeParams,
    /// Limit to these services.
    #[serde(default)]
    pub services: Vec<String>,
    /// `summary` (default), `services` (names only), `images`, `resources` (volumes and
    /// networks, and what uses them), `profiles` (what each `--profile` brings up), or
    /// `validate` (`config --quiet`: valid, or the error).
    #[serde(default)]
    pub mode: Option<String>,
    /// Leave `${VAR}` unresolved.
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EventsParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Containers, images, networks or volumes to narrow to, by name or id.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Object types to keep: `container`, `image`, `network`, `volume`, `daemon`.
    #[serde(default)]
    pub types: Vec<String>,
    /// Actions to keep: `die`, `start`, `kill`, `oom`, `health_status`, `restart`.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Docker filters as `key=value`, for anything the fields above do not cover.
    #[serde(default)]
    pub filter: Vec<String>,
    /// Start of the window: `15m` (the default), `2h`, or a timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// End of the window. Defaults to `0s`, meaning now.
    #[serde(default)]
    pub until: Option<String>,
    /// Fold runs of identical events into one line with a `×N` count.
    #[serde(default)]
    pub collapse: bool,
    /// Cap on events rendered, keeping the recent end. Defaults to 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ComposeActionParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    #[serde(flatten)]
    pub compose: ComposeParams,
    /// `up`, `down`, `start`, `stop`, `restart`, `kill`, `build`, `pull`, `create`, `rm`.
    pub verb: String,
    /// Services to act on. Empty means the whole project.
    #[serde(default)]
    pub services: Vec<String>,
    /// Required for `down`, `rm` and `kill`, which throw something away.
    #[serde(default)]
    pub confirm: bool,
    /// `up --build`: rebuild images first.
    #[serde(default)]
    pub build: bool,
    /// `up --force-recreate`.
    #[serde(default)]
    pub force_recreate: bool,
    /// Act only on the services named, not on what they depend on.
    #[serde(default)]
    pub no_deps: bool,
    /// Remove containers for services no longer in the compose file.
    #[serde(default)]
    pub remove_orphans: bool,
    /// `up --wait`: return once containers are healthy. Bounded by `timeout_seconds`.
    #[serde(default)]
    pub wait: bool,
    /// With `down` or `rm`: also delete the project's named volumes and their data. Never
    /// implied.
    #[serde(default)]
    pub volumes: bool,
    /// Seconds a container gets to shut down before SIGKILL.
    #[serde(default)]
    pub stop_timeout: Option<u64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ExecParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    #[serde(flatten)]
    pub compose: ComposeParams,
    /// A container name or id, or a compose service when `project`/`files` is given.
    pub target: String,
    /// The command as a list: `["ls", "-la", "/data"]`.
    #[serde(default)]
    pub argv: Vec<String>,
    /// A command line run as `sh -c`, instead of `argv`, for pipes and redirects.
    #[serde(default)]
    pub shell: Option<String>,
    /// `--user`.
    #[serde(default)]
    pub user: Option<String>,
    /// `--workdir`.
    #[serde(default)]
    pub workdir: Option<String>,
    /// `--env` entries, as `KEY=value`.
    #[serde(default)]
    pub env: Vec<String>,
    /// Keep only output lines matching this regex.
    #[serde(default)]
    pub grep: Option<String>,
    /// Drop output lines matching this regex, applied after `grep`.
    #[serde(default)]
    pub exclude: Option<String>,
    #[serde(default)]
    pub ignore_case: bool,
    /// Keep the first lines rather than the last.
    #[serde(default)]
    pub head: bool,
    /// Cap on lines rendered. Defaults to 200.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Image reference to run (`hello-world`, `postgres:17`, `ghcr.io/acme/api:sha`).
    pub image: String,
    /// Command and arguments after the image.
    #[serde(default)]
    pub command: Vec<String>,
    /// `--name`.
    #[serde(default)]
    pub name: Option<String>,
    /// `--user`.
    #[serde(default)]
    pub user: Option<String>,
    /// `--workdir`.
    #[serde(default)]
    pub workdir: Option<String>,
    /// `--env` entries, as `KEY=value`.
    #[serde(default)]
    pub env: Vec<String>,
    /// `--publish` entries (`8080:80`, `127.0.0.1:5432:5432`).
    #[serde(default)]
    pub publish: Vec<String>,
    /// `--volume` entries (`host:container[:mode]`).
    #[serde(default)]
    pub volume: Vec<String>,
    /// `--network`.
    #[serde(default)]
    pub network: Option<String>,
    /// `--detach` (`-d`).
    #[serde(default)]
    pub detach: bool,
    /// `--rm`. Defaults to true for one-off runs when not set.
    #[serde(default)]
    pub rm: Option<bool>,
    /// Keep only output lines matching this regex.
    #[serde(default)]
    pub grep: Option<String>,
    /// Drop output lines matching this regex, applied after `grep`.
    #[serde(default)]
    pub exclude: Option<String>,
    #[serde(default)]
    pub ignore_case: bool,
    /// Keep the first lines rather than the last.
    #[serde(default)]
    pub head: bool,
    /// Cap on lines rendered. Defaults to 200.
    #[serde(default)]
    pub max_lines: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PassthroughParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    #[serde(flatten)]
    pub compose: ComposeParams,
    /// Subcommand and arguments: `["volume", "inspect", "ens-data"]`. No global flags; use
    /// `host` and `cwd`.
    pub args: Vec<String>,
    /// Required for subcommands classed destroy: `rm`, `rmi`, `prune`, `kill`, `down`,
    /// under any noun.
    #[serde(default)]
    pub confirm: bool,
    /// Keep only output lines matching this regex.
    #[serde(default)]
    pub grep: Option<String>,
    /// Drop output lines matching this regex, applied after `grep`.
    #[serde(default)]
    pub exclude: Option<String>,
    #[serde(default)]
    pub ignore_case: bool,
    /// Keep the first lines rather than the last.
    #[serde(default)]
    pub head: bool,
    /// Cap on lines rendered. Defaults to 300.
    #[serde(default)]
    pub max_lines: Option<usize>,
    /// Text written to stdin, then closed (max 1 MiB), e.g. a SQL script for `["exec",
    /// "-i", "db", "psql"]`. `run`, `create`, `exec` and `start` need `-i` to pass it on.
    #[serde(default)]
    pub stdin: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct VerifyStartupParams {
    #[serde(flatten)]
    pub engine: EngineParams,
    /// Images to verify, in order. Each image is run in an isolated fresh volume.
    pub images: Vec<String>,
    /// `KEY=value` entries passed as `docker run --env`.
    #[serde(default)]
    pub env: Vec<String>,
    /// `host:container` entries passed as `docker run --publish`.
    #[serde(default)]
    pub publish: Vec<String>,
    /// Container path where the fresh test volume is mounted.
    pub volume_target: String,
    /// Readiness URL, for example `http://127.0.0.1:3000/ready`.
    pub probe_url: String,
    /// Delay before the first probe. Defaults to 30.
    #[serde(default)]
    pub initial_delay_seconds: Option<u64>,
    /// Delay between probes while the container is still running. Defaults to 10.
    #[serde(default)]
    pub interval_seconds: Option<u64>,
    /// Maximum probe attempts before timeout. Defaults to 120.
    #[serde(default)]
    pub max_attempts: Option<usize>,
    /// `docker logs --tail` line count captured per image. Defaults to 80.
    #[serde(default)]
    pub log_tail_lines: Option<usize>,
    /// Remove the test container and volume after each image run.
    #[serde(default = "bool_true")]
    pub cleanup: bool,
    /// Delay between image runs when several images are verified in one call.
    #[serde(default)]
    pub pause_between_images_seconds: Option<u64>,
    /// Fixed suffix for container and volume names, for deterministic runs.
    #[serde(default)]
    pub run_id: Option<String>,
}

fn bool_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct DockerOpsServer {
    /// Built once, with its schemas compacted; see [`compact`].
    tool_router: ToolRouter<Self>,
    /// Server-wide default budget, from `--max-bytes`. A request may still name its own.
    default_max_bytes: Option<usize>,
    /// Docker binary, from `--docker-bin`, for a podman shim or an unusual install.
    docker_bin: Option<String>,
    /// From `--read-only`: `compose`, `run`, `exec` and anything but a read through
    /// `passthrough` refuse outright. The read tools are unaffected.
    read_only: bool,
}

#[tool_router]
impl DockerOpsServer {
    pub fn new(
        default_max_bytes: Option<usize>,
        docker_bin: Option<String>,
        read_only: bool,
    ) -> Self {
        Self {
            tool_router: compact(Self::tool_router()),
            default_max_bytes,
            docker_bin,
            read_only,
        }
    }

    fn invocation(&self, engine: &EngineParams) -> Invocation {
        Invocation {
            docker_bin: self.docker_bin.clone(),
            host: engine.host.clone(),
            cwd: engine.cwd.clone().map(std::path::PathBuf::from),
            timeout_seconds: engine.timeout_seconds,
        }
    }

    fn scope(&self, compose: &ComposeParams) -> ComposeScope {
        ComposeScope {
            files: compose.files.clone(),
            project: compose.project.clone(),
            profiles: compose.profiles.clone(),
            env_file: compose.env_file.clone(),
        }
    }

    fn budget(&self, requested: Option<usize>) -> Option<usize> {
        requested.or(self.default_max_bytes)
    }

    #[tool(
        name = "ps",
        description = "What is running, one line per container: name, state and age, image, published ports. `project`/`files` asks `docker compose ps` instead. `all` includes stopped containers; `filter` takes Docker's `key=value` filters. Health is shown only when it is bad."
    )]
    fn ps(&self, Parameters(params): Parameters<PsParams>) -> Result<CallToolResult, ErrorData> {
        let sort = match params.sort.as_deref() {
            None | Some("name") => dockerops_cli::ps::Sort::Name,
            Some("state") => dockerops_cli::ps::Sort::State,
            Some("given") => dockerops_cli::ps::Sort::Given,
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`sort` must be `name`, `state` or `given`, not `{other}`"
                ))));
            }
        };
        let request = PsRequest {
            invocation: self.invocation(&params.engine),
            scope: self.scope(&params.compose),
            all: params.all,
            services: params.services,
            filter: params.filter,
            sort,
            limit: params.limit,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::ps(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "logs",
        description = "Log tails from several containers or compose services in one call. `grep`, `exclude`, `context`, `level` and `since`/`until` filter before anything is rendered; `tail` is always bounded; runs of identical lines fold into `… ×N`; stdout and stderr interleave in the order written. No follow mode: call again with `since`. A target that fails is one line of output, not a failed call."
    )]
    fn logs(
        &self,
        Parameters(params): Parameters<LogsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let level = match params.level.as_deref() {
            None => None,
            Some("error") => Some(Level::Error),
            Some("warn" | "warning") => Some(Level::Warn),
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`level` must be `error` or `warn`, not `{other}`; use `grep` for anything else"
                ))));
            }
        };
        let request = LogsRequest {
            invocation: self.invocation(&params.engine),
            scope: self.scope(&params.compose),
            targets: params.targets,
            tail: params.tail,
            since: params.since,
            until: params.until,
            grep: params.grep,
            exclude: params.exclude,
            level,
            context: params.context,
            ignore_case: params.ignore_case,
            timestamps: params.timestamps,
            collapse: params.collapse,
            head: params.head,
            max_lines: params.max_lines,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::logs(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "inspect",
        description = "Chosen fields of `docker inspect`, for many targets in one call: dotted paths in `fields` (`State.Health.Status`, `Mounts.*.Destination`) or a `preset`. `*` matches every element or key and reports which; `keys: true` lists a level's keys instead of values. A path that matches nothing is reported as such."
    )]
    fn inspect(
        &self,
        Parameters(params): Parameters<InspectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let kind = match params.kind.as_deref() {
            None => Kind::Any,
            Some("container") => Kind::Container,
            Some("image") => Kind::Image,
            Some("network") => Kind::Network,
            Some("volume") => Kind::Volume,
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`kind` must be `container`, `image`, `network` or `volume`, not `{other}`"
                ))));
            }
        };
        let preset = match params.preset.as_deref() {
            None => None,
            Some(name) => match Preset::parse(name) {
                Some(preset) => Some(preset),
                None => {
                    return Ok(failed(dockerops_cli::Error::request(format!(
                        "no preset `{name}`; try `health`, `env`, `mounts`, `ports`, \
                         `network`, `image`, `labels`, `restart`, `entrypoint`, `limits` \
                         or `summary`, or name the paths in `fields`"
                    ))));
                }
            },
        };
        let request = InspectRequest {
            invocation: self.invocation(&params.engine),
            targets: params.targets,
            kind,
            fields: params.fields,
            preset,
            keys: params.keys,
            max_value: params.max_value,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::inspect(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "stats",
        description = "A resource snapshot, one line per container, worst first, memory as a share of its limit. Always `--no-stream`. Network and disk figures appear only when nonzero."
    )]
    fn stats(
        &self,
        Parameters(params): Parameters<StatsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let sort = match params.sort.as_deref() {
            None | Some("cpu") => dockerops_cli::stats::Sort::Cpu,
            Some("memory" | "mem") => dockerops_cli::stats::Sort::Memory,
            Some("name") => dockerops_cli::stats::Sort::Name,
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`sort` must be `cpu`, `memory` or `name`, not `{other}`"
                ))));
            }
        };
        let request = StatsRequest {
            invocation: self.invocation(&params.engine),
            targets: params.targets,
            sort,
            limit: params.limit,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::stats(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "images",
        description = "Images on disk, largest first: reference, size, age, and a total. Narrow with `patterns` or `key=value` `filter`s; `dangling: false` hides untagged images."
    )]
    fn images(
        &self,
        Parameters(params): Parameters<ImagesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let sort = match params.sort.as_deref() {
            None | Some("size") => dockerops_cli::images::Sort::Size,
            Some("created") => dockerops_cli::images::Sort::Created,
            Some("name") => dockerops_cli::images::Sort::Name,
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`sort` must be `size`, `created` or `name`, not `{other}`"
                ))));
            }
        };
        let request = ImagesRequest {
            invocation: self.invocation(&params.engine),
            patterns: params.patterns,
            filter: params.filter,
            all: params.all,
            dangling: params.dangling,
            sort,
            limit: params.limit,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::images(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "usage",
        description = "`docker system df` with a total and the reclaimable share, for images, containers, volumes and build cache. `detail` adds the per-item breakdown."
    )]
    fn usage(
        &self,
        Parameters(params): Parameters<UsageParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = UsageRequest {
            invocation: self.invocation(&params.engine),
            detail: params.detail,
            top: params.top,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::usage(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "config",
        description = "A compose project as it resolves, with overrides, profiles and `.env` applied, as facts per service: image, ports, mounts, dependencies. `mode` selects another view; `raw: true` leaves `${VAR}` unresolved. A file that does not parse returns the error line."
    )]
    fn config(
        &self,
        Parameters(params): Parameters<ConfigParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let mode = match params.mode.as_deref() {
            None | Some("summary") => Mode::Summary,
            Some("services") => Mode::Services,
            Some("images") => Mode::Images,
            Some("resources" | "volumes" | "networks") => Mode::Resources,
            Some("profiles") => Mode::Profiles,
            Some("validate" | "quiet") => Mode::Validate,
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`mode` must be `summary`, `services`, `images`, `resources`, \
                     `profiles` or `validate`, not `{other}`"
                ))));
            }
        };
        let request = ConfigRequest {
            invocation: self.invocation(&params.engine),
            scope: self.scope(&params.compose),
            services: params.services,
            mode,
            raw: params.raw,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::config(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "events",
        description = "A bounded window of daemon events, in order: restarts, OOM kills, health transitions, with exit codes and signals. Both ends are always set; the default is the last 15 minutes. Narrow with `targets`, `types` or `actions`; `collapse: true` folds a crash loop into one line with a count."
    )]
    fn events(
        &self,
        Parameters(params): Parameters<EventsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = EventsRequest {
            invocation: self.invocation(&params.engine),
            targets: params.targets,
            types: params.types,
            actions: params.actions,
            filter: params.filter,
            since: params.since,
            until: params.until,
            collapse: params.collapse,
            limit: params.limit,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::events(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "compose",
        description = "Compose lifecycle verbs, summarised to one line per outcome: `up`, `down`, `start`, `stop`, `restart`, `kill`, `build`, `pull`, `create`, `rm`. `up` is always detached; `wait: true` returns once healthy. `down`, `rm` and `kill` need `confirm: true`, and `volumes: true` also deletes named volumes. For other verbs use `passthrough`."
    )]
    fn compose(
        &self,
        Parameters(params): Parameters<ComposeActionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.read_only {
            return Ok(failed(dockerops_cli::Error::refused(
                "this server is running read-only; `compose` changes state. The read \
                 tools — `ps`, `logs`, `config`, `events` — still work.",
            )));
        }
        let Some(verb) = Verb::parse(&params.verb) else {
            return Ok(failed(dockerops_cli::Error::request(format!(
                "no compose verb `{}`; this tool takes {}. Anything else goes through \
                 `passthrough`.",
                params.verb,
                Verb::all()
            ))));
        };
        let request = ComposeRequest {
            invocation: self.invocation(&params.engine),
            scope: self.scope(&params.compose),
            verb,
            services: params.services,
            confirm: params.confirm,
            build: params.build,
            force_recreate: params.force_recreate,
            no_deps: params.no_deps,
            remove_orphans: params.remove_orphans,
            wait: params.wait,
            volumes: params.volumes,
            stop_timeout: params.stop_timeout,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::compose(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "run",
        description = "A bounded `docker run` with structured arguments and capped output; prefer it to `passthrough` for `docker run`. Set `detach: true` for a background container (its id is returned), or keep the default `rm: true` for a one-off run."
    )]
    fn run(&self, Parameters(params): Parameters<RunParams>) -> Result<CallToolResult, ErrorData> {
        if self.read_only {
            return Ok(failed(dockerops_cli::Error::refused(
                "this server is running read-only; `run` starts new containers and changes state.",
            )));
        }
        let request = RunRequest {
            invocation: self.invocation(&params.engine),
            image: params.image,
            command: params.command,
            name: params.name,
            user: params.user,
            workdir: params.workdir,
            env: params.env,
            publish: params.publish,
            volume: params.volume,
            network: params.network,
            detach: params.detach,
            rm: params.rm.unwrap_or(true),
            grep: params.grep,
            exclude: params.exclude,
            ignore_case: params.ignore_case,
            head: params.head,
            max_lines: params.max_lines,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::run(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "exec",
        description = "A command in a container, with `grep`, `exclude` and `max_lines` applied here instead of in a pipe, and the exit status reported. Send `argv` as a list, or `shell` (run as `sh -c`) for pipes and redirects. No TTY is allocated. `project`/`files` addresses a compose service."
    )]
    fn exec(
        &self,
        Parameters(params): Parameters<ExecParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.read_only {
            return Ok(failed(dockerops_cli::Error::refused(
                "this server is running read-only; `exec` runs whatever it is given, \
                 which cannot be classified in advance.",
            )));
        }
        let request = ExecRequest {
            invocation: self.invocation(&params.engine),
            scope: self.scope(&params.compose),
            target: params.target,
            argv: params.argv,
            shell: params.shell,
            user: params.user,
            workdir: params.workdir,
            env: params.env,
            grep: params.grep,
            exclude: params.exclude,
            ignore_case: params.ignore_case,
            head: params.head,
            max_lines: params.max_lines,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::exec(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "passthrough",
        description = "Any other docker or compose subcommand as an argument list (`cp`, `top`, `history`, `network inspect`, `volume prune`, …), under the same byte budget and deadline. Commands that never return are refused, naming the bounded form: `attach`, `wait`, `compose watch`, `logs -f`, `stats` without `--no-stream`, `events` without `--until`, `compose up` without `-d`. `-i` reads `stdin`; `-t` output has escape codes stripped; `-it` runs as `-i`. Subcommands are classed read, mutate or destroy by verb (unknown means mutate), and destroy needs `confirm: true`. Prefer a dedicated tool when one exists."
    )]
    fn passthrough(
        &self,
        Parameters(params): Parameters<PassthroughParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let request = PassthroughRequest {
            invocation: self.invocation(&params.engine),
            scope: self.scope(&params.compose),
            args: params.args,
            confirm: params.confirm,
            read_only: self.read_only,
            grep: params.grep,
            exclude: params.exclude,
            ignore_case: params.ignore_case,
            head: params.head,
            max_lines: params.max_lines,
            max_bytes: self.budget(params.engine.max_bytes),
            stdin: params.stdin,
        };

        match dockerops_cli::passthrough(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }

    #[tool(
        name = "verify_startup",
        description = "Cold-start check of one or more images in one call. For each: create a fresh volume, run detached with `env`/`publish`, poll `probe_url` up to `max_attempts` times, capture a log tail, and clean up when `cleanup` is set. Returns pass/fail with evidence per image."
    )]
    fn verify_startup(
        &self,
        Parameters(params): Parameters<VerifyStartupParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if self.read_only {
            return Ok(failed(dockerops_cli::Error::refused(
                "this server is running read-only; `verify_startup` creates/removes containers and volumes.",
            )));
        }
        let request = VerifyStartupRequest {
            invocation: self.invocation(&params.engine),
            images: params.images,
            env: params.env,
            publish: params.publish,
            volume_target: params.volume_target,
            probe_url: params.probe_url,
            initial_delay_seconds: params.initial_delay_seconds,
            interval_seconds: params.interval_seconds,
            max_attempts: params.max_attempts,
            log_tail_lines: params.log_tail_lines,
            cleanup: params.cleanup,
            pause_between_images_seconds: params.pause_between_images_seconds,
            run_id: params.run_id,
            max_bytes: self.budget(params.engine.max_bytes),
        };

        match dockerops_cli::verify_startup(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }
}

#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for DockerOpsServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Remove what a schema says that a model learns nothing from.
///
/// Every tool's schema is re-sent with every request, so this is paid per turn: the
/// `$schema` URL, a `default` of `null`, `[]` or `false`, the `null` half of an optional
/// field's type (not being `required` already says it is optional), and the
/// unsigned-integer `format` and `minimum: 0`, which serde enforces on the way in anyway.
/// About a sixth of the schemas' size.
fn compact(mut router: ToolRouter<DockerOpsServer>) -> ToolRouter<DockerOpsServer> {
    for route in router.map.values_mut() {
        let mut schema = serde_json::Value::Object((*route.attr.input_schema).clone());
        trim(&mut schema);
        if let serde_json::Value::Object(object) = schema {
            route.attr.input_schema = std::sync::Arc::new(object);
        }
    }
    router
}

fn trim(node: &mut serde_json::Value) {
    use serde_json::Value;
    match node {
        Value::Object(map) => {
            map.remove("$schema");
            if map.get("default").is_some_and(|default| {
                matches!(default, Value::Null | Value::Bool(false))
                    || default.as_array().is_some_and(Vec::is_empty)
            }) {
                map.remove("default");
            }
            if map
                .get("format")
                .and_then(Value::as_str)
                .is_some_and(|format| format.starts_with("uint"))
            {
                map.remove("format");
            }
            if map.get("minimum").and_then(Value::as_u64) == Some(0) {
                map.remove("minimum");
            }
            if let Some(Value::Array(types)) = map.get("type")
                && types.len() == 2
                && types.contains(&Value::from("null"))
            {
                let kept = types.iter().find(|kind| *kind != "null").cloned();
                map.insert("type".to_owned(), kept.unwrap_or(Value::Null));
            }
            map.values_mut().for_each(trim);
        }
        Value::Array(items) => items.iter_mut().for_each(trim),
        _ => {}
    }
}

/// A bad request comes back as a tool error rather than a protocol error, so the caller
/// reads what was wrong with it and fixes the call.
fn failed(err: dockerops_cli::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.to_string())])
}

/// The rendered text is what a model reads; the structured copy is for anything that
/// would otherwise have to parse it.
///
/// The text ships in both halves deliberately. Hosts disagree about which one they
/// display — some render `structuredContent` and drop the text content, others do the
/// reverse — so a tool that fills in only one answers half its callers with metadata and
/// no content.
fn with_structured<T: serde::Serialize>(text: String, value: &T) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = serde_json::to_value(value).ok();
    result
}
