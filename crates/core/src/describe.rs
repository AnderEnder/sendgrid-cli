//! Op-description logic — turn one operation into an agent-actionable description.
//!
//! This is the shared engine behind the MCP `describe_operation` tool and the CLI
//! `--explain` flag. The pure, token-bounded building blocks live here so both
//! surfaces synthesize the same example, the same field-menus, and the same
//! `invoke_hint` from one implementation.
//!
//! The synthesized example follows `required` chains and is **repaired to satisfy
//! the op's curated cross-field constraints** (e.g. SendMail gets `content` +
//! `subject`) so it is **structurally valid** — it passes schema validation and the
//! op's cross-field rules, and placeholder values are biased to the field's kind (a
//! real email syntax, a real MIME type). It is NOT a guarantee the values are
//! *semantically sendable* (e.g. `user@example.com` is not a real inbox; swap in
//! real values before a live call).
//!
//! [`describe`] returns the CLI-facing [`Describe`] convenience struct
//! (`{params, example, invoke_hint, response_fields}`); MCP consumes the individual
//! `pub` functions to assemble its (unchanged) tool JSON shape.

use crate::Registry;
use crate::ir::{Constraint, Location, OperationIr};
use serde_json::{Map, Value, json};

const DESC_TRUNCATE: usize = 140;
/// Depth cap on the synthesized example / constraint walk (keeps tokens bounded).
const MAX_DEPTH: u32 = 6;
const MAX_CONSTRAINTS: usize = 12;

/// A CLI-facing, token-bounded description of one operation: the parameter menu, a
/// synthesized+repaired request-body example (`Value::Null` for bodyless ops), a
/// one-line invoke hint, and a compact response field-menu (`Value::Null` when the
/// op has no embedded response schema). MCP builds its richer tool JSON from the
/// individual `pub` functions in this module; this struct is the shared subset the
/// CLI's `--explain` renders.
#[derive(Debug, Clone)]
pub struct Describe {
    /// Per-param descriptors: `{name, in, required, type, format?, default?, description?}`.
    pub params: Vec<Value>,
    /// The synthesized, constraint-repaired request-body example, or `Value::Null`
    /// when the op takes no body.
    pub example: Value,
    /// A one-line "how to invoke" hint tailored to the op's shape.
    pub invoke_hint: String,
    /// A compact response field-menu (top-level names→types, one level into a result
    /// array's element), or `Value::Null` when no response schema is embedded.
    pub response_fields: Value,
}

/// Build the shared [`Describe`] view of an operation, resolving request/response
/// schemas from the process-wide [`Registry`] (schemas are keyed by id, so any
/// registry resolves an op identically).
pub fn describe(op: &OperationIr) -> Describe {
    let reg = Registry::global();

    let example = if op.has_body {
        match reg.schema_for(op) {
            Some(schema) => {
                let element = if op.body_is_array {
                    schema.get("items").unwrap_or(schema)
                } else {
                    schema
                };
                let mut ex = synth_example(element, 0);
                satisfy_constraints(&mut ex, element, op.constraints());
                if op.body_is_array { json!([ex]) } else { ex }
            }
            None => Value::Null,
        }
    } else {
        Value::Null
    };

    let response_fields = reg
        .response_schema(op)
        .map(response_menu)
        .unwrap_or(Value::Null);

    Describe {
        params: params_json(op),
        example,
        invoke_hint: invoke_hint(op),
        response_fields,
    }
}

/// A compact, token-bounded menu of a response schema: top-level field names→types,
/// plus one level into array-of-object fields (their element field menu), so an
/// agent can see e.g. `result[]` carries `id`/`name` for chaining — without the full
/// schema. Names→types only; never values or descriptions.
pub fn response_menu(schema: &Value) -> Value {
    // A top-level array response: describe its element.
    if schema.get("type").and_then(Value::as_str) == Some("array") || schema.get("items").is_some()
    {
        let element = schema.get("items").unwrap_or(schema);
        let mut m = Map::new();
        m.insert("is_array".into(), json!(true));
        if let Some(fields) = field_menu(element) {
            m.insert("item_fields".into(), fields);
        }
        return Value::Object(m);
    }

    let mut m = Map::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        let mut fields = Map::new();
        let mut items = Map::new();
        for (name, sub) in props {
            fields.insert(name.clone(), json!(type_label(sub)));
            // Descend one level into array<object> fields (the chaining case).
            if let Some(element) = array_object_element(sub)
                && let Some(menu) = field_menu(element)
            {
                items.insert(name.clone(), menu);
            }
        }
        m.insert("fields".into(), Value::Object(fields));
        if !items.is_empty() {
            m.insert("items".into(), Value::Object(items));
        }
    } else {
        m.insert("type".into(), json!(type_label(schema)));
    }
    Value::Object(m)
}

