//! The pooled HTTP client (r4 §6 / brief item 8).
//!
//! - **ring TLS** (pure Rust, not aws-lc): reqwest is built with
//!   `rustls-tls-no-provider`, so we install ring as the process-default crypto
//!   provider here, then `use_rustls_tls()` picks it up.
//! - **No auto-redirect** (`redirect::Policy::none()`): we never follow redirects,
//!   so the `Authorization` bearer is never forwarded to a response-supplied host
//!   (key-exfil defense); callers surface `Location` themselves.
//! - One client per process is enough — `reqwest::Client` is an `Arc` internally
//!   and pools connections; clone it freely.

use std::sync::OnceLock;
use std::time::Duration;

/// Install ring as the process-default rustls crypto provider exactly once.
/// Idempotent: a second install attempt (or one already installed elsewhere) is
/// ignored.
fn ensure_crypto_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Returns Err if a default is already set — fine, we just need *a* ring
        // default to exist before `use_rustls_tls()`.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build the hardened pooled client. Panics only if rustls/reqwest cannot
/// initialize a TLS stack, which is an environment/build fault, not runtime input.
pub fn build_client() -> reqwest::Client {
    ensure_crypto_provider();
    reqwest::Client::builder()
        .use_rustls_tls()
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
        // Proves the ring provider installs and the rustls client constructs.
        let _client = super::build_client();
        let _shared = super::shared_client();
    }
}
