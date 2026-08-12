//! Protocol-level smoke test: drives a real MCP client against [`SgServer`] over an
//! in-memory duplex transport, exercising the actual `ServerHandler` trait methods
//! (`call_tool`, `get_prompt`, `read_resource`) — not the `dispatch_tool` router the
//! unit tests in `server.rs` call directly, which bypasses them.
//!
//! This is the regression guard for the rmcp 1.x -> 3.x migration: `call_tool`,
//! `get_prompt`, and `read_resource` now return MRTR-aware response enums
//! (`CallToolResponse`/`GetPromptResponse`/`ReadResourceResponse`), and only a real
//! `ServiceExt::serve()` round-trip exercises the `.into()` conversion at that
//! boundary. The client-side high-level API auto-unwraps MRTR and still hands back
//! the plain `CallToolResult`/`GetPromptResult`/`ReadResourceResult` types, so these
//! assertions read the same as the `dispatch_tool` unit tests.

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, GetPromptRequestParams, ReadResourceRequestParams};
use rmcp::service::RunningService;
use sendgrid_core::{ApiKey, RuntimeConfig};
use sendgrid_mcp::{McpServerConfig, SgServer};
use serde_json::{Map, json};

fn cfg() -> McpServerConfig {
    McpServerConfig {
        runtime: RuntimeConfig::new(ApiKey::new("SG.test.key")),
        include_legacy: false,
        expose_tags: vec![],
        expose_ops: vec![],
    }
}

/// Serve `SgServer` over one end of an in-memory duplex pipe and connect a plain
/// (no-op) client to the other end — a real `initialize`/`notifications/initialized`
/// handshake happens, unlike calling `dispatch_tool` directly.
async fn connect() -> RunningService<rmcp::RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        SgServer::new(cfg())
            .serve(server_transport)
            .await
            .expect("server should initialize")
            .waiting()
            .await
    });
    ().serve(client_transport)
        .await
        .expect("client should connect")
}

#[tokio::test]
async fn list_tools_over_real_transport() {
    let client = connect().await;
    let tools = client.list_tools(None).await.expect("tools/list");
    let names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    for want in [
        "search_operations",
        "describe_operation",
        "invoke_operation",
        "read_doc",
    ] {
        assert!(names.contains(&want), "missing meta-tool {want}");
    }
    client.cancel().await.expect("client should cancel");
}

#[tokio::test]
async fn call_tool_over_real_transport_returns_structured_content() {
    let client = connect().await;
    let mut args = Map::new();
    args.insert("query".into(), json!("send email"));
    let result = client
        .call_tool(CallToolRequestParams::new("search_operations").with_arguments(args))
        .await
        .expect("tools/call");
    assert_eq!(result.is_error, Some(false));
    assert!(
        result
            .structured_content
            .as_ref()
            .and_then(|v| v.get("results"))
            .is_some(),
        "expected structured_content.results, got {:?}",
        result.structured_content
    );
    client.cancel().await.expect("client should cancel");
}

#[tokio::test]
async fn read_resource_over_real_transport() {
    let client = connect().await;
    let result = client
        .read_resource(ReadResourceRequestParams::new(
            "sendgrid://skill/using-the-server",
        ))
        .await
        .expect("resources/read");
    assert_eq!(result.contents.len(), 1);
    client.cancel().await.expect("client should cancel");
}

#[tokio::test]
async fn read_resource_unknown_uri_is_protocol_error() {
    let client = connect().await;
    let err = client
        .read_resource(ReadResourceRequestParams::new("sendgrid://nope"))
        .await
        .expect_err("unknown uri should error");
    assert!(format!("{err:?}").contains("unknown resource uri"));
    client.cancel().await.expect("client should cancel");
}

#[tokio::test]
async fn get_prompt_over_real_transport() {
    let client = connect().await;
    let mut args = Map::new();
    args.insert("goal".into(), json!("send a test email"));
    let result = client
        .get_prompt(GetPromptRequestParams::new("find_operation").with_arguments(args))
        .await
        .expect("prompts/get");
    let text = result.messages[0]
        .content
        .as_text()
        .expect("expected text content");
    assert!(text.text.contains("send a test email"));
    client.cancel().await.expect("client should cancel");
}
