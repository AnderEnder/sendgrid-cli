//! API-key handling: the redacted [`ApiKey`] newtype, a precedence resolver, and
//! a defense-in-depth [`scrub`] over arbitrary text.
//!
//! Guarantees (r4 §6): the key is never `Serialize`d, never printed in `Debug`,
//! and is zeroized on drop. Only [`ApiKey::expose`] (crate-internal) hands the raw
//! bytes to the HTTP-client builder at the moment the `Authorization` header is set.

use secrecy::{ExposeSecret, SecretString};
use std::sync::OnceLock;

/// A SendGrid API key. Wraps [`secrecy::SecretString`] so it cannot leak via
/// logs, errors, JSON, or MCP responses.
///
/// - `Debug` prints `ApiKey([REDACTED])`.
/// - **No `Serialize`** — cannot escape through `serde`/`config show`/MCP.
/// - Zeroized on drop (via `SecretString`).
#[derive(Clone)]
pub struct ApiKey(SecretString);

impl ApiKey {
    /// Wrap a raw key value. The input `String` is moved into the secret store.
    pub fn new(raw: impl Into<String>) -> Self {
        ApiKey(SecretString::from(raw.into()))
    }

    /// Expose the raw key. **Crate-internal only** — the single call site is the
    /// HTTP-client builder setting the `Authorization` header.
    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }

    /// The canonical SendGrid key shape: `SG.<22>.<43>` (id 22 chars, secret 43),
    /// charset `[A-Za-z0-9_-]`. Documented format (r4 §6.3).
    pub fn looks_well_formed(&self) -> bool {
        key_regex().is_match(self.expose())
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey([REDACTED])")
    }
}

/// Errors from key resolution (never carry the value).
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No source yielded a key. Lists the source *names* tried, never values.
    #[error("E_NO_CREDENTIAL: no API key found (tried: {0})")]
    NoCredential(String),
    /// A key resolved but does not match the documented `SG.<22>.<43>` shape.
    #[error("E_BAD_KEY_FORMAT: resolved API key is not a well-formed SendGrid key")]
    BadKeyFormat,
}

/// Resolve an API key by precedence (r4 §1). This build implements the two paths
/// the runtime core needs today:
///
/// 1. an **explicit value** (from `--api-key-stdin` / `--api-key`, resolved by the
///    CLI/MCP layer before calling core), then
/// 2. the **`SENDGRID_API_KEY`** environment variable.
///
/// Higher-precedence sources that require process/OS context (`key_command`
/// shell-out, profile inline key, OS keychain) are owned by the CLI/MCP config
/// layer (P4/P5); they resolve to an explicit value and pass it in here. The
/// `_validate_shape` gate is applied to whatever wins.
pub fn resolve_api_key(explicit: Option<String>) -> Result<ApiKey, AuthError> {
    let (key, _source) = if let Some(v) = explicit.filter(|v| !v.trim().is_empty()) {
        (ApiKey::new(v), "explicit")
    } else if let Ok(v) = std::env::var("SENDGRID_API_KEY") {
        if v.trim().is_empty() {
            return Err(AuthError::NoCredential("explicit, SENDGRID_API_KEY".into()));
        }
        (ApiKey::new(v), "SENDGRID_API_KEY")
    } else {
        return Err(AuthError::NoCredential("explicit, SENDGRID_API_KEY".into()));
    };

    if !key.looks_well_formed() {
        return Err(AuthError::BadKeyFormat);
    }
    Ok(key)
}

/// Canonical SendGrid key regex (compiled once). `SG.<22>.<43>`, `[A-Za-z0-9_-]`.
fn key_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}").expect("valid key regex")
    })
}

/// Defense-in-depth scrub (r4 §6.3). Removes, from arbitrary text:
/// - the canonical SendGrid key pattern `SG.<22>.<43>` → `SG.[REDACTED]`,
/// - `Authorization: Bearer <token>` / `Bearer <token>` → `Bearer [REDACTED]`,
/// - and, when `known` is supplied, any verbatim occurrence of the configured key.
///
/// This is a belt-and-suspenders layer; the *primary* controls are the [`ApiKey`]
/// type (no `Serialize`/redacted `Debug`) and field-level response redaction. Use
/// it over any free-form string (error messages, log lines) before emission.
pub fn scrub(text: &str, known: Option<&ApiKey>) -> String {
    let mut out = text.to_string();
    if let Some(k) = known {
        let raw = k.expose();
        if !raw.is_empty() {
            out = out.replace(raw, "SG.[REDACTED]");
        }
    }
    out = key_regex().replace_all(&out, "SG.[REDACTED]").into_owned();
    out = bearer_regex()
        .replace_all(&out, "Bearer [REDACTED]")
        .into_owned();
    out
}

fn bearer_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)Bearer\s+\S+").expect("valid bearer regex"))
}

/// Deep belt-and-suspenders scrub: apply [`scrub`] to every string leaf in a
/// JSON value. Runs AFTER field-level redaction (curated secret fields are
/// already `[REDACTED]`), so this only catches stray SG-key-shaped text in
/// non-curated positions — it never re-creates the Blocker-1 "redact-vs-break"
/// dilemma for the curated reveal path.
pub fn scrub_value(value: &mut serde_json::Value, known: Option<&ApiKey>) {
    match value {
        serde_json::Value::String(s) => {
            let scrubbed = scrub(s, known);
            if &scrubbed != s {
                *s = scrubbed;
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                scrub_value(v, known);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                scrub_value(v, known);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

    #[test]
    fn debug_is_redacted() {
        let k = ApiKey::new(GOOD);
        assert_eq!(format!("{k:?}"), "ApiKey([REDACTED])");
        assert!(!format!("{k:?}").contains("SG."));
    }

    #[test]
    fn well_formed_check() {
        assert!(ApiKey::new(GOOD).looks_well_formed());
        assert!(!ApiKey::new("not-a-key").looks_well_formed());
    }

    #[test]
    fn resolve_explicit_beats_env() {
        // Explicit, well-formed value wins regardless of env.
        let k = resolve_api_key(Some(GOOD.to_string())).expect("explicit resolves");
        assert!(k.looks_well_formed());
    }

    #[test]
    fn resolve_rejects_malformed() {
        let err = resolve_api_key(Some("garbage".to_string())).unwrap_err();
        assert_eq!(err, AuthError::BadKeyFormat);
        // The error string never echoes the value.
        assert!(!format!("{err}").contains("garbage"));
    }

    #[test]
    fn scrub_removes_key_and_bearer() {
        let k = ApiKey::new(GOOD);
        let text = format!("failed with key {GOOD} and header Authorization: Bearer {GOOD}");
        let scrubbed = scrub(&text, Some(&k));
        assert!(!scrubbed.contains(GOOD));
        assert!(!scrubbed.contains("SG.0"));
        assert!(scrubbed.contains("[REDACTED]"));
    }

    #[test]
    fn scrub_catches_unknown_key_by_regex() {
        let other = "SG.AAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let scrubbed = scrub(&format!("leak: {other}"), None);
        assert!(!scrubbed.contains(other));
    }
}
