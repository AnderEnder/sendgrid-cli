//! The runtime core: the single `execute()` chokepoint that turns an
//! `(OperationIr, args)` pair into a uniform [`ExecuteResult`], plus the
//! data-driven dispatcher (**Backend D**) that performs the HTTP I/O.
//!
//! Pipeline (brief item 12):
//! `coerce → sanitize-headers → govern-OBO → validate → policy → bulk → region →
//!  build → [dry-run preview] → send(retry) → [paginate if --all] → envelope`,
//! with always-on secret redaction (field-level + a final belt-and-suspenders
//! scrub) layered over the result.
//!
//! ## Frozen public API (what CLI/MCP consume)
//! - [`execute`] / [`execute_with`] — the entrypoint.
//! - [`RuntimeConfig`] — everything a call needs (credential, region, policy, …).
//! - [`ExecuteResult`] / [`envelope::Payload`] — the uniform envelope.
//! - [`auth::ApiKey`], [`region::Region`], [`safety::Policy`],
//!   [`dispatch::OperationDispatcher`] / [`dispatch::ReqwestDispatcher`].

pub mod auth;
pub mod build;
pub mod coerce;
pub mod dispatch;
pub mod envelope;
pub mod http;
pub mod jobs;
pub mod paginate;
pub mod region;
pub mod retry;
pub mod safety;
pub mod validate;

use crate::ir::{OperationIr, PaginationKind};
use crate::registry::Registry;
use serde_json::{Map, Value};

pub use auth::{ApiKey, AuthError};
pub use dispatch::{
    DispatchError, DispatchResponse, OperationDispatcher, RawResponse, ReqwestDispatcher,
};
pub use envelope::{ExecuteResult, Payload};
pub use jobs::{JobError, PollConfig, await_job, external_download, external_upload};
pub use region::Region;
pub use retry::RetryConfig;
pub use safety::{Policy, SafetyDenial};
pub use validate::{ValidationIssue, ValidationReport};

/// Everything a single [`execute`] call needs. Construct with [`RuntimeConfig::new`]
/// and override fields; the credential is the only required input.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// The bearer credential (redacted, non-serializable).
    pub api_key: ApiKey,
    /// Target data region.
    pub region: Region,
    /// Escape-hatch base URL (proxy/test). Overrides region routing when set.
    pub base_url_override: Option<String>,
    /// Allowed side-effect classes. Default = ALL (see [`Policy`]).
    pub policy: Policy,
    /// Allow-list for governed `on-behalf-of` (empty ⇒ impersonation disabled).
    pub allowed_subusers: Vec<String>,
    /// The governed impersonation value to inject (validated against the list).
    pub on_behalf_of: Option<String>,
    /// Construct + preview the request but DO NOT send it.
    pub dry_run: bool,
    /// Permit operations whose `bulk_triggers` fire.
    pub allow_bulk: bool,
    /// Permit routing a global-only op to global when region == EU.
    pub allow_region_fallback: bool,
    /// Auto-paginate (`--all`) up to the caps below.
    pub paginate_all: bool,
    /// Hard cap on accumulated items under `--all` (r5 default 1000).
    pub max_items: usize,
    /// Hard cap on page count under `--all` (r5 default 50).
    pub max_pages: usize,
    /// Retry/backoff policy.
    pub retry: RetryConfig,
}

impl RuntimeConfig {
    /// Defaults: region=Global, policy=ALL, no impersonation, single-shot, caps
    /// 1000 items / 50 pages, default retry.
    pub fn new(api_key: ApiKey) -> Self {
        RuntimeConfig {
            api_key,
            region: Region::Global,
            base_url_override: None,
            policy: Policy::default(),
            allowed_subusers: Vec::new(),
            on_behalf_of: None,
            dry_run: false,
            allow_bulk: false,
            allow_region_fallback: false,
            paginate_all: false,
            max_items: 1000,
            max_pages: 50,
            retry: RetryConfig::default(),
        }
    }
}

/// Run `op` with `args` using the default pooled [`ReqwestDispatcher`] (ring TLS,
/// no auto-redirect). The dispatcher is process-global so its connection pool is
/// shared across calls.
pub async fn execute(cfg: &RuntimeConfig, op: &OperationIr, args: Value) -> ExecuteResult {
    use std::sync::OnceLock;
    static DISPATCHER: OnceLock<ReqwestDispatcher> = OnceLock::new();
    let dispatcher = DISPATCHER.get_or_init(ReqwestDispatcher::new);
    execute_with(cfg, op, args, dispatcher).await
}

