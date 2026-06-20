//! `invoke_operation` (+ promoted-tool routing) — the only path that touches the
//! network, and it does so **exclusively** through `sendgrid_core::execute()`.
//!
//! This module assembles the `{path, query, header, body}` args envelope and hands
//! it to the runtime chokepoint. ALL safety (policy, bulk, region, header
//! sanitization, redaction, validation) lives inside `execute()` and is NOT
//! reimplemented here. `dry_run` is passed through `RuntimeConfig`; `confirm` is
//! accepted but intentionally ignored (it is not a security control).
//!
//! Two agent-ergonomics layers wrap (never replace) that envelope, applied AFTER
//! core has produced the result:
//! - **output shaping** (`fields`/`max_items`) — a structural projector + array cap
//!   over a real success `data` so an agent can trim a large page (see [`crate::shape`]).
//! - **async legibility** — for `async_job != none` ops, surface the job kind + the
//!   next step; for Poll ops an optional `await` polls the companion status op to a
//!   terminal state (branching on the job's own status, mirroring the CLI `--await`);
//!   for ExternalDownload ops the presigned URL(s) are surfaced for the agent to fetch.

use sendgrid_core::ir::{AsyncJob, Location, OperationIr};
use sendgrid_core::{
    ExecuteResult, PollConfig, Registry, ReqwestDispatcher, RuntimeConfig, await_job, execute,
};
use serde_json::{Map, Value, json};

use crate::shape;

/// Result of resolving + dispatching: either the serialized `ExecuteResult`, or a
/// tool-usage error (unknown id / missing id) the server surfaces as `isError`.
pub enum InvokeOutcome {
    /// The serialized `ExecuteResult` envelope (success OR encoded API/preflight error).
    Envelope(Value),
    /// Tool misuse — the op could not be resolved.
    UsageError(String),
}

/// Run `invoke_operation` with the meta-tool's `{id, path_params, query, headers,
/// body, dry_run, confirm, fields, max_items, await}` shape.
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
    InvokeOutcome::Envelope(run(base, op, envelope, args).await)
}

/// Run a promoted (first-class) tool: its args are the op's params *flat* (one key
/// per param), plus an optional `body`, `dry_run`, and the shaping/`await` controls.
/// We bucket each provided arg into the envelope by its declared [`Location`].
pub async fn invoke_promoted(
    base: &RuntimeConfig,
    op: &OperationIr,
    args: &Map<String, Value>,
) -> Value {
    let envelope = envelope_from_flat(op, args);
    run(base, op, envelope, args).await
}

/// The shared invoke pipeline for both surfaces: dispatch (or await), then layer on
/// async legibility + opt-in output shaping. Returns the serialized envelope.
async fn run(
    base: &RuntimeConfig,
    op: &OperationIr,
    envelope: Value,
    args: &Map<String, Value>,
) -> Value {
    let requested_dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // `await` is a Rust keyword; the JSON arg key is "await".
    let await_requested = args.get("await").and_then(Value::as_bool).unwrap_or(false);
    let shaping = Shaping::from_args(args);

    let do_await = await_requested && op.async_job == AsyncJob::Poll;
    let env = if do_await {
        run_await(base, op, envelope, requested_dry_run).await
    } else {
        dispatch(base, op, envelope, requested_dry_run).await
    };
    finalize(op, env, &shaping, do_await)
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
    to_value(execute(&cfg, op, envelope).await)
}

/// Serialize an [`ExecuteResult`] to the envelope JSON (never fails in practice).
fn to_value(result: ExecuteResult) -> Value {
    serde_json::to_value(&result).unwrap_or_else(|e| json!({ "error": e.to_string() }))
}

// ---- output shaping (`fields` / `max_items`) ---------------------------------

/// Opt-in output-shaping controls parsed from the invoke args.
struct Shaping {
    /// Dotted paths to project from the success `data` (jq-lite).
    fields: Option<Vec<String>>,
    /// Cap on the result array length (≥1).
    max_items: Option<usize>,
}

