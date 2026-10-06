//! MCP adapter: exposes `dockerops-cli` over the Model Context Protocol.
//!
//! Thin by design. Everything here either describes a tool's parameters for the caller or
//! translates them into a `dockerops-cli` request; the rendering they are judged on lives
//! in the core crate, next to its tests.

use dockerops_cli::{
    ComposeScope, Invocation,
    images::ImagesRequest,
    inspect::{InspectRequest, Kind, Preset},
    logs::{Level, LogsRequest},
    ps::PsRequest,
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

Two habits make the difference:

1. **Batch.** One `logs` call naming four services, or one `inspect` naming four \
containers, costs one round trip. Four calls, or a shell command joining four invocations, \
cost far more for the same bytes.
2. **Filter in the call.** `level: \"error\"`, `grep`, a dotted `fields` path — all of it \
runs before the byte budget, so the budget is spent on the lines that answer the question \
rather than on startup noise. Every response is capped and says what it left out, so a \
reply that reports truncation is an instruction to narrow the request, not to retry it \
unchanged.

Nothing here follows a stream: there is no `-f`. A tool call that does not terminate takes \
the conversation with it, so the way to see what happened next is another call with \
`since`. A failure that belongs to one target is one line of output next to the targets \
that worked, never a failed call. Every tool is read-only.";

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

#[derive(Debug, Clone)]
pub struct DockerOpsServer {
    /// Server-wide default budget, from `--max-bytes`. A request may still name its own.
    default_max_bytes: Option<usize>,
    /// Docker binary, from `--docker-bin`, for a podman shim or an unusual install.
    docker_bin: Option<String>,
    /// Reserved for the lifecycle tools: with `--read-only` they refuse outright. Every
    /// tool that exists today is read-only regardless.
    #[allow(dead_code)]
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
