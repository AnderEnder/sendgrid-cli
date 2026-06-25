//! **L2 — golden requests** (`insta` snapshots over the dry-run / build path).
//!
//! These lock the *constructed request bytes* (method + URL + REDACTED headers +
//! body) for the request shapes that are easy to silently regress: SendMail's
//! full envelope, a top-level-array body, the two array-query encodings, and
//! CLI-style string→typed coercion. Every assertion runs through the real
//! `execute()` chokepoint in `dry_run` mode, so nothing is sent — the preview is
//! exactly what would have gone on the wire.
//!
//! Snapshots are committed (`crates/core/tests/snapshots/`); the suite is fully
//! deterministic (no network, no clock, sorted-key JSON). Regenerate with
//! `INSTA_UPDATE=always cargo test -p sendgrid-core`.

use sendgrid_core::runtime::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use sendgrid_core::{ApiKey, ExecuteResult, Registry, RuntimeConfig, execute_with};
use serde_json::{Value, json};

/// A syntactically valid dummy credential (`SG.<22>.<43>`). Dry-run never sends it.
const CONFIG_KEY: &str =
    "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

fn dry_run_cfg() -> RuntimeConfig {
    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.dry_run = true;
    c
}

/// Proves dry-run does not dispatch.
struct NeverDispatcher;
impl OperationDispatcher for NeverDispatcher {
    async fn dispatch(&self, _req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        panic!("dispatch must not be called on a dry-run golden");
    }
}

/// Run an op through `execute()` in dry-run mode and return the built preview,
/// asserting the build actually succeeded (so a snapshot can never silently lock
/// an `E_VALIDATION`/`E_BUILD` *error* envelope as the golden).
async fn preview_of(op_id: &str, args: Value) -> Value {
    let op = Registry::global()
        .by_id(op_id)
        .unwrap_or_else(|| panic!("op {op_id} exists"));
    let result: ExecuteResult = execute_with(&dry_run_cfg(), op, args, &NeverDispatcher).await;
    assert!(
        result.is_success(),
        "{op_id}: dry-run build failed (not a valid golden): code={:?} error={:?}",
        result.code,
        result.error()
    );
    result
        .request_preview
        .unwrap_or_else(|| panic!("{op_id}: dry-run yields a request_preview"))
}

/// Convenience: the built URL string from a dry-run preview.
async fn url_of(op_id: &str, args: Value) -> String {
    preview_of(op_id, args).await["url"]
        .as_str()
        .expect("url is a string")
        .to_string()
}

/// **Mandatory golden — SendMail.** ≥2 personalizations + base64 attachment +
/// `content[]` + `template_id` + `send_at` + `asm`. Locks the full constructed
/// request and proves the Authorization header is redacted (never the raw key).
#[tokio::test]
async fn golden_sendmail_full_envelope() {
    let body = json!({
        "from": { "email": "sales@example.com", "name": "Example Sales" },
        "reply_to": { "email": "reply@example.com" },
        "personalizations": [
            {
                "to": [ { "email": "customer@example.net", "name": "A Customer" } ],
                "cc": [ { "email": "manager@example.net" } ],
                "subject": "Your June invoice",
                "dynamic_template_data": { "first_name": "Dana", "total": "$42.00" },
                "send_at": 1718900000
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
                "content": "JVBERi0xLjQKJYGBgYEKMSAwIG9iago8PAo+PgplbmRvYmoK",
                "type": "application/pdf",
                "filename": "invoice.pdf",
                "disposition": "attachment",
                "content_id": "invoice-001"
            }
        ],
        "template_id": "d-abc123def4567890abcdef1234567890",
        "categories": ["invoice", "june"],
        "send_at": 1718900000,
        "asm": { "group_id": 12345, "groups_to_display": [12345, 6789] },
        "tracking_settings": { "click_tracking": { "enable": true } }
    });

    let preview = preview_of("sg_mail_send_SendMail", json!({ "body": body })).await;

    // The whole constructed request is frozen (method + url + redacted headers + body).
    insta::assert_json_snapshot!(preview);

    // Hard invariants, asserted explicitly so a regression fails loudly (not just
    // as a snapshot diff): auth redacted, raw key absent anywhere.
    assert_eq!(preview["method"], json!("POST"));
    assert_eq!(
        preview["url"],
        json!("https://api.sendgrid.com/v3/mail/send")
    );
    assert_eq!(
        preview["headers"]["authorization"],
        json!("Bearer [REDACTED]")
    );
    let serialized = serde_json::to_string(&preview).unwrap();
    assert!(
        !serialized.contains(CONFIG_KEY),
        "raw key leaked into preview"
    );
    assert!(
        !serialized.contains("SG.0123"),
        "key prefix leaked into preview"
    );
}

/// **Top-level-array body** golden — `DeleteRecipients` (DELETE, `body_is_array`).
/// Locks that the request body serializes as a JSON *array*, not an object.
#[tokio::test]
async fn golden_top_level_array_body() {
    let body = json!(["recipient_id_aaa", "recipient_id_bbb", "recipient_id_ccc"]);
    let preview = preview_of(
        "sg_legacy_contactdb_DeleteRecipients",
        json!({ "body": body }),
    )
    .await;

    insta::assert_json_snapshot!(preview);

    assert_eq!(preview["method"], json!("DELETE"));
    assert!(
        preview["body"].is_array(),
        "top-level body must be a JSON array, got {}",
        preview["body"]
    );
    assert_eq!(preview["body"].as_array().unwrap().len(), 3);
}