/// Same as [`execute`] but with an injected [`OperationDispatcher`] (tests use a
/// mock; advanced consumers can supply a custom transport).
pub async fn execute_with<D: OperationDispatcher>(
    cfg: &RuntimeConfig,
    op: &OperationIr,
    args: Value,
    dispatcher: &D,
) -> ExecuteResult {
    let result = run(cfg, op, args, dispatcher).await;
    // Belt-and-suspenders: scrub any stray key-shaped text from the whole
    // envelope (payload + preview + warnings) AFTER field-level redaction.
    finalize(result, &cfg.api_key, op)
}

async fn run<D: OperationDispatcher>(
    cfg: &RuntimeConfig,
    op: &OperationIr,
    args: Value,
    dispatcher: &D,
) -> ExecuteResult {
    let se = op.side_effect;
    let registry = Registry::global();
    let mut warnings: Vec<String> = Vec::new();

    // Normalize the args envelope: null/absent → empty object; non-object is a
    // caller bug.
    let mut args = match args {
        Value::Null => Value::Object(Map::new()),
        v @ Value::Object(_) => v,
        _ => {
            return ExecuteResult::preflight_error(
                "E_BAD_ARGS",
                se,
                "args must be a JSON object envelope {path,query,header,body}",
            );
        }
    };

    // 1. Coerce string args → declared types (path/query/header).
    coerce::coerce_args(op, &mut args);

    // 2. Always-on header sanitization (strip caller on-behalf-of/authorization).
    let stripped = safety::sanitize_headers(&mut args);
    if !stripped.is_empty() {
        warnings.push(format!(
            "stripped caller-supplied header(s) {stripped:?}; on-behalf-of is set only from governed config"
        ));
    }

    // 3. Governed on-behalf-of (validated against the allow-list).
    let governed_obo =
        match safety::resolve_on_behalf_of(cfg.on_behalf_of.as_deref(), &cfg.allowed_subusers) {
            Ok(v) => v,
            Err(d) => return ExecuteResult::preflight_error(d.code, se, d.message),
        };

    // 4. Validation (required params + body schema).
    let report = validate::validate(registry, op, &args);
    if !report.is_ok() {
        let issues = serde_json::to_value(&report.issues).unwrap_or(Value::Null);
        return ExecuteResult::validation_error(se, issues);
    }

    // 5. Policy gate (bypassed by dry-run).
    if let Err(d) = safety::check_policy(op, &cfg.policy, cfg.dry_run) {
        return ExecuteResult::preflight_error(d.code, se, d.message);
    }

    // 6. Bulk detection. Bypassed by dry-run for the same reason as the policy
    //    gate: nothing is sent, so a preview should never be blocked — the agent
    //    sees what the bulk call would look like, and the real run still enforces.
    if !cfg.dry_run
        && let Err(d) = safety::check_bulk(op, &args, cfg.allow_bulk)
    {
        return ExecuteResult::preflight_error(d.code, se, d.message);
    }

    // 7. Region routing (fail-closed for EU + global-only).
    let base_url = match region::resolve_base_url(
        op,
        cfg.region,
        cfg.allow_region_fallback,
        cfg.base_url_override.as_deref(),
    ) {
        region::RegionDecision::Route { base_url } => base_url,
        region::RegionDecision::RouteWithFallbackWarning { base_url, warning } => {
            warnings.push(warning);
            base_url
        }
        region::RegionDecision::Unavailable { message } => {
            return ExecuteResult::preflight_error("E_REGION_UNAVAILABLE", se, message)
                .with_warnings(warnings);
        }
    };

    let client = http::shared_client();
    let obo = governed_obo.as_deref();

    // 8. Dry-run: build + preview, do not send.
    if cfg.dry_run {
        return match build::build_request(client, op, &args, &cfg.api_key, &base_url, obo) {
            Ok(built) => ExecuteResult::dry_run(se, built.preview).with_warnings(warnings),
            Err(e) => {
                ExecuteResult::preflight_error("E_BUILD", se, e.to_string()).with_warnings(warnings)
            }
        };
    }

    // 9. Auto-paginate (`--all`) when the op actually paginates.
    let paginates = !matches!(
        op.pagination.kind,
        PaginationKind::None | PaginationKind::CappedSingle
    );
    if cfg.paginate_all && paginates {
        return run_paginated(dispatcher, cfg, op, args, &base_url, obo, warnings).await;
    }
    if cfg.paginate_all && op.pagination.kind == PaginationKind::CappedSingle {
        warnings.push("endpoint returns a single capped page (no cursor); --all is a no-op".into());
    }

    // 10. Single-shot send (with retry).
    let built = match build::build_request(client, op, &args, &cfg.api_key, &base_url, obo) {
        Ok(b) => b,
        Err(e) => {
            return ExecuteResult::preflight_error("E_BUILD", se, e.to_string())
                .with_warnings(warnings);
        }
    };
    match retry::send_with_retry(dispatcher, op, &cfg.retry, built.request).await {
        Ok(resp) => map_response(op, resp).with_warnings(warnings),
        Err(e) => ExecuteResult::network_error(se, &e).with_warnings(warnings),
    }
}