impl Shaping {
    fn from_args(args: &Map<String, Value>) -> Self {
        let fields = args
            .get("fields")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());
        let max_items = args
            .get("max_items")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).max(1));
        Shaping { fields, max_items }
    }

    fn active(&self) -> bool {
        self.fields.is_some() || self.max_items.is_some()
    }

    /// A self-describing record of what shaping was applied, echoed in the envelope.
    fn descriptor(&self) -> Value {
        let mut m = Map::new();
        if let Some(f) = &self.fields {
            m.insert("fields".into(), json!(f));
        }
        if let Some(n) = self.max_items {
            m.insert("max_items".into(), json!(n));
        }
        Value::Object(m)
    }
}

/// Layer async legibility + opt-in output shaping onto the serialized envelope.
/// Never touches secret handling — only restructures the success `data` the agent
/// asked to trim, and adds purely additive metadata keys (never `error`/`code`).
fn finalize(op: &OperationIr, mut env: Value, shaping: &Shaping, awaited: bool) -> Value {
    // 1. Async legibility: surface the job kind + next step for any async op.
    if op.async_job != AsyncJob::None
        && let Value::Object(map) = &mut env
    {
        map.insert("async".into(), async_legend(op, awaited));
    }

    // 2. ExternalDownload: surface presigned URL(s) from a real success response so
    //    the agent can fetch them (binary streaming over MCP is out of scope).
    if op.async_job == AsyncJob::ExternalDownload && is_real_success(&env) {
        let field = op.async_uri_field.as_deref().unwrap_or("presigned_url");
        let urls = env
            .get("data")
            .map(|d| shape::collect_uris(d, field))
            .unwrap_or_default();
        if !urls.is_empty()
            && let Value::Object(map) = &mut env
        {
            map.insert("download_urls".into(), json!(urls));
        }
    }

    // 3. Output shaping over the real success `data` (opt-in only).
    if shaping.active()
        && is_real_success(&env)
        && let Value::Object(map) = &mut env
    {
        let data_key = op.pagination.data_key.as_deref();
        let mut truncation = None;
        if let Some(data) = map.get_mut("data") {
            if let Some(fields) = &shaping.fields {
                *data = shape::project(data, fields);
            }
            if let Some(max) = shaping.max_items {
                truncation = shape::cap_result(data, max, data_key);
            }
        }
        if let Some(t) = truncation {
            map.insert("truncated".into(), t.to_note());
        }
        map.insert("shaped".into(), shaping.descriptor());
    }

    env
}

/// True when the envelope is a success carrying data from a *real* response (status
/// != 0): excludes dry-run/pre-flight (status 0) and error payloads. Shaping and
/// URL-surfacing apply only here.
fn is_real_success(env: &Value) -> bool {
    env.get("error").is_none()
        && env.get("data").is_some()
        && env
            .get("status")
            .and_then(Value::as_u64)
            .is_some_and(|s| s != 0)
}

/// Build the legend describing an async op's multi-step flow + next action.
fn async_legend(op: &OperationIr, awaited: bool) -> Value {
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
            m.insert("awaited".into(), json!(awaited));
            if let Some(s) = &op.async_status_op {
                m.insert("status_op".into(), json!(s));
            }
            let next = if awaited {
                "Awaited: the companion status op was polled to a terminal state (its job status \
                 is in `data`). If it did not finish in time a warning is present — re-invoke with \
                 \"await\": true, or call the status op yourself."
            } else {
                "Submits a job (HTTP 202). Re-invoke with \"await\": true to poll the companion \
                 status op to completion, or call the status op yourself with the returned job id."
            };
            m.insert("next".into(), json!(next));
        }
        AsyncJob::FireAndForget => {
            m.insert(
                "next".into(),
                json!(
                    "Fire-and-forget (HTTP 202): no status endpoint to poll; the work completes \
                     server-side."
                ),
            );
        }
        AsyncJob::ExternalUpload => {
            if let Some(f) = &op.async_uri_field {
                m.insert("uri_field".into(), json!(f));
            }
            m.insert(
                "next".into(),
                json!(
                    "The success response carries an upload URL (see uri_field); PUT your file's \
                     bytes to it directly. Binary upload is out of MCP scope (use the CLI \
                     `--upload-file`, or upload from your own client)."
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
                    "The success response carries presigned download URL(s) (surfaced as \
                     `download_urls` when present); fetch them directly. Binary download is out of \
                     MCP scope. If absent, the job may not be ready — re-run a submit op with \
                     \"await\": true, or wait for the webhook-delivered id."
                ),
            );
        }
        AsyncJob::None => {}
    }
    Value::Object(m)
}

