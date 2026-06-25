//! The pooled HTTP client (r4 §6 / brief item 8).
//!
//! - **ring TLS** (pure Rust, not aws-lc): reqwest 0.13 dropped the per-roots
//!   rustls feature variants and now defaults roots to `rustls-platform-verifier`.
//!   We instead build an explicit [`rustls::ClientConfig`] over the **ring** crypto
//!   provider + **webpki** (Mozilla) trust anchors and hand it to reqwest via
//!   [`reqwest::ClientBuilder::tls_backend_preconfigured`]. Baking the roots in
//!   keeps real TLS handshakes verifying in headless/minimal containers that have
//!   no OS trust store (the platform-verifier default would fail `UnknownIssuer`).
//! - **No auto-redirect** (`redirect::Policy::none()`): we never follow redirects,
//!   so the `Authorization` bearer is never forwarded to a response-supplied host
//!   (key-exfil defense); callers surface `Location` themselves.
//! - One client per process is enough — `reqwest::Client` is an `Arc` internally
//!   and pools connections; clone it freely.

use std::sync::OnceLock;
use std::time::Duration;

/// A rustls client config pinned to the **ring** provider (not aws-lc) and the
/// Mozilla **webpki** trust anchors. Self-contained: it carries its own provider,
/// so no process-default provider need be installed.
fn rustls_config() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring provider supports the default TLS protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth()
}

/// Build the hardened pooled client. Panics only if rustls/reqwest cannot
/// initialize a TLS stack, which is an environment/build fault, not runtime input.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .tls_backend_preconfigured(rustls_config())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("sendgrid-core/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to build reqwest client (TLS init)")
}

/// A process-wide client used only to *construct* `reqwest::Request` objects
/// (no connections are opened by building). The entrypoint uses this so request
/// construction works even when the [`super::dispatch::OperationDispatcher`] is a
/// mock with no client of its own. The real [`super::dispatch::ReqwestDispatcher`]
/// has its own pooled client for sending.
pub(crate) fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_client)
}

#[cfg(test)]
mod tests {
    #[test]
    fn client_builds_with_ring_tls() {
        // Proves the explicit ring+webpki rustls config is accepted and the client
        // constructs. Does NOT exercise a real handshake — see the ignored test below.
        let _client = super::build_client();
        let _shared = super::shared_client();
    }

    /// Live network: proves the hand-built `rustls::ClientConfig` (ring provider +
    /// webpki/Mozilla roots, handed to reqwest via `tls_backend_preconfigured`)
    /// actually completes a TLS handshake. The wiremock suite only drives localhost
    /// HTTP, so this is the sole guard against a broken trust path (an empty/wrong
    /// root store would surface here as `UnknownIssuer`, not in any other test).
    /// A 401 (no API key) is success — reqwest only `Err`s on transport/TLS faults.
    #[tokio::test]
    #[ignore = "requires network; verifies the real TLS handshake against webpki roots"]
    async fn real_tls_handshake_verifies() {
        let client = super::build_client();
        let resp = client
            .get("https://api.sendgrid.com/v3/scopes")
            .send()
            .await;
        assert!(resp.is_ok(), "TLS handshake failed: {resp:?}");
    }
}
