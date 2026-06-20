//! Spec parsing — `serde_json::Value`-based (the spike-proven 46/46 path). Ports
//! the spike's `$ref` resolver, the two-level `requestBody → requestBodies/X →
//! schema` indirection, path-item + operation param merge, and array style/explode.
//!
//! Produces the raw HTTP shape per operation; taxonomy/safety/pagination merge and
//! schema normalization happen in [`crate::build`] / [`crate::schema`].

use anyhow::{Context, Result, bail};
use sendgrid_core::ir::{Location, ParamIr};
use serde_json::{Map, Value};
use std::path::Path;

/// Resolution diagnostics accumulated across the whole build. A healthy build has
/// both empty; codegen/L1 treat any entry as a hard failure.
#[derive(Default, Debug)]
pub struct Stats {
    /// `$ref` strings that did not resolve to a node in their document.
    pub unresolved_refs: Vec<String>,
    /// `$ref` strings that formed a cycle during recursive schema inlining.
    pub cycles: Vec<String>,
}

/// One parsed spec file: its namespace, the raw root (kept for schema resolution),
/// and the raw operations.
pub struct SpecFile {
    pub namespace: String,
    pub stem: String,
    pub root: Value,
    pub ops: Vec<RawOp>,
}

/// The raw HTTP shape of one operation, before curated-table merge.
pub struct RawOp {
    pub operation_id: String,
    pub method: String,
    pub path: String,
    pub tags: Vec<String>,
    pub summary: Option<String>,
    pub params: Vec<ParamIr>,
    pub request_body: Option<RawBody>,
    /// The raw JSON schema node of the chosen success (2xx) response body, if any
    /// (preferring 200, then 201, then 202, then the first 2xx). May be a `$ref`;
    /// resolution happens in [`crate::build`]. Drives `pagination.data_key`
    /// derivation + async `uri_field` verification.
    pub response_2xx: Option<Value>,
}

/// The request body, located but not yet resolved/normalized.
pub struct RawBody {
    pub content_type: String,
    /// `Some("X")` when the JSON schema is `$ref: #/components/schemas/X`.
    pub schema_ref_name: Option<String>,
    /// The raw schema node (the `$ref` node or an inline schema).
    pub schema_node: Value,
}

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// `tsg_mc_contacts_v3` → `mc_contacts`.
pub fn namespace_from_stem(stem: &str) -> String {
    let s = stem.strip_prefix("tsg_").unwrap_or(stem);
    s.strip_suffix("_v3").unwrap_or(s).to_string()
}

/// Resolve a local JSON-pointer `$ref` (`#/components/...`); records misses.
fn resolve_ref<'a>(root: &'a Value, ref_str: &str, stats: &mut Stats) -> Option<&'a Value> {
    let target = ref_str.strip_prefix('#').and_then(|p| root.pointer(p));
    if target.is_none() {
        stats.unresolved_refs.push(ref_str.to_string());
    }
    target
}

/// Follow a `$ref` one hop if present, else return the node as-is.
fn deref<'a>(root: &'a Value, node: &'a Value, stats: &mut Stats) -> &'a Value {
    if let Some(r) = node.get("$ref").and_then(Value::as_str)
        && let Some(t) = resolve_ref(root, r, stats)
    {
        return t;
    }
    node
}

