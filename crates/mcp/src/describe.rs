//! `describe_operation` — turn one op into an agent-actionable description.
//!
//! `minimal` (default) is deliberately token-bounded: metadata + params + the
//! top-level body field menu (name→type) + a **synthesized compact example**
//! (required chains only, recipient/email placeholders) + cross-field constraint
//! notes derived from the schema. It never dumps the full body schema (SendMail's
//! is ~22 KB / ~5k tokens).
//!
//! `full` adds the complete resolved request-body JSON Schema for callers that
//! explicitly opt into the cost.

use crate::text::truncate;
use sendgrid_core::Registry;
use sendgrid_core::ir::{Location, OperationIr};
use serde_json::{Map, Value, json};

const DESC_TRUNCATE: usize = 140;
/// Depth cap on the synthesized example / constraint walk (keeps tokens bounded).
const MAX_DEPTH: u32 = 6;
const MAX_CONSTRAINTS: usize = 12;

/// Run `describe_operation`. Returns `Ok(body)` or `Err(message)` for an unknown id.
pub fn describe_operation(args: &Map<String, Value>) -> Result<Value, String> {
    let reg = Registry::global();
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "describe_operation requires a non-empty `id`".to_string())?;
    let expand = args
        .get("expand")
        .and_then(Value::as_str)
        .unwrap_or("minimal");

    let op = reg.by_id(id).ok_or_else(|| {
        format!("unknown operation id `{id}`. Use search_operations to find a valid id.")
    })?;

    let mut out = Map::new();
    out.insert("id".into(), json!(op.id));
    if let Some(alias) = &op.id_alias {
        out.insert("id_alias".into(), json!(alias));
    }
    out.insert("operation_id".into(), json!(op.operation_id));
    out.insert("domain".into(), json!(op.domain));
    out.insert("subgroup".into(), json!(op.subgroup));
    out.insert("method".into(), json!(op.method));
    out.insert("path".into(), json!(op.path));
    if let Some(s) = &op.summary {
        out.insert("summary".into(), json!(s));
    }
    out.insert("side_effect".into(), json!(op.side_effect));
    out.insert("hidden".into(), json!(op.hidden));
    if !matches!(op.pagination.kind, sendgrid_core::ir::PaginationKind::None) {
        out.insert("pagination".into(), json!(op.pagination.kind));
    }
    out.insert("params".into(), Value::Array(params_json(op)));
    out.insert("invoke_hint".into(), json!(invoke_hint(op)));

    if op.has_body {
        let schema = reg.schema_for(op);
        match expand {
            "full" => {
                out.insert(
                    "request_body_schema".into(),
                    schema.cloned().unwrap_or(Value::Null),
                );
                out.insert("body_is_array".into(), json!(op.body_is_array));
            }
            _ => {
                if let Some(schema) = schema {
                    out.insert("body".into(), minimal_body(schema, op.body_is_array));
                } else {
                    out.insert(
                        "body".into(),
                        json!({ "note": "operation takes a body but no schema is embedded" }),
                    );
                }
            }
        }
    }

    Ok(Value::Object(out))
}

/// Compact per-param descriptors: `{name, in, required, type, format?, description?}`.
fn params_json(op: &OperationIr) -> Vec<Value> {
    op.params
        .iter()
        .map(|p| {
            let mut m = Map::new();
            m.insert("name".into(), json!(p.name));
            m.insert(
                "in".into(),
                json!(serde_json::to_value(p.location).unwrap_or(Value::Null)),
            );
            m.insert("required".into(), json!(p.required));
            m.insert("type".into(), json!(p.ty));
            if let Some(f) = &p.format {
                m.insert("format".into(), json!(f));
            }
            if let Some(d) = &p.description {
                m.insert("description".into(), json!(truncate(d, DESC_TRUNCATE)));
            }
            Value::Object(m)
        })
        .collect()
}

