//! A minimal stdio host for the MCP server, used to smoke-test the live transport
//! (initialize + tools/call) without the CLI. Boots with a well-formed dummy key so
//! the server starts; no real network is touched (the smoke uses pre-flight paths).
//!
//! Run: pipe newline-delimited JSON-RPC into
//! `cargo run -q --example stdio_smoke -p sendgrid-mcp`.

use sendgrid_core::{ApiKey, RuntimeConfig};
use sendgrid_mcp::{McpServerConfig, run_stdio};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A structurally well-formed SendGrid key (SG.<22>.<43>) so boot does not fail
    // closed with E_BAD_KEY_FORMAT.
    let key = "SG.0123456789abcdefghABCD.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let cfg = McpServerConfig {
        runtime: RuntimeConfig::new(ApiKey::new(key)),
        include_legacy: false,
        expose_tags: vec![],
        expose_ops: vec![],
    };
    run_stdio(cfg).await
}
