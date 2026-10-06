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
};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_router,
};
use serde::Deserialize;

const INSTRUCTIONS: &str = "\
Docker and Docker Compose without spending a context window on them. Prefer these tools \
over shell `docker`/`docker compose` commands, and especially over pipelines that end in \
`| grep`, `| tail`, `| jq` or `--format` — the filtering is a parameter here, so the \
output never has to be produced before it is thrown away.

- `ps` — what is running, one line per container: name, state, image, published ports. \
Containers and compose services alike; `project` or `files` routes the question through \
`docker compose ps`.
- `logs` — log tails from several containers or services in one call, with `grep`, \
`exclude`, `context`, `level`, `since`/`until` and a `tail` that is always bounded. \
Repeated lines fold into `… ×N`, and a container's stdout and stderr come back in the \
order they were written.
- `inspect` — the fields of the inspect document you asked for, by dotted path \
(`State.Health.Status`, `Mounts.*.Destination`) or by preset (`health`, `env`, `mounts`, \
`ports`, `network`, `image`, `labels`, `restart`, `entrypoint`, `limits`). Four hundred \
lines of JSON become the six you meant.
- `stats` — a resource snapshot, worst first. Never streams.
- `images` — what is on disk, largest first, which is the question `docker images` \
answers by date.
- `usage` — `docker system df` with the totals line it leaves out, and what is actually \
reclaimable.
- `config` — a compose project as it resolves: which image each service is really on, \
what it publishes, what it mounts, what it waits for. Replaces reading several hundred \
lines of `docker compose config`.
- `events` — a window of the daemon's event stream, both ends bounded. The restarts, OOM \
kills and health transitions of the last fifteen minutes, in order.
- `compose` — the lifecycle verbs: `up`, `down`, `start`, `stop`, `restart`, `kill`, \
`build`, `pull`, `create`, `rm`. `up` is always detached, and forty lines of progress \
come back as one line per outcome.
- `run` — a bounded `docker run` for one-off or detached containers. It carries the same \
line and byte caps as the other tools, defaults to ephemeral runs, and returns the \
container id on detached starts.
- `exec` — a command in a container, with the filtering you were going to pipe it \
through: `grep`, `exclude`, `max_lines`. The exit status comes back attached, which a \
pipeline throws away.
- `passthrough` — any other docker or compose subcommand, as an argument list. This is \
the rest of both CLIs — `cp`, `top`, `port`, `history`, `network inspect`, `volume \
prune`, `buildx bake`, `swarm init` — under the same budget, deadline and rules.

Two habits make the difference:

1. **Batch.** One `logs` call naming four services, or one `inspect` naming four \
containers, costs one round trip. Four calls, or a shell command joining four invocations, \
cost far more for the same bytes.
2. **Filter in the call.** `level: \"error\"`, `grep`, a dotted `fields` path — all of it \
runs before the byte budget, so the budget is spent on the lines that answer the question \
rather than on startup noise. Every response is capped and says what it left out, so a \
reply that reports truncation is an instruction to narrow the request, not to retry it \
unchanged.

Nothing here follows a stream: there is no `-f`, `up` is always `-d`, `stats` is always \
`--no-stream`, and `events` always has a far end. A tool call that does not terminate \
takes the conversation with it, so the way to see what happened next is another call with \
`since`. `passthrough` refuses `attach`, `wait`, `compose watch` and the unbounded flags \
by name, and says which tool answers the same question.

A failure that belongs to one target is one line of output next to the targets that \
worked, never a failed call. Anything that destroys state — `compose down`, `rm`, `rmi`, \
`prune`, `kill` — needs `confirm: true`, and the refusal says what would have gone; ask a \
read tool what exists before confirming. The read tools are unaffected by `--read-only`.";

