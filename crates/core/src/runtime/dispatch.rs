//! The transport seam. [`OperationDispatcher`] sends one already-built request and
//! returns a backend-blind [`DispatchResponse`] (status + headers + raw JSON), so
//! the retry/pagination/error/envelope layers above it never touch a `reqwest`
//! type. **Backend D** ([`ReqwestDispatcher`]) implements it over a pooled client.
//!
//! Deviation from the originally-frozen signature (documented): the brief named
//! `dispatch -> Result<(StatusCode, Value), DispatchError>`. Honoring `Retry-After`
//! / `X-RateLimit-Reset` (r5 §4) is impossible if the seam discards headers, so the
//! return is a small struct carrying `headers` as well. `http::HeaderMap` is
//! provider-neutral, so the seam stays backend-blind. Consumers call `execute()`,
//! not `dispatch()`, so the change does not ripple to the CLI/MCP.

use serde_json::Value;

/// A backend-blind response: HTTP status, response headers, and the parsed JSON
/// body (or `Value::Null` for an empty/`204` body).
#[derive(Debug, Clone)]
pub struct DispatchResponse {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: Value,
}

impl DispatchResponse {
    /// `Retry-After` as a [`std::time::Duration`], if present (seconds form only;
    /// HTTP-date form is treated as absent and falls back to computed backoff).
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        self.headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
    }
}

/// What went wrong sending a request (never carries secret material).
#[derive(Debug, thiserror::Error, Clone)]
pub enum DispatchError {
    #[error("request timed out")]
    Timeout,
    #[error("connection error")]
    Connect,
    #[error("transport error")]
    Transport,
    #[error("failed to decode response body: {0}")]
    Decode(String),
}

impl DispatchError {
    /// Whether this class of failure is safe to retry for an idempotent op.
    pub fn is_retryable(&self) -> bool {
        matches!(self, DispatchError::Timeout | DispatchError::Connect)
    }
}

/// The transport contract. Sends one request; returns status + headers + body.
///
/// Implemented with native `async fn`, so the trait is **generic-only** (usable as
/// `&D` / `impl OperationDispatcher`), not `dyn`-safe. Consumers go through
/// [`super::execute`]; anyone needing `Box<dyn OperationDispatcher>` would wrap it
/// with `async-trait`.
#[allow(async_fn_in_trait)]
pub trait OperationDispatcher {
    async fn dispatch(&self, req: reqwest::Request) -> Result<DispatchResponse, DispatchError>;
}

/// Backend D: a pooled-`reqwest::Client` dispatcher.
#[derive(Debug, Clone)]
pub struct ReqwestDispatcher {
    client: reqwest::Client,
}

impl ReqwestDispatcher {
    /// Build with the hardened pooled client (ring TLS, no auto-redirect).
    pub fn new() -> Self {
        ReqwestDispatcher {
            client: super::http::build_client(),
        }
    }

    /// Use a caller-provided client (e.g. with custom timeouts).
    pub fn with_client(client: reqwest::Client) -> Self {
        ReqwestDispatcher { client }
    }
}

impl Default for ReqwestDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationDispatcher for ReqwestDispatcher {
    async fn dispatch(&self, req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
        let resp = self.client.execute(req).await.map_err(classify)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(classify)?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                // Non-JSON body: surface verbatim text so callers/error mapping can
                // still see it, rather than discarding it.
                Value::String(String::from_utf8_lossy(&bytes).into_owned())
            })
        };
        Ok(DispatchResponse {
            status,
            headers,
            body,
        })
    }
}

fn classify(e: reqwest::Error) -> DispatchError {
    if e.is_timeout() {
        DispatchError::Timeout
    } else if e.is_connect() {
        DispatchError::Connect
    } else if e.is_decode() || e.is_body() {
        DispatchError::Decode(e.to_string())
    } else {
        DispatchError::Transport
    }
}
