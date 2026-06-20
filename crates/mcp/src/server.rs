//! The dynamic `ServerHandler`: a fixed 3-tool meta surface (+ optional promoted
//! tools), with names and input schemas built from runtime IR data. We override
//! `list_tools`/`call_tool` and validate the tool name ourselves (rmcp's default
//! `get_tool` returns `None`, bypassing its built-in validation).

use crate::{describe, invoke, schema, search};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
};
use sendgrid_core::{Registry, RuntimeConfig};
use serde_json::{Map, Value};
use std::sync::Arc;

use crate::McpServerConfig;

/// One promoted (`--expose-*`) operation, exposed as a first-class tool.
struct PromotedTool {
    /// Tool name == operation id.
    name: String,
    description: String,
    input_schema: Arc<Map<String, Value>>,
    /// Canonical op id used to resolve the op at call time.
    op_id: String,
}

/// The MCP server handler.
#[derive(Clone)]
pub struct SgServer {
    runtime: Arc<RuntimeConfig>,
    include_legacy: bool,
    promoted: Arc<Vec<PromotedTool>>,
}

impl SgServer {
    /// Build the handler from the CLI-provided config, resolving promoted tools.
    pub fn new(cfg: McpServerConfig) -> Self {
        let promoted = resolve_promoted(&cfg);
        SgServer {
            runtime: Arc::new(cfg.runtime),
            include_legacy: cfg.include_legacy,
            promoted: Arc::new(promoted),
        }
    }

    /// The names of the promoted tools (for tests/inspection).
    pub fn promoted_names(&self) -> Vec<String> {
        self.promoted.iter().map(|p| p.name.clone()).collect()
    }

    fn ok(v: Value) -> CallToolResult {
        CallToolResult::success(vec![Content::text(v.to_string())])
    }

    fn err(msg: String) -> CallToolResult {
        CallToolResult::error(vec![Content::text(msg)])
    }

    /// The advertised tool list: 3 meta-tools + any promoted tools.
    fn build_tools(&self) -> Vec<Tool> {
        let mut tools = vec![
            Tool::new(
                "search_operations",
                "Search the 391 SendGrid operations by keyword. Returns metadata-only hits \
                 (id, summary, method, path, side_effect, tags) ranked by relevance. START HERE.",
                schema::arc_object(schema::search_schema()),
            ),
            Tool::new(
                "describe_operation",
                "Describe one operation by id: params, required fields, a compact body example, \
                 and constraints. Use before invoke_operation. expand=full returns the entire schema.",
                schema::arc_object(schema::describe_schema()),
            ),
            Tool::new(
                "invoke_operation",
                "Invoke an operation by id with {path_params, query, headers, body}. Safety policy, \
                 validation, and secret redaction are enforced server-side. Use dry_run:true to preview.",
                schema::arc_object(schema::invoke_schema()),
            ),
        ];
        for p in self.promoted.iter() {
            tools.push(Tool::new(
                p.name.clone(),
                p.description.clone(),
                p.input_schema.clone(),
            ));
        }
        tools
    }
}

impl ServerHandler for SgServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(crate::instructions::INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.build_tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args: Map<String, Value> = request.arguments.unwrap_or_default();
        self.dispatch_tool(request.name.as_ref(), args).await
    }
}

