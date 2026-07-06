//! `describe_operation` — turn one op into an agent-actionable description.
//!
//! `minimal` (default) is deliberately token-bounded: metadata + params + the
//! top-level body field menu (name→type) + a **synthesized compact example**
//! (required chains + curated cross-field constraints, recipient/email
//! placeholders) + constraint notes (the op's curated `Constraint`s plus
//! schema-derived ones) + a compact **response** field-menu for chaining calls.
//! The example is **repaired to satisfy the cross-field constraints** (e.g. SendMail
//! gets `content` + `subject`) so it is **structurally valid** — it passes schema
//! validation and the op's cross-field rules, and placeholder values are biased to
//! the field's kind (a real email syntax, a real MIME type). It is NOT a guarantee
//! the values are *semantically sendable* (e.g. `user@example.com` is not a real
//! inbox; swap in real values before a live call). It never dumps the full body
//! schema (SendMail's is ~22 KB / ~5k tokens).
//!
//! `full` adds the complete resolved request-body AND response-body JSON Schemas for
//! callers that explicitly opt into the cost.
//!
//! The pure op-description building blocks (params, example synthesis, field-menus,
//! invoke hint) live in [`sendgrid_core::describe`] so the CLI's `--explain` renders
//! the same content; this module only assembles them into the MCP tool JSON shape.

use sendgrid_core::Registry;
use sendgrid_core::describe::{
    async_describe, constraint_notes, invoke_hint, minimal_body, params_json, response_menu,
};
use serde_json::{Map, Value, json};

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

    // Response schema (independent of the request body — a GET has a response but no
    // body). `full` includes the complete resolved 2xx response schema; `minimal`
    // adds a compact field-menu (top-level names→types, descending one level into a
    // result array's element) so an agent can chain calls — e.g. learn that a list
    // returns `result[]` with `id` — without paying for the whole schema. Ops with
    // no embedded response schema (e.g. 204) simply omit the block.
    if let Some(resp) = reg.response_schema(op) {
        match expand {
            "full" => {
                out.insert("response_body_schema".into(), resp.clone());
            }
            _ => {
                out.insert("response".into(), response_menu(resp));
            }
        }
    }

    // Async/export legibility: surface the multi-step flow + next action so the
    // agent knows a 202/job is coming and how to retrieve the result.
    if op.async_job != sendgrid_core::ir::AsyncJob::None {
        out.insert("async".into(), async_describe(op));
    }

    Ok(Value::Object(out))
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
    fn describe_surfaces_curated_param_default() {
        // Self-consistent MCP contract: a curated client-side default is shown on
        // the param, so an agent knows omitting `generations` yields legacy,dynamic
        // (not SendGrid's legacy-only server default) and can override it.
        let out = describe("sg_templates_ListTemplate", "minimal").unwrap();
        let generations = out["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "generations")
            .expect("generations param present");
        assert_eq!(generations["default"], json!("legacy,dynamic"));
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
    fn content_type_placeholder_is_mime_not_literal_string() {
        // Fix 3: content[].type is documented as a MIME type → biased to a real MIME
        // placeholder, not the literal "string" (which is 400-bait).
        let out = describe("sg_mail_send_SendMail", "minimal").unwrap();
        let ty = &out["body"]["example"]["content"][0]["type"];
        assert_eq!(
            ty,
            &json!("text/plain"),
            "content[].type should be a MIME placeholder, got {ty}"
        );
        // And email-ish fields stay valid email syntax.
        assert_eq!(
            out["body"]["example"]["from"]["email"],
            json!("user@example.com")
        );
    }

    #[test]
    fn minimal_includes_compact_response_field_menu() {
        // Enh 4: a list op's minimal describe carries a response field-menu so an
        // agent can chain calls (knows `result[]` carries `id`/`email`) cheaply.
        let out = describe("sg_account_teammates_ListTeammate", "minimal").unwrap();
        let resp = &out["response"];
        assert_eq!(
            resp["fields"]["result"],
            json!("array<object>"),
            "top-level result field menu: {resp}"
        );
        // One level into the result element — the chaining menu.
        assert!(
            resp["items"]["result"]["email"].is_string(),
            "result element field menu should surface `email`: {resp}"
        );
        // Minimal must not embed the full response schema and must stay bounded.
        let min = serde_json::to_string(&out).unwrap();
        assert!(!min.contains("response_body_schema"));
        let full =
            serde_json::to_string(&describe("sg_account_teammates_ListTeammate", "full").unwrap())
                .unwrap();
        assert!(
            min.len() < full.len(),
            "minimal ({}) must be far smaller than full ({})",
            min.len(),
            full.len()
        );
    }

    #[test]
    fn full_includes_response_schema() {
        let out = describe("sg_account_teammates_ListTeammate", "full").unwrap();
        assert!(
            out["response_body_schema"].is_object(),
            "full should embed the resolved response schema"
        );
    }

    #[test]
    fn describe_surfaces_async_flow() {
        // Enh 6: async ops carry an `async` block naming the kind + next step.
        let poll = describe("sg_marketing_contacts_ExportContact", "minimal").unwrap();
        assert_eq!(poll["async"]["kind"], json!("poll"));
        assert_eq!(
            poll["async"]["status_op"],
            json!("sg_marketing_contacts_GetExportContact")
        );
        let dl = describe("sg_marketing_contacts_GetExportContact", "minimal").unwrap();
        assert_eq!(dl["async"]["kind"], json!("external_download"));
        assert_eq!(dl["async"]["uri_field"], json!("urls"));
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