/// **Array query, `explode=false`** — `ExportSingleSendStat` `ids[]` → a single
/// comma-joined `ids=` (reqwest percent-encodes the comma as `%2C`).
#[tokio::test]
async fn golden_array_query_explode_false_comma_joined() {
    let url = url_of(
        "sg_marketing_stats_ExportSingleSendStat",
        json!({ "query": { "ids": ["ss_111", "ss_222", "ss_333"], "timezone": "America/New_York" } }),
    )
    .await;

    insta::assert_snapshot!(url);

    assert_eq!(
        url.matches("ids=").count(),
        1,
        "expected ONE comma-joined ids="
    );
    assert!(url.contains("ss_111%2Css_222%2Css_333"), "url={url}");
}

/// **Array query, `explode=true` (repeated keys)** — no real SendGrid op declares
/// `explode=true` (all 14 array query params are `explode=false`). The builder's
/// default for any array param whose `explode` is unset is `true` → repeated keys,
/// which is the exact bytes an `explode=true` param would produce. We exercise that
/// branch through `execute()` by passing an array to an *undeclared* query param
/// (`decl=None` ⇒ `is_array=val.is_array()`, `explode=unwrap_or(true)`). Documented
/// deviation: synthetic input, real builder codepath.
#[tokio::test]
async fn golden_array_query_explode_true_repeated_keys() {
    // `tag` is not a declared param on ListIp, so it takes the default-explode path.
    let url = url_of(
        "sg_ips_manage_ListIp",
        json!({ "query": { "tag": ["alpha", "beta", "gamma"] } }),
    )
    .await;

    insta::assert_snapshot!(url);

    assert_eq!(
        url.matches("tag=").count(),
        3,
        "explode=true must repeat the key, url={url}"
    );
    assert!(url.contains("tag=alpha"));
    assert!(url.contains("tag=beta"));
    assert!(url.contains("tag=gamma"));
}

/// **CLI-style coercion (integer + boolean)** — the CLI passes every arg as a
/// string; `"50"` must build as the integer `limit=50` and `"true"` as the bool
/// `is_enabled=true` in the constructed URL.
#[tokio::test]
async fn golden_cli_coercion_integer_and_boolean() {
    let url = url_of(
        "sg_ips_manage_ListIp",
        json!({ "query": { "limit": "50", "is_enabled": "true" } }),
    )
    .await;

    insta::assert_snapshot!(url);

    // Coerced to the declared types (NOT quoted strings) in the built query.
    assert!(url.contains("limit=50"), "url={url}");
    assert!(url.contains("is_enabled=true"), "url={url}");
}

/// **`number`-typed param renders as an integer** — SendGrid declares `page_size`
/// (and `limit`, `lastSeenID`) as `type: number`, but its API rejects a fractional
/// rendering with `"must be an integer"`. Whether the value arrives as a CLI string
/// (`"10"`), a CLI decimal string (`"10.0"`), or a typed JSON float (`10.0`), the
/// built URL must read `page_size=10` — never `page_size=10.0`. Regression for the
/// `templates list-template --page_size 10` bug.
#[tokio::test]
async fn golden_number_param_renders_as_integer() {
    for q in [
        json!({ "query": { "page_size": "10" } }),   // CLI string
        json!({ "query": { "page_size": "10.0" } }), // CLI decimal string
        json!({ "query": { "page_size": 10.0 } }),   // typed JSON float (MCP)
    ] {
        let url = url_of("sg_templates_ListTemplate", q.clone()).await;
        assert!(
            url.contains("page_size=10") && !url.contains("page_size=10.0"),
            "input {q} must render page_size=10, got url={url}"
        );
    }
}

/// **Curated client-side default injection** — `GET /v3/templates` defaults
/// `generations=legacy` server-side, hiding modern (dynamic) templates. The
/// curated `data/defaults.toml` entry injects `generations=legacy,dynamic` when
/// the caller omits it, so the CLI/MCP "just work" and return every template. An
/// explicit value must still win. Verified through the real `execute()` pipeline.
#[tokio::test]
async fn golden_curated_default_injected_when_omitted() {
    // Omitted → injected (comma percent-encoded as %2C by reqwest).
    let url = url_of(
        "sg_templates_ListTemplate",
        json!({ "query": { "page_size": "10" } }),
    )
    .await;
    assert!(
        url.contains("generations=legacy%2Cdynamic"),
        "omitted generations must default to legacy,dynamic; url={url}"
    );

    // Explicit value wins — no injection, no duplication. (`page_size` is a
    // required query param, so it's supplied here too.)
    let explicit = url_of(
        "sg_templates_ListTemplate",
        json!({ "query": { "page_size": "10", "generations": "legacy" } }),
    )
    .await;
    assert!(explicit.contains("generations=legacy"), "url={explicit}");
    assert!(
        !explicit.contains("legacy%2Cdynamic"),
        "explicit value must not be overridden; url={explicit}"
    );
    assert_eq!(
        explicit.matches("generations=").count(),
        1,
        "exactly one generations param; url={explicit}"
    );
}

/// **CLI-style coercion (comma → array)** — `"x,y,z"` for an array param coerces to
/// `["x","y","z"]`, then encodes per the param's `explode=false` (comma-joined).
/// Also coerces a sibling boolean.
#[tokio::test]
async fn golden_cli_coercion_comma_array() {
    let url = url_of(
        "sg_marketing_segments_v1_ListSegment",
        json!({ "query": { "ids": "seg_a,seg_b,seg_c", "no_parent_list_id": "false" } }),
    )
    .await;

    insta::assert_snapshot!(url);

    assert_eq!(url.matches("ids=").count(), 1, "explode=false ⇒ one ids=");
    assert!(
        url.contains("seg_a%2Cseg_b%2Cseg_c"),
        "comma-joined, url={url}"
    );
    assert!(
        url.contains("no_parent_list_id=false"),
        "bool coerced, url={url}"
    );
}
