//! `cargo xtask drift` — **semantic** spec-drift detection.
//!
//! The 46 vendored `specs/*.json` all stamp `info.version = "1.0.0"`, so the pin
//! lives in `specs.lock` (an upstream git SHA). Upstream self-regenerates with
//! cosmetic churn (key/whitespace reordering), so a naive byte-diff is useless.
//!
//! This compares a freshly-fetched upstream spec set against the vendored set:
//!   1. **Canonicalize** each spec — parse JSON, recursively sort every object's
//!      keys (via [`crate::emit::sort_value`], the same routine codegen emits with).
//!      This kills cosmetic churn while preserving array order (params, enums, …).
//!   2. The **gate / source of truth** is whole-spec canonical-string equality: any
//!      post-canonicalization difference is real semantic drift.
//!   3. For each changed spec, produce a best-effort **operation-set breakdown**
//!      (added / removed / changed ops, keyed by `operationId`) for the changelog.
//!      A change the op-set can't explain — a shared `#/components/schemas/*` target,
//!      `servers`, `security`, etc. — surfaces as the spec's `other_changes` flag.
//!
//! `run` exits non-zero when any drift is detected, so CI can gate on it.
//!
//! Note on hashing: the 64-bit fingerprints are **display-only** — every
//! added/removed/changed decision is made on canonical-string equality, so the gate
//! has no collision surface. Hashes are only ever compared within a single process
//! run (never persisted), so a stdlib `DefaultHasher` is sufficient and stable enough.

use crate::emit::sort_value;
use crate::specs_dir;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// One operation's stable identity plus a display fingerprint of its canonical
/// `(method, path, op-node)`.
#[derive(Serialize, Clone, Debug)]
pub struct OpRef {
    pub operation_id: String,
    pub method: String,
    pub path: String,
    /// Hex `u64` fingerprint of the canonical `(method, path, op-node)`. Display only.
    pub fingerprint: String,
}

/// An operation whose identity is unchanged but whose canonical content differs.
#[derive(Serialize, Clone, Debug)]
pub struct OpChange {
    pub operation_id: String,
    pub method: String,
    pub path: String,
    /// Vendored (old) fingerprint.
    pub old: String,
    /// Upstream (new) fingerprint.
    pub new: String,
}

/// The semantic diff of a single spec file that exists in both sets.
#[derive(Serialize, Debug)]
pub struct SpecDiff {
    pub spec: String,
    pub added_ops: Vec<OpRef>,
    pub removed_ops: Vec<OpRef>,
    pub changed_ops: Vec<OpChange>,
    /// `true` when the whole-spec canonical content differs but no op-level signal
    /// explains it (shared components, `servers`, `security`, `info`, …).
    pub other_changes: bool,
}

/// The full drift report across the two spec sets.
#[derive(Serialize, Debug)]
pub struct DriftReport {
    pub vendored_count: usize,
    pub upstream_count: usize,
    /// Filenames present upstream but not vendored.
    pub added_specs: Vec<String>,
    /// Filenames vendored but gone upstream.
    pub removed_specs: Vec<String>,
    /// Files present in both whose canonical content differs.
    pub changed_specs: Vec<SpecDiff>,
    pub drift_detected: bool,
}

