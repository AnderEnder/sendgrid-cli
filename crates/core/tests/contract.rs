//! **L3 — contract tests** (`wiremock` + the real `ReqwestDispatcher`).
//!
//! Each test stands up a localhost SendGrid mock and points the runtime at it via
//! `RuntimeConfig::base_url_override`, then drives the *real* `execute()` so the
//! whole transport runs end-to-end: reqwest header serialization, the no-follow
//! redirect policy, and the retry / pagination engines over real HTTP. This is the
//! layer that the in-process `MockDispatcher` unit tests can't reach.
//!
//! Guarantees pinned here: cursor page-2 accumulation, verbatim error passthrough
//! with the right exit class, 429 retry honoring `Retry-After`, NON-retry of a
//! non-idempotent send on 5xx (no double-send), region fail-closed (no request),
//! Authorization never forwarded to a response-supplied foreign host, and a 303
//! `Location` surfaced rather than followed.

use sendgrid_core::runtime::RetryConfig;
use sendgrid_core::runtime::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use sendgrid_core::{
    ApiKey, ExecuteResult, Region, Registry, RuntimeConfig, execute, execute_with,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONFIG_KEY: &str =
    "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

/// A config pointed at `base` (the mock's URI). `base_url_override` deliberately
/// bypasses region routing — that is the one case the region test must avoid.
fn cfg_at(base: &str) -> RuntimeConfig {
    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.base_url_override = Some(base.to_string());
    c
}

/// Proves a code path issues NO request (it panics if the transport is touched).
struct NeverDispatcher;
impl OperationDispatcher for NeverDispatcher {
    async fn dispatch(&self, _req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        panic!("dispatch must not be called — a request was issued that should have been refused");
    }
}

fn op(id: &str) -> &'static sendgrid_core::ir::OperationIr {
    Registry::global()
        .by_id(id)
        .unwrap_or_else(|| panic!("op {id} exists"))
}

fn sendgrid_error(message: &str) -> Value {
    json!({ "errors": [ { "message": message, "field": null, "help": null } ] })
}

// ---------------------------------------------------------------------------
// Cursor pagination — page 2 is fetched and accumulated.
// ---------------------------------------------------------------------------

/// `--all` over a `CursorKey` op (`ListIp`): page 1 advertises
/// `_metadata.next_params.after_key`; the engine injects it as `after_key` on
/// page 2 and accumulates both pages' `result[]` records.
#[tokio::test]
async fn cursor_pagination_fetches_page_2_and_accumulates() {
    let server = MockServer::start().await;

    // Page 1: no after_key in the request → 2 records + a forward cursor.
    Mock::given(method("GET"))
        .and(path("/v3/send_ips/ips"))
        .and(query_param_is_missing("after_key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [ { "ip": "1.1.1.1" }, { "ip": "2.2.2.2" } ],
            "_metadata": { "next_params": { "after_key": 9999 } }
        })))
        .mount(&server)
        .await;

    // Page 2: after_key=9999 → 1 record, no further cursor (terminate).
    Mock::given(method("GET"))
        .and(path("/v3/send_ips/ips"))
        .and(query_param("after_key", "9999"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [ { "ip": "3.3.3.3" } ],
            "_metadata": { "next_params": {} }
        })))
        .mount(&server)
        .await;

    let mut c = cfg_at(&server.uri());
    c.paginate_all = true;

    let result = execute(&c, op("sg_ips_manage_ListIp"), json!({ "query": {} })).await;

    assert!(result.is_success(), "error: {:?}", result.error());
    let items = result.data().unwrap().as_array().expect("array of records");
    assert_eq!(items.len(), 3, "accumulated page1 (2) + page2 (1)");
    assert_eq!(items[2]["ip"], json!("3.3.3.3"), "page 2 record present");
    assert!(result.next.is_none(), "terminated naturally");

    let n = server.received_requests().await.unwrap().len();
    assert_eq!(n, 2, "exactly two pages were fetched");
}

// ---------------------------------------------------------------------------
// Error codes — verbatim passthrough + correct exit class per status.
// ---------------------------------------------------------------------------

