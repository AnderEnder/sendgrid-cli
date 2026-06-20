//! The uniform result envelope (r5 §2/§3, brief item 11).
//!
//! `{ status, side_effect, exit_code, code?, request_preview?, next?, warnings?,
//!   data | error }`. SendGrid error bodies are passed **verbatim** under `error`;
//! pre-flight/runtime failures use a structured `{code, message}` error plus a
//! stable top-level `code`. Status → exit-code class follows r5 §3.

use super::dispatch::DispatchError;
use crate::ir::SideEffect;
use serde::Serialize;
use serde_json::{Value, json};

/// Success/error payload. Serializes inline as `{"data": …}` or `{"error": …}`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Payload {
    Data(Value),
    Error(Value),
}

/// The result of [`super::execute`]. Always returned (errors are encoded, not
/// thrown) so CLI/MCP consumers branch on `exit_code` / `code` / `status`.
#[derive(Debug, Clone, Serialize)]
pub struct ExecuteResult {
    /// HTTP status of the (final) response; `0` when no request was sent
    /// (dry-run or a pre-flight failure).
    pub status: u16,
    /// The operation's side-effect class.
    pub side_effect: SideEffect,
    /// Process exit-code class (r5 §3).
    pub exit_code: i32,
    /// Stable machine code for pre-flight/runtime failures (`E_*`). `None` on
    /// success and on verbatim HTTP error passthrough (there, `status` is the
    /// signal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The constructed request (key + secret request-fields redacted). Always set
    /// on dry-run; set on pre-flight errors where a request was built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_preview: Option<Value>,
    /// Continuation hint when `--all` stopped at a cap (`{param: value}` etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Value>,
    /// Non-fatal warnings (e.g. region fallback).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// `data` on success, `error` on failure.
    #[serde(flatten)]
    pub payload: Payload,
}

impl ExecuteResult {
    /// A successful response (`data` = redacted body).
    pub fn success(status: u16, side_effect: SideEffect, data: Value) -> Self {
        ExecuteResult {
            status,
            side_effect,
            exit_code: exit_code_for_status(status),
            code: None,
            request_preview: None,
            next: None,
            warnings: Vec::new(),
            payload: Payload::Data(data),
        }
    }

    /// A non-2xx HTTP response — SendGrid's error body passed verbatim.
    pub fn http_error(status: u16, side_effect: SideEffect, body: Value) -> Self {
        ExecuteResult {
            status,
            side_effect,
            exit_code: exit_code_for_status(status),
            code: None,
            request_preview: None,
            next: None,
            warnings: Vec::new(),
            payload: Payload::Error(body),
        }
    }

    /// A pre-flight/runtime failure (validation, safety, region, build) with a
    /// stable `E_*` code. `exit_code` defaults to the usage class (64).
    pub fn preflight_error(
        code: impl Into<String>,
        side_effect: SideEffect,
        message: impl Into<String>,
    ) -> Self {
        let code = code.into();
        ExecuteResult {
            status: 0,
            side_effect,
            exit_code: 64,
            code: Some(code.clone()),
            request_preview: None,
            next: None,
            warnings: Vec::new(),
            payload: Payload::Error(json!({ "code": code, "message": message.into() })),
        }
    }

    /// A transport failure (network/timeout). Exit class 8.
    pub fn network_error(side_effect: SideEffect, err: &DispatchError) -> Self {
        ExecuteResult {
            status: 0,
            side_effect,
            exit_code: 8,
            code: Some("E_NETWORK".into()),
            request_preview: None,
            next: None,
            warnings: Vec::new(),
            payload: Payload::Error(json!({ "code": "E_NETWORK", "message": err.to_string() })),
        }
    }

    /// A dry-run result: the redacted preview, nothing sent.
    pub fn dry_run(side_effect: SideEffect, preview: Value) -> Self {
        ExecuteResult {
            status: 0,
            side_effect,
            exit_code: 0,
            code: None,
            request_preview: Some(preview),
            next: None,
            warnings: Vec::new(),
            payload: Payload::Data(json!({ "dry_run": true })),
        }
    }

    /// Attach a validation report's issues as a structured pre-flight error.
    pub fn validation_error(side_effect: SideEffect, issues: Value) -> Self {
        ExecuteResult {
            status: 0,
            side_effect,
            exit_code: 64,
            code: Some("E_VALIDATION".into()),
            request_preview: None,
            next: None,
            warnings: Vec::new(),
            payload: Payload::Error(json!({ "code": "E_VALIDATION", "issues": issues })),
        }
    }

    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings.extend(warnings);
        self
    }

    pub fn with_request_preview(mut self, preview: Value) -> Self {
        self.request_preview = Some(preview);
        self
    }

    pub fn with_next(mut self, next: Option<Value>) -> Self {
        self.next = next;
        self
    }

    /// True when the call produced a 2xx response (or a successful dry-run).
    pub fn is_success(&self) -> bool {
        matches!(self.payload, Payload::Data(_))
    }

    /// The success data, if any.
    pub fn data(&self) -> Option<&Value> {
        match &self.payload {
            Payload::Data(v) => Some(v),
            Payload::Error(_) => None,
        }
    }

    /// The error body, if any (verbatim SendGrid body or structured `{code,…}`).
    pub fn error(&self) -> Option<&Value> {
        match &self.payload {
            Payload::Error(v) => Some(v),
            Payload::Data(_) => None,
        }
    }
}

/// Map an HTTP status to the r5 §3 exit-code class. A GET that returns 202 is a
/// 2xx → success → 0 (the "treat GET-with-202 as success" rule needs nothing
/// special here since all 2xx map to 0).
pub fn exit_code_for_status(status: u16) -> i32 {
    match status {
        200..=299 => 0,
        400 | 405 | 413 => 1,
        401 => 3,
        403 => 4,
        404 => 5,
        429 => 6,
        500..=599 => 7,
        _ => 1, // any other 4xx → client error class
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_mapping() {
        assert_eq!(exit_code_for_status(200), 0);
        assert_eq!(exit_code_for_status(202), 0);
        assert_eq!(exit_code_for_status(401), 3);
        assert_eq!(exit_code_for_status(403), 4);
        assert_eq!(exit_code_for_status(404), 5);
        assert_eq!(exit_code_for_status(429), 6);
        assert_eq!(exit_code_for_status(503), 7);
    }

    #[test]
    fn payload_serializes_inline() {
        let ok = ExecuteResult::success(200, SideEffect::Read, json!({"x": 1}));
        let s = serde_json::to_value(&ok).unwrap();
        assert_eq!(s["data"], json!({"x": 1}));
        assert_eq!(s["status"], json!(200));
        assert!(s.get("error").is_none());

        let err = ExecuteResult::preflight_error("E_X", SideEffect::Write, "nope");
        let s = serde_json::to_value(&err).unwrap();
        assert_eq!(s["error"]["code"], json!("E_X"));
        assert_eq!(s["code"], json!("E_X"));
    }
}