/// Which Docker to talk to, and for how long. Shared by every tool.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EngineParams {
    /// Directory to run in — where a relative compose file is found. Defaults to the
    /// server's own working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Daemon to talk to (`DOCKER_HOST`), for a remote or rootless socket.
    #[serde(default)]
    pub host: Option<String>,
    /// Seconds to wait for the Docker CLI before giving up. Defaults to 30.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Ceiling on rendered output in bytes (default 40000, max 400000). Output is
    /// truncated with a marker rather than silently cut.
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
    /// Include stopped containers. Off by default: `docker ps` without `-a` is the
    /// question people usually mean.
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
    /// Containers, or compose services when a `project`/`files` is given. Several in one
    /// call is the point of this tool. Empty, with a compose project, reads the whole
    /// project and attributes each line to the service that wrote it.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Lines to ask Docker for, per target, before filtering. Defaults to 80. Raise it
    /// when combining with `grep`: the filter runs on what this fetched.
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
    /// Drop lines matching any of these regexes, applied after `grep` — the health check
    /// that logs every ten seconds, for instance.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// `error` or `warn`: a built-in pattern for the usual question, instead of writing
    /// the regex. Combines with `grep`, which keeps lines matching either.
    #[serde(default)]
    pub level: Option<String>,
    /// Lines of context either side of a kept line, so a stack trace arrives with the
    /// line that caused it.
    #[serde(default)]
    pub context: Option<usize>,
    /// Case-insensitive matching for `grep` and `exclude`. On by default: logs are prose.
    #[serde(default)]
    pub ignore_case: Option<bool>,
    /// Keep the time of day on each line. Off by default — in a tail, the timestamps are
    /// usually a column of near-identical text.
    #[serde(default)]
    pub timestamps: bool,
    /// Fold runs of identical lines into one with a `×N` count. On by default, and the
    /// difference between reading a crash loop and paying for it.
    #[serde(default)]
    pub collapse: Option<bool>,
    /// Render the start of the window rather than the end: the first lines of a startup
    /// rather than the last lines of a failure.
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
    /// `container`, `image`, `network` or `volume`. Omitted, Docker resolves a bare name
    /// against containers first, which is occasionally the wrong guess.
    #[serde(default)]
    pub kind: Option<String>,
    /// Dotted paths into the inspect document: `State.Health.Status`,
    /// `Mounts.*.Destination`, `Config.Env`, `NetworkSettings.Networks.*.IPAddress`. `*`
    /// means every element of an array or every key of an object, and comes back as the
    /// index or key it matched.
    #[serde(default)]
    pub fields: Vec<String>,
    /// A named field set, used when `fields` is empty: `health`, `env`, `mounts`, `ports`,
    /// `network`, `image`, `labels`, `restart`, `entrypoint`, `limits`, `summary`.
    /// Defaults to `summary`.
    #[serde(default)]
    pub preset: Option<String>,
    /// List the keys at each path instead of their values — how to find out what a
    /// document holds before asking for any of it.
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
    /// Include untagged images. On by default: they are usually where the disk went.
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
    /// `summary` (default: image, ports, mounts, dependencies per service), `services`
    /// (names only), `images`, `resources` (volumes and networks, and what uses them) or
    /// `profiles` (which services a `--profile` would bring up).
    #[serde(default)]
    pub mode: Option<String>,
    /// Leave `${VAR}` unresolved, to see the file's own intent rather than this machine's
    /// environment.
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
    /// End of the window. Defaults to `0s`, meaning now. There is no open-ended form: an
    /// unbounded `docker events` waits for the next event forever.
    #[serde(default)]
    pub until: Option<String>,
    /// Fold a run of identical events into one line and a `×N` count — a crash loop.
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
    /// `up --wait`: return once the containers are healthy rather than merely started.
    /// Bounded by `timeout_seconds` like everything else.
    #[serde(default)]
    pub wait: bool,
    /// With `down` or `rm`: also delete the project's named volumes, and whatever data is
    /// in them. Never implied.
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
    /// The command as a list: `["ls", "-la", "/data"]`. No shell is involved, so there is
    /// nothing to quote.
    #[serde(default)]
    pub argv: Vec<String>,
    /// A shell command line instead of `argv`, run as `sh -c`. The only way to get a pipe
    /// or a redirect, and it has to be asked for by name.
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
    /// The subcommand and its arguments as a list: `["volume", "inspect", "ens-data"]`,
    /// `["image", "history", "postgres:17"]`, `["cp", "api:/etc/conf", "./conf"]`. Global
    /// flags do not go here — `host` is `-H`, `cwd` is the working directory.
    pub args: Vec<String>,
    /// Required when the subcommand is classed `destroy`: `rm`, `rmi`, `prune`, `kill`,
    /// `down`, and the same verbs under `volume`, `network`, `image` or `system`.
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
}

