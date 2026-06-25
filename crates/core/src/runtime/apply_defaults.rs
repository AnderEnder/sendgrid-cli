//! Curated client-side parameter defaults (`data/defaults.toml` → IR
//! `param_defaults`). Some SendGrid endpoints apply a *server-side* default that
//! silently narrows results to legacy behavior when a query param is omitted —
//! the canonical case being `GET /v3/templates`, which defaults
//! `generations=legacy` and so hides every modern (dynamic) template, leaving the
//! caller with a misleading `count: 0`.
//!
//! This step injects the curated default into the args envelope **only when the
//! caller omits the param**, so the CLI and MCP "just work" by defaulting to the
//! full, modern result set. It runs first in the pipeline (before `coerce`), so an
//! injected value is coerced/validated exactly like a caller-supplied one.
//!
//! "Omitted" is treated leniently: an absent key, JSON `null`, or an empty string
//! all count as not-provided (the CLI only ever inserts a key when the flag is
//! actually passed, but an MCP caller may send `null`). Any other present value —
//! including an explicit `--generations legacy` — always wins.

use crate::ir::{Location, OperationIr};
use serde_json::{Map, Value};

/// Inject curated defaults for omitted query/header params, in place.
pub fn apply_defaults(op: &OperationIr, args: &mut Value) {
    if op.param_defaults.is_empty() {
        return;
    }
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    for d in &op.param_defaults {
        let bucket_key = match d.location {
            Location::Query => "query",
            Location::Header => "header",
            // Path params are required, so a default never applies (codegen also
            // rejects a `path` default — this is just belt-and-suspenders).
            Location::Path => continue,
        };
        let bucket = obj
            .entry(bucket_key)
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(map) = bucket.as_object_mut() else {
            continue;
        };
        let provided = match map.get(&d.name) {
            None | Some(Value::Null) => false,
            Some(Value::String(s)) => !s.is_empty(),
            Some(_) => true,
        };
        if !provided {
            map.insert(d.name.clone(), Value::String(d.value.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;
    use serde_json::json;

    fn list_template() -> &'static OperationIr {
        Registry::global()
            .by_id("sg_templates_ListTemplate")
            .expect("ListTemplate exists")
    }

    #[test]
    fn injects_default_when_omitted() {
        let op = list_template();
        // Sanity: the curated default is present in the IR.
        assert!(
            op.param_defaults
                .iter()
                .any(|d| d.name == "generations" && d.value == "legacy,dynamic"),
            "ListTemplate should carry the generations default"
        );

        let mut args = json!({ "query": { "page_size": "10" } });
        apply_defaults(op, &mut args);
        assert_eq!(args["query"]["generations"], json!("legacy,dynamic"));

        // Also injects when there is no query bucket at all.
        let mut bare = json!({});
        apply_defaults(op, &mut bare);
        assert_eq!(bare["query"]["generations"], json!("legacy,dynamic"));
    }

    #[test]
    fn explicit_value_is_never_overridden() {
        let op = list_template();
        let mut args = json!({ "query": { "generations": "legacy" } });
        apply_defaults(op, &mut args);
        assert_eq!(
            args["query"]["generations"],
            json!("legacy"),
            "explicit caller value must win"
        );
    }

    #[test]
    fn null_or_empty_counts_as_omitted() {
        let op = list_template();
        for sent in [json!(null), json!("")] {
            let mut args = json!({ "query": { "generations": sent } });
            apply_defaults(op, &mut args);
            assert_eq!(args["query"]["generations"], json!("legacy,dynamic"));
        }
    }

    #[test]
    fn op_without_defaults_is_untouched() {
        let op = Registry::global()
            .by_id("sg_ips_manage_ListIp")
            .expect("ListIp exists");
        let mut args = json!({ "query": { "limit": "50" } });
        let before = args.clone();
        apply_defaults(op, &mut args);
        assert_eq!(args, before, "ops with no curated default are untouched");
    }
}