/// Extract `(type, item_type)` from a param schema. Prefers a sibling/inline
/// `type`, falling back to the `$ref` target's type (OpenAPI 3.1 sibling rule).
fn schema_type(root: &Value, schema: &Value, stats: &mut Stats) -> (String, Option<String>) {
    let ty = schema
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            schema
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| resolve_ref(root, r, stats))
                .and_then(|t| t.get("type"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "string".to_string());

    let item_ty = if ty == "array" {
        schema
            .get("items")
            .map(|i| deref(root, i, stats))
            .and_then(|i| i.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    (ty, item_ty)
}

/// Pick the success (2xx) response's `application/json` schema node, preferring
/// `200`, then `201`, then `202`, then the lowest other 2xx code. Returns the raw
/// node (possibly a `$ref`); `None` when there is no JSON success body.
fn success_response_schema(op: &Value) -> Option<Value> {
    let responses = op.get("responses").and_then(Value::as_object)?;
    let schema_for = |code: &str| -> Option<Value> {
        responses
            .get(code)
            .and_then(|r| r.get("content"))
            .and_then(|c| c.get("application/json"))
            .and_then(|m| m.get("schema"))
            .cloned()
    };
    for code in ["200", "201", "202"] {
        if let Some(s) = schema_for(code) {
            return Some(s);
        }
    }
    // Any other 2xx (e.g. 204 carries no schema; deterministic by lowest code).
    let mut twoxx: Vec<&String> = responses
        .keys()
        .filter(|k| k.starts_with('2'))
        .collect::<Vec<_>>();
    twoxx.sort();
    for code in twoxx {
        if let Some(s) = schema_for(code) {
            return Some(s);
        }
    }
    None
}

fn parse_location(s: &str) -> Result<Location> {
    Ok(match s {
        "path" => Location::Path,
        "query" => Location::Query,
        "header" => Location::Header,
        other => bail!("unsupported param location {other:?} (only path/query/header occur)"),
    })
}

fn parse_param(root: &Value, raw: &Value, stats: &mut Stats) -> Result<Option<ParamIr>> {
    let p = deref(root, raw, stats);
    let Some(name) = p.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(loc_str) = p.get("in").and_then(Value::as_str) else {
        return Ok(None);
    };
    let location = parse_location(loc_str)?;
    // Path params are implicitly required.
    let required = p
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(location == Location::Path);
    let empty = Value::Object(Map::new());
    let schema = p.get("schema").unwrap_or(&empty);
    let (ty, item_ty) = schema_type(root, schema, stats);
    let format = schema
        .get("format")
        .and_then(Value::as_str)
        .map(str::to_string);
    let style = p.get("style").and_then(Value::as_str).map(str::to_string);
    let explode = p.get("explode").and_then(Value::as_bool);
    let description = p
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(ParamIr {
        name: name.to_string(),
        location,
        required,
        ty,
        item_ty,
        format,
        style,
        explode,
        description,
    }))
}

/// Parse one spec file into its raw operations.
pub fn parse_spec_file(path: &Path, stats: &mut Stats) -> Result<SpecFile> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let root: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse JSON {}", path.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("non-UTF8 file stem for {}", path.display()))?
        .to_string();
    let namespace = namespace_from_stem(&stem);

    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .with_context(|| format!("{stem}: missing `paths`"))?
        .clone();

    let mut ops = Vec::new();
    for (path_tmpl, item) in &paths {
        let item_params: Vec<Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            let operation_id = op
                .get("operationId")
                .and_then(Value::as_str)
                .with_context(|| format!("{stem} {method} {path_tmpl}: missing operationId"))?
                .to_string();

            // Merge path-item params + operation params (path-item first), resolve each.
            let mut params = Vec::new();
            for raw in item_params.iter().chain(
                op.get("parameters")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            ) {
                if let Some(p) = parse_param(&root, raw, stats)? {
                    params.push(p);
                }
            }

            // requestBody → (maybe requestBodies/X) → content[json].schema.
            let request_body = match op.get("requestBody") {
                None => None,
                Some(rb_raw) => {
                    let rb = deref(&root, rb_raw, stats);
                    let content = rb.get("content").and_then(Value::as_object);
                    let content_type = content
                        .and_then(|c| c.keys().next().cloned())
                        .unwrap_or_else(|| "application/json".to_string());
                    let schema_node = content
                        .and_then(|c| c.get("application/json"))
                        .and_then(|m| m.get("schema"))
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new()));
                    let schema_ref_name = schema_node
                        .get("$ref")
                        .and_then(Value::as_str)
                        .filter(|r| r.contains("/components/schemas/"))
                        .map(|r| r.rsplit('/').next().unwrap_or(r).to_string());
                    Some(RawBody {
                        content_type,
                        schema_ref_name,
                        schema_node,
                    })
                }
            };

            let tags = op
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let summary = op
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_string);

            // Success (2xx) response JSON schema node, for data_key derivation +
            // async uri_field verification. Preference: 200, 201, 202, then any 2xx.
            let response_2xx = success_response_schema(op);

            ops.push(RawOp {
                operation_id,
                method: method.to_uppercase(),
                path: path_tmpl.clone(),
                tags,
                summary,
                params,
                request_body,
                response_2xx,
            });
        }
    }

    Ok(SpecFile {
        namespace,
        stem,
        root,
        ops,
    })
}

/// Parse every `specs/*.json`, sorted, into `SpecFile`s.
pub fn parse_all(specs_dir: &Path, stats: &mut Stats) -> Result<Vec<SpecFile>> {
    let mut entries: Vec<_> = std::fs::read_dir(specs_dir)
        .with_context(|| format!("read specs dir {}", specs_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    entries.sort();
    let mut files = Vec::with_capacity(entries.len());
    for p in &entries {
        files.push(parse_spec_file(p, stats)?);
    }
    Ok(files)
}

/// L1 validity gate: parse every spec with `openapiv3 2.2.0` (the parser backend T
/// consumes). Returns the count parsed OK; the caller asserts it equals 46.
pub fn openapiv3_parse_count(specs_dir: &Path) -> Result<(usize, usize, Option<String>)> {
    let mut entries: Vec<_> = std::fs::read_dir(specs_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    entries.sort();
    let total = entries.len();
    let mut ok = 0usize;
    let mut first_err = None;
    for p in &entries {
        let text = std::fs::read_to_string(p)?;
        match serde_json::from_str::<openapiv3::OpenAPI>(&text) {
            Ok(_) => ok += 1,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(format!("{}: {e}", p.display()));
                }
            }
        }
    }
    Ok((ok, total, first_err))
}
