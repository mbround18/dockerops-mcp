//! Docker and Docker Compose, read and driven without spending a context window on it.
//!
//! The Docker CLI is built for a terminal that is 200 columns wide and a human who will
//! read one answer. `docker ps` spends a line of header and seven padded columns on
//! facts that fit in forty characters; `docker inspect` answers a yes/no question with
//! four hundred lines of JSON; `docker compose logs` is unbounded by construction. The
//! usual repair is a shell chain — `docker compose logs api 2>&1 | grep -i error | tail
//! -40` — which costs a round trip to write, a round trip to fix, and sends the output
//! through two more processes before anyone reads it.
//!
//! So: one call per question, batched over targets, rendered to the shortest text that
//! still answers, under a byte ceiling that always reports what it cut.
//!
//! * [`ps::ps`] — what is running, one line each, containers and compose services alike.
//! * [`logs::logs`] — log tails from several targets at once, filtered, deduplicated.
//! * [`inspect::inspect`] — the fields of `docker inspect` you asked for, not the document.
//! * [`stats::stats`] — a resource snapshot that terminates, unlike `docker stats`.
//! * [`images::images`] — what is on disk, largest first.
//! * [`usage::usage`] — `docker system df`, including what is reclaimable.
//! * [`config::config`] — a compose project resolved: services, images, ports, volumes.
//! * [`events::events`] — a bounded window of the daemon's event stream.
//! * [`compose::compose`] — the lifecycle verbs, summarised rather than streamed.
//! * [`exec::exec`] — a command in a container, with its output filtered like a log.
//! * [`passthrough::passthrough`] — any other docker or compose subcommand, under the
//!   same budget and the same rules. The long tail of the CLI without a tool schema each.
//!
//! The architecture is the one the sibling servers use: this crate holds all the logic
//! *and* all the rendering, and reaches the outside world through one port,
//! [`runner::CommandRunner`]. The MCP adapter in `apps/dockerops-mcp` only describes
//! parameters and translates them. Nothing here needs a daemon to be tested.

pub mod budget;
pub mod compose;
pub mod config;
pub mod container;
pub mod events;
pub mod exec;
pub mod images;
pub mod inspect;
pub mod invoke;
pub mod logs;
pub mod passthrough;
pub mod ps;
pub mod run;
pub mod runner;
pub mod stats;
pub mod text;
pub mod usage;
pub mod verify_startup;

pub use budget::{Budget, DEFAULT_MAX_BYTES, MAX_MAX_BYTES};
pub use compose::{ComposeOutcome, ComposeRequest, Verb, compose};
pub use config::{ConfigOutcome, ConfigRequest, config};
pub use container::{Container, Port, State, Status};
pub use events::{EventsOutcome, EventsRequest, events};
pub use exec::{ExecOutcome, ExecRequest, exec};
pub use images::{ImagesOutcome, ImagesRequest, images};
pub use inspect::{InspectOutcome, InspectRequest, inspect};
pub use invoke::{ComposeScope, Invocation};
pub use logs::{Level, LogsOutcome, LogsRequest, logs};
pub use passthrough::{Class, PassthroughOutcome, PassthroughRequest, passthrough};
pub use ps::{PsOutcome, PsRequest, ps};
pub use run::{RunOutcome, RunRequest, run};
pub use runner::{CommandOutput, CommandRunner, ScriptedRunner, SystemRunner};
pub use stats::{StatsOutcome, StatsRequest, stats};
pub use usage::{UsageOutcome, UsageRequest, usage};
pub use verify_startup::{VerifyImageResult, VerifyStartupOutcome, VerifyStartupRequest, verify_startup};

/// What can go wrong before any container is touched.
///
/// Everything in here is the caller's request being unusable. A failure that belongs to a
/// *target* — no such container, a service that is not running, a daemon that refused —
/// is never an `Error`: it is one line in the output next to the targets that worked.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request cannot be interpreted. The message says what to send instead.
    #[error("{detail}")]
    InvalidRequest { detail: String },

    /// The binary could not be spawned at all.
    #[error("`{bin}` could not be run ({detail}); install Docker or set `docker_bin`")]
    DockerMissing { bin: String, detail: String },

    /// The request asks for something this server will not do, whatever the arguments.
    #[error("refused: {detail}")]
    Refused { detail: String },

    /// The request would change or destroy state and did not say so out loud.
    #[error("`{verb}` {effect}. Re-send with `confirm: true` if that is what you want. {hint}")]
    NeedsConfirmation {
        verb: String,
        effect: String,
        hint: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// A request error, for the common case of a sentence.
    pub fn request(detail: impl Into<String>) -> Self {
        Self::InvalidRequest {
            detail: detail.into(),
        }
    }

    /// A refusal, for a request this server will not carry out in any form.
    pub fn refused(detail: impl Into<String>) -> Self {
        Self::Refused {
            detail: detail.into(),
        }
    }
}
