//! Pre-flight validation: required-param presence (per location) + JSON-Schema
//! 2020-12 body validation against the embedded schema (`registry.schema_for`) +
//! cross-field [`Constraint`] enforcement (the spec-prose rules JSON Schema can't
//! express, from `data/constraints.toml`).
//!
//! Errors are **agent-actionable**: each carries a JSON-pointer-style `pointer`
//! into the offending location plus a precise message.
//!
//! Order: required params → body schema → cross-field constraints. Constraints run
//! AFTER schema validation and only inspect the body; all issues are collected (no
//! early stop), so an agent sees everything wrong in one pass.

use crate::ir::{Constraint, Location, OperationIr};
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
        // SECURITY (P6 item 1): jsonschema embeds the offending INSTANCE VALUE
        // verbatim in its error string, so a wrong-typed secret (e.g. a numeric or
        // object `password`) would leak into the `E_VALIDATION` envelope. Two guards:
        //   1. an error whose instance path passes through a secret field gets a
        //      generic message (no value at all);
        //   2. as defense-in-depth, any secret value present in the body is scrubbed
        //      out of every other message.
        // The body itself is NOT pre-redacted (that would pass `type:string` and lose
        // the validation), so this is the sole guard for this leak.
        let secret_values = collect_secret_request_values(&body, &op.secret_request_fields);
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
                        let in_secret_subtree = ptr.split('/').any(|seg| {
                            !seg.is_empty()
                                && op
                                    .secret_request_fields
                                    .iter()
                                    .any(|f| f.eq_ignore_ascii_case(seg))
                        });
                        let message = if in_secret_subtree {
                            format!(
                                "value at `{pointer}` failed body-schema validation \
                                 (value omitted: secret field)"
                            )
                        } else {
                            scrub_secret_values(&e.to_string(), &secret_values)
                        };
                        report.issues.push(ValidationIssue { pointer, message });
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

    // 3. Cross-field constraints (spec-prose rules; the API would otherwise 400).
    if !op.constraints().is_empty() {
        let body = args
            .get("body")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        check_constraints(op, &body, &mut report);
    }

    report
}

/// Collect the (serialized) values of every body field whose name is in
/// `secret_fields` (case-insensitive, deep). Both the compact-JSON form (what
/// jsonschema embeds for a non-string instance, e.g. `12345` or `{"a":"b"}`) and,
/// for string values, the unquoted inner form are returned, so either can be scrubbed
/// from an error message.
fn collect_secret_request_values(body: &Value, secret_fields: &[String]) -> Vec<String> {
    if secret_fields.is_empty() {
        return Vec::new();
    }
    let lower: Vec<String> = secret_fields
        .iter()
        .map(|f| f.to_ascii_lowercase())
        .collect();
    let mut out = Vec::new();
    collect_walk(body, &lower, &mut out);
    out
}

fn collect_walk(v: &Value, fields: &[String], out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if fields.contains(&k.to_ascii_lowercase()) && !val.is_null() {
                    out.push(val.to_string());
                    if let Value::String(s) = val {
                        out.push(s.clone());
                    }
                }
                collect_walk(val, fields, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_walk(val, fields, out);
            }
        }
        _ => {}
    }
}

/// Replace any occurrence of a known secret value in `message` with `[REDACTED]`.
fn scrub_secret_values(message: &str, secret_values: &[String]) -> String {
    let mut msg = message.to_string();
    for v in secret_values {
        if !v.is_empty() && msg.contains(v.as_str()) {
            msg = msg.replace(v.as_str(), "[REDACTED]");
        }
    }
    msg
}

/// A body field counts as **present** only when it is a non-`null`, non-empty value
/// (`""` and `[]` are absent — they wouldn't satisfy the API either).
fn is_present(body: &Value, field: &str) -> bool {
    match body.get(field) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    }
}

/// True when `array_field` is a non-empty array AND every element has `field`
/// present (the per-item escape hatch, e.g. each personalization has its own
/// `subject`).
fn present_in_each(body: &Value, array_field: &str, field: &str) -> bool {
    match body.get(array_field).and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr.iter().all(|el| is_present(el, field)),
        _ => false,
    }
}

