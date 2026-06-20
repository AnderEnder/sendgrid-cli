//! Multi-step async-job helpers (brief item: async/upload ops). A handful of ops
//! aren't a single request/response — they return `202` + a status endpoint to
//! poll, or a presigned URL to PUT-to / GET-from a non-SendGrid host. The IR tags
//! them ([`crate::ir::AsyncJob`], plus [`crate::ir::OperationIr::async_status_op`] /
//! [`async_uri_field`](crate::ir::OperationIr::async_uri_field)); these helpers do
//! the work the CLI's `--await` / `--upload-file` / `--download` flags drive.
//!
//! Two public capabilities:
//! - [`await_job`] — poll a status op until terminal (status-field driven, with a
//!   cap + interval). Runtime-agnostic sleep ([`futures_timer`]), like the retry
//!   engine — core never binds to a specific executor.
//! - [`external_upload`] / [`external_download`] — PUT/GET a presigned URL with **no
//!   SendGrid bearer** (the URL is pre-authorized; sending our key to a third-party
//!   host like S3/Azure would leak it). The URL is absolute and region-appropriate
//!   (it comes from the in-region API response), so residency is preserved.

use super::dispatch::{DispatchError, DispatchResponse, OperationDispatcher};
use super::{ExecuteResult, RuntimeConfig, execute_with};
use crate::ir::OperationIr;
use serde_json::Value;
use std::time::Duration;

/// How [`await_job`] polls a status op until it reports a terminal state.
#[derive(Debug, Clone)]
pub struct PollConfig {
    /// Delay between polls (use [`Duration::ZERO`] in tests for no wall-clock sleep).
    pub interval: Duration,
    /// Hard cap on poll attempts before giving up (and warning).
    pub max_attempts: usize,
    /// JSON pointer to the status field in the status-op response body
    /// (default `/status`). Absent field on a 2xx ⇒ treated as terminal.
    pub status_pointer: String,
    /// Status values (case-insensitive) that mean the job is finished.
    pub terminal_values: Vec<String>,
    /// HTTP statuses that mean "not ready yet, keep polling" (default none). Set to
    /// `[404]` for the email-activity `DownloadCsv` flow, where the link 404s until
    /// the export lands; any other non-2xx is a real error and stops the loop.
    pub pending_http_statuses: Vec<u16>,
}

impl Default for PollConfig {
    fn default() -> Self {
        PollConfig {
            interval: Duration::from_secs(3),
            max_attempts: 40,
            status_pointer: "/status".to_string(),
            terminal_values: [
                "ready",
                "complete",
                "completed",
                "done",
                "success",
                "succeeded",
                "failure",
                "failed",
                "error",
                "canceled",
                "cancelled",
                "expired",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            pending_http_statuses: Vec::new(),
        }
    }
}

/// Poll `status_op` (e.g. the GET companion of an `ExportContact`/`ImportContact`
/// job) until it reports a terminal status, the poll cap is hit, or a real error
/// occurs. Returns the final [`ExecuteResult`]:
/// - terminal status reached → that 2xx response;
/// - a non-2xx not in [`PollConfig::pending_http_statuses`] → returned immediately;
/// - cap reached → the last response, with a `--await` cap warning appended.
///
/// The same `args` (typically `{path:{id}}`) is sent every poll. Goes through the
/// full [`execute_with`] chokepoint, so redaction/region/policy all still apply.
pub async fn await_job<D: OperationDispatcher>(
    cfg: &RuntimeConfig,
    status_op: &OperationIr,
    args: Value,
    dispatcher: &D,
    poll: &PollConfig,
) -> ExecuteResult {
    let max = poll.max_attempts.max(1);
    let mut last: Option<ExecuteResult> = None;
    for attempt in 0..max {
        let result = execute_with(cfg, status_op, args.clone(), dispatcher).await;
        if result.is_success() {
            let terminal = match result.data().and_then(|d| d.pointer(&poll.status_pointer)) {
                Some(Value::String(s)) => poll
                    .terminal_values
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(s)),
                // No status field (or non-string) on a 2xx ⇒ the job is done.
                _ => true,
            };
            if terminal {
                return result;
            }
        } else if !poll.pending_http_statuses.contains(&result.status) {
            // A genuine error (auth/not-found/etc.) — surface it, don't spin.
            return result;
        }
        last = Some(result);
        if attempt + 1 < max {
            futures_timer::Delay::new(poll.interval).await;
        }
    }
    let mut result = last.expect("poll loop runs at least once");
    result.warnings.push(format!(
        "--await: `{}` did not reach a terminal status after {max} attempt(s); returning the \
         last status response (poll again later)",
        status_op.id
    ));
    result
}

/// Failure of an out-of-band presigned-URL transfer.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("could not build the transfer request: {0}")]
    Build(String),
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
}

