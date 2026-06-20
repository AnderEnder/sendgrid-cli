//! Pre-flight validation: required-param presence (per location) + JSON-Schema
//! 2020-12 body validation against the embedded schema (`registry.schema_for`).
//!
//! Errors are **agent-actionable**: each carries a JSON-pointer-style `pointer`
//! into the offending location plus a precise message.
//!
//! Cross-field `constraints.toml` validation is **P4** — a hook is left
//! ([`ValidationReport`] is additive), but no cross-field rules run here.

use crate::ir::{Location, OperationIr};
use crate::registry::Registry;
use jsonschema::{Draft, JSONSchema};
use serde::Serialize;
use serde_json::{Map, Value};

/// A single validation problem, addressed by a JSON pointer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ValidationIssue {
    /// JSON pointer to the offending value, e.g. `/personalizations/0/to` for a
    /// body field, or `query/start_date` for a missing required param.
    pub pointer: String,
    /// Human/agent-readable description.
    pub message: String,
}

/// The result of validating an args envelope against an operation.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.issues.is_empty()
    }
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

/// Validate `args` (already coerced) against `op` using `registry` for the body
/// schema. Collects ALL issues (does not stop at the first).
pub fn validate(registry: &Registry, op: &OperationIr, args: &Value) -> ValidationReport {
    let mut report = ValidationReport::default();

    // 1. Required params present in the right bucket.
    let in_path = bucket(args, "path");
    let in_query = bucket(args, "query");
    let in_header = bucket(args, "header");
    for p in &op.params {
        if !p.required {
            continue;
        }
        let (present, loc_key) = match p.location {
            Location::Path => (in_path.contains_key(&p.name), "path"),
            Location::Query => (in_query.contains_key(&p.name), "query"),
            Location::Header => (in_header.contains_key(&p.name), "header"),
        };
        if !present {
            report.issues.push(ValidationIssue {
                pointer: format!("{loc_key}/{}", p.name),
                message: format!("missing required {loc_key} parameter `{}`", p.name),
            });
        }
    }

    // 2. Body schema validation (only when the op has a body schema AND a body
    //    is supplied; an absent body on a body op is caught by the schema's own
    //    `required` if the body is mandatory — we validate `{}` in that case).
    if op.has_body
        && let Some(schema) = registry.schema_for(op)
    {
        let body = args
            .get("body")
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        match compile(schema) {
            Ok(compiled) => {
                if let Err(errors) = compiled.validate(&body) {
                    for e in errors {
                        let ptr = e.instance_path.to_string();
                        let pointer = if ptr.is_empty() {
                            "body".to_string()
                        } else {
                            format!("body{ptr}")
                        };
                        report.issues.push(ValidationIssue {
                            pointer,
                            message: e.to_string(),
                        });
                    }
                }
            }
            Err(why) => {
                // A malformed embedded schema is a codegen bug, not caller input;
                // surface it loudly rather than silently skipping validation.
                report.issues.push(ValidationIssue {
                    pointer: "body".to_string(),
                    message: format!("internal: body schema failed to compile: {why}"),
                });
            }
        }
    }

    report
}

/// Compile a schema as Draft 2020-12 (the normalized dialect of the embedded
/// schemas). Compiled per call; the embedded schemas are small.
fn compile(schema: &Value) -> Result<JSONSchema, String> {
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_required_query_param_is_flagged() {
        let r = Registry::global();
        let op = r.by_id("sg_stats_global_ListBrowserStat").expect("op");
        // ListBrowserStat requires `start_date`.
        let report = validate(r, op, &json!({ "query": { "limit": 50 } }));
        assert!(!report.is_ok());
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.pointer == "query/start_date")
        );
    }

    #[test]
    fn valid_sendmail_body_passes() {
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ]
            }
        });
        let report = validate(r, op, &args);
        assert!(report.is_ok(), "expected valid, got {:?}", report.issues);
    }

    #[test]
    fn invalid_sendmail_body_is_rejected_with_pointer() {
        // Proves the jsonschema seam is live (not vacuously passing): omit the
        // required `to` inside a personalization.
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [ { "subject": "no recipients" } ]
            }
        });
        let report = validate(r, op, &args);
        assert!(!report.is_ok(), "expected schema rejection");
        assert!(
            report.issues.iter().any(|i| i.pointer.starts_with("body")),
            "expected a body pointer, got {:?}",
            report.issues
        );
    }
}
