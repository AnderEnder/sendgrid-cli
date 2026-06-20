//! `search_operations` — in-memory lexical ranking over the operation registry.
//!
//! Ranking is field-weighted (`id > tags > summary > path`) with **IDF² emphasis**
//! so rare, discriminating tokens (e.g. `campaign`, df=11) dominate over the very
//! common ones (`list`, df=137; `get`, df=101). Two refinements make agent queries
//! rank intuitively:
//!   - a **List↔Retrieve / Create↔Add synonym map** (98 `List*` ops summarize as
//!     "Retrieve"), applied at a discount so a query verb still finds the op;
//!   - an **action-verb boost**: when the op's leading `operation_id` verb (e.g.
//!     `Create`, `Send`) is itself a query token, it is boosted by its own IDF² —
//!     this is what ranks `CreateMarketingList` over `ListContactCount` for
//!     "create contact list" (the `create` verb is rare; `list` is not).
//!
//! IDF is computed **once over all 391 ops including hidden** (corpus statistics
//! shouldn't shift with the `--include-legacy` flag); the hidden filter is applied
//! only when collecting results.

use crate::text::{tokenize, truncate};
use sendgrid_core::Registry;
use sendgrid_core::ir::OperationIr;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// --- Tuning constants (calibrated offline against the two review-U2 sanity
// cases; see crate tests). Case 1 is comfortable (CreateMarketingList 85.1 vs
// ListContactCount <69); case 2 is tight (SendCampaign 113.0 vs
// SendTestMarketingEmail 108.4, ~1.04x) but deterministic and stable while the IR
// artifact is frozen — it would only shift if the corpus DF changes. ---
const W_ID: f64 = 3.0;
const W_TAGS: f64 = 2.0;
const W_SUMMARY: f64 = 1.5;
const W_PATH: f64 = 1.0;
/// Synonym matches count, but at a discount vs. a literal hit.
const SYN_DISCOUNT: f64 = 0.4;
/// Extra weight for the op's action verb appearing in the query.
const VERB_BOOST: f64 = 2.0;
/// Reward for covering more distinct query terms.
const COVERAGE_BONUS: f64 = 0.15;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
const SUMMARY_TRUNCATE: usize = 80;

/// Query tokens that carry no ranking signal.
fn is_stopword(t: &str) -> bool {
    matches!(
        t,
        "a" | "an"
            | "the"
            | "of"
            | "to"
            | "for"
            | "and"
            | "in"
            | "on"
            | "with"
            | "by"
            | "from"
            | "my"
            | "me"
            | "i"
            | "all"
            | "any"
    )
}

/// Extra doc tokens a query token should also match (at [`SYN_DISCOUNT`]).
fn synonyms(t: &str) -> &'static [&'static str] {
    match t {
        "list" => &["get", "retrieve"],
        "get" => &["list", "retrieve"],
        "retrieve" => &["list", "get"],
        "create" => &["add", "new"],
        "add" => &["create"],
        "delete" => &["remove", "erase"],
        "remove" => &["delete", "erase"],
        "update" => &["edit", "patch"],
        "send" => &["dispatch"],
        _ => &[],
    }
}

/// Verbs that meaningfully describe an operation's action (gates the verb boost so
/// a non-verb leading token like `Email...` doesn't get spuriously promoted).
fn is_action_verb(t: &str) -> bool {
    matches!(
        t,
        "list"
            | "get"
            | "retrieve"
            | "create"
            | "add"
            | "update"
            | "patch"
            | "delete"
            | "remove"
            | "erase"
            | "send"
            | "schedule"
            | "search"
            | "export"
            | "import"
            | "test"
            | "validate"
            | "verify"
            | "duplicate"
            | "cancel"
            | "enable"
            | "disable"
            | "reset"
            | "activate"
            | "deactivate"
    )
}

/// Per-op precomputed token sets + action verb, parallel to `registry.operations()`.
struct OpTokens {
    id: HashSet<String>,
    tags: HashSet<String>,
    summary: HashSet<String>,
    path: HashSet<String>,
    verb: String,
}

