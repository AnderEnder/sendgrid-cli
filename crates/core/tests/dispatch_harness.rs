//! Adversarial dispatch/IR harness (code-review).
//!
//! (1) Drive the REAL `execute()` dry-run build over ALL 391 ops with synthesized
//!     path params; assert 0 build failures and no `{` left in any URL.
//! (2) Repro: the 303 AuthenticateAccount op — does the envelope surface
//!     `Location`, or is it dropped + mis-reported as an error?
//! (3) GET-with-202 (the two seq ops) is treated as success.

use sendgrid_core::ir::Location;
use sendgrid_core::runtime::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use sendgrid_core::{ApiKey, Registry, RuntimeConfig, execute_with};
use serde_json::{Map, Value, json};

const CONFIG_KEY: &str =
    "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

fn cfg() -> RuntimeConfig {
    RuntimeConfig::new(ApiKey::new(CONFIG_KEY))
}

/// Never sends — dry-run must not dispatch.
struct NeverDispatcher;
impl OperationDispatcher for NeverDispatcher {
    async fn dispatch(&self, _req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        panic!("dispatch must not be called in dry-run");
    }
}

/// Returns a fixed canned response (with arbitrary headers).
struct CannedDispatcher {
    status: u16,
    headers: http::HeaderMap,
    body: Value,
}
impl OperationDispatcher for CannedDispatcher {
    async fn dispatch(&self, _req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        Ok(DispatchResponse {
            status: http::StatusCode::from_u16(self.status).unwrap(),
            headers: self.headers.clone(),
            body: self.body.clone(),
        })
    }
}

#[tokio::test]
async fn build_all_391_ops_dry_run_no_failures_no_unsubstituted_path() {
    let r = Registry::global();
    let mut c = cfg();
    c.dry_run = true;

    let mut failures: Vec<String> = Vec::new();
    let mut count = 0usize;

    for op in r.operations() {
        count += 1;
        // Synthesize a dummy for every declared path param so substitution runs.
        let mut path_obj = Map::new();
        for p in &op.params {
            if p.location == Location::Path {
                path_obj.insert(p.name.clone(), json!("SYNTH"));
            }
        }
        let args = json!({ "path": Value::Object(path_obj) });

        let result = execute_with(&c, op, args, &NeverDispatcher).await;

        // Dry-run success means build_request succeeded. A non-success here is
        // either E_BUILD (a real build defect) or E_VALIDATION (a synth gap on a
        // body op — recorded but not a build defect).
        if !result.is_success() {
            let code = result.code.clone().unwrap_or_default();
            failures.push(format!(
                "{} [{} {}] -> code={} err={:?}",
                op.id,
                op.method,
                op.path,
                code,
                result.error()
            ));
            continue;
        }
        let preview = result.request_preview.expect("dry-run preview");
        let url = preview["url"].as_str().unwrap_or("");
        if url.contains('{') {
            failures.push(format!("{} -> unsubstituted '{{' in url: {}", op.id, url));
        }
    }

    assert_eq!(count, 391, "expected 391 ops");

    // Validation failures on body ops are an artifact of empty synthesized bodies;
    // the build-defect signal is E_BUILD or a `{` in the URL. Split them out.
    let build_defects: Vec<&String> = failures
        .iter()
        .filter(|f| f.contains("E_BUILD") || f.contains("unsubstituted"))
        .collect();
    assert!(
        build_defects.is_empty(),
        "{} BUILD defects (path/method):\n{}",
        build_defects.len(),
        build_defects
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    eprintln!(
        "build harness: {count} ops driven through execute()+dry_run; {} reached build_request \
         and built cleanly (no E_BUILD, no '{{' in URL); {} stopped at validation (empty synth \
         body) BEFORE build ran. Path-template coverage for ALL {count} is via the static \
         placeholder<->param check, not this path.",
        count - failures.len(),
        failures.len()
    );
}

#[tokio::test]
async fn authenticate_account_303_location_is_surfaced() {
    let r = Registry::global();
    let op = r
        .by_id("sg_account_provisioning_AuthenticateAccount")
        .expect("AuthenticateAccount op exists");

    // The op's ONLY documented success response is 303 + Location (SSO redirect).
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::LOCATION,
        "https://app.sendgrid.com/sso/landing?token=XYZ"
            .parse()
            .unwrap(),
    );
    let dispatcher = CannedDispatcher {
        status: 303,
        headers,
        body: Value::Null,
    };

    let args = json!({ "path": { "accountID": "acct_123" } });
    let result = execute_with(&cfg(), op, args, &dispatcher).await;

    // M6 FIXED: the 303 `Location` IS surfaced. For this op the redirect target is
    // the entire useful payload, so a documented 3xx-with-Location is a SUCCESS:
    // `data = {"location": <url>}` at the 3xx status, exit class 0.
    assert!(
        result.is_success(),
        "303-with-Location is success (the SSO redirect is the payload)"
    );
    assert_eq!(result.status, 303);
    assert_eq!(result.exit_code, 0, "documented 3xx success maps to exit 0");
    assert_eq!(
        result.data().unwrap()["location"],
        json!("https://app.sendgrid.com/sso/landing?token=XYZ")
    );
}

