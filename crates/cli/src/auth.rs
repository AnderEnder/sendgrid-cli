//! `sendgrid auth <scopes|whoami|doctor>` — credential, region, and scope
//! introspection so an agent can verify its setup before firing real operations.
//!
//! - **scopes** runs the live `GET /v3/scopes` op through the normal `execute()`
//!   chokepoint and renders it with the standard output machinery (honors
//!   `--output`/`--query`).
//! - **whoami** adds the active region + a *redacted, non-reversible* key
//!   fingerprint (never the key itself) on top of the granted scopes.
//! - **doctor** is resilient by design: it reports key presence/format and region
//!   availability as *checks* (proceeding even when the key is missing/malformed)
//!   and only attempts the live scopes call when a usable key is present.

use crate::globals::GlobalOpts;
use crate::output;
use sendgrid_core::ir::OperationIr;
use sendgrid_core::{ApiKey, Region, Registry, execute};
use serde_json::{Value, json};

/// Locate the `GET /v3/scopes` operation by HTTP path + method (the brief's
/// contract), independent of its CLI path or generated id.
fn scopes_op() -> Option<&'static OperationIr> {
    Registry::global()
        .operations()
        .iter()
        .find(|op| op.method == "GET" && op.path == "/v3/scopes")
}

/// Resolve the raw key string by the same precedence `core` uses (explicit
/// `--api-key`, then `SENDGRID_API_KEY`). Returned only to derive a fingerprint
/// and a well-formedness check — it is never emitted.
fn resolve_raw_key(globals: &GlobalOpts) -> Option<String> {
    if let Some(k) = globals.api_key.as_ref().filter(|v| !v.trim().is_empty()) {
        return Some(k.clone());
    }
    std::env::var("SENDGRID_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// A stable, non-reversible 64-bit FNV-1a fingerprint of the key. Lets an operator
/// confirm *which* key is configured without ever revealing key material.
fn fingerprint(raw: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in raw.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a:{h:016x}")
}

/// The `api_key` block shared by whoami/doctor: presence, well-formedness, a
/// fingerprint, and the length — but never any key bytes.
fn key_report(raw: Option<&str>) -> Value {
    match raw {
        Some(r) => json!({
            "present": true,
            "well_formed": ApiKey::new(r.to_string()).looks_well_formed(),
            "fingerprint": fingerprint(r),
            "length": r.chars().count(),
        }),
        None => json!({ "present": false, "well_formed": false }),
    }
}

fn region_str(region: Region) -> &'static str {
    match region {
        Region::Global => "global",
        Region::Eu => "eu",
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// `auth scopes` — run the live scopes op and render it like any other op.
pub async fn scopes(globals: &GlobalOpts) -> i32 {
    let Some(op) = scopes_op() else {
        eprintln!("error: GET /v3/scopes operation is not present in the registry");
        return 70;
    };
    let cfg = match globals.runtime_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };
    let result = execute(&cfg, op, json!({})).await;
    output::render(&result, globals)
}

/// `auth whoami` — region + redacted fingerprint + granted scopes.
pub async fn whoami(globals: &GlobalOpts) -> i32 {
    let raw = resolve_raw_key(globals);
    let region = globals.region;
    let mut report = json!({
        "region": region_str(region),
        "base_url": region.base_url(),
        "api_key": key_report(raw.as_deref()),
    });

    if raw.is_none() {
        report["scopes_error"] =
            json!("no API key configured (set SENDGRID_API_KEY or pass --api-key)");
        println!("{}", pretty(&report));
        return 64;
    }

    let cfg = match globals.runtime_config() {
        Ok(c) => c,
        Err(e) => {
            report["scopes_error"] = json!(e.to_string());
            println!("{}", pretty(&report));
            return 64;
        }
    };
    let Some(op) = scopes_op() else {
        report["scopes_error"] = json!("GET /v3/scopes not in registry");
        println!("{}", pretty(&report));
        return 70;
    };

    let result = execute(&cfg, op, json!({})).await;
    let exit = if result.is_success() {
        report["scopes"] = result.data().cloned().unwrap_or(Value::Null);
        0
    } else {
        report["scopes_error"] = result.error().cloned().unwrap_or(Value::Null);
        result.exit_code
    };
    for w in &result.warnings {
        eprintln!("warning: {w}");
    }
    println!("{}", pretty(&report));
    exit
}

fn check(name: &str, pass: bool, detail: impl Into<String>) -> Value {
    json!({ "name": name, "status": if pass { "pass" } else { "fail" }, "detail": detail.into() })
}

fn check_status(name: &str, status: &str, detail: impl Into<String>) -> Value {
    json!({ "name": name, "status": status, "detail": detail.into() })
}

/// `auth doctor` — resilient setup diagnostics. Never bails on a missing/bad key;
/// reports every check and exits non-zero if any check FAILED.
pub async fn doctor(globals: &GlobalOpts) -> i32 {
    let raw = resolve_raw_key(globals);
    let present = raw.is_some();
    let well_formed = raw
        .as_ref()
        .map(|r| ApiKey::new(r.clone()).looks_well_formed())
        .unwrap_or(false);

    let mut checks = vec![check(
        "api_key_present",
        present,
        if present {
            "an API key is configured"
        } else {
            "no API key found — set SENDGRID_API_KEY (preferred) or pass --api-key"
        },
    )];
    if present {
        checks.push(check(
            "api_key_well_formed",
            well_formed,
            if well_formed {
                "matches the documented SG.<22>.<43> shape"
            } else {
                "does NOT match SG.<22>.<43> — the key will be rejected before any request"
            },
        ));
    }

    // Region availability for the configured region.
    let region = globals.region;
    let global_only = Registry::global()
        .operations()
        .iter()
        .filter(|o| o.region_global_only)
        .count();
    let scopes = scopes_op();
    let region_ok = match region {
        Region::Global => true,
        // EU is "available" for the scopes flow as long as the scopes op has an EU
        // endpoint (it does); the global-only count is informational.
        Region::Eu => scopes.map(|o| !o.region_global_only).unwrap_or(false),
    };
    let region_detail = match region {
        Region::Global => format!(
            "region=global ({}) — all {} operations are reachable",
            region.base_url(),
            Registry::global().len()
        ),
        Region::Eu => format!(
            "region=eu ({}) — {global_only} operation(s) are global-only and fail closed in EU \
             unless --allow-region-fallback is set",
            region.base_url()
        ),
    };
    checks.push(check("region_available", region_ok, region_detail));

    // Live scopes call — only when a usable key is present.
    let mut scopes_value = Value::Null;
    if present && well_formed {
        match globals.runtime_config() {
            Ok(cfg) => match scopes {
                Some(op) => {
                    let result = execute(&cfg, op, json!({})).await;
                    for w in &result.warnings {
                        eprintln!("warning: {w}");
                    }
                    if result.is_success() {
                        scopes_value = result.data().cloned().unwrap_or(Value::Null);
                        let n = scopes_value
                            .get("scopes")
                            .and_then(Value::as_array)
                            .map(|a| a.len());
                        checks.push(check_status(
                            "scopes_call",
                            "pass",
                            match n {
                                Some(n) => format!("GET /v3/scopes returned {n} scope(s)"),
                                None => "GET /v3/scopes returned 200".to_string(),
                            },
                        ));
                    } else {
                        scopes_value = result.error().cloned().unwrap_or(Value::Null);
                        checks.push(check_status(
                            "scopes_call",
                            "fail",
                            format!("GET /v3/scopes failed (HTTP {})", result.status),
                        ));
                    }
                }
                None => checks.push(check_status(
                    "scopes_call",
                    "fail",
                    "GET /v3/scopes not in registry",
                )),
            },
            Err(e) => checks.push(check_status(
                "scopes_call",
                "fail",
                format!("could not build runtime config: {e}"),
            )),
        }
    } else {
        checks.push(check_status(
            "scopes_call",
            "skip",
            "skipped — requires a present, well-formed key",
        ));
    }

    let ok = checks.iter().all(|c| c["status"] != "fail");
    let report = json!({
        "ok": ok,
        "region": region_str(region),
        "base_url": region.base_url(),
        "api_key": key_report(raw.as_deref()),
        "checks": checks,
        "scopes": scopes_value,
    });
    println!("{}", pretty(&report));
    if ok { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

    #[test]
    fn finds_scopes_op() {
        let op = scopes_op().expect("GET /v3/scopes present");
        assert_eq!(op.method, "GET");
        assert_eq!(op.path, "/v3/scopes");
    }

    #[test]
    fn fingerprint_is_stable_and_hides_the_key() {
        let fp = fingerprint(GOOD);
        assert_eq!(fp, fingerprint(GOOD), "deterministic");
        assert!(fp.starts_with("fnv1a:"));
        assert!(!fp.contains(GOOD));
        assert!(!fp.contains("SG.0"));
        assert_ne!(fp, fingerprint("SG.different"));
    }

    #[test]
    fn key_report_present_vs_absent() {
        let present = key_report(Some(GOOD));
        assert_eq!(present["present"], json!(true));
        assert_eq!(present["well_formed"], json!(true));
        assert!(
            present["fingerprint"]
                .as_str()
                .unwrap()
                .starts_with("fnv1a:")
        );
        // The full key must never appear anywhere in the report.
        assert!(!serde_json::to_string(&present).unwrap().contains(GOOD));

        let absent = key_report(None);
        assert_eq!(absent["present"], json!(false));
        assert!(absent.get("fingerprint").is_none());
    }

    #[test]
    fn region_strings() {
        assert_eq!(region_str(Region::Global), "global");
        assert_eq!(region_str(Region::Eu), "eu");
    }
}