/// The element schema if `node` is an `array` whose items are an object; else `None`.
fn array_object_element(node: &Value) -> Option<&Value> {
    if node.get("type").and_then(Value::as_str) != Some("array") {
        return None;
    }
    let items = node.get("items")?;
    let is_object = items.get("type").and_then(Value::as_str) == Some("object")
        || items.get("properties").is_some();
    is_object.then_some(items)
}

/// A `{name: type_label}` menu of an object schema's top-level properties (`None`
/// when it has none).
fn field_menu(node: &Value) -> Option<Value> {
    let props = node.get("properties").and_then(Value::as_object)?;
    let mut fields = Map::new();
    for (name, sub) in props {
        fields.insert(name.clone(), json!(type_label(sub)));
    }
    Some(Value::Object(fields))
}

/// Describe an async op's multi-step flow: the job kind, the companion status op
/// (Poll) or presigned-URL field (upload/download), and the next action an agent
/// should take.
pub fn async_describe(op: &OperationIr) -> Value {
    use crate::ir::AsyncJob;
    let mut m = Map::new();
    let kind = match op.async_job {
        AsyncJob::Poll => "poll",
        AsyncJob::FireAndForget => "fire_and_forget",
        AsyncJob::ExternalUpload => "external_upload",
        AsyncJob::ExternalDownload => "external_download",
        AsyncJob::None => "none",
    };
    m.insert("kind".into(), json!(kind));
    match op.async_job {
        AsyncJob::Poll => {
            if let Some(s) = &op.async_status_op {
                m.insert("status_op".into(), json!(s));
            }
            m.insert(
                "next".into(),
                json!(
                    "Returns HTTP 202 + a job. invoke_operation with \"await\": true to poll the \
                     status op to completion, or invoke the status op yourself with the returned id."
                ),
            );
        }
        AsyncJob::FireAndForget => {
            m.insert(
                "next".into(),
                json!("Returns HTTP 202; no status endpoint — the work completes server-side."),
            );
        }
        AsyncJob::ExternalUpload => {
            if let Some(f) = &op.async_uri_field {
                m.insert("uri_field".into(), json!(f));
            }
            m.insert(
                "next".into(),
                json!(
                    "Returns an upload URL (see uri_field); PUT the file's bytes to it. Binary \
                     upload is out of MCP scope (use the CLI `--upload-file`)."
                ),
            );
        }
        AsyncJob::ExternalDownload => {
            if let Some(f) = &op.async_uri_field {
                m.insert("uri_field".into(), json!(f));
            }
            m.insert(
                "next".into(),
                json!(
                    "Returns presigned download URL(s) (see uri_field; invoke_operation surfaces \
                     them as `download_urls`); fetch them directly. Binary download is out of MCP scope."
                ),
            );
        }
        AsyncJob::None => {}
    }
    Value::Object(m)
}

/// Compact per-param descriptors: `{name, in, required, type, format?, description?}`.
pub fn params_json(op: &OperationIr) -> Vec<Value> {
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
            // Surface any curated client-side default so the contract stays
            // self-consistent: omitting this param injects `default` (an explicit
            // value still wins), rather than inheriting SendGrid's server default.
            if let Some(def) = op
                .param_defaults
                .iter()
                .find(|d| d.location == p.location && d.name == p.name)
            {
                m.insert("default".into(), json!(def.value));
            }
            if let Some(d) = &p.description {
                m.insert("description".into(), json!(truncate(d, DESC_TRUNCATE)));
            }
            Value::Object(m)
        })
        .collect()
}