/// Stdlib hash → fixed-width hex. Display only (see module docs).
fn hash_hex(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Canonical (recursively key-sorted) serialization of a JSON value. Two values
/// that differ only by key order / whitespace produce identical strings.
fn canonical_string(v: &Value) -> String {
    serde_json::to_string(&sort_value(v)).unwrap_or_default()
}

/// Extract every operation from a spec root, keyed by `operationId` (falling back
/// to `"METHOD path"` when an op has none). The value is the op's identity record
/// plus the canonical `(method, path, op-node)` string used for change detection.
fn extract_ops(root: &Value) -> BTreeMap<String, (OpRef, String)> {
    let mut out = BTreeMap::new();
    let Some(paths) = root.get("paths").and_then(Value::as_object) else {
        return out;
    };
    for (path, item) in paths {
        for &m in &METHODS {
            let Some(op_node) = item.get(m) else { continue };
            let method = m.to_uppercase();
            // Identity + content are bound together: a path move under a stable
            // operationId still registers as "changed" because path is in the hash.
            let wrapper = serde_json::json!({
                "method": method,
                "path": path,
                "op": op_node,
            });
            let canon = canonical_string(&wrapper);
            let operation_id = op_node
                .get("operationId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let key = if operation_id.is_empty() {
                format!("{method} {path}")
            } else {
                operation_id.clone()
            };
            let op_ref = OpRef {
                operation_id,
                method,
                path: path.clone(),
                fingerprint: hash_hex(&canon),
            };
            out.insert(key, (op_ref, canon));
        }
    }
    out
}

/// Sort key for op vectors: deterministic, stable week-to-week.
fn op_sort_key(method: &str, path: &str, id: &str) -> (String, String, String) {
    (method.to_string(), path.to_string(), id.to_string())
}

/// Diff one spec that exists in both sets. Returns `None` when canonical content is
/// identical (no semantic drift), `Some` with the op-set breakdown otherwise.
fn diff_spec(spec: &str, vendored: &Value, upstream: &Value) -> Option<SpecDiff> {
    // Gate / source of truth: canonical-string equality catches *any* semantic
    // change, including shared-component edits an op-node hash would miss.
    if canonical_string(vendored) == canonical_string(upstream) {
        return None;
    }

    let v_ops = extract_ops(vendored);
    let u_ops = extract_ops(upstream);

    let mut added_ops = Vec::new();
    let mut removed_ops = Vec::new();
    let mut changed_ops = Vec::new();

    for (key, (op_ref, _)) in &u_ops {
        if !v_ops.contains_key(key) {
            added_ops.push(op_ref.clone());
        }
    }
    for (key, (v_ref, v_canon)) in &v_ops {
        match u_ops.get(key) {
            None => removed_ops.push(v_ref.clone()),
            Some((u_ref, u_canon)) => {
                if v_canon != u_canon {
                    changed_ops.push(OpChange {
                        operation_id: u_ref.operation_id.clone(),
                        method: u_ref.method.clone(),
                        path: u_ref.path.clone(),
                        old: v_ref.fingerprint.clone(),
                        new: u_ref.fingerprint.clone(),
                    });
                }
            }
        }
    }

    added_ops.sort_by(|a, b| {
        op_sort_key(&a.method, &a.path, &a.operation_id).cmp(&op_sort_key(
            &b.method,
            &b.path,
            &b.operation_id,
        ))
    });
    removed_ops.sort_by(|a, b| {
        op_sort_key(&a.method, &a.path, &a.operation_id).cmp(&op_sort_key(
            &b.method,
            &b.path,
            &b.operation_id,
        ))
    });
    changed_ops.sort_by(|a, b| {
        op_sort_key(&a.method, &a.path, &a.operation_id).cmp(&op_sort_key(
            &b.method,
            &b.path,
            &b.operation_id,
        ))
    });

    // We are here only because the whole-spec canonical content differs. If no op
    // signal explains it, the change is in shared components / metadata.
    let other_changes = added_ops.is_empty() && removed_ops.is_empty() && changed_ops.is_empty();

    Some(SpecDiff {
        spec: spec.to_string(),
        added_ops,
        removed_ops,
        changed_ops,
        other_changes,
    })
}

/// Pure, in-memory diff of two spec sets (filename → root JSON). Fully testable;
/// no disk IO. Both maps are keyed by spec filename (e.g. `tsg_alerts_v3.json`).
pub fn diff_spec_sets(
    vendored: &BTreeMap<String, Value>,
    upstream: &BTreeMap<String, Value>,
) -> DriftReport {
    let mut added_specs: Vec<String> = upstream
        .keys()
        .filter(|k| !vendored.contains_key(*k))
        .cloned()
        .collect();
    let mut removed_specs: Vec<String> = vendored
        .keys()
        .filter(|k| !upstream.contains_key(*k))
        .cloned()
        .collect();
    added_specs.sort();
    removed_specs.sort();

    let mut changed_specs = Vec::new();
    for (name, v) in vendored {
        if let Some(u) = upstream.get(name)
            && let Some(diff) = diff_spec(name, v, u)
        {
            changed_specs.push(diff);
        }
    }
    changed_specs.sort_by(|a, b| a.spec.cmp(&b.spec));

    let drift_detected =
        !added_specs.is_empty() || !removed_specs.is_empty() || !changed_specs.is_empty();

    DriftReport {
        vendored_count: vendored.len(),
        upstream_count: upstream.len(),
        added_specs,
        removed_specs,
        changed_specs,
        drift_detected,
    }
}

/// Read every `*.json` in `dir` into a filename-keyed map of raw JSON values.
pub fn load_spec_set(dir: &Path) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("read spec dir {}", dir.display()))?;
    for e in entries {
        let p = e?.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("non-UTF8 filename {}", p.display()))?
            .to_string();
        let bytes = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        let v: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse JSON {}", p.display()))?;
        out.insert(name, v);
    }
    if out.is_empty() {
        bail!("no `*.json` specs found in {}", dir.display());
    }
    Ok(out)
}

