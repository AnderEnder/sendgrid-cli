//! **Backend D** — the generic request builder. Ported from the spike-proven
//! `build_request` (391/391 ops) and hardened: it takes a resolved base URL and a
//! governed `on-behalf-of` value (never a caller-supplied one), uses the redacted
//! [`ApiKey`] for bearer auth, and also emits a redaction-safe `request_preview`.
//!
//! Body pass-through: the caller supplies the JSON `body` verbatim; the embedded
//! schema is for validation, never for construction.

use super::auth::ApiKey;
use super::safety::redact_fields;
use crate::ir::{Location, OperationIr, ParamIr};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// A constructed request plus its redaction-safe preview.
pub(crate) struct BuiltRequest {
    pub request: reqwest::Request,
    /// `{method, url, headers (authorization+secret request-fields redacted), body?}`.
    pub preview: Value,
}

/// Failure to construct a request from `(op, args)`. These are caller/coercion
/// problems surfaced before any network I/O.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum BuildError {
    #[error("args must be a JSON object envelope {{path,query,header,body}}")]
    NotAnObject,
    #[error("unsubstituted path placeholder remains in `{0}`")]
    UnsubstitutedPath(String),
    #[error("invalid HTTP method `{0}`")]
    BadMethod(String),
    #[error("could not build request: {0}")]
    Reqwest(String),
}

fn empty_map() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn bucket<'a>(args: &'a Value, key: &str) -> &'a Map<String, Value> {
    args.get(key)
        .and_then(Value::as_object)
        .unwrap_or_else(|| empty_map())
}

/// Build the `reqwest::Request` for `op` from the (coerced, sanitized, validated)
/// `args` envelope. `base_url` is already region-resolved. `governed_obo`, when
/// `Some`, is the allow-list-validated impersonation value to inject.
pub(crate) fn build_request(
    client: &reqwest::Client,
    op: &OperationIr,
    args: &Value,
    api_key: &ApiKey,
    base_url: &str,
    governed_obo: Option<&str>,
) -> Result<BuiltRequest, BuildError> {
    if !args.is_object() {
        return Err(BuildError::NotAnObject);
    }
    let in_path = bucket(args, "path");
    let in_query = bucket(args, "query");
    let in_header = bucket(args, "header");
    let in_body = args.get("body");

    // 1. Path-param substitution (percent-encoded).
    let mut path = op.path.clone();
    for (name, val) in in_path {
        let placeholder = format!("{{{name}}}");
        path = path.replace(&placeholder, &percent_encode_path(&scalar_to_string(val)));
    }
    if path.contains('{') {
        return Err(BuildError::UnsubstitutedPath(path));
    }
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);

    // 2. Method + base builder.
    let method = reqwest::Method::from_bytes(op.method.as_bytes())
        .map_err(|_| BuildError::BadMethod(op.method.clone()))?;
    let mut req = client.request(method.clone(), &url);

    // 3. Query params (array style/explode driven by the IR).
    let query_pairs = build_query_pairs(&op.params, in_query);
    if !query_pairs.is_empty() {
        req = req.query(&query_pairs);
    }

    // 4. Auth.
    req = req.bearer_auth(api_key.expose());

    // 5. Governed impersonation header (only source for on-behalf-of).
    if let Some(obo) = governed_obo {
        req = req.header("on-behalf-of", obo);
    }

    // 6. Declared header params (already sanitized of on-behalf-of/authorization).
    for (name, val) in in_header {
        req = req.header(name, scalar_to_string(val));
    }

    // 7. Body pass-through.
    if op.has_body
        && let Some(body) = in_body
    {
        req = req.json(body);
    }

    let request = req
        .build()
        .map_err(|e| BuildError::Reqwest(e.to_string()))?;
    let preview = build_preview(&request, op, in_body);
    Ok(BuiltRequest { request, preview })
}

/// Build query (name, value) pairs honoring array style/explode (OpenAPI default
/// `style=form, explode=true`; SendGrid sets `explode=false` on most arrays →
/// comma-joined). Extracted for direct unit testing of both branches.
pub(crate) fn build_query_pairs(
    params: &[ParamIr],
    query: &Map<String, Value>,
) -> Vec<(String, String)> {
    let mut declared: HashMap<&str, &ParamIr> = HashMap::new();
    for p in params {
        if p.location == Location::Query {
            declared.insert(p.name.as_str(), p);
        }
    }
    let mut pairs = Vec::new();
    for (name, val) in query {
        let decl = declared.get(name.as_str()).copied();
        let is_array = decl
            .map(|p| p.ty == "array")
            .unwrap_or_else(|| val.is_array());
        if is_array {
            // Default explode=true for the form style; SendGrid mostly sets false.
            let explode = decl.and_then(|p| p.explode).unwrap_or(true);
            match val.as_array() {
                Some(arr) if explode => {
                    for item in arr {
                        pairs.push((name.clone(), scalar_to_string(item)));
                    }
                }
                Some(arr) => {
                    let joined = arr
                        .iter()
                        .map(scalar_to_string)
                        .collect::<Vec<_>>()
                        .join(",");
                    pairs.push((name.clone(), joined));
                }
                None => pairs.push((name.clone(), scalar_to_string(val))),
            }
        } else {
            pairs.push((name.clone(), scalar_to_string(val)));
        }
    }
    pairs
}

