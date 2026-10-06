//! MCP adapter: exposes `dockerops-cli` over the Model Context Protocol.
//!
//! Thin by design. Everything here either describes a tool's parameters for the caller or
//! translates them into a `dockerops-cli` request; the rendering they are judged on lives
//! in the core crate, next to its tests.

use rmcp::{
    ServerHandler,
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool_router,
};

const INSTRUCTIONS: &str = "Docker and Docker Compose, without the context window.";

#[derive(Debug, Clone)]
pub struct DockerOpsServer {
    default_max_bytes: Option<usize>,
    docker_bin: Option<String>,
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
