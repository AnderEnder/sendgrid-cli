//! **L4 — opt-in live smoke** (gated behind `SENDGRID_API_KEY`).
//!
//! These talk to the *real* SendGrid API. They are `#[ignore]`d (excluded from the
//! default `cargo test`) AND no-op when `SENDGRID_API_KEY` is unset, so the suite
//! stays hermetic. Run explicitly:
//!
//! ```text
//! SENDGRID_API_KEY=SG.xxx.yyy cargo test -p sendgrid-core --test live_smoke -- --ignored --nocapture
//! ```
//!
//! **They NEVER deliver mail** — the only send sets `mail_settings.sandbox_mode`,
//! which SendGrid validates and accepts (2xx) without sending. Optional region
//! override via `SENDGRID_REGION=eu|global`.

use sendgrid_core::{ApiKey, ExecuteResult, Region, Registry, RuntimeConfig, execute};
use serde_json::json;

/// Resolve the live key + region, or `None` to skip (env unset/blank).
fn live_cfg() -> Option<(String, RuntimeConfig)> {
    let key = std::env::var("SENDGRID_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())?;
    let mut cfg = RuntimeConfig::new(ApiKey::new(key.clone()));
    if let Some(r) = std::env::var("SENDGRID_REGION")
        .ok()
        .and_then(|s| Region::parse(&s))
    {
        cfg.region = r;
    }
    Some((key, cfg))
}

fn op(id: &str) -> &'static sendgrid_core::ir::OperationIr {
    Registry::global()
        .by_id(id)
        .unwrap_or_else(|| panic!("op {id} exists"))
}

/// The configured key must never appear in the serialized envelope, for any result.
/// (Note: a *redacted* marker is `SG.[REDACTED]` — which contains `SG.` — so we
/// assert the absence of the RAW key, not the substring `SG.`.)
fn assert_no_key_leak(result: &ExecuteResult, key: &str) {
    let serialized = serde_json::to_string(result).unwrap();
    assert!(
        !serialized.contains(key),
        "the live API key leaked into the output"
    );
    // The key's id segment (between the two dots) is distinctive; it must not
    // appear in cleartext either. The full-key check above is the real guarantee.
    if let Some(id_seg) = key.strip_prefix("SG.").and_then(|s| s.split('.').next())
        && id_seg.len() >= 8
    {
        assert!(
            !serialized.contains(id_seg),
            "the API key's id segment leaked into the output"
        );
    }
}

/// A sandbox SendMail is accepted (2xx) and delivers nothing. Proves the happy-path
/// build + auth + send works against the real API without sending email.
#[tokio::test]
#[ignore = "live: requires SENDGRID_API_KEY; opt-in via --ignored"]
async fn live_sandbox_send_returns_2xx() {
    let Some((key, cfg)) = live_cfg() else {
        eprintln!("SKIP live_sandbox_send_returns_2xx: SENDGRID_API_KEY unset");
        return;
    };

    let body = json!({
        "from": { "email": "smoke-test@example.com" },
        "personalizations": [ { "to": [ { "email": "nobody@example.com" } ] } ],
        "subject": "sendgrid-core L4 smoke (sandbox; never delivered)",
        "content": [ { "type": "text/plain", "value": "sandbox" } ],
        // The guard that makes this safe: SendGrid validates but does NOT deliver.
        "mail_settings": { "sandbox_mode": { "enable": true } }
    });

    let result = execute(&cfg, op("sg_mail_send_SendMail"), json!({ "body": body })).await;

    assert!(
        (200..300).contains(&result.status),
        "expected a 2xx from sandbox send, got status={} error={:?}",
        result.status,
        result.error()
    );
    assert_no_key_leak(&result, &key);
}

/// `%2C` (comma-joined array) and `%2F` (slash in a value) survive a real round
/// trip: the builder produces them (asserted via dry-run), and the live server
/// accepts the encoded URL and returns a response (status > 0, not a transport
/// failure). Uses a read-only stats export so nothing is mutated.
#[tokio::test]
#[ignore = "live: requires SENDGRID_API_KEY; opt-in via --ignored"]
async fn live_query_encoding_round_trips() {
    let Some((key, mut cfg)) = live_cfg() else {
        eprintln!("SKIP live_query_encoding_round_trips: SENDGRID_API_KEY unset");
        return;
    };

    let o = op("sg_marketing_stats_ExportSingleSendStat");
    let args = json!({ "query": {
        "ids": ["id_one", "id_two"],            // explode=false → id_one%2Cid_two
        "timezone": "America/New_York"          // slash → America%2FNew_York
    }});

    // 1) Builder-level: the encoding is produced (no network).
    let mut dry = cfg.clone();
    dry.dry_run = true;
    let preview = execute(&dry, o, args.clone()).await;
    let url = preview.request_preview.as_ref().unwrap()["url"]
        .as_str()
        .unwrap();
    assert!(
        url.contains("id_one%2Cid_two"),
        "%2C encoding missing: {url}"
    );
    assert!(
        url.contains("America%2FNew_York"),
        "%2F encoding missing: {url}"
    );

    // 2) Live: the server accepts the encoded URL and answers (any HTTP status is a
    //    completed round trip; a 0 status would mean a transport/parse failure).
    cfg.region = Region::Global; // this export is global-only
    let result = execute(&cfg, o, args).await;
    assert!(
        result.status > 0,
        "no response — encoded URL did not round-trip"
    );
    assert_no_key_leak(&result, &key);
}