// ---- `await`: poll a Poll-class job to terminal (mirrors CLI `--await`) -------

/// Submit a Poll op, then poll its companion status op until terminal. Mirrors the
/// CLI `run_await`, including the three gotchas: (a) out-of-band job ids (return the
/// submit response + guidance, don't poll blind); (b) 404-means-pending for
/// download-link status ops; (c) await-success-on-FAILURE — a 2xx poll whose
/// terminal job status is a failure gets a synthetic `code` so the server flags
/// `isError`, with the job `data` kept intact.
async fn run_await(
    base: &RuntimeConfig,
    op: &OperationIr,
    envelope: Value,
    requested_dry_run: bool,
) -> Value {
    let mut cfg = base.clone();
    cfg.dry_run = base.dry_run || requested_dry_run;

    // dry-run: preview the submit only; never poll.
    if cfg.dry_run {
        return to_value(execute(&cfg, op, envelope).await);
    }

    let initial = execute(&cfg, op, envelope).await;
    if !initial.is_success() {
        return to_value(initial);
    }

    let reg = Registry::global();
    let Some(status_op) = op.async_status_op.as_deref().and_then(|id| reg.by_id(id)) else {
        let mut r = initial;
        r.warnings.push(format!(
            "await: `{}` is a poll job but has no resolvable companion status op; returning the \
             submit response.",
            op.id
        ));
        return to_value(r);
    };

    let data = initial.data().cloned().unwrap_or(Value::Null);
    let status_args = match build_status_args(status_op, &data) {
        Ok(a) => a,
        Err(msg) => {
            // Out-of-band id (e.g. RequestCsv → webhook): surface guidance, don't poll.
            let mut r = initial;
            r.warnings.push(msg);
            return to_value(r);
        }
    };

    let dispatcher = ReqwestDispatcher::new();
    let poll = poll_config_for(status_op);
    let mut result = await_job(&cfg, status_op, status_args, &dispatcher, &poll).await;

    // The await-success-on-FAILURE gotcha: a 2xx poll can still carry a terminal
    // `failure` status. Inject a synthetic top-level code so `isError` is set, while
    // keeping the job `data` intact for the agent to inspect.
    if result.is_success()
        && let Some(status) = job_status(&result, &poll)
        && is_failure_status(&status)
    {
        result.code = Some("E_ASYNC_JOB_FAILED".into());
        result.warnings.push(format!(
            "await: job `{}` reached terminal status `{status}` — treated as a failure.",
            status_op.id
        ));
    }
    to_value(result)
}

/// Build the status op's args (`{path:{<param>:<id>}}`) from the submit response.
/// Errors (with actionable guidance) when no id for the status op's path param can
/// be found — the signal the id is delivered out-of-band (webhook).
fn build_status_args(status_op: &OperationIr, data: &Value) -> Result<Value, String> {
    let path_params: Vec<&str> = status_op
        .params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.name.as_str())
        .collect();
    if path_params.is_empty() {
        return Ok(json!({}));
    }
    let obj = data.as_object();
    let mut path = Map::new();
    for pname in &path_params {
        match find_id(obj, pname) {
            Some(v) => {
                path.insert((*pname).to_string(), v);
            }
            None => {
                let fields: Vec<String> =
                    obj.map(|m| m.keys().cloned().collect()).unwrap_or_default();
                return Err(format!(
                    "await: could not find a value for the status op's `{pname}` path param in the \
                     submit response (response fields: {fields:?}). Some jobs deliver the id \
                     out-of-band via webhook (e.g. email-activity RequestCsv → DownloadCsv); once \
                     you have the id, invoke the status op `{}` directly with it.",
                    status_op.id
                ));
            }
        }
    }
    Ok(json!({ "path": Value::Object(path) }))
}

