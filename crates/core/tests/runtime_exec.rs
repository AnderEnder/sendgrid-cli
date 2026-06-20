//! Integration tests for the `execute()` chokepoint (the frozen public API).
//!
//! Network-touching paths use a [`MockDispatcher`] so tests are hermetic and fast
//! (no retries fire — mocks return terminal statuses). Build/region/redaction
//! paths use `dry_run` or canned responses.

use sendgrid_core::runtime::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use sendgrid_core::{ApiKey, ExecuteResult, Policy, Region, Registry, RuntimeConfig, execute_with};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;

// A well-formed configured key (the credential the runtime holds).
const CONFIG_KEY: &str =
    "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

fn cfg() -> RuntimeConfig {
    RuntimeConfig::new(ApiKey::new(CONFIG_KEY))
}

/// A canned-response dispatcher. Records request URLs for assertions.
struct MockDispatcher {
    responses: Mutex<VecDeque<DispatchResponse>>,
    urls: Mutex<Vec<String>>,
}

impl MockDispatcher {
    fn new(responses: Vec<(u16, Value)>) -> Self {
        let q = responses
            .into_iter()
            .map(|(code, body)| DispatchResponse {
                status: http::StatusCode::from_u16(code).unwrap(),
                headers: http::HeaderMap::new(),
                body,
            })
            .collect();
        MockDispatcher {
            responses: Mutex::new(q),
            urls: Mutex::new(Vec::new()),
        }
    }
}

impl OperationDispatcher for MockDispatcher {
    async fn dispatch(&self, req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        self.urls.lock().unwrap().push(req.url().to_string());
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("MockDispatcher: no more canned responses");
        Ok(resp)
    }
}

/// A dispatcher that must never be called (proves no request is sent).
struct NeverDispatcher;
impl OperationDispatcher for NeverDispatcher {
    async fn dispatch(&self, _req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        panic!("dispatch must not be called");
    }
}

fn realistic_sendmail_body() -> Value {
    json!({
        "from": { "email": "sales@example.com", "name": "Example Sales" },
        "personalizations": [
            {
                "to": [ { "email": "customer@example.net", "name": "A Customer" } ],
                "cc": [ { "email": "manager@example.net" } ],
                "subject": "Your June invoice",
                "dynamic_template_data": { "first_name": "Dana", "total": "$42.00" }
            },
            {
                "to": [ { "email": "second@example.net" } ],
                "subject": "Your June invoice (copy)"
            }
        ],
        "subject": "Your June invoice",
        "content": [
            { "type": "text/plain", "value": "Thanks for your business." },
            { "type": "text/html", "value": "<p>Thanks for your business.</p>" }
        ],
        "attachments": [
            {
                "content": "JVBERi0xLjQKJ4base64",
                "type": "application/pdf",
                "filename": "invoice.pdf",
                "disposition": "attachment"
            }
        ],
        "template_id": "d-abc123def4567890abcdef1234567890",
        "categories": ["invoice", "june"],
        "send_at": 1718900000
    })
}

#[tokio::test]
async fn sendmail_dry_run_golden() {
    let r = Registry::global();
    let op = r.by_id("sg_mail_send_SendMail").expect("SendMail");

    let mut c = cfg();
    c.dry_run = true;
    c.on_behalf_of = Some("subuser-marketing".into());
    c.allowed_subusers = vec!["subuser-marketing".into()];

    // A caller-supplied on-behalf-of MUST be stripped and replaced by the
    // governed value; a stray Authorization MUST be stripped.
    let args = json!({
        "header": { "on-behalf-of": "victim-subuser", "Authorization": "Bearer SG.attacker" },
        "body": realistic_sendmail_body()
    });

    // Dry-run must not send.
    let result = execute_with(&c, op, args, &NeverDispatcher).await;

    assert!(result.is_success(), "dry-run is a success envelope");
    let preview = result
        .request_preview
        .as_ref()
        .expect("dry-run yields a request_preview");

    assert_eq!(preview["method"], json!("POST"));
    assert_eq!(
        preview["url"],
        json!("https://api.sendgrid.com/v3/mail/send")
    );

    let headers = &preview["headers"];
    assert_eq!(headers["authorization"], json!("Bearer [REDACTED]"));
    // Governed value won — NOT the caller's "victim-subuser".
    assert_eq!(headers["on-behalf-of"], json!("subuser-marketing"));
    assert_eq!(headers["content-type"], json!("application/json"));

    // Body is passed through verbatim (multi-personalization + attachment + template).
    let body = &preview["body"];
    assert_eq!(body["personalizations"].as_array().unwrap().len(), 2);
    assert_eq!(
        body["personalizations"][0]["to"][0]["email"],
        json!("customer@example.net")
    );
    assert_eq!(body["attachments"][0]["filename"], json!("invoice.pdf"));
    assert_eq!(
        body["template_id"],
        json!("d-abc123def4567890abcdef1234567890")
    );
    assert_eq!(body["send_at"], json!(1718900000));

    // The configured key never appears anywhere in the envelope.
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains(CONFIG_KEY));
    assert!(!serialized.contains("SG.0123"));
    // The stripped caller Authorization token is gone too.
    assert!(!serialized.contains("SG.attacker"));
}

