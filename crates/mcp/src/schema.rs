//! JSON-Schema (input_schema) construction for the tools we advertise.
//!
//! The 3 meta-tool schemas are fixed. Promoted (`--expose-*`) tools build their
//! schema from the op's `params` (bucketed flat, one property per param) plus a
//! `body` property when the op carries a request body.

use sendgrid_core::ir::OperationIr;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Wrap a JSON object value as rmcp's `Arc<Map>` input-schema type.
pub fn arc_object(v: Value) -> Arc<Map<String, Value>> {
    Arc::new(v.as_object().cloned().unwrap_or_default())
}

pub fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Free-text query, e.g. 'create a contact list' or 'send transactional email'." },
            "tags": { "type": "array", "items": { "type": "string" }, "description": "Restrict to ops carrying any of these OpenAPI tags." },
            "side_effect": { "type": "string", "enum": ["read", "write", "destructive", "send"], "description": "Restrict to one side-effect class." },
            "method": { "type": "string", "description": "Restrict to an HTTP method (GET/POST/PUT/PATCH/DELETE)." },
            "domain": { "type": "string", "description": "Restrict to a domain (e.g. 'marketing', 'mail', 'stats')." },
            "limit": { "type": "integer", "default": 20, "description": "Max hits to return (1-100, default 20)." }
        },
        "required": ["query"]
    })
}

pub fn describe_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Operation id (e.g. sg_mail_send_SendMail) or its alias." },
            "expand": {
                "type": "string",
                "enum": ["minimal", "full"],
                "default": "minimal",
                "description": "minimal = required fields + a compact body example (token-cheap). full = the complete request-body JSON Schema (can be large)."
            }
        },
        "required": ["id"]
    })
}

pub fn invoke_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Operation id (or alias) to invoke." },
            "path_params": { "type": "object", "description": "Path parameter values keyed by name (e.g. {\"id\": \"123\"})." },
            "query": { "type": "object", "description": "Query parameter values keyed by name." },
            "headers": { "type": "object", "description": "Header values keyed by name (on-behalf-of/authorization are ignored — set via server config)." },
            "body": { "description": "Request body JSON (object, or array for the few array-body ops)." },
            "dry_run": { "type": "boolean", "description": "If true, build + return a redacted request_preview without sending." },
            "confirm": { "type": "boolean", "description": "Acknowledgement only; NOT a security control and never bypasses policy." },
            "fields": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional jq-lite projection of the success result `data`: dotted paths to keep (e.g. [\"result.id\", \"result.name\"]). A path crossing an array projects each element. Trims a large response before it enters context. Does not affect errors/previews or secret handling."
            },
            "max_items": {
                "type": "integer",
                "description": "Optional cap on the result array length (the top-level array, or the op's paginated result array). When it truncates, a `truncated` note is added. Use with pagination for more."
            },
            "await": {
                "type": "boolean",
                "description": "Async Poll-class ops only: if true, submit then poll the companion status op to a terminal state (bounded). The result's `async` block names the job kind + next step; for non-Poll ops this is ignored."
            }
        },
        "required": ["id"]
    })
}

/// Map an IR JSON-schema type to a JSON-Schema `type` keyword.
fn json_type(ty: &str) -> &str {
    match ty {
        "string" | "integer" | "number" | "boolean" | "array" | "object" => ty,
        _ => "string",
    }
}

/// Build the input schema for a promoted (first-class) tool: each declared param
/// becomes a flat top-level property; a request body becomes a `body` property.
/// `dry_run` is offered for parity with `invoke_operation`.
pub fn promoted_schema(op: &OperationIr) -> Value {
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::new();

    for p in &op.params {
        let mut prop = Map::new();
        prop.insert("type".into(), json!(json_type(&p.ty)));
        if p.ty == "array" {
            let item = p.item_ty.as_deref().unwrap_or("string");
            prop.insert("items".into(), json!({ "type": json_type(item) }));
        }
        if let Some(desc) = &p.description {
            prop.insert("description".into(), json!(desc));
        }
        let mut prop = Value::Object(prop);
        // Note the param location so an agent knows where it routes.
        prop.as_object_mut().unwrap().insert(
            "x-in".into(),
            json!(serde_json::to_value(p.location).unwrap_or(Value::Null)),
        );
        props.insert(p.name.clone(), prop);
        if p.required {
            required.push(json!(p.name));
        }
    }

    if op.has_body {
        props.insert(
            "body".into(),
            json!({ "description": "Request body JSON. Use describe_operation for the full schema." }),
        );
    }
    props.insert(
        "dry_run".into(),
        json!({ "type": "boolean", "description": "If true, preview the request without sending." }),
    );
    // Output shaping is universally useful; advertise it on promoted tools too.
    props.insert(
        "fields".into(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "Optional jq-lite projection of the success result `data` (dotted paths; an array path projects each element)."
        }),
    );
    props.insert(
        "max_items".into(),
        json!({
            "type": "integer",
            "description": "Optional cap on the result array length (adds a `truncated` note when it truncates)."
        }),
    );
    // `await` is only meaningful for Poll-class ops — advertise it just there.
    if op.async_job == sendgrid_core::ir::AsyncJob::Poll {
        props.insert(
            "await".into(),
            json!({
                "type": "boolean",
                "description": "If true, submit then poll the companion status op to a terminal state (bounded)."
            }),
        );
    }

    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}