async fn run_paginated<D: OperationDispatcher>(
    dispatcher: &D,
    cfg: &RuntimeConfig,
    op: &OperationIr,
    args: Value,
    base_url: &str,
    obo: Option<&str>,
    warnings: Vec<String>,
) -> ExecuteResult {
    use paginate::PaginateOutcome;
    let se = op.side_effect;
    let outcome = paginate::paginate_all(
        dispatcher,
        http::shared_client(),
        op,
        args,
        &cfg.api_key,
        base_url,
        obo,
        &cfg.retry,
        cfg.max_items,
        cfg.max_pages,
    )
    .await;
    match outcome {
        PaginateOutcome::Collected {
            mut items,
            next,
            last_status,
            warnings: page_warnings,
        } => {
            // Field-redact secret response fields across every accumulated item.
            for item in items.iter_mut() {
                safety::redact_response(op, item);
            }
            let status = if last_status == 0 { 200 } else { last_status };
            let mut all_warnings = warnings;
            all_warnings.extend(page_warnings);
            ExecuteResult::success(status, se, Value::Array(items))
                .with_next(next)
                .with_warnings(all_warnings)
        }
        PaginateOutcome::HttpError { status, mut body } => {
            safety::redact_response(op, &mut body);
            ExecuteResult::http_error(status, se, body).with_warnings(warnings)
        }
        PaginateOutcome::Network(e) => ExecuteResult::network_error(se, &e).with_warnings(warnings),
        PaginateOutcome::Build(msg) => {
            ExecuteResult::preflight_error("E_BUILD", se, msg).with_warnings(warnings)
        }
    }
}

/// Map a single dispatched response into an envelope, redacting secret response
/// fields on success and passing the SendGrid error body verbatim otherwise.
///
/// Special case (M6): a documented 3xx whose `Location` header IS the payload (the
/// SSO `AuthenticateAccount` op, whose only success response is `303`). The client
/// never follows redirects, so we surface the target as `data = {"location": ...}`
/// at the 3xx status — otherwise the entire output of that op is dropped.
fn map_response(op: &OperationIr, resp: DispatchResponse) -> ExecuteResult {
    let status = resp.status.as_u16();
    let mut body = resp.body;
    if resp.status.is_success() {
        safety::redact_response(op, &mut body);
        ExecuteResult::success(status, op.side_effect, body)
    } else if resp.status.is_redirection()
        && let Some(location) = resp
            .headers
            .get(::http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
    {
        ExecuteResult::redirect(status, op.side_effect, location.to_string())
    } else {
        // Verbatim SendGrid error body (still field-redacted in case an error
        // payload echoes a secret field).
        safety::redact_response(op, &mut body);
        ExecuteResult::http_error(status, op.side_effect, body)
    }
}

/// Final redaction pass over the whole envelope.
///
/// The configured auth key is removed verbatim everywhere (always). The generic
/// `SG.<id>.<secret>` pattern scrub is also applied everywhere EXCEPT the success
/// body of a `reveal_response_fields` op (e.g. `CreateApiKey`), where the newly
/// created key — the intended output — must survive. Request previews and warnings
/// are always fully scrubbed (a preview is the *request*, never the created key).
fn finalize(mut result: ExecuteResult, key: &ApiKey, op: &OperationIr) -> ExecuteResult {
    let reveal = !op.reveal_response_fields.is_empty();
    match &mut result.payload {
        Payload::Data(v) if reveal => auth::scrub_value_keep_sg_pattern(v, Some(key)),
        Payload::Data(v) | Payload::Error(v) => auth::scrub_value(v, Some(key)),
    }
    if let Some(preview) = result.request_preview.as_mut() {
        auth::scrub_value(preview, Some(key));
    }
    for w in result.warnings.iter_mut() {
        *w = auth::scrub(w, Some(key));
    }
    result
}
