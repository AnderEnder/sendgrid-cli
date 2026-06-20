//! The safety chokepoint: side-effect [`Policy`], bulk-trigger detection,
//! always-on header sanitization + governed `on-behalf-of`, and always-on secret
//! redaction (response fields + request-preview fields).
//!
//! Security posture notes (from `.research-notes/review-security.md`):
//! - **`confirm` is NOT a security control** and is intentionally absent here. It
//!   gates only an interactive human path (CLI); an autonomous agent can self-set
//!   it, so it never bypasses [`Policy`]. The allow-list is the sole effective gate.
//! - **Header sanitization is always-on**, independent of policy: a caller-supplied
//!   `on-behalf-of`/`authorization` in the args `header` bucket is stripped, and the
//!   header is set **only** from a governed value validated against `allowed_subusers`.
//! - **Secret redaction is always-on**: `secret_response_fields` are redacted from
//!   response `data`, `secret_request_fields` from any `request_preview`.

use crate::ir::{BulkLocation, OperationIr, SideEffect};
use serde_json::Value;

const READ: u8 = 1 << 0;
const WRITE: u8 = 1 << 1;
const DESTRUCTIVE: u8 = 1 << 2;
const SEND: u8 = 1 << 3;

fn bit(e: SideEffect) -> u8 {
    match e {
        SideEffect::Read => READ,
        SideEffect::Write => WRITE,
        SideEffect::Destructive => DESTRUCTIVE,
        SideEffect::Send => SEND,
    }
}

/// The set of [`SideEffect`] classes a deployment permits at the chokepoint.
///
/// **DEFAULT = ALL classes allowed** (an explicit user/team-lead decision that
/// overrides the security memo's read-only recommendation). The mechanism is
/// intact: an operator who wants the locked-down posture uses [`Policy::read_only`]
/// (or [`Policy::from_classes`]). Because destructiveness is encoded semantically
/// in the IR (`EraseRecipientEmailData` is `Destructive`, not `Write`), a
/// read-only policy genuinely blocks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    allowed: u8,
}

impl Policy {
    /// All four classes allowed. This is the default.
    pub fn all() -> Self {
        Policy {
            allowed: READ | WRITE | DESTRUCTIVE | SEND,
        }
    }

    /// Nothing allowed (deny-all).
    pub fn none() -> Self {
        Policy { allowed: 0 }
    }

    /// Only `Read` allowed — the security-memo's recommended locked-down posture.
    pub fn read_only() -> Self {
        Policy { allowed: READ }
    }

    /// Build from an explicit set of classes. **`Read` is always implied**: a
    /// restricted policy like `--allow write` becomes `{Read, Write}`, never
    /// `{Write}` alone — read access is what powers the discovery/verify loop an
    /// agent relies on (a write-only policy would silently break it). To deny reads
    /// too, use [`Policy::none`] (deny-all).
    pub fn from_classes(classes: impl IntoIterator<Item = SideEffect>) -> Self {
        let mut allowed = READ;
        for c in classes {
            allowed |= bit(c);
        }
        Policy { allowed }
    }

    /// Add a class (builder-style).
    pub fn with(mut self, e: SideEffect) -> Self {
        self.allowed |= bit(e);
        self
    }

    /// Whether `e` is permitted.
    pub fn allows(&self, e: SideEffect) -> bool {
        self.allowed & bit(e) != 0
    }
}

impl Default for Policy {
    fn default() -> Self {
        // User decision: default to ALL classes allowed (see type docs).
        Policy::all()
    }
}

/// A safety refusal, with a stable machine `code` and a clear message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyDenial {
    pub code: &'static str,
    pub message: String,
}

/// Enforce the side-effect policy. `dry_run` calls bypass the gate (nothing is
/// sent), per the runtime contract.
pub fn check_policy(op: &OperationIr, policy: &Policy, dry_run: bool) -> Result<(), SafetyDenial> {
    if dry_run || policy.allows(op.side_effect) {
        return Ok(());
    }
    Err(SafetyDenial {
        code: "E_POLICY_DENIED",
        message: format!(
            "operation `{}` is class `{:?}`, which the active policy does not allow",
            op.id, op.side_effect
        ),
    })
}