/// Render the human-readable operation-set changelog.
pub fn render_human(r: &DriftReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "spec-drift report");
    let _ = writeln!(s, "  vendored specs: {}", r.vendored_count);
    let _ = writeln!(s, "  upstream specs: {}", r.upstream_count);
    let _ = writeln!(s);

    if !r.drift_detected {
        let _ = writeln!(
            s,
            "  no semantic drift — vendored specs match upstream after canonicalization."
        );
        return s;
    }

    if !r.added_specs.is_empty() {
        let _ = writeln!(
            s,
            "  added specs (upstream, not vendored) ({}):",
            r.added_specs.len()
        );
        for n in &r.added_specs {
            let _ = writeln!(s, "    + {n}");
        }
        let _ = writeln!(s);
    }
    if !r.removed_specs.is_empty() {
        let _ = writeln!(
            s,
            "  removed specs (vendored, gone upstream) ({}):",
            r.removed_specs.len()
        );
        for n in &r.removed_specs {
            let _ = writeln!(s, "    - {n}");
        }
        let _ = writeln!(s);
    }
    if !r.changed_specs.is_empty() {
        let _ = writeln!(s, "  changed specs ({}):", r.changed_specs.len());
        for d in &r.changed_specs {
            let _ = writeln!(s, "    ~ {}", d.spec);
            for op in &d.added_ops {
                let _ = writeln!(
                    s,
                    "        + op  {:<6} {}  [{}]  {}",
                    op.method, op.path, op.operation_id, op.fingerprint
                );
            }
            for op in &d.removed_ops {
                let _ = writeln!(
                    s,
                    "        - op  {:<6} {}  [{}]  {}",
                    op.method, op.path, op.operation_id, op.fingerprint
                );
            }
            for op in &d.changed_ops {
                let _ = writeln!(
                    s,
                    "        ~ op  {:<6} {}  [{}]  {} -> {}",
                    op.method, op.path, op.operation_id, op.old, op.new
                );
            }
            if d.other_changes {
                let _ = writeln!(
                    s,
                    "        (shared components / metadata changed — no op-level signal)"
                );
            }
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "DRIFT DETECTED. To adopt upstream (maintainer steps):");
    let _ = writeln!(
        s,
        "  1. Re-vendor: copy the fetched upstream `spec/json/*.json` over `specs/`."
    );
    let _ = writeln!(
        s,
        "  2. Bump `specs.lock` `rev` to the upstream HEAD SHA (and `spec_count` if it changed)."
    );
    let _ = writeln!(s, "  3. Regenerate: `cargo run -p xtask -- codegen`.");
    let _ = writeln!(
        s,
        "  4. Review the diff under `crates/core/generated/`, run `cargo test`, then commit."
    );
    s
}

