//! `sendgrid-mcp` — the rmcp-based MCP server. The core surface is the
//! search_operations → describe_operation → invoke_operation workflow over the IR,
//! plus a `read_doc` tool and MCP resources (an on-demand usage skill + reference
//! docs) and prompts. Tools carry behavior annotations and return structured content.
//!
//! The CLI hosts this via `sendgrid mcp`, so the public entrypoint below is a
//! **frozen contract** the CLI depends on. P3 fills in the implementation; the
//! signature must remain stable.
//!
//! ## Design (proven by spike-c)
//! The `#[tool]` derive macros need one compile-time symbol per tool, which is
//! unusable for a data-driven surface over 391 runtime ops. Instead we
//! hand-implement [`rmcp::ServerHandler`] (see [`server::SgServer`]) and build the
//! tool list + dispatch entirely from registry data, served over stdio.

mod describe;
mod docs;
mod instructions;
mod invoke;
mod prompts;
mod schema;
mod search;
mod server;
mod shape;
mod text;

use rmcp::{ServiceExt, transport::stdio};
use sendgrid_core::RuntimeConfig;

pub use server::SgServer;

/// Configuration for the stdio MCP server, built by the CLI from flags/env.
pub struct McpServerConfig {
    /// Credentials, region, policy, etc. — the same runtime config the CLI uses.
    pub runtime: RuntimeConfig,
    /// Include legacy/hidden operations in search + tool exposure.
    pub include_legacy: bool,
    /// Promote operations carrying these tags to first-class MCP tools.
    pub expose_tags: Vec<String>,
    /// Promote these specific operation ids to first-class MCP tools.
    pub expose_ops: Vec<String>,
}

/// Run the MCP server over stdio until the client disconnects.
pub async fn run_stdio(cfg: McpServerConfig) -> anyhow::Result<()> {
    let server = SgServer::new(cfg);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