/// Detect a `bulk` promotion: if any of the op's `bulk_triggers` matches the
/// actual args, require `allow_bulk`. Matching is stringly (so a JSON `true` and
/// the string `"true"` both match a `"true"` trigger).
pub fn check_bulk(op: &OperationIr, args: &Value, allow_bulk: bool) -> Result<(), SafetyDenial> {
    if op.bulk_triggers.is_empty() || allow_bulk {
        return Ok(());
    }
    for trig in &op.bulk_triggers {
        let bucket_key = match trig.location {
            BulkLocation::Query => "query",
            BulkLocation::Body => "body",
        };
        let actual = args.get(bucket_key).and_then(|b| b.get(&trig.field));
        if let Some(v) = actual
            && stringly_eq(v, &trig.value)
        {
            return Err(SafetyDenial {
                code: "E_BULK_NOT_ALLOWED",
                message: format!(
                    "operation `{}` is a bulk action (`{}={}` in {bucket_key}); set allow_bulk to permit it",
                    op.id, trig.field, trig.value
                ),
            });
        }
    }
    Ok(())
}

fn stringly_eq(v: &Value, want: &str) -> bool {
    match v {
        Value::String(s) => s.eq_ignore_ascii_case(want),
        Value::Bool(b) => b.to_string().eq_ignore_ascii_case(want),
        Value::Number(n) => n.to_string() == want,
        _ => false,
    }
}

/// **Always-on** header sanitization (independent of policy). Strips any
/// caller-supplied `on-behalf-of` or `authorization` from the args `header`
/// bucket (case-insensitive), so impersonation can only be set by the governed
/// path. Returns the names that were stripped (for an optional warning).
pub fn sanitize_headers(args: &mut Value) -> Vec<String> {
    let mut stripped = Vec::new();
    if let Some(Value::Object(headers)) = args.get_mut("header") {
        let to_remove: Vec<String> = headers
            .keys()
            .filter(|k| {
                let k = k.to_ascii_lowercase();
                k == "on-behalf-of" || k == "authorization"
            })
            .cloned()
            .collect();
        for k in to_remove {
            headers.remove(&k);
            stripped.push(k);
        }
    }
    stripped
}

/// Resolve the governed `on-behalf-of` value to inject (or `None`). The value is
/// sent **only if** it is present in `allowed_subusers` (exact match, which
/// covers both the `<username>` and `account-id <id>` forms an operator lists).
/// An empty allow-list means impersonation is disabled.
pub fn resolve_on_behalf_of(
    on_behalf_of: Option<&str>,
    allowed_subusers: &[String],
) -> Result<Option<String>, SafetyDenial> {
    let Some(value) = on_behalf_of.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if allowed_subusers.iter().any(|a| a == value) {
        Ok(Some(value.to_string()))
    } else {
        Err(SafetyDenial {
            code: "E_IMPERSONATION_NOT_ALLOWED",
            message: format!(
                "on-behalf-of `{value}` is not in the configured allowed_subusers list \
                 ({} entr{})",
                allowed_subusers.len(),
                if allowed_subusers.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            ),
        })
    }
}

/// Deep-redact every object key whose name (case-insensitive) is in `fields`,
/// anywhere in `value`. Returns the number of values redacted.
pub fn redact_fields(value: &mut Value, fields: &[String]) -> usize {
    if fields.is_empty() {
        return 0;
    }
    let lower: Vec<String> = fields.iter().map(|f| f.to_ascii_lowercase()).collect();
    redact_walk(value, &lower)
}

fn redact_walk(value: &mut Value, fields: &[String]) -> usize {
    let mut n = 0;
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if fields.contains(&k.to_ascii_lowercase()) && !v.is_null() {
                    *v = Value::String("[REDACTED]".to_string());
                    n += 1;
                } else {
                    n += redact_walk(v, fields);
                }
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                n += redact_walk(v, fields);
            }
        }
        _ => {}
    }
    n
}

