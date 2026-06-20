//! `search_operations` — the MCP transport wrapper around the shared lexical
//! ranking in [`sendgrid_core::search`].
//!
//! All ranking, stemming, and filtering live in core so this surface ranks
//! **identically** to the CLI `sendgrid search` subcommand (P5 unification). This
//! module only translates the JSON tool args into [`SearchFilters`] and renders
//! the ranked hits as the tool result body.

use crate::text::truncate;
use sendgrid_core::Registry;
use sendgrid_core::ir::OperationIr;
use sendgrid_core::search::{DEFAULT_LIMIT, MAX_LIMIT, SearchFilters, search};
use serde_json::{Map, Value, json};

const SUMMARY_TRUNCATE: usize = 80;

/// Run `search_operations`. Returns the tool result body
/// `{ query, count, results: [{id, summary, method, path, side_effect, tags}] }`.
pub fn search_operations(args: &Map<String, Value>, include_legacy: bool) -> Value {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    let tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let side_effect = args
        .get("side_effect")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase());
    let method = args
        .get("method")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_uppercase());
    let domain = args
        .get("domain")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase());
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);

    let filters = SearchFilters {
        tags,
        side_effect,
        method,
        domain,
        limit: Some(limit),
        include_legacy,
    };

    let hits = search(Registry::global(), query, &filters);
    let results: Vec<Value> = hits.iter().map(|h| hit_json(h.op)).collect();

    json!({
        "query": query,
        "count": results.len(),
        "results": results,
    })
}

/// A metadata-only hit (~≤50 tokens): no params, no schema.
fn hit_json(op: &OperationIr) -> Value {
    json!({
        "id": op.id,
        "summary": op.summary.as_deref().map(|s| truncate(s, SUMMARY_TRUNCATE)).unwrap_or_default(),
        "method": op.method,
        "path": op.path,
        "side_effect": op.side_effect,
        "tags": op.tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_ids(out: &Value) -> Vec<String> {
        out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn envelope_shape_and_truncation() {
        // The wrapper-owned contract: `{query, count, results:[{id,summary,method,
        // path,side_effect,tags}]}` with summaries truncated to <=80 chars.
        let mut args = Map::new();
        args.insert("query".into(), json!("send a campaign"));
        let out = search_operations(&args, false);
        assert_eq!(out["query"], "send a campaign");
        assert_eq!(
            out["count"].as_u64().unwrap() as usize,
            out["results"].as_array().unwrap().len()
        );
        let first = &out["results"][0];
        for key in ["id", "summary", "method", "path", "side_effect", "tags"] {
            assert!(first.get(key).is_some(), "result missing `{key}`");
        }
        for r in out["results"].as_array().unwrap() {
            assert!(r["summary"].as_str().unwrap().chars().count() <= SUMMARY_TRUNCATE);
        }
    }

    #[test]
    fn limit_defaults_and_clamps() {
        // No limit arg → DEFAULT_LIMIT; an over-large value clamps to MAX_LIMIT.
        let mut base = Map::new();
        base.insert("query".into(), json!("list"));
        let default = search_operations(&base, false);
        assert_eq!(
            default["count"].as_u64().unwrap() as usize,
            DEFAULT_LIMIT,
            "a bare query should return DEFAULT_LIMIT hits"
        );

        let mut big = base.clone();
        big.insert("limit".into(), json!(10_000));
        let clamped = search_operations(&big, false);
        assert!(
            (clamped["count"].as_u64().unwrap() as usize) <= MAX_LIMIT,
            "limit must clamp to MAX_LIMIT"
        );
    }

    #[test]
    fn arg_mapping_filters_case_insensitively() {
        // The wrapper lowercases/uppercases filter args before handing them to core.
        let mut args = Map::new();
        args.insert("query".into(), json!("list"));
        args.insert("method".into(), json!("post")); // lowercase in → matches POST
        args.insert("limit".into(), json!(MAX_LIMIT));
        let out = search_operations(&args, false);
        assert!(!out["results"].as_array().unwrap().is_empty());
        for r in out["results"].as_array().unwrap() {
            assert_eq!(r["method"], "POST");
        }
    }

    #[test]
    fn hidden_excluded_unless_include_legacy() {
        // The wrapper threads `include_legacy` through to core's hidden filter.
        let mut args = Map::new();
        args.insert("query".into(), json!("send campaign"));
        args.insert("limit".into(), json!(MAX_LIMIT));

        let default = search_operations(&args, false);
        assert!(
            !result_ids(&default)
                .iter()
                .any(|id| id == "sg_legacy_campaigns_SendCampaign")
        );
        let legacy = search_operations(&args, true);
        assert!(
            result_ids(&legacy)
                .iter()
                .any(|id| id == "sg_legacy_campaigns_SendCampaign")
        );
    }

    #[test]
    fn ranking_parity_smoke_send_a_campaign() {
        // One end-to-end smoke that the shared ranking reaches the wrapper: the
        // hard "send a campaign" case surfaces a Single Sends op at the top.
        let mut args = Map::new();
        args.insert("query".into(), json!("send a campaign"));
        let out = search_operations(&args, false);
        let ids = result_ids(&out);
        assert!(
            ids[0].contains("singlesends"),
            "top hit should be a Single Sends op, got {}",
            ids[0]
        );
    }
}