/// Derive the redaction-safe preview from the built request (Authorization →
/// `Bearer [REDACTED]`) and the body (`secret_request_fields` redacted).
fn build_preview(request: &reqwest::Request, op: &OperationIr, body: Option<&Value>) -> Value {
    let mut headers = Map::new();
    for (name, value) in request.headers() {
        let shown = if name == reqwest::header::AUTHORIZATION {
            "Bearer [REDACTED]".to_string()
        } else {
            value.to_str().unwrap_or("<non-utf8>").to_string()
        };
        headers.insert(name.as_str().to_string(), Value::String(shown));
    }

    let mut preview = Map::new();
    preview.insert(
        "method".into(),
        Value::String(request.method().as_str().to_string()),
    );
    preview.insert(
        "url".into(),
        Value::String(request.url().as_str().to_string()),
    );
    preview.insert("headers".into(), Value::Object(headers));
    if op.has_body
        && let Some(b) = body
    {
        let mut redacted = b.clone();
        redact_fields(&mut redacted, &op.secret_request_fields);
        preview.insert("body".into(), redacted);
    }
    Value::Object(preview)
}

/// Coerce a JSON value to its query/path string form.
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Minimal percent-encoding for a path-segment value (RFC 3986 unreserved kept).
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Location;

    fn array_param(name: &str, explode: Option<bool>) -> ParamIr {
        ParamIr {
            name: name.into(),
            location: Location::Query,
            required: false,
            ty: "array".into(),
            item_ty: Some("string".into()),
            format: None,
            style: Some("form".into()),
            explode,
            description: None,
        }
    }

    #[test]
    fn array_query_explode_false_comma_joins() {
        let params = vec![array_param("ids", Some(false))];
        let mut q = Map::new();
        q.insert("ids".into(), serde_json::json!(["a", "b", "c"]));
        let pairs = build_query_pairs(&params, &q);
        assert_eq!(pairs, vec![("ids".to_string(), "a,b,c".to_string())]);
    }

    #[test]
    fn array_query_explode_true_repeats() {
        let params = vec![array_param("ids", Some(true))];
        let mut q = Map::new();
        q.insert("ids".into(), serde_json::json!(["a", "b"]));
        let pairs = build_query_pairs(&params, &q);
        assert_eq!(
            pairs,
            vec![
                ("ids".to_string(), "a".to_string()),
                ("ids".to_string(), "b".to_string())
            ]
        );
    }

    #[test]
    fn array_query_explode_default_is_repeat() {
        // explode = None → defaults to true (form style) → repeated keys.
        let params = vec![array_param("ids", None)];
        let mut q = Map::new();
        q.insert("ids".into(), serde_json::json!(["x", "y"]));
        let pairs = build_query_pairs(&params, &q);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|(k, _)| k == "ids"));
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode_path("a b/c"), "a%20b%2Fc");
        assert_eq!(percent_encode_path("HkJ5-_.~"), "HkJ5-_.~");
    }

    // ALL 391 ops build directly (validation bypassed: build_request never runs the
    // body schema). Synthesize a dummy per path param + an empty body. Asserts no
    // BuildError and no unsubstituted `{` in any URL — the literal "391/391 build"
    // claim, exercising the 118 body ops the execute()+dry_run harness misses.
    #[test]
    fn all_391_ops_build_directly_no_unsubstituted_path() {
        use crate::Registry;
        let client = crate::runtime::http::shared_client();
        let key = ApiKey::new(
            "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123",
        );
        let mut failures = Vec::new();
        let r = Registry::global();
        for op in r.operations() {
            let mut path = Map::new();
            for p in &op.params {
                if p.location == Location::Path {
                    path.insert(p.name.clone(), Value::String("SYNTH".into()));
                }
            }
            let mut envelope = Map::new();
            envelope.insert("path".into(), Value::Object(path));
            if op.has_body {
                // A trivial body so the pass-through has something to serialize.
                envelope.insert("body".into(), serde_json::json!({}));
            }
            let args = Value::Object(envelope);
            match build_request(client, op, &args, &key, "https://api.sendgrid.com", None) {
                Ok(built) => {
                    let url = built.request.url().as_str();
                    if url.contains('{') {
                        failures.push(format!("{}: unsubstituted '{{' in {url}", op.id));
                    }
                }
                Err(e) => failures.push(format!("{}: {e}", op.id)),
            }
        }
        assert_eq!(r.operations().len(), 391);
        assert!(
            failures.is_empty(),
            "{} build failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