/// Convenience: redact `secret_response_fields` from a response `data` value.
pub fn redact_response(op: &OperationIr, data: &mut Value) -> usize {
    redact_fields(data, &op.secret_response_fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;
    use serde_json::json;

    #[test]
    fn default_policy_allows_all_but_read_only_blocks_destructive() {
        let r = Registry::global();
        let erase = r
            .operations()
            .iter()
            .find(|o| o.operation_id == "EraseRecipientEmailData")
            .expect("erase op");
        assert_eq!(erase.side_effect, SideEffect::Destructive);
        // Default = ALL → allowed.
        assert!(check_policy(erase, &Policy::default(), false).is_ok());
        // read_only → blocked (capability intact).
        let denial = check_policy(erase, &Policy::read_only(), false).unwrap_err();
        assert_eq!(denial.code, "E_POLICY_DENIED");
        // dry_run bypasses the gate.
        assert!(check_policy(erase, &Policy::read_only(), true).is_ok());
    }

    #[test]
    fn from_classes_always_implies_read() {
        // `--allow write` → {Read, Write}: Read is implied so the discovery loop
        // keeps working; Write is granted; Destructive/Send stay denied.
        let p = Policy::from_classes([SideEffect::Write]);
        assert!(p.allows(SideEffect::Read), "Read must be implied");
        assert!(p.allows(SideEffect::Write));
        assert!(!p.allows(SideEffect::Destructive));
        assert!(!p.allows(SideEffect::Send));

        // `--allow send` → {Read, Send}.
        let p = Policy::from_classes([SideEffect::Send]);
        assert!(p.allows(SideEffect::Read));
        assert!(p.allows(SideEffect::Send));
        assert!(!p.allows(SideEffect::Write));

        // read_only() is still exactly {Read}.
        let ro = Policy::read_only();
        assert!(ro.allows(SideEffect::Read));
        assert!(!ro.allows(SideEffect::Write));
        assert!(!ro.allows(SideEffect::Destructive));
        assert!(!ro.allows(SideEffect::Send));

        // none() is still deny-all (NOT touched by the implied-Read rule).
        let n = Policy::none();
        assert!(!n.allows(SideEffect::Read));
        assert!(!n.allows(SideEffect::Write));
    }

    #[test]
    fn caller_on_behalf_of_is_stripped() {
        let mut args = json!({
            "header": { "on-behalf-of": "victim", "On-Behalf-Of": "victim2", "x-keep": "ok" }
        });
        let stripped = sanitize_headers(&mut args);
        assert_eq!(stripped.len(), 2);
        assert!(args["header"].get("on-behalf-of").is_none());
        assert!(args["header"].get("On-Behalf-Of").is_none());
        assert_eq!(args["header"]["x-keep"], json!("ok"));
    }

    #[test]
    fn governed_obo_requires_allowlist() {
        // Not in list → rejected.
        let denial = resolve_on_behalf_of(Some("marketing"), &[]).unwrap_err();
        assert_eq!(denial.code, "E_IMPERSONATION_NOT_ALLOWED");
        // In list (account-id form) → injected.
        let allowed = vec!["account-id 42".to_string(), "brand-a".to_string()];
        assert_eq!(
            resolve_on_behalf_of(Some("account-id 42"), &allowed).unwrap(),
            Some("account-id 42".to_string())
        );
        // None → no injection.
        assert_eq!(resolve_on_behalf_of(None, &allowed).unwrap(), None);
    }

    #[test]
    fn bulk_trigger_detected_in_query() {
        let r = Registry::global();
        let op = r
            .operations()
            .iter()
            .find(|o| o.operation_id == "DeleteMarketingList")
            .expect("op with delete_contacts trigger");
        // delete_contacts=true (query) → bulk.
        let args = json!({ "path": { "id": "1" }, "query": { "delete_contacts": true } });
        let denial = check_bulk(op, &args, false).unwrap_err();
        assert_eq!(denial.code, "E_BULK_NOT_ALLOWED");
        // allow_bulk bypasses.
        assert!(check_bulk(op, &args, true).is_ok());
        // Without the trigger value → not bulk.
        let args2 = json!({ "path": { "id": "1" } });
        assert!(check_bulk(op, &args2, false).is_ok());
    }

    #[test]
    fn deep_redaction() {
        let mut data = json!({
            "api_key": "SG.realkey.value",
            "nested": { "password": "hunter2", "ok": "keep" },
            "list": [ { "client_secret": "s3cr3t" } ]
        });
        let n = redact_fields(
            &mut data,
            &["api_key".into(), "password".into(), "client_secret".into()],
        );
        assert_eq!(n, 3);
        assert_eq!(data["api_key"], json!("[REDACTED]"));
        assert_eq!(data["nested"]["password"], json!("[REDACTED]"));
        assert_eq!(data["nested"]["ok"], json!("keep"));
        assert_eq!(data["list"][0]["client_secret"], json!("[REDACTED]"));
    }
}