#[tokio::test]
async fn governed_obo_not_in_allowlist_is_rejected() {
    let r = Registry::global();
    let op = r.by_id("sg_mail_send_SendMail").expect("SendMail");

    let mut c = cfg();
    c.dry_run = true;
    c.on_behalf_of = Some("not-approved".into());
    c.allowed_subusers = vec!["only-this-one".into()]; // does not include "not-approved"

    let result = execute_with(
        &c,
        op,
        json!({ "body": realistic_sendmail_body() }),
        &NeverDispatcher,
    )
    .await;
    assert!(!result.is_success());
    assert_eq!(result.code.as_deref(), Some("E_IMPERSONATION_NOT_ALLOWED"));
    assert_eq!(result.exit_code, 64);
}

#[tokio::test]
async fn create_api_key_response_secret_is_redacted() {
    let r = Registry::global();
    let op = r
        .by_id("sg_security_api_keys_CreateApiKey")
        .expect("CreateApiKey");

    // A freshly-created, real-shaped key returned in the 201 body.
    const CREATED_KEY: &str =
        "SG.AAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let dispatcher = MockDispatcher::new(vec![(
        201,
        json!({
            "api_key": CREATED_KEY,
            "api_key_id": "abc123",
            "name": "my key",
            "scopes": ["mail.send"]
        }),
    )]);

    let result = execute_with(
        &cfg(),
        op,
        json!({ "body": { "name": "my key" } }),
        &dispatcher,
    )
    .await;

    assert!(result.is_success(), "201 is success");
    assert_eq!(result.status, 201);
    // The secret field is redacted in `data` (curated `secret_response_fields`).
    assert_eq!(result.data().unwrap()["api_key"], json!("[REDACTED]"));
    // Non-secret fields survive.
    assert_eq!(result.data().unwrap()["name"], json!("my key"));

    // The created key never appears anywhere in the serialized envelope, and
    // neither does the configured key.
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains(CREATED_KEY), "created key leaked");
    assert!(!serialized.contains("SG.AAAA"), "created key prefix leaked");
    assert!(!serialized.contains(CONFIG_KEY), "config key leaked");
    assert!(serialized.contains("[REDACTED]"));
}

#[tokio::test]
async fn eu_region_global_only_op_fails_closed() {
    let r = Registry::global();
    // ListSegment (marketing) is region_global_only.
    let op = r
        .by_id("sg_marketing_segments_v1_ListSegment")
        .expect("ListSegment");
    assert!(op.region_global_only);

    let mut c = cfg();
    c.region = Region::Eu;

    let result = execute_with(&c, op, json!({}), &NeverDispatcher).await;
    assert!(!result.is_success());
    assert_eq!(result.code.as_deref(), Some("E_REGION_UNAVAILABLE"));

    // With the override flag, it routes (we still don't send: dry-run off but the
    // op is a read; use dry_run to avoid needing a mock) and warns.
    c.allow_region_fallback = true;
    c.dry_run = true;
    let result = execute_with(&c, op, json!({}), &NeverDispatcher).await;
    assert!(result.is_success());
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("region fallback"))
    );
    assert_eq!(
        result.request_preview.as_ref().unwrap()["url"],
        json!("https://api.sendgrid.com/v3/marketing/segments")
    );
}

#[tokio::test]
async fn array_query_explode_false_comma_joins_in_url() {
    let r = Registry::global();
    // ExportSingleSendStat: `ids` array explode=false; region_global_only → Global.
    let op = r
        .by_id("sg_marketing_stats_ExportSingleSendStat")
        .expect("op");

    let mut c = cfg();
    c.dry_run = true;
    let args = json!({ "query": { "ids": ["ss_111", "ss_222", "ss_333"], "timezone": "America/New_York" } });

    let result = execute_with(&c, op, args, &NeverDispatcher).await;
    let url = result.request_preview.as_ref().unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();

    // Comma-joined (explode=false): a single `ids=` with comma-separated values
    // (reqwest percent-encodes the comma as %2C). NOT repeated `ids=` keys.
    assert_eq!(
        url.matches("ids=").count(),
        1,
        "expected one ids= (comma-joined), url={url}"
    );
    assert!(url.contains("ss_111%2Css_222%2Css_333"), "url={url}");
}

