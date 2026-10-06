//! stdio entry point for the dockerops MCP server.

mod server;

use anyhow::Result;
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

/// Token-efficient Docker and Docker Compose operations over the Model Context Protocol.
///
/// Speaks MCP on stdin/stdout, so it is normally launched by an MCP client rather than run
/// by hand. Register it with:
/// `claude mcp add --scope user dockerops dockerops-mcp`
#[derive(Debug, Parser)]
#[command(name = "dockerops-mcp", version)]
struct Cli {
    /// Log filter for diagnostics on stderr, in `tracing` syntax (e.g. `debug`).
    #[arg(long, env = "DOCKEROPS_MCP_LOG", default_value = "info")]
    log: String,

    /// Default ceiling, in bytes, on a single response's rendered text. A request may
    /// raise or lower it; it cannot remove it.
    #[arg(long, env = "DOCKEROPS_MCP_MAX_BYTES")]
    max_bytes: Option<usize>,

    /// Binary to invoke. Set this to `podman` for a Docker-compatible CLI elsewhere.
    #[arg(long, env = "DOCKEROPS_MCP_DOCKER_BIN")]
    docker_bin: Option<String>,

    /// Refuse every tool that changes state (`compose`, `run`, `exec`, and mutating
    /// passthrough calls), leaving only read-only tools. For a session that should be
    /// able to look at containers and nothing else.
    #[arg(long, env = "DOCKEROPS_MCP_READ_ONLY")]
    read_only: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // stdout carries the protocol; diagnostics go to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        read_only = cli.read_only,
        "starting dockerops-mcp"
    );
    let service = server::DockerOpsServer::new(cli.max_bytes, cli.docker_bin, cli.read_only)
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
