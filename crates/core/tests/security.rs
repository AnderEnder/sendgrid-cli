//! **Security tests** — the hard, always-on guarantees, consolidated.
//!
//! 1. CreateApiKey's freshly-minted key (the intended output) IS revealed in the
//!    serialized [`ExecuteResult`], while the configured AUTH key is still removed
//!    everywhere — over the real transport (P6 item 10 product decision).
//! 2. `password` / `*_secret` request fields are redacted from `request_preview`.
//! 3. A caller-supplied `on-behalf-of` in the args header bucket is dropped
//!    (impersonation is governed-only).
//! 4. `Policy::read_only()` blocks `Send` and `Destructive` ops with
//!    `E_POLICY_DENIED` before any request is issued.

use sendgrid_core::runtime::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use sendgrid_core::{
    ApiKey, ExecuteResult, Policy, Registry, RuntimeConfig, execute, execute_with,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONFIG_KEY: &str =
    "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

/// A freshly-created, real-shaped key returned in a 201 body (`SG.<22>.<43>`).
const CREATED_KEY: &str = "SG.AAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

fn op(id: &str) -> &'static sendgrid_core::ir::OperationIr {
    Registry::global()
        .by_id(id)
        .unwrap_or_else(|| panic!("op {id} exists"))
}

struct NeverDispatcher;
impl OperationDispatcher for NeverDispatcher {
    async fn dispatch(&self, _req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        panic!("dispatch must not be called — the op should have been refused pre-flight");
    }
}

/// **(1) Reveal the created key; never the auth key.** CreateApiKey returns the
/// freshly-minted key in its 201 body — the intended output, so it must be REVEALED.
/// The configured AUTH key (a *different* SG key) must still be absent everywhere.
/// Driven over the real transport against a mock.
#[tokio::test]
async fn created_api_key_is_revealed_but_configured_auth_key_is_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/api_keys"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "api_key": CREATED_KEY,
            "api_key_id": "abc123",
            "name": "my key",
            // The response also echoes the configured auth key in another field; the
            // reveal exemption must NOT let this (a different SG key) survive.
            "created_by": CONFIG_KEY,
            "scopes": ["mail.send"]
        })))
        .mount(&server)
        .await;

    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.base_url_override = Some(server.uri());

    let result: ExecuteResult = execute(
        &c,
        op("sg_security_api_keys_CreateApiKey"),
        json!({ "body": { "name": "my key", "scopes": ["mail.send"] } }),
    )
    .await;

    assert!(result.is_success());
    assert_eq!(result.status, 201);
    // The created key is the intended output → revealed verbatim.
    assert_eq!(result.data().unwrap()["api_key"], json!(CREATED_KEY));
    assert_eq!(
        result.data().unwrap()["name"],
        json!("my key"),
        "non-secret fields survive"
    );

    let serialized = serde_json::to_string(&result).unwrap();
    assert!(
        serialized.contains(CREATED_KEY),
        "created key must be revealed"
    );
    // The security invariant that still holds: the configured auth key never leaks,
    // even when the (revealed) response body echoes it verbatim in another field.
    assert!(
        !serialized.contains(CONFIG_KEY),
        "configured auth key leaked"
    );
}

/// **(2) Secret request fields redacted in the preview.** A dry-run CreateSubuser
/// carrying a `password` (declared) and a `client_secret` (a `*_secret` field) must
/// show both as `[REDACTED]` in `request_preview.body`; the raw values must not
/// appear anywhere in the serialized result.
#[tokio::test]
async fn password_and_secret_redacted_in_request_preview() {
    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.dry_run = true;

    let result = execute_with(
        &c,
        op("sg_account_subusers_CreateSubuser"),
        json!({ "body": {
            "username": "sub1",
            "email": "sub1@example.com",
            "password": "hunter2-PLAINTEXT",
            "client_secret": "oauth-PLAINTEXT-secret",
            "ips": ["1.2.3.4"]
        }}),
        &NeverDispatcher,
    )
    .await;

    assert!(
        result.is_success(),
        "dry-run build failed: {:?}",
        result.error()
    );
    let body = &result.request_preview.as_ref().unwrap()["body"];
    assert_eq!(body["password"], json!("[REDACTED]"));
    assert_eq!(body["client_secret"], json!("[REDACTED]"));
    assert_eq!(body["username"], json!("sub1"), "non-secret fields survive");

    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains("hunter2-PLAINTEXT"), "password leaked");
    assert!(
        !serialized.contains("oauth-PLAINTEXT-secret"),
        "client_secret leaked"
    );
}