/// 400/401/403/413/429 each pass the SendGrid error body through verbatim and map
/// to the documented exit class (r5 §3). `no_delay(0)` ⇒ the 429 is NOT retried
/// here — this row asserts passthrough, not retry (covered separately below).
#[tokio::test]
async fn error_status_codes_pass_through_verbatim_with_exit_code() {
    // (http status, expected exit_code)
    let cases = [(400u16, 1i32), (401, 3), (403, 4), (413, 1), (429, 6)];

    for (status, expected_exit) in cases {
        let server = MockServer::start().await;
        let body = sendgrid_error(&format!("synthetic {status}"));
        Mock::given(method("GET"))
            .and(path("/v3/send_ips/ips"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
            .mount(&server)
            .await;

        let mut c = cfg_at(&server.uri());
        c.retry = RetryConfig::no_delay(0); // no retries: isolate passthrough

        let result: ExecuteResult =
            execute(&c, op("sg_ips_manage_ListIp"), json!({ "query": {} })).await;

        assert!(!result.is_success(), "{status} must be an error envelope");
        assert_eq!(result.status, status, "status surfaced");
        assert_eq!(result.exit_code, expected_exit, "exit class for {status}");
        assert!(
            result.code.is_none(),
            "HTTP errors carry no E_* code (status is the signal)"
        );
        assert_eq!(
            result.error().unwrap(),
            &body,
            "{status}: SendGrid error body must pass through verbatim"
        );
        // Exactly one request — no retry on a deterministic 4xx.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

/// **429 → retry.** A rate-limited request (safe to retry on any method) with a
/// `Retry-After` header is retried; the second attempt succeeds. Two requests hit
/// the wire and the final envelope is the 200.
#[tokio::test]
async fn rate_limit_429_with_retry_after_then_succeeds() {
    let server = MockServer::start().await;

    // First call: 429 + Retry-After (served at most once, highest priority).
    Mock::given(method("GET"))
        .and(path("/v3/send_ips/ips"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_json(sendgrid_error("rate limited")),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;

    // Fallthrough: 200.
    Mock::given(method("GET"))
        .and(path("/v3/send_ips/ips"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "result": [ { "ip": "9.9.9.9" } ] })),
        )
        .mount(&server)
        .await;

    let mut c = cfg_at(&server.uri());
    // no_delay so Retry-After is honored without a real wall-clock sleep (capped to 0).
    c.retry = RetryConfig::no_delay(4);

    let result = execute(&c, op("sg_ips_manage_ListIp"), json!({ "query": {} })).await;

    assert!(
        result.is_success(),
        "retry should reach the 200; got {:?}",
        result.error()
    );
    assert_eq!(result.status, 200);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "one 429 + one retried 200"
    );
}

/// **`Retry-After` parsing is honored** (the seconds form), and the HTTP-date form
/// is treated as absent so the engine falls back to computed backoff. The wiremock
/// 429 test above proves a retry *happens*; this isolates the header-parsing path
/// (which `no_delay` clamps to 0 there, so it can't be observed end-to-end).
#[test]
fn retry_after_parses_seconds_and_ignores_http_date() {
    fn resp_with(retry_after: Option<&str>) -> DispatchResponse {
        let mut headers = http::HeaderMap::new();
        if let Some(v) = retry_after {
            headers.insert(http::header::RETRY_AFTER, v.parse().unwrap());
        }
        DispatchResponse {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            headers,
            body: Value::Null,
        }
    }

    // Seconds form → parsed.
    assert_eq!(
        resp_with(Some("5")).retry_after(),
        Some(std::time::Duration::from_secs(5))
    );
    // HTTP-date form → treated as absent (fall back to computed backoff).
    assert_eq!(
        resp_with(Some("Wed, 21 Oct 2026 07:28:00 GMT")).retry_after(),
        None
    );
    // No header → None.
    assert_eq!(resp_with(None).retry_after(), None);
}

/// **No double-send.** A non-idempotent send (`SendMail`, `retry_safe_5xx=false`)
/// that gets a 5xx is NOT retried — even under the default multi-retry config — so
/// it can never be delivered twice on an ambiguous server error.
#[tokio::test]
async fn sendmail_not_double_sent_on_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/mail/send"))
        .respond_with(ResponseTemplate::new(503).set_body_json(sendgrid_error("upstream down")))
        .mount(&server)
        .await;

    // Default retry would retry an *idempotent* op up to 4×; the per-op guard must
    // override that for a send.
    let c = cfg_at(&server.uri());
    let body = json!({
        "from": { "email": "s@example.com" },
        "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
        "subject": "hi",
        "content": [ { "type": "text/plain", "value": "hello" } ]
    });

    let result = execute(&c, op("sg_mail_send_SendMail"), json!({ "body": body })).await;

    assert!(!result.is_success());
    assert_eq!(result.status, 503);
    assert_eq!(result.exit_code, 7, "5xx → server-error class");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the send was issued EXACTLY once (no retry on 5xx)"
    );
}

// ---------------------------------------------------------------------------
// Region fail-closed — no request is made.
// ---------------------------------------------------------------------------

/// EU region + a `region_global_only` op fails closed with `E_REGION_UNAVAILABLE`
/// and issues NO request. Proven with a panicking dispatcher (and, deliberately,
/// no `base_url_override`, since an override would bypass region routing entirely).
#[tokio::test]
async fn region_fail_closed_makes_no_request() {
    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.region = Region::Eu;

    // ListSegment (mc_segments) is region_global_only.
    let result = execute_with(
        &c,
        op("sg_marketing_segments_v1_ListSegment"),
        json!({}),
        &NeverDispatcher,
    )
    .await;

    assert!(!result.is_success());
    assert_eq!(result.code.as_deref(), Some("E_REGION_UNAVAILABLE"));
    assert_eq!(result.status, 0, "no response was received");
    // NeverDispatcher panics if dispatched → reaching here proves nothing was sent.
}

// ---------------------------------------------------------------------------
// No-auth-to-foreign-host — Authorization is only ever sent to the configured base.
// ---------------------------------------------------------------------------

/// A `PageToken` op (`ListDesign`) whose page-1 `_metadata.next` points at a
/// FOREIGN host. The engine genuinely parses that URL to read the `page_token`,
/// but re-issues page 2 against the configured base URL — so the bearer never
/// leaves the configured host. The page-2 mock matches ONLY when the request both
/// carries the right `Authorization` and went to this (the configured) server; the
/// foreign host receives nothing.
#[tokio::test]
async fn no_auth_forwarded_to_foreign_next_host() {
    let server = MockServer::start().await;

    // Page 1: a foreign continuation URL embedded in the response envelope.
    Mock::given(method("GET"))
        .and(path("/v3/designs"))
        .and(query_param_is_missing("page_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [ { "id": "d1" } ],
            "_metadata": {
                "next": "https://attacker.example.com/v3/designs?page_size=100&page_token=TOK2"
            }
        })))
        .mount(&server)
        .await;

    // Page 2: served ONLY if it reaches THIS server carrying the bearer AND the
    // token extracted from the foreign URL — proving auth went to base, not foreign.
    Mock::given(method("GET"))
        .and(path("/v3/designs"))
        .and(query_param("page_token", "TOK2"))
        .and(header(
            "authorization",
            format!("Bearer {CONFIG_KEY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": [ { "id": "d2" } ],
            "_metadata": {}
        })))
        .mount(&server)
        .await;

    let mut c = cfg_at(&server.uri());
    c.paginate_all = true;

    let result = execute(
        &c,
        op("sg_marketing_designs_ListDesign"),
        json!({ "query": {} }),
    )
    .await;

    assert!(result.is_success(), "error: {:?}", result.error());
    let items = result.data().unwrap().as_array().unwrap();
    assert_eq!(
        items.len(),
        2,
        "page 2 (auth-matched) was served from the configured host"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "both requests went to the configured base — none to the foreign host"
    );
}

// ---------------------------------------------------------------------------
// 303 redirect — Location is surfaced, not followed.
// ---------------------------------------------------------------------------

/// `AuthenticateAccount`'s only documented success is a 303 SSO redirect. The real
/// client does NOT follow it (bearer-leak defense); the runtime surfaces the
/// `Location` as `data = {"location": …}` at the 3xx status with exit class 0.
#[tokio::test]
async fn redirect_303_location_is_surfaced() {
    let server = MockServer::start().await;
    let landing = "https://app.sendgrid.com/sso/landing?token=XYZ";
    Mock::given(method("POST"))
        .and(path("/v3/partners/accounts/acct_123/sso"))
        .respond_with(ResponseTemplate::new(303).insert_header("location", landing))
        .mount(&server)
        .await;

    let c = cfg_at(&server.uri());
    let result = execute(
        &c,
        op("sg_account_provisioning_AuthenticateAccount"),
        json!({ "path": { "accountID": "acct_123" } }),
    )
    .await;

    assert!(
        result.is_success(),
        "303-with-Location is a success envelope"
    );
    assert_eq!(result.status, 303);
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.data().unwrap()["location"], json!(landing));
    // Not followed: exactly one request (the POST), no chase of the Location.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