/// A one-line "how to invoke" hint tailored to the op's shape.
pub fn invoke_hint(op: &OperationIr) -> String {
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
///
/// `constraints` are the op's curated cross-field [`Constraint`]s (the spec-prose
/// rules the validator enforces after schema validation). They are surfaced as
/// human-readable rules AND used to **repair** the synthesized example so it
/// satisfies them — otherwise the required-only skeleton for SendMail would omit
/// `content`/`subject` and be valid-locally-but-400-remotely "bait" (M1/F1).
pub fn minimal_body(schema: &Value, body_is_array: bool, constraints: &[Constraint]) -> Value {
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

    // Synthesize the required-chain skeleton, then repair it to satisfy the curated
    // cross-field constraints (e.g. add `content` + `subject` for SendMail).
    let mut example = synth_example(element, 0);
    satisfy_constraints(&mut example, element, constraints);
    let example = if body_is_array {
        json!([example])
    } else {
        example
    };
    m.insert("example".into(), example);

    // Curated cross-field rules first (most actionable), then schema-derived notes.
    let mut notes = constraint_notes(constraints);
    notes.extend(collect_constraints(element));
    notes.truncate(MAX_CONSTRAINTS);
    if !notes.is_empty() {
        m.insert("constraints".into(), json!(notes));
    }

    Value::Object(m)
}

/// Render the curated cross-field [`Constraint`]s as agent-readable rules, using the
/// curated `message` when present (it carries the precise, actionable wording the
/// validator also emits).
pub fn constraint_notes(constraints: &[Constraint]) -> Vec<String> {
    constraints
        .iter()
        .map(|c| match c {
            Constraint::RequiresOneOf { fields, message } => message
                .clone()
                .unwrap_or_else(|| format!("provide at least one of: {}", fields.join(", "))),
            Constraint::MutuallyExclusive { fields, message } => message
                .clone()
                .unwrap_or_else(|| format!("set at most one of: {}", fields.join(", "))),
            Constraint::RequiredUnlessPresent {
                field,
                unless_present,
                message,
                ..
            } => message.clone().unwrap_or_else(|| {
                format!("`{field}` is required unless `{unless_present}` is set")
            }),
        })
        .collect()
}

/// A body field counts as **present** only when non-`null`, non-empty (`""`/`[]` are
/// absent). Mirrors `sendgrid_core::runtime::validate::is_present` so the repaired
/// example agrees with what the validator will accept.
fn is_present(body: &Value, field: &str) -> bool {
    match body.get(field) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    }
}

/// Synthesize a value for a top-level body `field` from the element schema (falls
/// back to a string placeholder when the property isn't described).
fn synth_field(element: &Value, field: &str) -> Value {
    element
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|p| p.get(field))
        .map(|sub| synth_example_named(sub, 1, Some(field)))
        .unwrap_or_else(|| json!("string"))
}

/// Repair the synthesized example so it satisfies the op's cross-field constraints.
/// Idempotent: re-running on an already-satisfying body changes nothing. Operates
/// only on an object body (the scope the curated rules address).
fn satisfy_constraints(example: &mut Value, element: &Value, constraints: &[Constraint]) {
    let Value::Object(_) = example else { return };
    for c in constraints {
        match c {
            Constraint::RequiresOneOf { fields, .. } => {
                if !fields.iter().any(|f| is_present(example, f))
                    && let Some(first) = fields.first()
                {
                    let v = synth_field(element, first);
                    example.as_object_mut().unwrap().insert(first.clone(), v);
                }
            }
            Constraint::RequiredUnlessPresent {
                field,
                unless_present,
                or_each_in,
                ..
            } => {
                let satisfied = is_present(example, field)
                    || is_present(example, unless_present)
                    || or_each_in
                        .as_deref()
                        .is_some_and(|arr| present_in_each(example, arr, field));
                if !satisfied {
                    let v = synth_field(element, field);
                    example.as_object_mut().unwrap().insert(field.clone(), v);
                }
            }
            Constraint::MutuallyExclusive { fields, .. } => {
                // Keep the first present field, drop the rest (synth never produces a
                // conflict, but stay correct if the skeleton ever does).
                let mut seen_one = false;
                for f in fields {
                    if is_present(example, f) {
                        if seen_one {
                            example.as_object_mut().unwrap().remove(f);
                        } else {
                            seen_one = true;
                        }
                    }
                }
            }
        }
    }
}

