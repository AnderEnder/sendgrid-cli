//! `describe_operation` — turn one op into an agent-actionable description.
//!
//! `minimal` (default) is deliberately token-bounded: metadata + params + the
//! top-level body field menu (name→type) + a **synthesized compact example**
//! (required chains + curated cross-field constraints, recipient/email
//! placeholders) + constraint notes (the op's curated `Constraint`s plus
//! schema-derived ones). The example is **repaired to satisfy the cross-field
//! constraints** (e.g. SendMail gets `content` + `subject`) so it is genuinely
//! usable, not valid-locally-but-400-remotely. It never dumps the full body schema
//! (SendMail's is ~22 KB / ~5k tokens).
//!
//! `full` adds the complete resolved request-body JSON Schema for callers that
//! explicitly opt into the cost.

use crate::text::truncate;
use sendgrid_core::Registry;
use sendgrid_core::ir::{Constraint, Location, OperationIr};
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
                // Cross-field constraints apply regardless of expand level — they are
                // the rules JSON Schema can't encode, so surface them in `full` too.
                if !op.constraints().is_empty() {
                    out.insert(
                        "constraints".into(),
                        json!(constraint_notes(op.constraints())),
                    );
                }
            }
            _ => {
                if let Some(schema) = schema {
                    out.insert(
                        "body".into(),
                        minimal_body(schema, op.body_is_array, op.constraints()),
                    );
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
///
/// `constraints` are the op's curated cross-field [`Constraint`]s (the spec-prose
/// rules the validator enforces after schema validation). They are surfaced as
/// human-readable rules AND used to **repair** the synthesized example so it
/// satisfies them — otherwise the required-only skeleton for SendMail would omit
/// `content`/`subject` and be valid-locally-but-400-remotely "bait" (M1/F1).
fn minimal_body(schema: &Value, body_is_array: bool, constraints: &[Constraint]) -> Value {
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
fn constraint_notes(constraints: &[Constraint]) -> Vec<String> {
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
        .map(|sub| synth_example(sub, 1))
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
    fn minimal_sendmail_surfaces_and_satisfies_constraints() {
        // M1/M4: the curated cross-field rules must be surfaced as readable notes AND
        // the synthesized example must satisfy them (so it isn't valid-locally-but-400).
        let out = describe("sg_mail_send_SendMail", "minimal").unwrap();
        let body = &out["body"];

        let notes = body["constraints"].as_array().expect("constraints array");
        let joined: String = notes
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("content") && joined.contains("template_id"),
            "expected the content/template_id rule in constraints, got: {joined}"
        );

        let ex = &body["example"];
        // RequiresOneOf(content|template_id) → content present and non-empty.
        assert!(
            ex["content"].as_array().is_some_and(|a| !a.is_empty()),
            "synthesized example must include non-empty content: {ex}"
        );
        // RequiredUnlessPresent(subject) → a subject was injected.
        assert!(
            ex["subject"].as_str().is_some_and(|s| !s.is_empty()),
            "synthesized example must include a subject: {ex}"
        );
        // MutuallyExclusive(reply_to|reply_to_list) → not both.
        assert!(
            !(ex.get("reply_to").is_some() && ex.get("reply_to_list").is_some()),
            "example must not set both reply_to and reply_to_list: {ex}"
        );
    }

    #[tokio::test]
    async fn synthesized_sendmail_example_round_trips_through_execute() {
        // The describe-synthesized example, fed back through the real runtime
        // chokepoint (dry-run), must pass validation (schema + cross-field
        // constraints) and produce a request preview — i.e. it is genuinely usable,
        // not 400-bait.
        use sendgrid_core::{ApiKey, RuntimeConfig, execute};

        let out = describe("sg_mail_send_SendMail", "minimal").unwrap();
        let example = out["body"]["example"].clone();

        let mut cfg = RuntimeConfig::new(ApiKey::new("SG.test.key"));
        cfg.dry_run = true;
        let op = Registry::global().by_id("sg_mail_send_SendMail").unwrap();
        let result = execute(&cfg, op, json!({ "body": example })).await;
        let v = serde_json::to_value(&result).unwrap();

        assert_ne!(
            v["code"],
            json!("E_VALIDATION"),
            "synthesized SendMail example failed validation: {v}"
        );
        assert!(
            v["request_preview"].is_object(),
            "expected a dry-run request_preview, got: {v}"
        );
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
