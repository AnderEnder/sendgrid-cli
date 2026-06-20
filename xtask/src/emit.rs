//! Deterministic emit — serialize the IR + schema map to JSON with **all object
//! keys recursively sorted**, so identical inputs yield byte-identical output (the
//! idempotence gate + the regen-diff-as-review both depend on this). The recursive
//! sort makes determinism independent of whether `serde_json`'s `preserve_order`
//! feature is enabled anywhere in the workspace.

use anyhow::{Context, Result};
use sendgrid_core::ir::OperationIr;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// Recursively rebuild every object with its keys in sorted order.
pub fn sort_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = Map::new();
            for (k, val) in sorted {
                out.insert(k.clone(), sort_value(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// Render `Vec<OperationIr>` to the deterministic `ir.json` string (trailing NL).
pub fn render_ir(ops: &[OperationIr]) -> Result<String> {
    let value = serde_json::to_value(ops).context("serialize ops to Value")?;
    let sorted = sort_value(&value);
    let mut s = serde_json::to_string_pretty(&sorted)?;
    s.push('\n');
    Ok(s)
}

/// Render the schema map to the deterministic `schemas.json` string (trailing NL).
pub fn render_schemas(schemas: &BTreeMap<String, Value>) -> Result<String> {
    let value = serde_json::to_value(schemas).context("serialize schemas to Value")?;
    let sorted = sort_value(&value);
    let mut s = serde_json::to_string_pretty(&sorted)?;
    s.push('\n');
    Ok(s)
}

/// Write both artifacts to `generated_dir`. Returns `(wrote_ir, wrote_schemas)`
/// indicating whether each file's bytes actually changed.
pub fn write_artifacts(
    generated_dir: &Path,
    ops: &[OperationIr],
    schemas: &BTreeMap<String, Value>,
) -> Result<(bool, bool)> {
    std::fs::create_dir_all(generated_dir)
        .with_context(|| format!("create {}", generated_dir.display()))?;
    let ir_path = generated_dir.join("ir.json");
    let schemas_path = generated_dir.join("schemas.json");

    let ir = render_ir(ops)?;
    let sch = render_schemas(schemas)?;

    let wrote_ir = write_if_changed(&ir_path, &ir)?;
    let wrote_schemas = write_if_changed(&schemas_path, &sch)?;
    Ok((wrote_ir, wrote_schemas))
}

fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    let current = std::fs::read_to_string(path).ok();
    if current.as_deref() == Some(contents) {
        return Ok(false);
    }
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}