impl SgServer {
    /// The router shared by `call_tool` (and exercised directly by tests, since
    /// constructing a `RequestContext` is impractical). Validates the tool name
    /// ourselves and routes to the matching meta-tool or promoted-tool handler.
    async fn dispatch_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        match name {
            "search_operations" => Ok(Self::ok(search::search_operations(
                &args,
                self.include_legacy,
            ))),
            "describe_operation" => match describe::describe_operation(&args) {
                Ok(v) => Ok(Self::ok(v)),
                Err(msg) => Ok(Self::err(msg)),
            },
            "invoke_operation" => match invoke::invoke_operation(&self.runtime, &args).await {
                invoke::InvokeOutcome::Envelope(v) => Ok(Self::ok(v)),
                invoke::InvokeOutcome::UsageError(msg) => Ok(Self::err(msg)),
            },
            other => {
                // Promoted (first-class) tool?
                if let Some(p) = self.promoted.iter().find(|p| p.name == other) {
                    let reg = Registry::global();
                    match reg.by_id(&p.op_id) {
                        Some(op) => {
                            let v = invoke::invoke_promoted(&self.runtime, op, &args).await;
                            Ok(Self::ok(v))
                        }
                        None => Ok(Self::err(format!(
                            "internal: promoted op `{}` not found in registry",
                            p.op_id
                        ))),
                    }
                } else {
                    Err(McpError::invalid_params(
                        format!("unknown tool: {other}"),
                        None,
                    ))
                }
            }
        }
    }
}