/// A one-line "how to invoke" hint tailored to the op's shape.
fn invoke_hint(op: &OperationIr) -> String {
    let mut parts = vec![format!("\"id\": \"{}\"", op.id)];
    if op.params.iter().any(|p| p.location == Location::Path) {
        parts.push("\"path_params\": {…}".into());
    }
    if op.params.iter().any(|p| p.location == Location::Query) {
        parts.push("\"query\": {…}".into());
    }
    if op.has_body {
        parts.push("\"body\": {…}".into());
    }
    format!(
        "invoke_operation {{ {} }} — side_effect={:?}; add \"dry_run\": true to preview.",
        parts.join(", "),
        op.side_effect
    )
}

/// The token-bounded body block: required field names, the top-level field menu,
/// a synthesized example, and cross-field constraint notes.
fn minimal_body(schema: &Value, body_is_array: bool) -> Value {
    // For an array body, describe the element schema.
    let element = if body_is_array {
        schema.get("items").unwrap_or(schema)
    } else {
        schema
    };

    let mut m = Map::new();
    if body_is_array {
        m.insert("is_array".into(), json!(true));
    }

    let required: Vec<&str> = element
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !required.is_empty() {
        m.insert("required".into(), json!(required));
    }

    // Top-level field menu (name → type), so the agent sees ALL available fields
    // (e.g. SendMail's `subject`, `content`, `template_id`) without the full schema.
    if let Some(props) = element.get("properties").and_then(Value::as_object) {
        let mut fields = Map::new();
        for (name, sub) in props {
            fields.insert(name.clone(), json!(type_label(sub)));
        }
        m.insert("fields".into(), Value::Object(fields));
    }

    let example = synth_example(element, 0);
    let example = if body_is_array {
        json!([example])
    } else {
        example
    };
    m.insert("example".into(), example);

    let constraints = collect_constraints(element);
    if !constraints.is_empty() {
        m.insert("constraints".into(), json!(constraints));
    }

    Value::Object(m)
}

/// A short type label for the field menu (`object`, `array<object>`, `string`, …).
fn type_label(node: &Value) -> String {
    if node.get("oneOf").is_some() || node.get("anyOf").is_some() {
        return "oneOf".into();
    }
    match node.get("type").and_then(Value::as_str) {
        Some("array") => {
            let item = node
                .get("items")
                .map(type_label)
                .unwrap_or_else(|| "any".into());
            format!("array<{item}>")
        }
        Some(t) => t.to_string(),
        None => {
            if node.get("properties").is_some() {
                "object".into()
            } else {
                "any".into()
            }
        }
    }
}

/// Synthesize a structurally-valid skeleton: follow `required` chains, one array
/// element, and use sensible placeholders by `format`. Depth-capped.
fn synth_example(node: &Value, depth: u32) -> Value {
    if depth >= MAX_DEPTH {
        return Value::Null;
    }
    // Resolve a combinator by taking the first alternative.
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(arr) = node.get(key).and_then(Value::as_array)
            && let Some(first) = arr.first()
        {
            return synth_example(first, depth);
        }
    }

    let ty = node.get("type").and_then(Value::as_str);
    match ty {
        Some("object") | None if node.get("properties").is_some() => {
            let props = node.get("properties").and_then(Value::as_object);
            let required: Vec<&str> = node
                .get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut obj = Map::new();
            if let Some(props) = props {
                for name in &required {
                    if let Some(sub) = props.get(*name) {
                        obj.insert((*name).to_string(), synth_example(sub, depth + 1));
                    } else {
                        obj.insert((*name).to_string(), Value::Null);
                    }
                }
            }
            Value::Object(obj)
        }
        Some("array") => {
            let item = node.get("items").map(|it| synth_example(it, depth + 1));
            match item {
                Some(v) => Value::Array(vec![v]),
                None => Value::Array(vec![]),
            }
        }
        Some("integer") | Some("number") => json!(0),
        Some("boolean") => json!(true),
        _ => string_placeholder(node),
    }
}

