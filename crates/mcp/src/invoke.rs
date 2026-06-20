//! `invoke_operation` (+ promoted-tool routing) — the only path that touches the
//! network, and it does so **exclusively** through `sendgrid_core::execute()`.
//!
//! This module assembles the `{path, query, header, body}` args envelope and hands
//! it to the runtime chokepoint. ALL safety (policy, bulk, region, header
//! sanitization, redaction, validation) lives inside `execute()` and is NOT
//! reimplemented here. `dry_run` is passed through `RuntimeConfig`; `confirm` is
//! accepted but intentionally ignored (it is not a security control).

use sendgrid_core::ir::{Location, OperationIr};
use sendgrid_core::{Registry, RuntimeConfig, execute};
use serde_json::{Map, Value, json};

/// Result of resolving + dispatching: either the serialized `ExecuteResult`, or a
/// tool-usage error (unknown id / missing id) the server surfaces as `isError`.
pub enum InvokeOutcome {
    /// The serialized `ExecuteResult` envelope (success OR encoded API/preflight error).
    Envelope(Value),
    /// Tool misuse — the op could not be resolved.
    UsageError(String),
}

/// Run `invoke_operation` with the meta-tool's `{id, path_params, query, headers,
/// body, dry_run, confirm}` shape.
pub async fn invoke_operation(base: &RuntimeConfig, args: &Map<String, Value>) -> InvokeOutcome {
    let reg = Registry::global();
    let Some(id) = args
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return InvokeOutcome::UsageError("invoke_operation requires a non-empty `id`".into());
    };
    let Some(op) = reg.by_id(id) else {
        return InvokeOutcome::UsageError(format!(
            "unknown operation id `{id}`. Use search_operations to find a valid id."
        ));
    };

    let envelope = envelope_from_meta(args);
    let requested_dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    InvokeOutcome::Envelope(dispatch(base, op, envelope, requested_dry_run).await)
}

/// Run a promoted (first-class) tool: its args are the op's params *flat* (one key
/// per param), plus an optional `body` and `dry_run`. We bucket each provided arg
/// into the envelope by its declared [`Location`].
pub async fn invoke_promoted(
    base: &RuntimeConfig,
    op: &OperationIr,
    args: &Map<String, Value>,
) -> Value {
    let envelope = envelope_from_flat(op, args);
    let requested_dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    dispatch(base, op, envelope, requested_dry_run).await
}

/// Build the args envelope from the explicit `invoke_operation` buckets.
fn envelope_from_meta(args: &Map<String, Value>) -> Value {
    let mut env = Map::new();
    for (src, dst) in [
        ("path_params", "path"),
        ("query", "query"),
        ("headers", "header"),
    ] {
        if let Some(obj) = args.get(src).and_then(Value::as_object) {
            env.insert(dst.into(), Value::Object(obj.clone()));
        }
    }
    if let Some(body) = args.get("body") {
        env.insert("body".into(), body.clone());
    }
    Value::Object(env)
}

/// Build the args envelope from a promoted tool's flat params (bucketed by location).
fn envelope_from_flat(op: &OperationIr, args: &Map<String, Value>) -> Value {
    let mut path = Map::new();
    let mut query = Map::new();
    let mut header = Map::new();
    for p in &op.params {
        if let Some(v) = args.get(&p.name) {
            let bucket = match p.location {
                Location::Path => &mut path,
                Location::Query => &mut query,
                Location::Header => &mut header,
            };
            bucket.insert(p.name.clone(), v.clone());
        }
    }
    let mut env = Map::new();
    if !path.is_empty() {
        env.insert("path".into(), Value::Object(path));
    }
    if !query.is_empty() {
        env.insert("query".into(), Value::Object(query));
    }
    if !header.is_empty() {
        env.insert("header".into(), Value::Object(header));
    }
    if let Some(body) = args.get("body") {
        env.insert("body".into(), body.clone());
    }
    Value::Object(env)
}