#[tokio::test]
async fn coercion_through_execute_makes_string_limit_an_integer() {
    let r = Registry::global();
    let op = r.by_id("sg_stats_global_ListBrowserStat").expect("op");

    let mut c = cfg();
    c.dry_run = true;
    // limit/offset arrive as strings (as the CLI would pass them).
    let args = json!({ "query": { "start_date": "2026-06-01", "limit": "50", "offset": "100" } });

    let result = execute_with(&c, op, args, &NeverDispatcher).await;
    let url = result.request_preview.as_ref().unwrap()["url"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(url.contains("limit=50"), "url={url}");
    assert!(url.contains("offset=100"), "url={url}");
}

#[tokio::test]
async fn auto_paginate_offset_accumulates_across_pages() {
    let r = Registry::global();
    // ListBrowserStat: PaginationKind::Offset, inject_param=offset.
    let op = r.by_id("sg_stats_global_ListBrowserStat").expect("op");

    // Page 1 returns 10 items (== limit → continue); page 2 returns 3 (< limit → stop).
    let page1 = json!({ "result": (0..10).map(|i| json!({ "i": i })).collect::<Vec<_>>() });
    let page2 = json!({ "result": (10..13).map(|i| json!({ "i": i })).collect::<Vec<_>>() });
    let dispatcher = MockDispatcher::new(vec![(200, page1), (200, page2)]);

    let mut c = cfg();
    c.paginate_all = true;
    let args = json!({ "query": { "start_date": "2026-06-01", "limit": 10 } });

    let result = execute_with(&c, op, args, &dispatcher).await;
    assert!(result.is_success());
    let items = result.data().unwrap().as_array().unwrap();
    assert_eq!(items.len(), 13, "accumulated both pages");
    assert!(result.next.is_none(), "terminated naturally (no cap hit)");

    // Two requests were made; the second injected offset=10.
    let urls = dispatcher.urls.lock().unwrap();
    assert_eq!(urls.len(), 2);
    assert!(urls[0].contains("offset=0"), "page1 url={}", urls[0]);
    assert!(urls[1].contains("offset=10"), "page2 url={}", urls[1]);
}

#[tokio::test]
async fn http_error_body_passed_verbatim_with_exit_code() {
    let r = Registry::global();
    let op = r.by_id("sg_stats_global_ListBrowserStat").expect("op");

    let err_body =
        json!({ "errors": [ { "message": "permission denied", "field": null, "help": null } ] });
    let dispatcher = MockDispatcher::new(vec![(403, err_body.clone())]);

    let result: ExecuteResult = execute_with(
        &cfg(),
        op,
        json!({ "query": { "start_date": "2026-06-01" } }),
        &dispatcher,
    )
    .await;
    assert!(!result.is_success());
    assert_eq!(result.status, 403);
    assert_eq!(result.exit_code, 4); // 403 → distinct exit class
    // SendGrid error body is passed through verbatim.
    assert_eq!(result.error().unwrap(), &err_body);
}

/// REVIEW FINDING F1 — FIXED (M5). `--all` now extracts records from the
/// non-standard `recipients` key via the derived `pagination.data_key`, instead of
/// wrapping each page envelope as one bogus item.
#[tokio::test]
async fn auto_paginate_nonstandard_key_extracts_records_f1_fixed() {
    let r = Registry::global();
    // ListRecipient: PaginationKind::PageNumber; the 200 array is under `recipients`.
    let op = r
        .by_id("sg_legacy_contactdb_ListRecipient")
        .expect("ListRecipient op");
    assert_eq!(
        op.pagination.data_key.as_deref(),
        Some("recipients"),
        "M5: data_key derived for the non-standard `recipients` key"
    );

    // Each "page" carries 3 real records under the `recipients` key.
    let page = || json!({ "recipients": [ {"id":"a"}, {"id":"b"}, {"id":"c"} ] });

    // Constant non-empty mock + a low cap: stops at the page cap (the API would
    // eventually return an empty page; the cap behavior at the boundary is correct).
    let mut c = cfg();
    c.paginate_all = true;
    c.max_pages = 3;
    c.max_items = 100_000;
    let dispatcher = MockDispatcher::new(vec![(200, page()), (200, page()), (200, page())]);

    let result = execute_with(&c, op, json!({ "query": {} }), &dispatcher).await;
    assert!(result.is_success());
    let items = result.data().unwrap().as_array().unwrap();

    // FIXED: 3 pages × 3 records each = 9 unwrapped records (NOT 3 envelopes).
    assert_eq!(
        items.len(),
        9,
        "records are unwrapped from the `recipients` key"
    );
    assert!(
        items[0].get("recipients").is_none(),
        "item is a recipient record, not the page envelope: {}",
        items[0]
    );
    assert_eq!(items[0]["id"], json!("a"));
    // Stopped at the page cap → a continuation hint is emitted (correct at a cap).
    assert!(result.next.is_some(), "continuation hint at the page cap");
    assert_eq!(dispatcher.urls.lock().unwrap().len(), 3, "hit the page cap");
}

#[tokio::test]
async fn policy_read_only_blocks_send_but_default_allows() {
    let r = Registry::global();
    let op = r.by_id("sg_mail_send_SendMail").expect("SendMail");

    // read_only policy blocks the Send class (no dispatch).
    let mut c = cfg();
    c.policy = Policy::read_only();
    let result = execute_with(
        &c,
        op,
        json!({ "body": realistic_sendmail_body() }),
        &NeverDispatcher,
    )
    .await;
    assert_eq!(result.code.as_deref(), Some("E_POLICY_DENIED"));

    // Default policy (ALL) would allow it — prove via dry-run (no network).
    let mut c2 = cfg();
    c2.dry_run = true;
    let ok = execute_with(
        &c2,
        op,
        json!({ "body": realistic_sendmail_body() }),
        &NeverDispatcher,
    )
    .await;
    assert!(ok.is_success());
}