/// Find a job-id value for `pname`: exact key first, then common id field names.
fn find_id(obj: Option<&Map<String, Value>>, pname: &str) -> Option<Value> {
    let obj = obj?;
    if let Some(v) = obj.get(pname) {
        return Some(v.clone());
    }
    const FALLBACKS: [&str; 4] = ["id", "job_id", "download_uuid", "uuid"];
    FALLBACKS.iter().find_map(|k| obj.get(*k).cloned())
}

/// Poll settings per status op, bounded tighter than the CLI (an MCP tool call
/// blocks the agent): ~12 attempts. Download-link status ops 404 until the artifact
/// lands, so 404 means "keep polling" for them.
fn poll_config_for(status_op: &OperationIr) -> PollConfig {
    let mut p = PollConfig {
        max_attempts: 12,
        ..PollConfig::default()
    };
    if status_op.async_job == AsyncJob::ExternalDownload {
        p.pending_http_statuses = vec![404];
    }
    p
}

fn job_status(result: &ExecuteResult, poll: &PollConfig) -> Option<String> {
    result
        .data()?
        .pointer(&poll.status_pointer)?
        .as_str()
        .map(str::to_string)
}

fn is_failure_status(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "failure" | "failed" | "error" | "errored" | "canceled" | "cancelled" | "expired"
    )
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

    // ---- live-mock tests (real ReqwestDispatcher → localhost wiremock) --------

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A freshly-created, real-shaped key (`SG.<22>.<43>`), distinct from the auth key.
    const CREATED_KEY: &str =
        "SG.AAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn mock_cfg(server: &MockServer) -> RuntimeConfig {
        let mut cfg = RuntimeConfig::new(ApiKey::new(
            "SG.0123456789abcdefghABCD.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ));
        cfg.base_url_override = Some(server.uri());
        cfg
    }

    async fn invoke(cfg: &RuntimeConfig, args: Map<String, Value>) -> Value {
        match invoke_operation(cfg, &args).await {
            InvokeOutcome::Envelope(v) => v,
            InvokeOutcome::UsageError(e) => panic!("unexpected usage error: {e}"),
        }
    }

    /// F1 reveal contract through the MCP invoke path: CreateApiKey's freshly-minted
    /// key is the intended output and MUST survive verbatim — MCP adds NO redaction
    /// on top of core (re-redacting would re-break the reveal).
    #[tokio::test]
    async fn created_api_key_is_revealed_not_redacted_by_mcp() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/api_keys"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "api_key": CREATED_KEY,
                "api_key_id": "abc123",
                "name": "my key",
                "scopes": ["mail.send"]
            })))
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_security_api_keys_CreateApiKey"));
        args.insert(
            "body".into(),
            json!({ "name": "my key", "scopes": ["mail.send"] }),
        );
        let env = invoke(&cfg, args).await;

        assert_eq!(env["status"], json!(201));
        assert_eq!(
            env["data"]["api_key"],
            json!(CREATED_KEY),
            "MCP must NOT redact the created key (F1 reveal)"
        );
    }

    /// `fields` projects the success `data` to the requested dotted paths (jq-lite),
    /// projecting array elements, and echoes a `shaped` descriptor.
    #[tokio::test]
    async fn fields_projection_trims_result_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/teammates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [
                    { "email": "a@x.com", "first_name": "A", "is_admin": true },
                    { "email": "b@x.com", "first_name": "B", "is_admin": false }
                ],
                "_metadata": { "count": 2 }
            })))
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_account_teammates_ListTeammate"));
        args.insert("fields".into(), json!(["result.email"]));
        let env = invoke(&cfg, args).await;

        assert_eq!(
            env["data"]["result"],
            json!([{ "email": "a@x.com" }, { "email": "b@x.com" }]),
            "only the projected field survives, pairing preserved"
        );
        assert!(
            env["data"].get("_metadata").is_none(),
            "unrequested top-level fields are dropped"
        );
        assert_eq!(env["shaped"]["fields"], json!(["result.email"]));
    }

    /// `max_items` caps the op's result array and adds a `truncated` note; secret /
    /// envelope keys are never clobbered.
    #[tokio::test]
    async fn max_items_caps_result_and_notes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/teammates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [ { "email": "a@x.com" }, { "email": "b@x.com" }, { "email": "c@x.com" } ],
                "_metadata": { "count": 3 }
            })))
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_account_teammates_ListTeammate"));
        args.insert("max_items".into(), json!(1));
        let env = invoke(&cfg, args).await;

        assert_eq!(env["data"]["result"].as_array().unwrap().len(), 1);
        assert_eq!(env["truncated"]["kept"], json!(1));
        assert_eq!(env["truncated"]["total"], json!(3));
        assert_eq!(env["truncated"]["field"], json!("result"));
    }

    /// Default invoke (no shaping) returns the body verbatim — proving shaping is
    /// opt-in and the reveal/data are never touched by default.
    #[tokio::test]
    async fn no_shaping_returns_data_verbatim() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/teammates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [ { "email": "a@x.com", "first_name": "A" } ],
                "_metadata": { "count": 1 }
            })))
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_account_teammates_ListTeammate"));
        let env = invoke(&cfg, args).await;

        assert_eq!(env["data"]["result"][0]["first_name"], json!("A"));
        assert!(env.get("shaped").is_none());
        assert!(env.get("truncated").is_none());
    }

    /// `await` on a Poll op polls the companion status op to terminal. The
    /// await-success-on-FAILURE gotcha: a 2xx poll whose terminal job status is a
    /// failure is flagged as an error (synthetic `code`) while the job data is kept.
    #[tokio::test]
    async fn await_failed_job_flags_error_keeps_data() {
        let server = MockServer::start().await;
        // Submit → 202 with the job id.
        Mock::given(method("POST"))
            .and(path("/v3/marketing/contacts/exports"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "id": "exp_1" })))
            .mount(&server)
            .await;
        // Status poll → terminal FAILURE on the first attempt (no sleep before attempt 0).
        Mock::given(method("GET"))
            .and(path("/v3/marketing/contacts/exports/exp_1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": "exp_1", "status": "failure" })),
            )
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_marketing_contacts_ExportContact"));
        args.insert("await".into(), json!(true));
        args.insert("body".into(), json!({}));
        let env = invoke(&cfg, args).await;

        assert_eq!(
            env["code"],
            json!("E_ASYNC_JOB_FAILED"),
            "a failed terminal status must be flagged as an error: {env}"
        );
        assert_eq!(
            env["data"]["status"],
            json!("failure"),
            "the job data is kept intact"
        );
        assert_eq!(env["async"]["kind"], json!("poll"));
        assert_eq!(env["async"]["awaited"], json!(true));
    }

    /// `await` on a Poll op that reaches a SUCCESS terminal status returns the
    /// terminal response without an error code.
    #[tokio::test]
    async fn await_ready_job_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/marketing/contacts/exports"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "id": "exp_2" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v3/marketing/contacts/exports/exp_2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "id": "exp_2", "status": "ready", "urls": ["https://dl/x.csv"] }),
            ))
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_marketing_contacts_ExportContact"));
        args.insert("await".into(), json!(true));
        args.insert("body".into(), json!({}));
        let env = invoke(&cfg, args).await;

        assert!(
            env.get("code").is_none(),
            "ready job is not an error: {env}"
        );
        assert_eq!(env["data"]["status"], json!("ready"));
        assert_eq!(env["async"]["awaited"], json!(true));
    }

    /// An ExternalDownload op surfaces the presigned URL(s) from the response as
    /// `download_urls` (binary streaming over MCP is out of scope).
    #[tokio::test]
    async fn external_download_surfaces_presigned_urls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v3/marketing/contacts/exports/exp_9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "exp_9",
                "status": "ready",
                "urls": ["https://dl.example/part1.csv.gz", "https://dl.example/part2.csv.gz"]
            })))
            .mount(&server)
            .await;

        let cfg = mock_cfg(&server);
        let mut args = Map::new();
        args.insert("id".into(), json!("sg_marketing_contacts_GetExportContact"));
        args.insert("path_params".into(), json!({ "id": "exp_9" }));
        let env = invoke(&cfg, args).await;

        assert_eq!(env["async"]["kind"], json!("external_download"));
        assert_eq!(
            env["download_urls"],
            json!([
                "https://dl.example/part1.csv.gz",
                "https://dl.example/part2.csv.gz"
            ])
        );
    }
}