/// PUT `body` to a presigned upload URL (the `upload_uri` an `ImportContact` /
/// bulk-verification job returns). **No SendGrid bearer / on-behalf-of is sent** —
/// the URL carries its own authorization and the host is not SendGrid.
pub async fn external_upload<D: OperationDispatcher>(
    dispatcher: &D,
    client: &reqwest::Client,
    url: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Result<DispatchResponse, JobError> {
    let mut builder = client.put(url).body(body);
    if let Some(ct) = content_type {
        builder = builder.header(reqwest::header::CONTENT_TYPE, ct);
    }
    let req = builder
        .build()
        .map_err(|e| JobError::Build(e.to_string()))?;
    // Invariant: we never attach the SendGrid credential to a third-party host.
    debug_assert!(
        req.headers().get(reqwest::header::AUTHORIZATION).is_none(),
        "external upload must not carry the SendGrid bearer"
    );
    Ok(dispatcher.dispatch(req).await?)
}

/// GET a presigned download URL (the `presigned_url` / `urls` a CSV export job
/// returns). Same no-bearer guarantee as [`external_upload`].
pub async fn external_download<D: OperationDispatcher>(
    dispatcher: &D,
    client: &reqwest::Client,
    url: &str,
) -> Result<DispatchResponse, JobError> {
    let req = client
        .get(url)
        .build()
        .map_err(|e| JobError::Build(e.to_string()))?;
    debug_assert!(
        req.headers().get(reqwest::header::AUTHORIZATION).is_none(),
        "external download must not carry the SendGrid bearer"
    );
    Ok(dispatcher.dispatch(req).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::http;
    use crate::{ApiKey, Registry};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const KEY: &str = "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

    fn cfg() -> RuntimeConfig {
        RuntimeConfig::new(ApiKey::new(KEY))
    }

    struct QueueDispatcher {
        responses: Mutex<VecDeque<(u16, Value)>>,
        requests: Mutex<Vec<reqwest::Request>>,
    }
    impl QueueDispatcher {
        fn new(responses: Vec<(u16, Value)>) -> Self {
            QueueDispatcher {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }
    impl OperationDispatcher for QueueDispatcher {
        async fn dispatch(&self, req: reqwest::Request) -> Result<DispatchResponse, DispatchError> {
            let (code, body) = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("no more canned responses");
            self.requests.lock().unwrap().push(req);
            Ok(DispatchResponse {
                status: ::http::StatusCode::from_u16(code).unwrap(),
                headers: ::http::HeaderMap::new(),
                body,
            })
        }
    }

    fn zero_poll() -> PollConfig {
        PollConfig {
            interval: Duration::ZERO,
            max_attempts: 10,
            ..PollConfig::default()
        }
    }

    #[tokio::test]
    async fn await_job_polls_until_terminal_status() {
        let r = Registry::global();
        let status_op = r
            .by_id("sg_marketing_contacts_GetExportContact")
            .expect("GetExportContact");

        let dispatcher = QueueDispatcher::new(vec![
            (200, json!({ "id": "exp_1", "status": "pending" })),
            (200, json!({ "id": "exp_1", "status": "pending" })),
            (
                200,
                json!({ "id": "exp_1", "status": "ready", "urls": ["https://dl.example/x.csv"] }),
            ),
        ]);

        let args = json!({ "path": { "id": "exp_1" } });
        let result = await_job(&cfg(), status_op, args, &dispatcher, &zero_poll()).await;

        assert!(result.is_success());
        assert_eq!(result.data().unwrap()["status"], json!("ready"));
        assert_eq!(
            dispatcher.requests.lock().unwrap().len(),
            3,
            "polled until ready"
        );
    }

    #[tokio::test]
    async fn await_job_returns_real_error_immediately() {
        let r = Registry::global();
        let status_op = r
            .by_id("sg_marketing_contacts_GetExportContact")
            .expect("GetExportContact");

        // A 403 is NOT in pending_http_statuses → stop polling, surface it.
        let dispatcher = QueueDispatcher::new(vec![(403, json!({ "errors": [] }))]);
        let args = json!({ "path": { "id": "exp_1" } });
        let result = await_job(&cfg(), status_op, args, &dispatcher, &zero_poll()).await;

        assert!(!result.is_success());
        assert_eq!(result.status, 403);
        assert_eq!(dispatcher.requests.lock().unwrap().len(), 1, "did not spin");
    }

    #[tokio::test]
    async fn await_job_caps_and_warns_when_never_terminal() {
        let r = Registry::global();
        let status_op = r
            .by_id("sg_marketing_contacts_GetExportContact")
            .expect("GetExportContact");

        let pending = || (200u16, json!({ "id": "exp_1", "status": "pending" }));
        let mut poll = zero_poll();
        poll.max_attempts = 3;
        let dispatcher = QueueDispatcher::new(vec![pending(), pending(), pending()]);
        let args = json!({ "path": { "id": "exp_1" } });
        let result = await_job(&cfg(), status_op, args, &dispatcher, &poll).await;

        assert!(result.is_success(), "last response is still a 2xx");
        assert_eq!(dispatcher.requests.lock().unwrap().len(), 3, "hit the cap");
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("terminal status")),
            "cap warning present: {:?}",
            result.warnings
        );
    }

    #[tokio::test]
    async fn external_transfer_carries_no_bearer() {
        // THE security property: a presigned-URL transfer must never carry the
        // SendGrid credential (it would leak to a non-SendGrid host).
        let client = http::build_client();
        let dispatcher = QueueDispatcher::new(vec![(200, Value::Null), (200, json!("ok"))]);

        external_upload(
            &dispatcher,
            &client,
            "https://upload.example.com/presigned?sig=abc",
            Some("text/csv"),
            b"email\na@example.com\n".to_vec(),
        )
        .await
        .expect("upload dispatched");

        external_download(
            &dispatcher,
            &client,
            "https://dl.example.com/presigned?sig=xyz",
        )
        .await
        .expect("download dispatched");

        let reqs = dispatcher.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        for req in reqs.iter() {
            assert!(
                req.headers().get(reqwest::header::AUTHORIZATION).is_none(),
                "presigned transfer leaked an Authorization header"
            );
            assert!(
                req.headers().get("on-behalf-of").is_none(),
                "presigned transfer leaked on-behalf-of"
            );
        }
        // The upload (first request) is a PUT with the body + content-type.
        assert_eq!(reqs[0].method(), reqwest::Method::PUT);
        assert_eq!(
            reqs[0]
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .unwrap(),
            "text/csv"
        );
        assert_eq!(reqs[1].method(), reqwest::Method::GET);
    }
}