/// Resolve the set of ops to promote from `expose_ops` (explicit ids/aliases —
/// always honored) and `expose_tags` (tag match — hidden ops included only when
/// `include_legacy`). Deduplicated by canonical op id, output in id order.
fn resolve_promoted(cfg: &McpServerConfig) -> Vec<PromotedTool> {
    let reg = Registry::global();
    let mut selected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Explicit op ids (resolve aliases to the canonical id). Always honored.
    for id in &cfg.expose_ops {
        if let Some(op) = reg.by_id(id) {
            selected.insert(op.id.clone());
        }
    }
    // Tag matches.
    if !cfg.expose_tags.is_empty() {
        let want: Vec<String> = cfg
            .expose_tags
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        for op in reg.operations() {
            if op.hidden && !cfg.include_legacy {
                continue;
            }
            let has = op
                .tags
                .iter()
                .any(|t| want.iter().any(|w| w == &t.to_ascii_lowercase()));
            if has {
                selected.insert(op.id.clone());
            }
        }
    }

    selected
        .into_iter()
        .filter_map(|id| reg.by_id(&id))
        .map(|op| PromotedTool {
            name: op.id.clone(),
            description: format!(
                "{} [{} {}] side_effect={:?}. First-class alias of invoke_operation for {}.",
                op.summary.as_deref().unwrap_or("(no summary)"),
                op.method,
                op.path,
                op.side_effect,
                op.id
            ),
            input_schema: schema::arc_object(schema::promoted_schema(op)),
            op_id: op.id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendgrid_core::ApiKey;
    use serde_json::json;

    fn cfg(
        expose_ops: Vec<String>,
        expose_tags: Vec<String>,
        include_legacy: bool,
    ) -> McpServerConfig {
        McpServerConfig {
            runtime: RuntimeConfig::new(ApiKey::new("SG.test.key")),
            include_legacy,
            expose_tags,
            expose_ops,
        }
    }

    #[test]
    fn default_surface_is_exactly_three_tools() {
        let s = SgServer::new(cfg(vec![], vec![], false));
        // build_tools mirrors list_tools without needing a RequestContext.
        let tools = s.build_tools();
        assert_eq!(tools.len(), 3, "default surface must be the 3 meta-tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains(&"search_operations"));
        assert!(names.contains(&"describe_operation"));
        assert!(names.contains(&"invoke_operation"));
    }

    #[test]
    fn expose_op_promotes_first_class_tool() {
        let s = SgServer::new(cfg(vec!["sg_mail_send_SendMail".into()], vec![], false));
        let tools = s.build_tools();
        assert_eq!(tools.len(), 4);
        assert!(
            tools
                .iter()
                .any(|t| t.name.as_ref() == "sg_mail_send_SendMail")
        );
        // Promoted tool advertises side-effect info in its description.
        let promoted = tools
            .iter()
            .find(|t| t.name.as_ref() == "sg_mail_send_SendMail")
            .unwrap();
        let desc = promoted.description.as_deref().unwrap_or_default();
        assert!(desc.contains("Send"), "side-effect not advertised: {desc}");
    }

    #[test]
    fn expose_tags_promote_matching_ops() {
        let s = SgServer::new(cfg(vec![], vec!["Mail Send".into()], false));
        let names = s.promoted_names();
        assert!(names.iter().any(|n| n == "sg_mail_send_SendMail"));
    }

    #[test]
    fn unknown_expose_op_is_ignored() {
        let s = SgServer::new(cfg(vec!["sg_nope_nope_Nope".into()], vec![], false));
        assert_eq!(s.build_tools().len(), 3);
    }

    /// Extract the JSON the handler put into a `CallToolResult`'s text content.
    fn result_json(res: &CallToolResult) -> Value {
        let v = serde_json::to_value(res).unwrap();
        let text = v["content"][0]["text"].as_str().expect("text content");
        serde_json::from_str(text).expect("content is JSON")
    }

    #[tokio::test]
    async fn router_dispatches_each_meta_tool() {
        let s = SgServer::new(cfg(vec![], vec![], false));

        // search_operations
        let mut a = Map::new();
        a.insert("query".into(), json!("send email"));
        let r = s.dispatch_tool("search_operations", a).await.unwrap();
        assert!(result_json(&r)["count"].as_u64().unwrap() > 0);

        // describe_operation
        let mut a = Map::new();
        a.insert("id".into(), json!("sg_mail_send_SendMail"));
        let r = s.dispatch_tool("describe_operation", a).await.unwrap();
        assert_eq!(result_json(&r)["id"], json!("sg_mail_send_SendMail"));

        // unknown tool → protocol error
        assert!(s.dispatch_tool("no_such_tool", Map::new()).await.is_err());
    }

    /// End-to-end proof that `describe` minimal's synthesized example is not just
    /// shaped right but is *valid*: round-trip it back through `invoke` (dry_run)
    /// and confirm execute()'s own validator accepts it (status 0, not E_VALIDATION).
    #[tokio::test]
    async fn synthesized_example_round_trips_through_invoke() {
        let s = SgServer::new(cfg(vec![], vec![], false));

        let mut a = Map::new();
        a.insert("id".into(), json!("sg_mail_send_SendMail"));
        a.insert("expand".into(), json!("minimal"));
        let desc = result_json(&s.dispatch_tool("describe_operation", a).await.unwrap());
        let example = desc["body"]["example"].clone();
        assert!(example.is_object(), "expected a synthesized example object");

        let mut a = Map::new();
        a.insert("id".into(), json!("sg_mail_send_SendMail"));
        a.insert("dry_run".into(), json!(true));
        a.insert("body".into(), example);
        let env = result_json(&s.dispatch_tool("invoke_operation", a).await.unwrap());
        assert_eq!(
            env["status"],
            json!(0),
            "synthesized example should pass validation, got {env}"
        );
        assert!(env["request_preview"].is_object());
    }

    #[tokio::test]
    async fn router_routes_promoted_tool_to_invoke() {
        // Promote a GET-by-id op; calling its first-class tool should bucket the
        // path param and (dry-run) preview the substituted URL.
        let s = SgServer::new(cfg(vec!["sg_templates_GetTemplate".into()], vec![], false));
        let reg = Registry::global();
        let op = reg.by_id("sg_templates_GetTemplate").unwrap();
        let path_param = op
            .params
            .iter()
            .find(|p| p.location == sendgrid_core::ir::Location::Path)
            .unwrap();

        let mut a = Map::new();
        a.insert(path_param.name.clone(), json!("tmpl_999"));
        a.insert("dry_run".into(), json!(true));
        let env = result_json(
            &s.dispatch_tool("sg_templates_GetTemplate", a)
                .await
                .unwrap(),
        );
        assert_eq!(env["status"], json!(0));
        assert!(
            env["request_preview"]["url"]
                .as_str()
                .unwrap_or_default()
                .contains("tmpl_999"),
            "promoted tool did not route path param: {env}"
        );
    }
}