/// The precomputed lexical index (IDF + per-op token sets), built once.
struct Index {
    idf: HashMap<String, f64>,
    unknown_idf: f64,
    ops: Vec<OpTokens>,
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(|| build_index(Registry::global()))
}

fn build_index(reg: &Registry) -> Index {
    let ops = reg.operations();
    let n = ops.len() as f64;

    let mut df: HashMap<String, usize> = HashMap::new();
    let mut op_tokens = Vec::with_capacity(ops.len());

    for op in ops {
        let id: HashSet<String> = tokenize(&op.id).into_iter().collect();
        let tags: HashSet<String> = op.tags.iter().flat_map(|t| tokenize(t)).collect();
        let summary: HashSet<String> = op
            .summary
            .as_deref()
            .map(tokenize)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let path: HashSet<String> = tokenize(&op.path).into_iter().collect();
        let verb = tokenize(&op.operation_id)
            .into_iter()
            .next()
            .unwrap_or_default();

        // Document frequency: count each token once per op across all fields.
        let mut seen: HashSet<&String> = HashSet::new();
        for set in [&id, &tags, &summary, &path] {
            for tok in set {
                seen.insert(tok);
            }
        }
        for tok in seen {
            *df.entry(tok.clone()).or_insert(0) += 1;
        }

        op_tokens.push(OpTokens {
            id,
            tags,
            summary,
            path,
            verb,
        });
    }

    let idf: HashMap<String, f64> = df
        .into_iter()
        .map(|(t, d)| (t, (1.0 + n / d as f64).ln()))
        .collect();
    let unknown_idf = (1.0 + n / 0.5).ln();

    Index {
        idf,
        unknown_idf,
        ops: op_tokens,
    }
}

fn idf_of(idx: &Index, t: &str) -> f64 {
    idx.idf.get(t).copied().unwrap_or(idx.unknown_idf)
}

/// Score one op against the (stopword-filtered) query terms. Returns 0 when no
/// query term matches anywhere.
fn score_op(idx: &Index, ot: &OpTokens, terms: &[String]) -> f64 {
    let mut total = 0.0;
    let mut covered = 0usize;

    for qt in terms {
        // Best field weight where the term (or a discounted synonym) appears.
        let mut best = 0.0_f64;
        let mut matched = false;
        for (set, w) in [
            (&ot.id, W_ID),
            (&ot.tags, W_TAGS),
            (&ot.summary, W_SUMMARY),
            (&ot.path, W_PATH),
        ] {
            if set.contains(qt) {
                best = best.max(w);
                matched = true;
            } else {
                for syn in synonyms(qt) {
                    if set.contains(*syn) {
                        best = best.max(w * SYN_DISCOUNT);
                        matched = true;
                    }
                }
            }
        }
        if matched {
            covered += 1;
            let idf = idf_of(idx, qt);
            total += idf * idf * best;
        }
    }

    // Action-verb boost: the op's verb is itself a query term.
    if is_action_verb(&ot.verb) && terms.iter().any(|t| t == &ot.verb) {
        let idf = idf_of(idx, &ot.verb);
        total += VERB_BOOST * idf * idf * W_ID;
    }

    if total == 0.0 {
        return 0.0;
    }
    total * (1.0 + COVERAGE_BONUS * covered as f64)
}

/// Run `search_operations`. Returns the tool result body
/// `{ query, count, results: [{id, summary, method, path, side_effect, tags}] }`.
pub fn search_operations(args: &Map<String, Value>, include_legacy: bool) -> Value {
    let reg = Registry::global();
    let idx = index();

    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let terms: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|t| !is_stopword(t))
        .collect();

    let filter_tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let filter_se = args
        .get("side_effect")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase());
    let filter_method = args
        .get("method")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_uppercase());
    let filter_domain = args
        .get("domain")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_lowercase());
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);

    let ops = reg.operations();
    let mut hits: Vec<(f64, usize)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        if op.hidden && !include_legacy {
            continue;
        }
        if !passes_filters(op, &filter_tags, &filter_se, &filter_method, &filter_domain) {
            continue;
        }
        let score = if terms.is_empty() {
            // No query: behave as a pure filter/browse listing.
            1.0
        } else {
            score_op(idx, &idx.ops[i], &terms)
        };
        if score > 0.0 {
            hits.push((score, i));
        }
    }

    // Sort by score desc, then id asc for a deterministic, stable order.
    hits.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ops[a.1].id.cmp(&ops[b.1].id))
    });
    hits.truncate(limit);

    let results: Vec<Value> = hits.iter().map(|&(_, i)| hit_json(&ops[i])).collect();

    json!({
        "query": query,
        "count": results.len(),
        "results": results,
    })
}

