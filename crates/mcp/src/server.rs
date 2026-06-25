//! The dynamic `ServerHandler`: a fixed meta-tool surface (search/describe/invoke +
//! read_doc, plus resources & prompts) and optional promoted tools, with names and
//! input schemas built from runtime IR data. We override `list_tools`/`call_tool` and
//! validate the tool name ourselves (rmcp's default `get_tool` returns `None`,
//! bypassing its built-in validation).

use crate::{describe, docs, invoke, prompts, schema, search};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Content, GetPromptRequestParams, GetPromptResult,
        Implementation, ListPromptsResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, ServerCapabilities,
        ServerInfo, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use sendgrid_core::ir::{OperationIr, SideEffect};
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
    /// Behavior hints derived from the op's side-effect class + HTTP method.
    annotations: ToolAnnotations,
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

    /// A successful JSON result, returned as both `structured_content` (for modern
    /// clients that parse/validate) and stringified text (for text-only clients).
    fn ok(v: Value) -> CallToolResult {
        CallToolResult::structured(v)
    }

    /// A usage error (a plain message, not a JSON envelope): text content, `isError`.
    fn err(msg: String) -> CallToolResult {
        CallToolResult::error(vec![Content::text(msg)])
    }

    /// Build a tool result from an `invoke` result envelope, flagging `isError`
    /// when the envelope encodes a failure. The full envelope body is preserved
    /// either way (success OR error) so the agent can read the detail — only the
    /// MCP-level `isError` bit changes, so a convention-following agent no longer
    /// reads a denied/failed call as success.
    fn from_envelope(v: Value) -> CallToolResult {
        if envelope_is_error(&v) {
            CallToolResult::structured_error(v)
        } else {
            CallToolResult::structured(v)
        }
    }

    /// The advertised tool list: the meta-tools + any promoted tools. Each tool carries
    /// MCP `annotations` (behavior hints) and, where the shape is stable, an
    /// `output_schema` declaring the structured result.
    fn build_tools(&self) -> Vec<Tool> {
        let mut tools = vec![
            Tool::new(
                "search_operations",
                "Search the 391 SendGrid operations by keyword. Returns metadata-only hits \
                 (id, summary, method, path, side_effect, tags) ranked by relevance. START HERE.",
                schema::arc_object(schema::search_schema()),
            )
            .with_annotations(read_only_annot())
            .with_raw_output_schema(schema::arc_object(schema::search_output_schema())),
            Tool::new(
                "describe_operation",
                "Describe one operation by id: params, required fields, a compact body example, \
                 cross-field constraints, and a compact response field-menu for chaining. Use \
                 before invoke_operation. expand=full returns the entire request + response schema.",
                schema::arc_object(schema::describe_schema()),
            )
            .with_annotations(read_only_annot()),
            Tool::new(
                "invoke_operation",
                "Invoke an operation by id with {path_params, query, headers, body}. Optional: \
                 dry_run (preview), fields/max_items (trim the result), await (poll async jobs). \
                 Safety policy, validation, and secret redaction are enforced server-side; isError \
                 reflects failures.",
                schema::arc_object(schema::invoke_schema()),
            )
            // Polymorphic dispatcher (GET..DELETE depending on `id`): no static
            // read-only/destructive hint would be correct — only the open-world hint is.
            .with_annotations(ToolAnnotations::new().open_world(true))
            .with_raw_output_schema(schema::arc_object(schema::invoke_output_schema())),
            Tool::new(
                "read_doc",
                "Read this server's docs: the `using-the-server` skill and reference docs \
                 (side-effects, regions, async-jobs). Call with no args to list them, or pass \
                 {uri} to read one. Same content as the MCP resources.",
                schema::arc_object(schema::read_doc_schema()),
            )
            .with_annotations(ToolAnnotations::new().read_only(true).open_world(false)),
        ];
        for p in self.promoted.iter() {
            tools.push(
                Tool::new(
                    p.name.clone(),
                    p.description.clone(),
                    p.input_schema.clone(),
                )
                .with_annotations(p.annotations.clone()),
            );
        }
        tools
    }
}

/// Hints for a read-only meta-tool that doesn't touch the outside world directly.
fn read_only_annot() -> ToolAnnotations {
    ToolAnnotations::new().read_only(true)
}

/// Derive a promoted tool's behavior hints from its side-effect class + HTTP method.
/// Each promoted tool is 1:1 with a known operation, so (unlike `invoke_operation`) a
/// static hint is correct here.
fn promoted_annotations(op: &OperationIr) -> ToolAnnotations {
    let a = ToolAnnotations::new().open_world(true);
    // GET/PUT/DELETE/HEAD are idempotent by HTTP semantics; POST/PATCH are not.
    let idempotent = matches!(op.method.as_str(), "GET" | "PUT" | "DELETE" | "HEAD");
    match &op.side_effect {
        SideEffect::Read => a.read_only(true),
        SideEffect::Write => a.read_only(false).destructive(false).idempotent(idempotent),
        SideEffect::Destructive => a.read_only(false).destructive(true).idempotent(idempotent),
        // `send` emits to the outside world but isn't "destructive" of existing state.
        SideEffect::Send => a.read_only(false).destructive(false).idempotent(false),
    }
}