#[derive(Debug, Clone)]
pub struct DockerOpsServer {
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
        description = "What is running, one line per container: name, state with its age, image, published ports. Replaces `docker ps`, whose header and seven padded columns spend a screen on facts that fit in forty characters. Naming a compose `project` or `files` asks `docker compose ps` instead, so services and containers are the same question. `all` includes stopped containers, `filter` takes Docker's own `key=value` filters. Dual-stack port pairs collapse to one entry and health is shown only when it is bad."
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
        description = "Log tails from several containers or compose services in one call, filtered the way you were going to filter them anyway. Replaces `docker logs`/`docker compose logs` and the `| grep … | tail -40` that always follows: `grep`, `exclude`, `context`, `level` and `since`/`until` are parameters, so the lines that do not answer the question are never rendered. `tail` is always bounded (80 by default), runs of identical lines fold into `… ×N`, and stdout and stderr come back interleaved in the order they were written. There is no follow mode — call again with `since` for what happened next. One unreadable target is one line of output, not a failed call."
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
        description = "The fields of `docker inspect` you asked for, not the document. Takes dotted paths (`State.Health.Status`, `Mounts.*.Destination`, `Config.Env`) or a named preset (`health`, `env`, `mounts`, `ports`, `network`, `image`, `labels`, `restart`, `entrypoint`, `limits`, `summary`), over as many targets as you like in one call. A `*` matches every element or key and comes back as the one it matched; `keys: true` lists a level's shape instead of its contents, so an unfamiliar document can be explored a level at a time. A path that matches nothing is reported, because the alternative is reading a typo as an empty field. Replaces `docker inspect | jq` and hand-written Go templates."
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
        description = "A resource snapshot: one line per container, worst first, memory as a share of its limit. Replaces `docker stats`, which is a live full-screen display that never returns — this one always passes `--no-stream`, because a tool call that does not terminate takes the conversation with it. Sort by `cpu` (default), `memory` or `name`; network and disk figures appear only when something has actually moved."
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
        description = "What is on disk, largest first — the question `docker images` answers by creation date. One line per image: reference, size, age. `patterns` and Docker's own `key=value` `filter`s narrow it, `dangling: false` hides untagged images once you have seen them. Sizes are parsed to bytes so the sort is numeric rather than alphabetical, and the footer totals what was listed."
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
        description = "Where the disk went: `docker system df` with the totals line it leaves out and the reclaimable share worked out. Four rows — images, containers, volumes, build cache — then the total and what fraction of it you can have back, which is the number a decision gets made on. `detail` asks Docker for the per-item breakdown as well."
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
        description = "A compose project as it actually resolves, rather than the merged document. `docker compose config` answers an important question — what the overrides, profiles and `.env` interpolation produced — with several hundred lines of YAML; this renders the facts people go looking for: which image each service is really on, what it publishes, what it mounts, what it waits for. `mode` switches to `services` (names only), `images`, `resources` (declared volumes and networks, and which services use them — including the ones nothing uses) or `profiles` (which services a `--profile` flag would bring up). `raw: true` leaves `${VAR}` unresolved. A compose file that does not parse comes back as the error line, which is usually the answer."
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
            Some(other) => {
                return Ok(failed(dockerops_cli::Error::request(format!(
                    "`mode` must be `summary`, `services`, `images`, `resources` or \
                     `profiles`, not `{other}`"
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
        description = "A bounded window of the daemon's event stream: what happened to these containers in the last fifteen minutes, in order — the restarts, the OOM kills, the health transitions, with exit codes and signals attached. Replaces `docker events`, which without `--until` waits for the next event forever; the window here always has both ends. Narrow by `targets`, `types` or `actions`, widen with `since`. `collapse: true` turns a crash loop into one line and a count, which is the shape that question has."
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
        description = "The compose lifecycle verbs, summarised rather than streamed: `up`, `down`, `start`, `stop`, `restart`, `kill`, `build`, `pull`, `create`, `rm`. `up` is always detached — an attached one never returns, and a tool call that never returns takes the conversation with it — and the forty lines of progress compose emits come back as one line per outcome, grouped by what happened to what. `down`, `rm` and `kill` need `confirm: true`, and the refusal says what would have gone; `volumes: true` is called out separately because named volumes are usually the data. `wait: true` returns when the containers are healthy rather than merely started. For anything beyond these verbs, use `passthrough`."
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
        description = "A bounded `docker run` for one-off and detached starts. Use this instead of passthrough for `docker run`: arguments are structured (`image`, `command`, `env`, `publish`, `volume`), output is line- and byte-capped, and detached starts return the container id in structured output. Safety rail: set `detach: true` for background containers or keep `rm: true` for ephemeral foreground runs."
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
        description = "A command in a container, with the filtering done here instead of in a pipe. Replaces `docker exec … | grep -v …`: `grep`, `exclude` and `max_lines` are parameters, so an installer's four hundred progress lines are never rendered and the one line that failed arrives with the exit status attached — which a pipeline throws away. Send the command as `argv` (a list, no quoting to get wrong) or, when you need a pipe or a redirect, as `shell`. No TTY is ever allocated: `-t` turns output into escape codes and `-i` waits for input that is not coming. `project`/`files` addresses a compose service instead of a container."
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
        description = "Any other docker or compose subcommand, as an argument list: `cp`, `top`, `port`, `history`, `network inspect`, `volume prune`, `buildx bake`, `swarm init` — the rest of both CLIs, which is far too much to describe one tool at a time. What you give up is the rendering; what you keep is the byte budget, the deadline, the flag-injection guard and two rules. Nothing may stream: `attach`, `wait`, `compose watch`, `logs -f`, `stats` without `--no-stream`, `events` without `--until` and `compose up` without `-d` are refused by name, with a pointer to the bounded form. Nothing destroys state by accident: every subcommand is classed read, mutate or destroy from its verb, and a destroy needs `confirm: true`. An unrecognised subcommand is assumed to mutate. Prefer the dedicated tool when there is one — it renders the answer instead of the CLI's table."
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
        };

        match dockerops_cli::passthrough(&dockerops_cli::SystemRunner, &request) {
            Ok(outcome) => Ok(with_structured(outcome.text.clone(), &outcome)),
            Err(err) => Ok(failed(err)),
        }
    }
}

#[rmcp::tool_handler]
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
