//! Request/INPUT schema resolution + normalization to JSON Schema 2020-12.
//!
//! Operates on a spec's raw root `Value` (zero-loss). For a request-body schema:
//!   1. **Recursively resolve** every `$ref` so the emitted schema is self-contained
//!      (no residual `#/components/...` refs that a standalone 2020-12 validator
//!      can't follow). Recon proved 0 cycles across all 143 body ops; a cycle is
//!      defended against (`{}`) and recorded so codegen can fail loudly if one ever
//!      appears (we'd switch to `$defs` bundling).
//!   2. **Normalize 3.0-isms** (these specs are 3.1-stamped but 3.0-shaped):
//!      - `nullable:true` + sibling scalar `type` → `type:[<t>,"null"]`
//!      - `nullable:true` + sibling array `type`   → append `"null"`
//!      - `nullable:true` with NO sibling `type`    → `{"type":"null"}`
//!      - `nullable:false`                          → drop the keyword (default)
//!      - singular `example`                        → `examples:[<example>]`

use crate::specs::Stats;
use serde_json::{Map, Value};

/// Recursively resolve all `$ref`s and normalize a single input schema node.
pub fn resolve_and_normalize(root: &Value, node: &Value, stats: &mut Stats) -> Value {
    let mut stack: Vec<String> = Vec::new();
    resolve(root, node, &mut stack, stats)
}

fn resolve(root: &Value, node: &Value, stack: &mut Vec<String>, stats: &mut Stats) -> Value {
    match node {
        Value::Object(map) => {
            if let Some(ref_str) = map.get("$ref").and_then(Value::as_str) {
                let ref_owned = ref_str.to_string();
                if stack.contains(&ref_owned) {
                    stats.cycles.push(ref_owned);
                    return Value::Object(Map::new()); // {} = accept-anything; breaks the cycle
                }
                let target = ref_owned
                    .strip_prefix('#')
                    .and_then(|p| root.pointer(p))
                    .cloned();
                let Some(target) = target else {
                    stats.unresolved_refs.push(ref_owned);
                    return Value::Object(Map::new());
                };
                stack.push(ref_owned);
                let resolved = resolve(root, &target, stack, stats);
                stack.pop();
                // Merge any siblings beside `$ref` (3.1/2020-12: siblings are applied).
                let mut out = match resolved {
                    Value::Object(m) => m,
                    other => return other,
                };
                for (k, v) in map {
                    if k != "$ref" {
                        out.insert(k.clone(), resolve(root, v, stack, stats));
                    }
                }
                normalize_object(out)
            } else {
                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), resolve(root, v, stack, stats));
                }
                normalize_object(out)
            }
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| resolve(root, v, stack, stats)).collect())
        }
        other => other.clone(),
    }
}

/// Apply the 3.0→2020-12 keyword rewrites to one (already child-resolved) object.
fn normalize_object(mut map: Map<String, Value>) -> Value {
    // nullable -> type union / {"type":"null"}. The `map.remove` happens regardless
    // of the bool (nullable:false is simply dropped — non-nullable is the default).
    if let Some(nullable) = map.remove("nullable")
        && nullable.as_bool() == Some(true)
    {
        match map.get_mut("type") {
            Some(Value::String(s)) => {
                let base = s.clone();
                if base != "null" {
                    map.insert(
                        "type".to_string(),
                        Value::Array(vec![Value::String(base), Value::String("null".into())]),
                    );
                }
            }
            Some(Value::Array(types)) => {
                if !types.iter().any(|t| t.as_str() == Some("null")) {
                    types.push(Value::String("null".into()));
                }
            }
            _ => {
                map.insert("type".to_string(), Value::String("null".into()));
            }
        }
    }

    // singular `example` -> `examples: [..]` (only if `examples` not already set).
    if let Some(example) = map.remove("example") {
        map.entry("examples".to_string())
            .or_insert_with(|| Value::Array(vec![example]));
    }

    Value::Object(map)
}

/// Collect the top-level property names of a resolved object schema (for bulk-trigger
/// field verification). Empty for array/scalar bodies.
pub fn top_level_property_names(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// True when a normalized schema carries no usable constraint — an empty object
/// `{}` (an unresolved/cyclic `$ref` collapses to this) or a non-object. Used to
/// avoid embedding a degenerate success-response schema (e.g. a `204`).
pub fn is_empty_schema(schema: &Value) -> bool {
    match schema {
        Value::Object(m) => m.is_empty(),
        _ => true,
    }
}

/// True when the resolved schema's top-level type is `array` (handles both scalar
/// `"array"` and a `["array", ...]` union).
pub fn is_array_schema(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(s)) => s == "array",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("array")),
        _ => false,
    }
}