/// **(3) Caller `on-behalf-of` is dropped.** A caller-supplied impersonation header
/// in the args `header` bucket is stripped (impersonation is set ONLY from governed
/// config); a warning is surfaced and the victim value never reaches the preview.
#[tokio::test]
async fn caller_supplied_on_behalf_of_is_dropped() {
    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.dry_run = true;
    // No governed impersonation configured, so nothing legitimate replaces it.

    let result = execute_with(
        &c,
        op("sg_mail_send_SendMail"),
        json!({
            "header": { "on-behalf-of": "victim-subuser", "Authorization": "Bearer SG.attacker.injected" },
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ]
            }
        }),
        &NeverDispatcher,
    )
    .await;

    assert!(result.is_success());
    let headers = &result.request_preview.as_ref().unwrap()["headers"];
    assert!(
        headers.get("on-behalf-of").is_none(),
        "caller on-behalf-of must be dropped"
    );
    // The bearer is always our governed key, redacted — never the caller's injected one.
    assert_eq!(headers["authorization"], json!("Bearer [REDACTED]"));
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("stripped caller-supplied header")),
        "expected a strip warning, got {:?}",
        result.warnings
    );

    let serialized = serde_json::to_string(&result).unwrap();
    assert!(
        !serialized.contains("victim-subuser"),
        "victim impersonation value leaked"
    );
    assert!(
        !serialized.contains("SG.attacker"),
        "injected caller token leaked"
    );
}

/// **(5) Wrong-typed secret never leaks via a validation error.** jsonschema embeds
/// the offending instance VALUE verbatim in its message, so a `password` sent as a
/// number / object on CreateSubuser would leak into the `E_VALIDATION` envelope. The
/// validator must reject it with NO secret value anywhere in the serialized result.
/// At execute() level (NeverDispatcher proves it fails pre-flight, before any send).
#[tokio::test]
async fn wrong_typed_secret_value_never_leaks_in_validation_error() {
    let c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));

    // (a) numeric password.
    let numeric = execute_with(
        &c,
        op("sg_account_subusers_CreateSubuser"),
        json!({ "body": {
            "username": "sub1", "email": "sub1@example.com",
            "password": 918273645, "ips": ["1.2.3.4"]
        }}),
        &NeverDispatcher,
    )
    .await;
    assert_eq!(numeric.code.as_deref(), Some("E_VALIDATION"));
    let s = serde_json::to_string(&numeric).unwrap();
    assert!(
        !s.contains("918273645"),
        "numeric secret value leaked into validation error: {s}"
    );

    // (b) object password (a nested secret string must not surface either).
    let object = execute_with(
        &c,
        op("sg_account_subusers_CreateSubuser"),
        json!({ "body": {
            "username": "sub1", "email": "sub1@example.com",
            "password": { "leak": "SUPER-SECRET-OBJECT-VALUE" }, "ips": ["1.2.3.4"]
        }}),
        &NeverDispatcher,
    )
    .await;
    assert_eq!(object.code.as_deref(), Some("E_VALIDATION"));
    let s = serde_json::to_string(&object).unwrap();
    assert!(
        !s.contains("SUPER-SECRET-OBJECT-VALUE"),
        "object secret value leaked into validation error: {s}"
    );
}

/// **(4) `read_only` policy blocks Send + Destructive.** Both refuse with
/// `E_POLICY_DENIED` (exit class 64) BEFORE any request — proven with a panicking
/// dispatcher. (dry-run bypasses policy, so these are real, non-dry-run calls.)
#[tokio::test]
async fn read_only_policy_blocks_send_and_destructive() {
    let mut c = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    c.policy = Policy::read_only();

    // Send class — SendMail (valid body so it reaches the policy gate, not validation).
    let send = execute_with(
        &c,
        op("sg_mail_send_SendMail"),
        json!({ "body": {
            "from": { "email": "s@example.com" },
            "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
            "subject": "hi",
            "content": [ { "type": "text/plain", "value": "hello" } ]
        }}),
        &NeverDispatcher,
    )
    .await;
    assert_eq!(
        send.code.as_deref(),
        Some("E_POLICY_DENIED"),
        "Send blocked"
    );
    assert_eq!(send.exit_code, 64);

    // Destructive class — DeleteRecipients (top-level array body).
    let destructive = execute_with(
        &c,
        op("sg_legacy_contactdb_DeleteRecipients"),
        json!({ "body": ["recipient_id_aaa", "recipient_id_bbb"] }),
        &NeverDispatcher,
    )
    .await;
    assert_eq!(
        destructive.code.as_deref(),
        Some("E_POLICY_DENIED"),
        "Destructive blocked"
    );
    assert_eq!(destructive.exit_code, 64);
    // NeverDispatcher never panicked → no request was issued for either.

    // Sanity: the same Destructive op IS allowed under the default (all) policy —
    // proven via dry-run (no network), so the block above is the policy, not the op.
    let mut allow = RuntimeConfig::new(ApiKey::new(CONFIG_KEY));
    allow.dry_run = true;
    let ok = execute_with(
        &allow,
        op("sg_legacy_contactdb_DeleteRecipients"),
        json!({ "body": ["recipient_id_aaa"] }),
        &NeverDispatcher,
    )
    .await;
    assert!(
        ok.is_success(),
        "default policy permits the op (dry-run preview)"
    );
}