#[tokio::test]
async fn style_undeclared_ids_comma_join_in_real_url() {
    let r = Registry::global();
    // The 3 ops whose `ids` array got explode=false ONLY via the pagination.toml
    // comma_join override (the spec declared no style/explode).
    for id in [
        "sg_marketing_integrations_DeleteIntegration",
        "sg_marketing_segments_ListSegment",    // mc_segments_2.0
        "sg_marketing_segments_v1_ListSegment", // mc_segments
    ] {
        let op = r.by_id(id).unwrap_or_else(|| panic!("missing {id}"));
        let mut c = cfg();
        c.dry_run = true;
        let args = json!({ "query": { "ids": ["a1", "b2", "c3"] } });
        let result = execute_with(&c, op, args, &NeverDispatcher).await;
        let url = result.request_preview.as_ref().unwrap()["url"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            url.matches("ids=").count(),
            1,
            "{id}: expected one comma-joined ids=, got {url}"
        );
        assert!(
            url.contains("a1%2Cb2%2Cc3"),
            "{id}: not comma-joined: {url}"
        );
    }
}

#[tokio::test]
async fn top_level_array_body_serializes_as_array() {
    let r = Registry::global();
    // One of the 6 body_is_array ops: AddRecipient takes a top-level JSON array.
    let op = r
        .by_id("sg_legacy_contactdb_AddRecipient")
        .expect("AddRecipient");
    assert!(op.body_is_array, "AddRecipient body_is_array");

    let mut c = cfg();
    c.dry_run = true;
    let body = json!([{ "email": "a@example.com" }, { "email": "b@example.com" }]);
    let args = json!({ "body": body });
    let result = execute_with(&c, op, args, &NeverDispatcher).await;

    // Build must succeed and the preview body must be the array verbatim.
    assert!(
        result.is_success(),
        "array-body build failed: {:?}",
        result.error()
    );
    let preview_body = &result.request_preview.as_ref().unwrap()["body"];
    assert!(preview_body.is_array(), "body not an array: {preview_body}");
    assert_eq!(preview_body.as_array().unwrap().len(), 2);
}