/// Binary entry point for `cargo xtask drift`. Returns `Ok(0)` when no drift,
/// `Ok(1)` when drift is detected; any `Err` (bad path, malformed JSON) is mapped
/// by `main` to exit code `2`, so CI can distinguish drift from a tool failure.
///
/// Flags: `--upstream <dir>` (required) `[--vendored <dir>]` `[--json]`.
pub fn run(args: &[String]) -> Result<i32> {
    let mut upstream: Option<String> = None;
    let mut vendored: Option<String> = None;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--upstream" => {
                i += 1;
                upstream = Some(args.get(i).context("--upstream needs a <dir>")?.clone());
            }
            "--vendored" => {
                i += 1;
                vendored = Some(args.get(i).context("--vendored needs a <dir>")?.clone());
            }
            "--json" => json = true,
            other => bail!(
                "unknown `drift` flag {other:?} \
                 (usage: cargo xtask drift --upstream <dir> [--vendored <dir>] [--json])"
            ),
        }
        i += 1;
    }

    let upstream = upstream.context(
        "`drift` requires --upstream <dir> (the freshly-fetched upstream `spec/json` directory)",
    )?;
    let upstream_dir = PathBuf::from(upstream);
    let vendored_dir = vendored.map(PathBuf::from).unwrap_or_else(specs_dir);

    let v = load_spec_set(&vendored_dir)?;
    let u = load_spec_set(&upstream_dir)?;
    let report = diff_spec_sets(&v, &u);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }
    use std::io::Write;
    std::io::stdout().flush().ok();

    Ok(i32::from(report.drift_detected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a minimal spec root from `(method, path, operationId, marker)` tuples.
    /// `marker` perturbs op content so we can simulate "changed" ops.
    fn spec_with_ops(ops: &[(&str, &str, &str, &str)]) -> Value {
        let mut paths = serde_json::Map::new();
        for (method, path, op_id, marker) in ops {
            let item = paths
                .entry((*path).to_string())
                .or_insert_with(|| json!({}));
            item[*method] = json!({ "operationId": op_id, "summary": marker });
        }
        json!({ "openapi": "3.1.0", "paths": Value::Object(paths) })
    }

    fn set(name: &str, root: Value) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(name.to_string(), root);
        m
    }

    #[test]
    fn canonicalization_is_order_insensitive() {
        let a = json!({ "a": 1, "b": { "x": 1, "y": [3, 2, 1] } });
        let b = json!({ "b": { "y": [3, 2, 1], "x": 1 }, "a": 1 });
        // Same content, different key order → identical canonical strings.
        assert_eq!(canonical_string(&a), canonical_string(&b));
        // Array order is preserved (semantically significant), so reordering it differs.
        let c = json!({ "a": 1, "b": { "x": 1, "y": [1, 2, 3] } });
        assert_ne!(canonical_string(&a), canonical_string(&c));
    }

    #[test]
    fn reordered_keys_produce_no_drift() {
        let vendored = spec_with_ops(&[("get", "/v3/alerts", "listAlerts", "m")]);
        // Same op, but wrapped object keys come out in a different source order.
        let upstream = json!({
            "paths": { "/v3/alerts": { "get": { "summary": "m", "operationId": "listAlerts" } } },
            "openapi": "3.1.0"
        });
        let r = diff_spec_sets(&set("s.json", vendored), &set("s.json", upstream));
        assert!(!r.drift_detected, "cosmetic key reorder must not be drift");
        assert!(r.changed_specs.is_empty());
    }

    #[test]
    fn added_op_is_detected() {
        let vendored = spec_with_ops(&[("get", "/v3/alerts", "listAlerts", "m")]);
        let upstream = spec_with_ops(&[
            ("get", "/v3/alerts", "listAlerts", "m"),
            ("post", "/v3/alerts", "createAlert", "m"),
        ]);
        let r = diff_spec_sets(&set("s.json", vendored), &set("s.json", upstream));
        assert!(r.drift_detected);
        let d = &r.changed_specs[0];
        assert_eq!(d.added_ops.len(), 1);
        assert_eq!(d.added_ops[0].operation_id, "createAlert");
        assert!(d.removed_ops.is_empty() && d.changed_ops.is_empty());
        assert!(!d.other_changes);
    }

    #[test]
    fn removed_op_is_detected() {
        let vendored = spec_with_ops(&[
            ("get", "/v3/alerts", "listAlerts", "m"),
            ("delete", "/v3/alerts/{id}", "deleteAlert", "m"),
        ]);
        let upstream = spec_with_ops(&[("get", "/v3/alerts", "listAlerts", "m")]);
        let r = diff_spec_sets(&set("s.json", vendored), &set("s.json", upstream));
        assert!(r.drift_detected);
        let d = &r.changed_specs[0];
        assert_eq!(d.removed_ops.len(), 1);
        assert_eq!(d.removed_ops[0].operation_id, "deleteAlert");
    }

    #[test]
    fn changed_op_is_detected() {
        let vendored = spec_with_ops(&[("get", "/v3/alerts/{id}", "getAlert", "old-summary")]);
        let upstream = spec_with_ops(&[("get", "/v3/alerts/{id}", "getAlert", "new-summary")]);
        let r = diff_spec_sets(&set("s.json", vendored), &set("s.json", upstream));
        assert!(r.drift_detected);
        let d = &r.changed_specs[0];
        assert_eq!(d.changed_ops.len(), 1);
        assert_eq!(d.changed_ops[0].operation_id, "getAlert");
        assert_ne!(d.changed_ops[0].old, d.changed_ops[0].new);
        assert!(d.added_ops.is_empty() && d.removed_ops.is_empty());
    }

    #[test]
    fn added_and_removed_specs_are_detected() {
        let mut vendored = BTreeMap::new();
        vendored.insert(
            "keep.json".to_string(),
            spec_with_ops(&[("get", "/a", "a", "m")]),
        );
        vendored.insert(
            "gone.json".to_string(),
            spec_with_ops(&[("get", "/b", "b", "m")]),
        );
        let mut upstream = BTreeMap::new();
        upstream.insert(
            "keep.json".to_string(),
            spec_with_ops(&[("get", "/a", "a", "m")]),
        );
        upstream.insert(
            "new.json".to_string(),
            spec_with_ops(&[("get", "/c", "c", "m")]),
        );
        let r = diff_spec_sets(&vendored, &upstream);
        assert!(r.drift_detected);
        assert_eq!(r.added_specs, vec!["new.json".to_string()]);
        assert_eq!(r.removed_specs, vec!["gone.json".to_string()]);
        assert!(r.changed_specs.is_empty(), "the shared spec is identical");
    }

    #[test]
    fn shared_component_change_surfaces_as_other_changes() {
        // Op nodes are byte-identical; only a shared component schema differs.
        let base = |t: &str| {
            json!({
                "openapi": "3.1.0",
                "paths": { "/v3/x": { "get": { "operationId": "getX",
                    "responses": { "200": { "content": { "application/json": {
                        "schema": { "$ref": "#/components/schemas/X" } } } } } } } },
                "components": { "schemas": { "X": { "type": "object",
                    "properties": { "field": { "type": t } } } } }
            })
        };
        let r = diff_spec_sets(
            &set("s.json", base("string")),
            &set("s.json", base("integer")),
        );
        assert!(r.drift_detected, "shared component change must still gate");
        let d = &r.changed_specs[0];
        assert!(d.added_ops.is_empty() && d.removed_ops.is_empty() && d.changed_ops.is_empty());
        assert!(
            d.other_changes,
            "must flag as a non-op (shared component) change"
        );
    }

    #[test]
    fn identical_sets_report_no_drift() {
        let v = set("s.json", spec_with_ops(&[("get", "/a", "a", "m")]));
        let u = set("s.json", spec_with_ops(&[("get", "/a", "a", "m")]));
        let r = diff_spec_sets(&v, &u);
        assert!(!r.drift_detected);
        assert!(render_human(&r).contains("no semantic drift"));
    }
}
