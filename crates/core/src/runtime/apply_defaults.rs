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

use crate::ir::{Location, OperationIr, ParamIr};
use serde_json::{Map, Value};

/// Inject client-side defaults for omitted query/header params, in place:
///
/// 1. Curated `param_defaults` (`data/defaults.toml`) — always applied.
/// 2. Under `--all` (`paginate_all`), a per-page size for a REQUIRED page-size /
///    limit query param the caller omitted — so an agent that just wants "all of
///    it" need not know the endpoint's pagination knob. The injected value is the
///    `max_items` cap (query params carry no `maximum` in the IR, so the cap is the
///    only available bound).
pub fn apply_defaults(op: &OperationIr, args: &mut Value, paginate_all: bool, max_items: usize) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    apply_curated_defaults(op, obj);
    if paginate_all {
        inject_page_size(op, obj, max_items);
    }
}

/// Whether a present JSON value counts as caller-provided. Absent / `null` / empty
/// string all read as omitted (the CLI only inserts a key when the flag is passed,
/// but an MCP caller may send `null`).
fn is_provided(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Apply curated `param_defaults` for omitted query/header params.
fn apply_curated_defaults(op: &OperationIr, obj: &mut Map<String, Value>) {
    if op.param_defaults.is_empty() {
        return;
    }
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
        if !is_provided(map.get(&d.name)) {
            map.insert(d.name.clone(), Value::String(d.value.clone()));
        }
    }
}

/// Query param names that control the per-page size / limit of a listing.
///
/// Shared with the CLI tree builder: under `--all` the CLI relaxes clap's
/// `required` on exactly this set so the runtime injection below can fill the
/// default — the CLI-relaxed set and the runtime-injected set must stay identical,
/// so both consult this single predicate.
pub fn is_page_size_param(p: &ParamIr) -> bool {
    p.location == Location::Query
        && p.required
        && matches!(p.name.as_str(), "page_size" | "limit" | "per_page")
}

/// Under `--all`, inject a per-page size for a required page-size/limit param the
/// caller omitted, so the request validates without the agent knowing the knob.
fn inject_page_size(op: &OperationIr, obj: &mut Map<String, Value>, max_items: usize) {
    let Some(param) = op.params.iter().find(|p| is_page_size_param(p)) else {
        return;
    };
    let bucket = obj
        .entry("query")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(map) = bucket.as_object_mut() else {
        return;
    };
    if !is_provided(map.get(&param.name)) {
        // No per-query-param `maximum` exists in the IR, so the `max_items` cap is
        // the only bound available; clamp to at least 1.
        let per_page = (max_items as u64).max(1);
        map.insert(param.name.clone(), Value::from(per_page));
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
        apply_defaults(op, &mut args, false, 1000);
        assert_eq!(args["query"]["generations"], json!("legacy,dynamic"));

        // Also injects when there is no query bucket at all.
        let mut bare = json!({});
        apply_defaults(op, &mut bare, false, 1000);
        assert_eq!(bare["query"]["generations"], json!("legacy,dynamic"));
    }

    #[test]
    fn explicit_value_is_never_overridden() {
        let op = list_template();
        let mut args = json!({ "query": { "generations": "legacy" } });
        apply_defaults(op, &mut args, false, 1000);
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
            apply_defaults(op, &mut args, false, 1000);
            assert_eq!(args["query"]["generations"], json!("legacy,dynamic"));
        }
    }

    #[test]
    fn paginate_all_injects_required_page_size_bounded_by_max_items() {
        let op = list_template(); // declares a REQUIRED `page_size` query param
        let mut args = json!({});
        apply_defaults(op, &mut args, true, 200);
        assert_eq!(
            args["query"]["page_size"],
            json!(200),
            "under --all the required page_size defaults to the max_items cap"
        );
    }

    #[test]
    fn paginate_all_does_not_override_explicit_page_size() {
        let op = list_template();
        let mut args = json!({ "query": { "page_size": 25 } });
        apply_defaults(op, &mut args, true, 200);
        assert_eq!(args["query"]["page_size"], json!(25), "caller value wins");
    }

    #[test]
    fn page_size_not_injected_without_paginate_all() {
        let op = list_template();
        let mut args = json!({});
        apply_defaults(op, &mut args, false, 200);
        assert!(
            args.get("query").and_then(|q| q.get("page_size")).is_none(),
            "no page_size injected when --all is off"
        );
    }

    #[test]
    fn op_without_defaults_is_untouched() {
        let op = Registry::global()
            .by_id("sg_ips_manage_ListIp")
            .expect("ListIp exists");
        let mut args = json!({ "query": { "limit": "50" } });
        let before = args.clone();
        apply_defaults(op, &mut args, false, 1000);
        assert_eq!(args, before, "ops with no curated default are untouched");
    }
}