fn passes_filters(
    op: &OperationIr,
    tags: &[String],
    se: &Option<String>,
    method: &Option<String>,
    domain: &Option<String>,
) -> bool {
    if !tags.is_empty() {
        let op_tags: Vec<String> = op.tags.iter().map(|t| t.to_ascii_lowercase()).collect();
        if !tags.iter().any(|t| op_tags.iter().any(|ot| ot == t)) {
            return false;
        }
    }
    if let Some(se) = se {
        let op_se = serde_json::to_value(op.side_effect)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        if &op_se != se {
            return false;
        }
    }
    if let Some(m) = method
        && &op.method != m
    {
        return false;
    }
    if let Some(d) = domain
        && &op.domain.to_ascii_lowercase() != d
    {
        return false;
    }
    true
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

    /// Returns the ranked ids for a query (helper for the sanity cases).
    fn ranked_ids(query: &str, include_legacy: bool) -> Vec<String> {
        // Large limit so we can find positions of both ops.
        let mut args = Map::new();
        args.insert("query".into(), json!(query));
        args.insert("limit".into(), json!(MAX_LIMIT));
        let out = search_operations(&args, include_legacy);
        out["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn pos(ids: &[String], id: &str) -> usize {
        ids.iter().position(|x| x == id).unwrap_or(usize::MAX)
    }

    #[test]
    fn case1_create_contact_list_ranks_create_above_list_count() {
        let ids = ranked_ids("create contact list", false);
        let create = pos(&ids, "sg_marketing_lists_CreateMarketingList");
        let count_a = pos(&ids, "sg_marketing_contacts_ListContactCount");
        let count_b = pos(&ids, "sg_marketing_lists_ListContactCount");
        assert!(
            create < count_a && create < count_b,
            "CreateMarketingList (pos {create}) must rank above ListContactCount \
             (positions {count_a}, {count_b})"
        );
    }

    #[test]
    fn case2_send_campaign_ranks_above_send_test() {
        // `campaign` appears only in hidden legacy ops, so this case requires
        // include_legacy=true to bring SendCampaign into the candidate set.
        let ids = ranked_ids("send a marketing email campaign", true);
        let real = pos(&ids, "sg_legacy_campaigns_SendCampaign");
        let test = pos(&ids, "sg_mail_test_SendTestMarketingEmail");
        assert!(
            real < test,
            "SendCampaign (pos {real}) must rank above SendTestMarketingEmail (pos {test})"
        );
    }

    #[test]
    fn hidden_excluded_unless_include_legacy() {
        // SendCampaign is hidden; absent by default, present with include_legacy.
        let default = ranked_ids("send campaign", false);
        assert!(
            !default
                .iter()
                .any(|id| id == "sg_legacy_campaigns_SendCampaign")
        );
        let legacy = ranked_ids("send campaign", true);
        assert!(
            legacy
                .iter()
                .any(|id| id == "sg_legacy_campaigns_SendCampaign")
        );
    }

    #[test]
    fn filters_apply() {
        let mut args = Map::new();
        args.insert("query".into(), json!("list"));
        args.insert("method".into(), json!("POST"));
        args.insert("limit".into(), json!(MAX_LIMIT));
        let out = search_operations(&args, false);
        for r in out["results"].as_array().unwrap() {
            assert_eq!(r["method"], "POST");
        }
    }
}