/// Records request URLs and serves a queue of canned responses (for --all).
struct QueueDispatcher {
    responses: std::sync::Mutex<std::collections::VecDeque<(u16, Value)>>,
    urls: std::sync::Mutex<Vec<String>>,
}
impl QueueDispatcher {
    fn new(responses: Vec<(u16, Value)>) -> Self {
        QueueDispatcher {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
            urls: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl OperationDispatcher for QueueDispatcher {
    async fn dispatch(&self, req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        self.urls.lock().unwrap().push(req.url().to_string());
        let (code, body) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("no more canned responses");
        Ok(DispatchResponse {
            status: http::StatusCode::from_u16(code).unwrap(),
            headers: http::HeaderMap::new(),
            body,
        })
    }
}

#[tokio::test]
async fn all_extracts_records_under_nonstandard_cursor_key() {
    // M5 FIXED: ListSubuserByTemplate (cursor_record) returns its records under
    // `subuser_access` — now derived into `pagination.data_key`, so `--all` unwraps
    // the N records instead of wrapping the whole page envelope as one item.
    let r = Registry::global();
    let op = r
        .by_id("sg_account_teammates_ListSubuserByTemplate")
        .expect("ListSubuserByTemplate");
    assert_eq!(
        op.pagination.data_key.as_deref(),
        Some("subuser_access"),
        "M5: data_key derived from the 2xx response"
    );

    // A realistic single page: 3 records under `subuser_access` + a terminal _metadata.
    let page = json!({
        "has_restricted_subuser_access": true,
        "subuser_access": [
            { "id": 1, "username": "su1", "permission_type": "admin" },
            { "id": 2, "username": "su2", "permission_type": "admin" },
            { "id": 3, "username": "su3", "permission_type": "restricted" }
        ],
        "_metadata": {}
    });
    let dispatcher = QueueDispatcher::new(vec![(200, page)]);

    let mut c = cfg();
    c.paginate_all = true;
    let args = json!({ "path": { "teammate_name": "alice" }, "query": { "limit": 100 } });
    let result = execute_with(&c, op, args, &dispatcher).await;

    assert!(result.is_success());
    let items = result
        .data()
        .unwrap()
        .as_array()
        .expect("--all returns an array");
    assert_eq!(items.len(), 3, "the 3 subuser records are unwrapped");
    assert_eq!(items[0]["username"], json!("su1"));
    assert!(
        items[0].get("subuser_access").is_none(),
        "items are records, not the page envelope: {}",
        items[0]
    );
    // page_len (3) < limit (100) → terminates after one page (no spurious next).
    assert!(result.next.is_none());
}

#[tokio::test]
async fn all_warns_and_collects_nothing_when_array_key_missing() {
    // M5: if the page shape changes and the known result array can't be located,
    // the engine collects NOTHING + emits a visible warning (it never silently
    // wraps the whole envelope as one bogus item).
    let r = Registry::global();
    let op = r
        .by_id("sg_account_teammates_ListSubuserByTemplate")
        .expect("ListSubuserByTemplate");
    assert_eq!(op.pagination.data_key.as_deref(), Some("subuser_access"));

    // A page WITHOUT the expected `subuser_access` array (shape drift).
    let page = json!({ "has_restricted_subuser_access": false, "_metadata": {} });
    let dispatcher = QueueDispatcher::new(vec![(200, page)]);

    let mut c = cfg();
    c.paginate_all = true;
    let args = json!({ "path": { "teammate_name": "alice" }, "query": { "limit": 100 } });
    let result = execute_with(&c, op, args, &dispatcher).await;

    assert!(result.is_success());
    let items = result.data().unwrap().as_array().unwrap();
    assert_eq!(items.len(), 0, "collected nothing (no envelope-wrap)");
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("could not locate a result array")),
        "expected a visible under-fetch warning, got {:?}",
        result.warnings
    );
}

#[tokio::test]
async fn get_with_202_is_treated_as_success() {
    let r = Registry::global();
    // The two seq engagement-quality GETs return 202.
    let op = r
        .by_id("sg_stats_engagement_quality_ListEngagementQualityScore")
        .expect("seq op exists");
    assert_eq!(op.method, "GET");

    let dispatcher = CannedDispatcher {
        status: 202,
        headers: http::HeaderMap::new(),
        body: json!({ "result": [] }),
    };
    let args = json!({ "query": { "from": "2026-06-01", "to": "2026-06-10" } });
    let result = execute_with(&cfg(), op, args, &dispatcher).await;

    assert!(result.is_success(), "GET-with-202 must be success");
    assert_eq!(result.status, 202);
    assert_eq!(result.exit_code, 0, "202 maps to exit 0");
}

#[tokio::test]
async fn offset_extracts_records_under_nonstandard_key_and_terminates() {
    // M5 FIXED: ListMonthlyStat (offset) returns records under `stats` — now derived
    // into `data_key`. `--all` extracts the records AND terminates naturally on an
    // empty page (previously the envelope-wrap made page_len always 1, looping to
    // the cap with no `limit`).
    let r = Registry::global();
    let op = r
        .by_id("sg_account_subusers_ListMonthlyStat")
        .expect("ListMonthlyStat");
    assert_eq!(
        op.pagination.kind,
        sendgrid_core::ir::PaginationKind::Offset
    );
    assert_eq!(op.pagination.data_key.as_deref(), Some("stats"));

    // page1 carries records; page2's `stats` is empty → clean termination.
    let page1 = json!({ "date": "2026-06", "stats": [ { "name": "a", "metrics": {} }, { "name": "b", "metrics": {} } ] });
    let page2 = json!({ "date": "2026-06", "stats": [] });
    let dispatcher = QueueDispatcher::new(vec![(200, page1), (200, page2)]);

    let mut c = cfg();
    c.paginate_all = true;
    c.max_pages = 50;
    c.max_items = 10_000;
    let args = json!({ "query": { "date": "2026-06" } }); // NO limit

    let result = execute_with(&c, op, args, &dispatcher).await;
    let items = result.data().unwrap().as_array().unwrap();
    let calls = dispatcher.urls.lock().unwrap().len();

    assert_eq!(
        items.len(),
        2,
        "extracted the 2 `stats` records, not envelopes"
    );
    assert_eq!(items[0]["name"], json!("a"));
    assert!(
        items[0].get("stats").is_none(),
        "items are records, not the page envelope"
    );
    assert_eq!(
        calls, 2,
        "terminated on the empty page — did NOT loop to the cap"
    );
    assert!(
        result.next.is_none(),
        "natural termination, no continuation hint"
    );
}
