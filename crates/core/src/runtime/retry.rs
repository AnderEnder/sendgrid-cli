//! Retry / rate-limit policy (r5 §4, brief item 9), backend-blind.
//!
//! - **429** → retry on ANY method when `op.retry_safe_429` (the default for all
//!   ops): a rate-limited request was rejected before processing, so it is safe
//!   even for non-idempotent POSTs. Honor `Retry-After` when present, else
//!   exponential backoff with full jitter.
//! - **5xx / 408 / network** → retry only when `op.retry_safe_5xx` (idempotent
//!   methods + curated safe POSTs). This is what stops `POST /v3/mail/send` from
//!   being retried on an ambiguous 5xx (double-send risk).
//! - **other 4xx** → never retried (deterministic client error).
//!
//! The timer is [`futures_timer::Delay`] so the engine sleeps without binding to a
//! specific async runtime.

use super::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use crate::ir::OperationIr;
use std::time::Duration;

/// Backoff configuration. Defaults from r5 §4.3.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub cap: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    /// A zero-delay config for tests (no wall-clock sleeps).
    pub fn no_delay(max_retries: u32) -> Self {
        RetryConfig {
            max_retries,
            base_delay: Duration::ZERO,
            cap: Duration::ZERO,
        }
    }
}

/// Send `request` with retry. `request` must be cloneable (in-memory body, which
/// is always the case for our JSON requests); if it is not, it is sent once.
pub(crate) async fn send_with_retry<D: OperationDispatcher>(
    dispatcher: &D,
    op: &OperationIr,
    cfg: &RetryConfig,
    request: reqwest::Request,
) -> Result<DispatchResponse, DispatchError> {
    if request.try_clone().is_none() {
        return dispatcher.dispatch(request).await;
    }

    let mut attempt: u32 = 0;
    loop {
        let req = request
            .try_clone()
            .expect("request cloneability verified above");
        match dispatcher.dispatch(req).await {
            Ok(resp) => {
                if attempt < cfg.max_retries
                    && let Some(after) = retry_status_delay(op, &resp, attempt, cfg)
                {
                    futures_timer::Delay::new(after).await;
                    attempt += 1;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < cfg.max_retries && e.is_retryable() && op.retry_safe_5xx {
                    futures_timer::Delay::new(backoff(attempt, cfg)).await;
                    attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// If the response status warrants a retry, return the delay to wait; else `None`.
fn retry_status_delay(
    op: &OperationIr,
    resp: &DispatchResponse,
    attempt: u32,
    cfg: &RetryConfig,
) -> Option<Duration> {
    let code = resp.status.as_u16();
    if code == 429 && op.retry_safe_429 {
        // Honor Retry-After (seconds) if present, capped; else computed backoff.
        return Some(
            resp.retry_after()
                .map(|d| d.min(cfg.cap))
                .unwrap_or_else(|| backoff(attempt, cfg)),
        );
    }
    let retry_5xx = (500..=599).contains(&code) || code == 408;
    if retry_5xx && op.retry_safe_5xx {
        return Some(backoff(attempt, cfg));
    }
    None
}

/// Exponential backoff with full jitter: `rand(0, min(cap, base * 2^attempt))`.
fn backoff(attempt: u32, cfg: &RetryConfig) -> Duration {
    if cfg.base_delay.is_zero() {
        return Duration::ZERO;
    }
    let exp = cfg.base_delay.saturating_mul(2u32.saturating_pow(attempt));
    let ceil = exp.min(cfg.cap);
    let ceil_ms = ceil.as_millis() as u64;
    let jitter = pseudo_rand(attempt) % (ceil_ms + 1);
    Duration::from_millis(jitter)
}

/// Cheap non-crypto jitter source (no `rand` dependency). Seeds from the wall
/// clock so successive retries differ; quality is irrelevant for backoff jitter.
fn pseudo_rand(salt: u32) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ (u64::from(salt).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_add(1);
    // xorshift64
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_zero_when_base_zero() {
        let cfg = RetryConfig::no_delay(4);
        assert_eq!(backoff(0, &cfg), Duration::ZERO);
        assert_eq!(backoff(3, &cfg), Duration::ZERO);
    }

    #[test]
    fn backoff_within_ceiling() {
        let cfg = RetryConfig {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        };
        for attempt in 0..5 {
            let d = backoff(attempt, &cfg);
            let ceil =
                (Duration::from_millis(500) * 2u32.pow(attempt)).min(Duration::from_secs(30));
            assert!(d <= ceil, "attempt {attempt}: {d:?} exceeds {ceil:?}");
        }
    }
}