/// Enforce the op's cross-field [`Constraint`]s against the body, appending an
/// actionable issue per violation.
fn check_constraints(op: &OperationIr, body: &Value, report: &mut ValidationReport) {
    for c in op.constraints() {
        match c {
            Constraint::RequiresOneOf { fields, message } => {
                if !fields.iter().any(|f| is_present(body, f)) {
                    report.issues.push(ValidationIssue {
                        pointer: "body".to_string(),
                        message: message
                            .clone()
                            .unwrap_or_else(|| format!("at least one of {fields:?} is required")),
                    });
                }
            }
            Constraint::MutuallyExclusive { fields, message } => {
                let present: Vec<&String> = fields.iter().filter(|f| is_present(body, f)).collect();
                if present.len() > 1 {
                    report.issues.push(ValidationIssue {
                        pointer: format!("body/{}", present[1]),
                        message: message.clone().unwrap_or_else(|| {
                            format!("at most one of {fields:?} may be set (found {present:?})")
                        }),
                    });
                }
            }
            Constraint::RequiredUnlessPresent {
                field,
                unless_present,
                or_each_in,
                message,
            } => {
                // Satisfied by: top-level `field`, OR `unless_present`, OR (when
                // `or_each_in` is set) `field` present in every element of that array.
                let satisfied = is_present(body, field)
                    || is_present(body, unless_present)
                    || or_each_in
                        .as_deref()
                        .is_some_and(|arr| present_in_each(body, arr, field));
                if !satisfied {
                    report.issues.push(ValidationIssue {
                        pointer: format!("body/{field}"),
                        message: message.clone().unwrap_or_else(|| {
                            format!("`{field}` is required unless `{unless_present}` is present")
                        }),
                    });
                }
            }
        }
    }
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
    fn sendmail_without_content_or_template_is_rejected_locally() {
        // M1: a body that PASSES the JSON schema (has from + personalizations[].to
        // + subject) but violates the prose rule "content OR template_id required".
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi"
                // no content, no template_id
            }
        });
        let report = validate(r, op, &args);
        assert!(!report.is_ok(), "expected a constraint rejection");
        let msg = report
            .issues
            .iter()
            .find(|i| i.message.contains("content") && i.message.contains("template_id"))
            .unwrap_or_else(|| panic!("no content/template_id issue: {:?}", report.issues));
        assert_eq!(msg.pointer, "body");
    }

    #[test]
    fn sendmail_reply_to_and_reply_to_list_are_mutually_exclusive() {
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "subject": "hi",
                "content": [ { "type": "text/plain", "value": "hello" } ],
                "reply_to": { "email": "a@example.com" },
                "reply_to_list": [ { "email": "b@example.com" } ]
            }
        });
        let report = validate(r, op, &args);
        assert!(!report.is_ok());
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.pointer == "body/reply_to_list"),
            "expected a mutually-exclusive issue, got {:?}",
            report.issues
        );
    }

    #[test]
    fn sendmail_with_template_id_but_no_subject_passes() {
        // required_unless_present: subject omitted is fine when a template supplies it.
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [ { "to": [ { "email": "c@example.net" } ] } ],
                "template_id": "d-abc123"
            }
        });
        let report = validate(r, op, &args);
        assert!(report.is_ok(), "expected valid, got {:?}", report.issues);
    }

    #[test]
    fn sendmail_batch_with_per_personalization_subject_passes() {
        // or_each_in: a valid batch send with NO top-level subject + NO template,
        // where every personalization carries its own subject, MUST pass (else M1
        // would create a new invalid-locally/valid-remotely false positive).
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [
                    { "to": [ { "email": "a@example.net" } ], "subject": "Invoice A" },
                    { "to": [ { "email": "b@example.net" } ], "subject": "Invoice B" }
                ],
                "content": [ { "type": "text/plain", "value": "hi" } ]
            }
        });
        let report = validate(r, op, &args);
        assert!(
            report.is_ok(),
            "valid batch send rejected: {:?}",
            report.issues
        );
    }

    #[test]
    fn sendmail_subject_rule_still_fires_when_a_personalization_lacks_subject() {
        // The escape hatch is "EVERY personalization has subject" — a mix (one
        // missing) with no top-level subject / template is still rejected.
        let r = Registry::global();
        let op = r.by_id("sg_mail_send_SendMail").expect("op");
        let args = json!({
            "body": {
                "from": { "email": "s@example.com" },
                "personalizations": [
                    { "to": [ { "email": "a@example.net" } ], "subject": "has one" },
                    { "to": [ { "email": "b@example.net" } ] }
                ],
                "content": [ { "type": "text/plain", "value": "hi" } ]
            }
        });
        let report = validate(r, op, &args);
        assert!(!report.is_ok(), "expected a subject rejection");
        assert!(
            report.issues.iter().any(|i| i.pointer == "body/subject"),
            "expected body/subject issue, got {:?}",
            report.issues
        );
    }

    #[test]
    fn wrong_typed_secret_value_is_not_in_the_issue_message() {
        // P6 item 1: jsonschema embeds the offending instance value verbatim; a
        // numeric/object `password` must NOT leak its value into the issue message.
        let r = Registry::global();
        let op = r
            .by_id("sg_account_subusers_CreateSubuser")
            .expect("CreateSubuser");
        for (secret, marker) in [
            (json!(918273645), "918273645"),
            (json!({ "leak": "SUPER-SECRET" }), "SUPER-SECRET"),
        ] {
            let args = json!({ "body": {
                "username": "sub1", "email": "sub1@example.com",
                "password": secret, "ips": ["1.2.3.4"]
            }});
            let report = validate(r, op, &args);
            assert!(!report.is_ok(), "expected a type rejection for {marker}");
            let serialized = serde_json::to_string(&report.issues).unwrap();
            assert!(
                !serialized.contains(marker),
                "secret value `{marker}` leaked into the issue(s): {serialized}"
            );
            assert!(
                report.issues.iter().any(|i| i.pointer == "body/password"),
                "expected a body/password issue, got {:?}",
                report.issues
            );
        }
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