/// True when `array_field` is a non-empty array whose every element has `field`
/// present (the per-item escape hatch, e.g. each personalization sets its own
/// `subject`). Mirrors the validator's `present_in_each`.
fn present_in_each(body: &Value, array_field: &str, field: &str) -> bool {
    match body.get(array_field).and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr.iter().all(|el| is_present(el, field)),
        _ => false,
    }
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
    synth_example_named(node, depth, None)
}

/// Like [`synth_example`] but threads the property `name` (when known) so string
/// placeholders can be biased by field name + description (e.g. an email-ish field
/// with no declared `format`, or a `type` field documented as a MIME type).
fn synth_example_named(node: &Value, depth: u32, name: Option<&str>) -> Value {
    if depth >= MAX_DEPTH {
        return Value::Null;
    }
    // Resolve a combinator by taking the first alternative.
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(arr) = node.get(key).and_then(Value::as_array)
            && let Some(first) = arr.first()
        {
            return synth_example_named(first, depth, name);
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
                for field in &required {
                    if let Some(sub) = props.get(*field) {
                        obj.insert(
                            (*field).to_string(),
                            synth_example_named(sub, depth + 1, Some(field)),
                        );
                    } else {
                        obj.insert((*field).to_string(), Value::Null);
                    }
                }
            }
            Value::Object(obj)
        }
        Some("array") => {
            // Array elements inherit the field name (an array of emails is still "…email").
            let item = node
                .get("items")
                .map(|it| synth_example_named(it, depth + 1, name));
            match item {
                Some(v) => Value::Array(vec![v]),
                None => Value::Array(vec![]),
            }
        }
        Some("integer") | Some("number") => json!(0),
        Some("boolean") => json!(true),
        _ => string_placeholder(node, name),
    }
}

/// A placeholder for a string field. Precedence (most → least specific): `enum`
/// (first member) → `format` → a field-name/description heuristic → `"string"`.
/// The heuristic is intentionally LOWEST precedence and is skipped when the schema
/// declares an explicit `pattern` (we can't guarantee a guess matches it), so a
/// schema-driven value is never overridden — the round-trip validation tests rely
/// on this ordering.
fn string_placeholder(node: &Value, name: Option<&str>) -> Value {
    if let Some(first) = node
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return first.clone();
    }
    let by_format = match node.get("format").and_then(Value::as_str) {
        Some("email") => Some("user@example.com"),
        Some("date") => Some("2026-01-01"),
        Some("date-time") => Some("2026-01-01T00:00:00Z"),
        Some("uri") | Some("url") => Some("https://example.com"),
        Some("uuid") => Some("00000000-0000-0000-0000-000000000000"),
        _ => None,
    };
    if let Some(v) = by_format {
        return json!(v);
    }
    if node.get("pattern").is_none()
        && let Some(v) = placeholder_by_hint(name, node)
    {
        return v;
    }
    json!("string")
}

/// A best-effort placeholder from a field's NAME + description keywords, for typed
/// strings that carry no `enum`/`format`. Returns a value that is **structurally**
/// of the field's kind (a real email syntax, a real MIME type) — not a guarantee
/// the value is *semantically deliverable* (e.g. `user@example.com` is not a real
/// inbox). Keep this conservative: only fire on unambiguous signals.
fn placeholder_by_hint(name: Option<&str>, node: &Value) -> Option<Value> {
    let name_l = name.unwrap_or("").to_ascii_lowercase();
    let desc_l = node
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    // Email-ish field name with no declared `format: email`.
    if name_l == "email" || name_l.ends_with("_email") || name_l.starts_with("email") {
        return Some(json!("user@example.com"));
    }
    // A `type`/`*_type` field documented as a MIME / media / content type.
    if (name_l == "type" || name_l.ends_with("_type"))
        && (desc_l.contains("mime")
            || desc_l.contains("media type")
            || desc_l.contains("content type"))
    {
        return Some(json!("text/plain"));
    }
    // URL-ish field name.
    if name_l == "url" || name_l == "uri" || name_l.ends_with("_url") || name_l.ends_with("_uri") {
        return Some(json!("https://example.com"));
    }
    None
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

/// Truncate a string to at most `max` chars (on a char boundary), appending `…`
/// when truncated. Keeps param descriptions token-bounded.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