impl ServerHandler for SgServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        // Identify as `sendgrid` (not rmcp's build-env default `sendgrid_mcp`),
        // with this crate's real version, so clients display a stable identity.
        .with_server_info(Implementation::new("sendgrid", env!("CARGO_PKG_VERSION")))
        .with_instructions(crate::instructions::INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.build_tools()))
    }

    // ---- Resources: the skill + reference docs (see `docs.rs`) ----

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(docs::list()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        docs::read(&request.uri).ok_or_else(|| {
            McpError::invalid_params(format!("unknown resource uri: {}", request.uri), None)
        })
    }

    // ---- Prompts: user-invokable workflow templates (see `prompts.rs`) ----

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult::with_all_items(prompts::list()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        let args = request.arguments.unwrap_or_default();
        prompts::get(&request.name, &args).map_err(|msg| McpError::invalid_params(msg, None))
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
                invoke::InvokeOutcome::Envelope(v) => Ok(Self::from_envelope(v)),
                invoke::InvokeOutcome::UsageError(msg) => Ok(Self::err(msg)),
            },
            "read_doc" => {
                let uri = args.get("uri").and_then(Value::as_str);
                match docs::read_doc(uri) {
                    Ok(body) => Ok(CallToolResult::success(vec![Content::text(body)])),
                    Err(msg) => Ok(Self::err(msg)),
                }
            }
            other => {
                // Promoted (first-class) tool?
                if let Some(p) = self.promoted.iter().find(|p| p.name == other) {
                    let reg = Registry::global();
                    match reg.by_id(&p.op_id) {
                        Some(op) => {
                            let v = invoke::invoke_promoted(&self.runtime, op, &args).await;
                            Ok(Self::from_envelope(v))
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
            annotations: promoted_annotations(op),
            op_id: op.id.clone(),
        })
        .collect()
}

/// True when an `invoke` result envelope encodes a failure (so the tool result
/// should carry `isError:true`). The canonical signal is a top-level `error`
/// payload (the `ExecuteResult` `Payload::Error` variant; success body fields nest
/// under `data`, so this never false-positives). A stable `E_*` `code` and an HTTP
/// failure `status` (>=400) are redundant-but-defensive signals — and the await
/// path injects a synthetic `code` for a 2xx poll whose terminal job status is a
/// failure, which this then catches.
fn envelope_is_error(v: &Value) -> bool {
    v.get("error").is_some()
        || v.get("code").and_then(Value::as_str).is_some()
        || v.get("status")
            .and_then(Value::as_u64)
            .is_some_and(|s| s >= 400)
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

    /// The default surface is the 4 meta-tools (search/describe/invoke + read_doc).
    #[test]
    fn default_surface_is_the_four_meta_tools() {
        let s = SgServer::new(cfg(vec![], vec![], false));
        // build_tools mirrors list_tools without needing a RequestContext.
        let tools = s.build_tools();
        assert_eq!(tools.len(), 4, "default surface must be the 4 meta-tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for want in [
            "search_operations",
            "describe_operation",
            "invoke_operation",
            "read_doc",
        ] {
            assert!(names.contains(&want), "missing meta-tool {want}");
        }
    }

    /// Annotations map from each tool's nature: search/describe/read_doc are read-only;
    /// invoke_operation carries NO static read-only/destructive hint (it's a polymorphic
    /// dispatcher) — only the open-world hint. search/invoke declare an output schema.
    #[test]
    fn meta_tool_annotations_and_output_schemas() {
        let s = SgServer::new(cfg(vec![], vec![], false));
        let tools = s.build_tools();
        let by = |n: &str| tools.iter().find(|t| t.name.as_ref() == n).unwrap().clone();

        assert_eq!(
            by("search_operations").annotations.unwrap().read_only_hint,
            Some(true)
        );
        let read_doc = by("read_doc").annotations.unwrap();
        assert_eq!(read_doc.read_only_hint, Some(true));
        assert_eq!(read_doc.open_world_hint, Some(false));

        let invoke = by("invoke_operation").annotations.unwrap();
        assert_eq!(
            invoke.read_only_hint, None,
            "invoke is polymorphic — no static read-only hint"
        );
        assert_eq!(invoke.destructive_hint, None);
        assert_eq!(invoke.open_world_hint, Some(true));

        assert!(by("search_operations").output_schema.is_some());
        assert!(by("invoke_operation").output_schema.is_some());
        assert!(by("describe_operation").output_schema.is_none());
    }

    #[tokio::test]
    async fn read_doc_dispatches_to_skill_and_lists() {
        let s = SgServer::new(cfg(vec![], vec![], false));

        // With a uri → that doc's markdown body.
        let mut a = Map::new();
        a.insert("uri".into(), json!("sendgrid://skill/using-the-server"));
        let r = s.dispatch_tool("read_doc", a).await.unwrap();
        let text = serde_json::to_value(&r).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("search \u{2192} describe \u{2192} invoke"));

        // With no uri → an index naming the skill.
        let r = s.dispatch_tool("read_doc", Map::new()).await.unwrap();
        let text = serde_json::to_value(&r).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(text.contains("sendgrid://skill/using-the-server"));
    }

    #[tokio::test]
    async fn search_result_carries_structured_content() {
        let s = SgServer::new(cfg(vec![], vec![], false));
        let mut a = Map::new();
        a.insert("query".into(), json!("send email"));
        let r = s.dispatch_tool("search_operations", a).await.unwrap();
        assert!(
            r.structured_content
                .as_ref()
                .and_then(|v| v.get("results"))
                .is_some(),
            "search result should carry structured_content"
        );
    }

    #[test]
    fn expose_op_promotes_first_class_tool() {
        let s = SgServer::new(cfg(vec!["sg_mail_send_SendMail".into()], vec![], false));
        let tools = s.build_tools();
        assert_eq!(tools.len(), 5, "4 meta-tools + 1 promoted");
        let promoted = tools
            .iter()
            .find(|t| t.name.as_ref() == "sg_mail_send_SendMail")
            .expect("promoted tool present");
        // Promoted tool advertises side-effect info in its description.
        let desc = promoted.description.as_deref().unwrap_or_default();
        assert!(desc.contains("Send"), "side-effect not advertised: {desc}");
        // A `send` op is not read-only and not destructive.
        let ann = promoted.annotations.clone().unwrap();
        assert_eq!(ann.read_only_hint, Some(false));
        assert_eq!(ann.destructive_hint, Some(false));
    }

    /// A promoted DELETE op carries a destructive + idempotent hint (annotations per op).
    #[test]
    fn promoted_delete_is_annotated_destructive() {
        let reg = Registry::global();
        let delete_id = reg
            .operations()
            .iter()
            .find(|o| o.side_effect == SideEffect::Destructive && o.method == "DELETE" && !o.hidden)
            .map(|o| o.id.clone())
            .expect("a destructive DELETE op exists");
        let s = SgServer::new(cfg(vec![delete_id.clone()], vec![], false));
        let ann = s
            .build_tools()
            .iter()
            .find(|t| t.name.as_ref() == delete_id)
            .unwrap()
            .annotations
            .clone()
            .unwrap();
        assert_eq!(ann.read_only_hint, Some(false));
        assert_eq!(ann.destructive_hint, Some(true));
        assert_eq!(ann.idempotent_hint, Some(true), "DELETE is idempotent");
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
        assert_eq!(s.build_tools().len(), 4);
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
    async fn invoke_error_envelope_sets_is_error() {
        // A policy-denied invoke (E_POLICY_DENIED, offline pre-flight) must surface
        // as isError:true so a convention-following agent doesn't read it as success,
        // while the envelope body is preserved as content.
        let mut c = cfg(vec![], vec![], false);
        c.runtime.policy = sendgrid_core::Policy::read_only();
        let s = SgServer::new(c);

        let mut a = Map::new();
        a.insert("id".into(), json!("sg_mail_send_SendMail"));
        a.insert(
            "body".into(),
            json!({
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ]
            }),
        );
        let r = s.dispatch_tool("invoke_operation", a).await.unwrap();
        assert_eq!(r.is_error, Some(true), "denied call must be isError:true");
        // Body intact: the envelope (with its code) is still the content.
        assert_eq!(result_json(&r)["code"], json!("E_POLICY_DENIED"));
    }

    #[tokio::test]
    async fn invoke_success_is_not_is_error() {
        // A successful dry-run invoke must be isError:false (not flagged as an error).
        let s = SgServer::new(cfg(vec![], vec![], false));
        let mut a = Map::new();
        a.insert("id".into(), json!("sg_mail_send_SendMail"));
        a.insert("dry_run".into(), json!(true));
        a.insert(
            "body".into(),
            json!({
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ]
            }),
        );
        let r = s.dispatch_tool("invoke_operation", a).await.unwrap();
        assert_eq!(
            r.is_error,
            Some(false),
            "successful dry-run is not an error"
        );
        assert_eq!(result_json(&r)["status"], json!(0));
    }

    #[test]
    fn envelope_is_error_classifies_envelopes() {
        // Unit-cover the discriminator used by from_envelope.
        assert!(envelope_is_error(
            &json!({ "error": { "code": "X" }, "code": "X" })
        ));
        assert!(envelope_is_error(&json!({ "status": 404, "error": {} })));
        assert!(envelope_is_error(
            &json!({ "status": 200, "code": "E_ASYNC_JOB_FAILED", "data": {} })
        ));
        assert!(!envelope_is_error(
            &json!({ "status": 200, "data": { "ok": true } })
        ));
        assert!(!envelope_is_error(
            &json!({ "status": 0, "data": { "dry_run": true } })
        ));
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