/// Clone the base config, apply the per-call `dry_run` escalation (a request can
/// only *enable* dry-run, never disable a server-enforced one), and dispatch.
async fn dispatch(
    base: &RuntimeConfig,
    op: &OperationIr,
    envelope: Value,
    requested_dry_run: bool,
) -> Value {
    let mut cfg = base.clone();
    cfg.dry_run = base.dry_run || requested_dry_run;
    let result = execute(&cfg, op, envelope).await;
    serde_json::to_value(&result).unwrap_or_else(|e| json!({ "error": e.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendgrid_core::ApiKey;

    fn dry_run_cfg() -> RuntimeConfig {
        // dry-run path builds + previews fully offline — no network, no real key.
        let mut cfg = RuntimeConfig::new(ApiKey::new("SG.test.key"));
        cfg.dry_run = true;
        cfg
    }

    #[tokio::test]
    async fn invoke_dry_run_previews_without_sending() {
        // SendMail with a valid minimal body → dry-run yields a request_preview.
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_mail_send_SendMail"));
        args.insert("dry_run".into(), json!(true));
        args.insert(
            "body".into(),
            json!({
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ]
            }),
        );
        let cfg = dry_run_cfg();
        let out = match invoke_operation(&cfg, &args).await {
            InvokeOutcome::Envelope(v) => v,
            InvokeOutcome::UsageError(e) => panic!("unexpected usage error: {e}"),
        };
        assert_eq!(out["status"], json!(0), "dry-run sends nothing (status 0)");
        assert!(
            out["request_preview"].is_object(),
            "expected a request_preview"
        );
        assert_eq!(out["request_preview"]["method"], json!("POST"));
        // The bearer token must be redacted in the preview.
        let headers = out["request_preview"]["headers"].to_string();
        assert!(
            !headers.contains("SG.test.key"),
            "api key leaked into preview"
        );
    }

    #[tokio::test]
    async fn invoke_unknown_id_is_usage_error() {
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_nope_nope_Nope"));
        let cfg = dry_run_cfg();
        assert!(matches!(
            invoke_operation(&cfg, &args).await,
            InvokeOutcome::UsageError(_)
        ));
    }

    #[tokio::test]
    async fn promoted_buckets_path_params_and_previews() {
        // A path-param op: GET single template by id. dry-run previews the URL.
        let reg = Registry::global();
        let op = reg
            .by_id("sg_templates_GetTemplate")
            .expect("GetTemplate op");
        let mut args = Map::new();
        // GetTemplate has a path param `template_id`.
        let path_param = op
            .params
            .iter()
            .find(|p| p.location == Location::Path)
            .expect("a path param");
        args.insert(path_param.name.clone(), json!("abc123"));
        args.insert("dry_run".into(), json!(true));
        let cfg = dry_run_cfg();
        let out = invoke_promoted(&cfg, op, &args).await;
        assert_eq!(out["status"], json!(0));
        let url = out["request_preview"]["url"].as_str().unwrap_or_default();
        assert!(url.contains("abc123"), "path param not substituted: {url}");
    }

    #[tokio::test]
    async fn confirm_does_not_bypass_anything() {
        // confirm is accepted but ignored; with a read-only policy a Send op is
        // still denied (policy lives in execute(), not here). Use non-dry-run.
        use sendgrid_core::Policy;
        let mut cfg = RuntimeConfig::new(ApiKey::new("SG.test.key"));
        cfg.policy = Policy::read_only();
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_mail_send_SendMail"));
        args.insert("confirm".into(), json!(true));
        args.insert(
            "body".into(),
            json!({
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ]
            }),
        );
        let out = match invoke_operation(&cfg, &args).await {
            InvokeOutcome::Envelope(v) => v,
            InvokeOutcome::UsageError(e) => panic!("unexpected: {e}"),
        };
        // Policy denial from execute(), NOT bypassed by confirm.
        assert_eq!(out["code"], json!("E_POLICY_DENIED"));
    }
}