/// A placeholder for a string field, biased by `format` and `enum`.
fn string_placeholder(node: &Value) -> Value {
    if let Some(first) = node
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return first.clone();
    }
    let by_format = match node.get("format").and_then(Value::as_str) {
        Some("email") => "user@example.com",
        Some("date") => "2026-01-01",
        Some("date-time") => "2026-01-01T00:00:00Z",
        Some("uri") | Some("url") => "https://example.com",
        Some("uuid") => "00000000-0000-0000-0000-000000000000",
        _ => "string",
    };
    // Common SendGrid email-ish fields without a declared format.
    json!(by_format)
}

/// Walk the schema (depth-bounded) and surface human-readable cross-field
/// constraints: nested `required`, combinators, and `minItems`.
fn collect_constraints(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_constraints(schema, "body", 0, &mut out);
    out.truncate(MAX_CONSTRAINTS);
    out
}

fn walk_constraints(node: &Value, path: &str, depth: u32, out: &mut Vec<String>) {
    if depth >= MAX_DEPTH || out.len() >= MAX_CONSTRAINTS {
        return;
    }
    if node.get("oneOf").is_some() || node.get("anyOf").is_some() {
        out.push(format!(
            "`{path}` must match one of several alternative shapes (see expand=full)"
        ));
    }
    if let Some(min) = node.get("minItems").and_then(Value::as_u64)
        && min > 0
    {
        out.push(format!("`{path}` needs at least {min} item(s)"));
    }

    match node.get("type").and_then(Value::as_str) {
        Some("array") => {
            if let Some(items) = node.get("items") {
                walk_constraints(items, &format!("{path}[]"), depth + 1, out);
            }
        }
        _ => {
            if let Some(props) = node.get("properties").and_then(Value::as_object) {
                // Surface this object's own required set (skip the top, already shown).
                if depth > 0
                    && let Some(req) = node.get("required").and_then(Value::as_array)
                    && !req.is_empty()
                {
                    let names: Vec<&str> = req.iter().filter_map(Value::as_str).collect();
                    out.push(format!("`{path}` requires: {}", names.join(", ")));
                }
                for (name, sub) in props {
                    walk_constraints(sub, &format!("{path}.{name}"), depth + 1, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(id: &str, expand: &str) -> Result<Value, String> {
        let mut args = Map::new();
        args.insert("id".into(), json!(id));
        args.insert("expand".into(), json!(expand));
        describe_operation(&args)
    }

    #[test]
    fn minimal_sendmail_is_usable_and_bounded() {
        let out = describe("sg_mail_send_SendMail", "minimal").unwrap();
        let s = serde_json::to_string(&out).unwrap();
        // Token-bounded: nowhere near the ~22 KB full schema.
        assert!(
            s.len() < 4000,
            "minimal describe too large: {} bytes",
            s.len()
        );
        assert!(!s.contains("request_body_schema"));

        let body = &out["body"];
        // Required fields surfaced.
        let req = body["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "personalizations"));
        assert!(req.iter().any(|v| v == "from"));
        // Synthesized example has a usable nested recipient shape.
        let ex = &body["example"];
        assert!(ex["from"]["email"].is_string());
        assert!(ex["personalizations"][0]["to"][0]["email"].is_string());
        // Field menu shows non-required fields too (subject, content).
        assert!(body["fields"]["subject"].is_string());
        assert!(body["fields"]["content"].is_string());
    }

    #[test]
    fn full_includes_schema() {
        let out = describe("sg_mail_send_SendMail", "full").unwrap();
        assert!(out["request_body_schema"].is_object());
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.len() > 10_000, "full schema should be large");
    }

    #[test]
    fn alias_resolves() {
        // The one curated alias: ...CreateAsmGroup -> ...CreatAsmGroup (spec typo).
        let reg = Registry::global();
        if let Some(op) = reg.operations().iter().find(|o| o.id_alias.is_some()) {
            let alias = op.id_alias.clone().unwrap();
            let out = describe(&alias, "minimal").unwrap();
            assert_eq!(out["id"], json!(op.id));
        }
    }

    #[test]
    fn unknown_id_errors() {
        assert!(describe("sg_nope_nope_Nope", "minimal").is_err());
    }
}
