//! Shared lexical search over the operation registry — the single ranking
//! implementation behind **both** the CLI `sendgrid search` subcommand and the
//! MCP `search_operations` meta-tool. (Before P5 the two surfaces had divergent
//! implementations; this module unifies them so a query ranks identically
//! whichever surface an agent uses.)
//!
//! Ranking is field-weighted (`id > tags ≈ keywords > summary > path`) with
//! **IDF² emphasis** so rare, discriminating tokens (e.g. `campaign`, df=11)
//! dominate over the very common ones (`list`, df=137; `get`, df=101). Four
//! refinements make agent queries rank intuitively:
//!   - **light stemming** ([`stem`]) folds query and doc tokens to a common stem,
//!     so `bounced`/`emails`/`suppress` match `bounces`/`email`/`suppression`
//!     instead of scoring zero (review-agent-ux F2);
//!   - a **List↔Retrieve / Create↔Add / Verify↔Validate synonym map** (98 `List*`
//!     ops summarize as "Retrieve"), applied at a discount so a query verb still
//!     finds the op;
//!   - **curated `search_keywords`** (e.g. `campaign`/`newsletter` → the modern
//!     Single Sends ops) indexed as a tag-weight, match-only field — so an agent's
//!     natural marketing word reaches the right op (review-agent-ux F4);
//!   - a **gated action-verb boost**: when the op's leading `operation_id` verb
//!     (e.g. `Create`, `Send`) is a query token AND the op covers a second distinct
//!     query term, it is boosted by its own IDF² — this ranks `CreateMarketingList`
//!     over `ListContactCount` for "create contact list" without letting a bare
//!     verb match (e.g. `Create*` on "create a contact") flood the top (F3).
//!
//! IDF is computed **once over all 391 ops including hidden** (corpus statistics
//! shouldn't shift with the `include_legacy` flag) over the **stemmed** tokens of
//! `id/tags/summary/path` (keywords excluded — see [`W_KEYWORDS`]); the hidden
//! filter is applied only when collecting results.

use crate::Registry;
use crate::ir::{OperationIr, SideEffect};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// --- Tuning constants (calibrated against the two review-U2 sanity cases; see
// the tests). Numbers below are POST-stemming (stemming folds the corpus DF, so
// the pre-P4b absolute scores no longer apply). Case 1: CreateMarketingList 84.96
// vs ListContactCount 44.81 (~1.9x) — comfortable, but the margin RELIES on the
// gated verb boost (create's IDF² ≫ list's); without it the two would invert.
// Case 2: SendCampaign 110.84 vs SendTestMarketingEmail 105.50 (~1.05x) — tight but
// deterministic/stable while the IR artifact is frozen; the rare `campaign` keeps
// its discriminative IDF because curated `search_keywords` are excluded from DF. ---
const W_ID: f64 = 3.0;
const W_TAGS: f64 = 2.0;
/// Curated search aliases (`OperationIr::search_keywords`) — weighted at tag level
/// since they are deliberately high-signal discovery hooks (e.g. `campaign` →
/// Single Sends). Match-only: they are NOT counted in document frequency, so they
/// don't dilute the IDF of a rare alias like `campaign` (review-agent-ux F4).
const W_KEYWORDS: f64 = 2.0;
const W_SUMMARY: f64 = 1.5;
const W_PATH: f64 = 1.0;
/// Synonym matches count, but at a discount vs. a literal hit.
const SYN_DISCOUNT: f64 = 0.4;
/// Extra weight for the op's action verb appearing in the query. Gated on the op
/// covering ≥2 distinct query terms (see [`score_op`]) so a verb-only match never
/// outranks the op that also matches the discriminating noun (review-agent-ux F3).
const VERB_BOOST: f64 = 2.0;
/// Reward for covering more distinct query terms.
const COVERAGE_BONUS: f64 = 0.15;

/// Default result cap when a caller passes no explicit limit.
pub const DEFAULT_LIMIT: usize = 20;
/// Upper bound a caller-supplied limit should be clamped to.
pub const MAX_LIMIT: usize = 100;

/// Optional result filters + paging, mirroring the MCP `search_operations` args.
/// All string filters are matched case-insensitively against the IR.
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    /// Keep only ops carrying at least one of these tags (lowercased compare).
    pub tags: Vec<String>,
    /// Keep only ops of this side-effect class: `read|write|destructive|send`.
    pub side_effect: Option<String>,
    /// Keep only ops with this HTTP method (uppercased compare).
    pub method: Option<String>,
    /// Keep only ops in this domain (lowercased compare).
    pub domain: Option<String>,
    /// Cap the number of returned hits. `None` returns every scoring hit (the
    /// caller paginates/truncates for display); `Some(n)` truncates to `n`.
    pub limit: Option<usize>,
    /// Include hidden/legacy ops in the candidate set.
    pub include_legacy: bool,
}

/// One ranked result: a borrow of the matched op plus its score.
#[derive(Debug, Clone, Copy)]
pub struct SearchHit<'a> {
    pub op: &'a OperationIr,
    pub score: f64,
}

/// Split `s` into lowercased alphanumeric tokens, breaking camelCase boundaries.
///
/// The rule: insert a word boundary between a lowercase/digit and an uppercase
/// letter (`SendMail` → `send mail`), then split on any non-alphanumeric run and
/// lowercase. This is the single source of truth for both corpus IDF and
/// per-query scoring, so the CLI and MCP tokenize identically.
pub fn tokenize(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut spaced = String::with_capacity(s.len() + 8);
    for (i, c) in chars.iter().enumerate() {
        if i > 0 {
            let prev = chars[i - 1];
            if (prev.is_ascii_lowercase() || prev.is_ascii_digit()) && c.is_ascii_uppercase() {
                spaced.push(' ');
            }
        }
        spaced.push(*c);
    }
    spaced
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Light morphological folding applied to **both** query and document tokens so
/// that inflected variants collide in the lexical match step. Without it the match
/// is exact token-set membership, so a query word like `bounced`/`emails`/`suppress`
/// scores **zero** against the spec's `bounces`/`email`/`suppression` — the target
/// op falls out of the candidate set entirely (review-agent-ux F2).
///
/// This is deliberately **not** a full Porter stemmer — it folds only the
/// inflections that actually occur in the SendGrid corpus + agent queries, and it
/// is biased toward **precision**:
///   - tokens ≤3 chars are returned untouched (protects `id`, `ip`, and — crucially
///     — the exact verb strings `get`/`add` that the synonym map and verb
///     classifier key on);
///   - the `-ion(s)` nominalization (`suppression`→`suppress`) only fires when it
///     leaves a ≥4-char root, so `region` is **not** reduced to `reg`;
///   - `-ss`/`-us`/`-is` endings are never treated as a plural (`address`, `status`,
///     `analysis` survive).
///
/// The final trailing-`e` fold is what makes `bounce`/`bounced`/`bounces` all reach
/// the single stem `bounc` (the canonical hard case a plural-only fold can't solve).
///
/// Examples: `bounces`/`bounced`/`bounce` → `bounc`; `emails` → `email`;
/// `contacts` → `contact`; `suppression`/`suppressions` → `suppress`;
/// `sending` → `send`; `validation`/`validate` → `validat`; `region`/`regions`
/// → `region`; `get` → `get`.
pub fn stem(token: &str) -> String {
    if token.len() <= 3 {
        return token.to_string();
    }

    // 1. Nominalization: `-ion`/`-ions`, guarded so a substantial root remains.
    let after_suffix = if let Some(root) = token.strip_suffix("ions").filter(|r| r.len() >= 4) {
        root.to_string()
    } else if let Some(root) = token.strip_suffix("ion").filter(|r| r.len() >= 4) {
        root.to_string()
    // 2. Plurals.
    } else if let Some(root) = token.strip_suffix("ies").filter(|r| r.len() >= 2) {
        format!("{root}y")
    } else if token.ends_with("sses")
        || token.ends_with("xes")
        || token.ends_with("zes")
        || token.ends_with("ches")
        || token.ends_with("shes")
    {
        // sibilant + "es" (addresses → address): drop only the "es".
        token[..token.len() - 2].to_string()
    } else if token.ends_with("ss") || token.ends_with("us") || token.ends_with("is") {
        // not a plural (address, status, analysis): leave intact.
        token.to_string()
    } else if let Some(root) = token.strip_suffix('s').filter(|r| r.len() >= 3) {
        root.to_string()
    // 3. Verb inflections.
    } else if let Some(root) = token.strip_suffix("ing").filter(|r| r.len() >= 3) {
        root.to_string()
    } else if let Some(root) = token.strip_suffix("ed").filter(|r| r.len() >= 3) {
        root.to_string()
    } else {
        token.to_string()
    };

    // 4. Trailing-`e` canonicalization: collapses bounce/bounced/bounces → bounc and
    //    create/created/creates → creat. Keep ≥3 chars and never touch "…ee".
    if after_suffix.len() >= 4 && after_suffix.ends_with('e') && !after_suffix.ends_with("ee") {
        let mut s = after_suffix;
        s.pop();
        s
    } else {
        after_suffix
    }
}

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
        // SendGrid's UI "verify a domain" button maps to the Validate* domain-auth
        // op; bridge the agent's natural verb to the spec's `validate`.
        "verify" => &["validate"],
        "validate" => &["verify"],
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
/// All token sets are **stemmed** ([`stem`]) so the match step folds inflected
/// query words; `verb` is kept **raw** because the synonym map and [`is_action_verb`]
/// classifier key on exact verb strings.
struct OpTokens {
    id: HashSet<String>,
    tags: HashSet<String>,
    summary: HashSet<String>,
    path: HashSet<String>,
    /// Curated aliases (`search_keywords`), stemmed. Match-only — excluded from IDF.
    keywords: HashSet<String>,
    verb: String,
}

/// The precomputed lexical index (IDF + per-op token sets), built once.
struct Index {
    idf: HashMap<String, f64>,
    unknown_idf: f64,
    ops: Vec<OpTokens>,
}

/// The process-wide index, built once from [`Registry::global`].
///
/// `Registry` has no public constructor other than [`Registry::global`], so the
/// only registry that can ever reach [`search`] is the global one — which means
/// the positional alignment between `Index::ops[i]` and `reg.operations()[i]`
/// always holds.
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
        let id = stemmed_set(tokenize(&op.id));
        let tags = stemmed_set(op.tags.iter().flat_map(|t| tokenize(t)));
        let summary = stemmed_set(op.summary.as_deref().map(tokenize).unwrap_or_default());
        let path = stemmed_set(tokenize(&op.path));
        let keywords = stemmed_set(op.search_keywords.iter().flat_map(|k| tokenize(k)));
        // Raw (unstemmed) leading verb — the synonym map + verb classifier key on it.
        let verb = tokenize(&op.operation_id)
            .into_iter()
            .next()
            .unwrap_or_default();

        // Document frequency: count each token once per op across the indexed
        // fields. `keywords` are intentionally EXCLUDED so a rare curated alias
        // (e.g. `campaign`) keeps its discriminative IDF (review-agent-ux F4).
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
            keywords,
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

/// Collect an iterator of raw tokens into a **stemmed** set (the form stored in the
/// index and compared against stemmed query tokens).
fn stemmed_set(tokens: impl IntoIterator<Item = String>) -> HashSet<String> {
    tokens.into_iter().map(|t| stem(&t)).collect()
}

/// IDF keyed by **stem** (the index DF is over stemmed tokens).
fn idf_of(idx: &Index, t: &str) -> f64 {
    idx.idf.get(&stem(t)).copied().unwrap_or(idx.unknown_idf)
}

/// Score one op against the (stopword-filtered) query terms. Returns 0 when no
/// query term matches anywhere. `terms` are **raw**; matching folds them with
/// [`stem`] against the index's stemmed token sets.
fn score_op(idx: &Index, ot: &OpTokens, terms: &[String]) -> f64 {
    let mut total = 0.0;
    let mut covered = 0usize;

    for qt in terms {
        let qs = stem(qt);
        // Best field weight where the term (or a discounted synonym) appears.
        let mut best = 0.0_f64;
        let mut matched = false;
        for (set, w) in [
            (&ot.id, W_ID),
            (&ot.tags, W_TAGS),
            (&ot.keywords, W_KEYWORDS),
            (&ot.summary, W_SUMMARY),
            (&ot.path, W_PATH),
        ] {
            if set.contains(&qs) {
                best = best.max(w);
                matched = true;
            } else {
                for syn in synonyms(qt) {
                    if set.contains(&stem(syn)) {
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

    // Action-verb boost: the op's verb is itself a query term — but ONLY when the op
    // also covers a second distinct query term. A verb-only match (e.g. `create` on
    // an unrelated `Create*` op for "create a contact") gets no boost, so it can't
    // outrank the op matching the discriminating noun (review-agent-ux F3).
    if covered > 1 && is_action_verb(&ot.verb) && terms.iter().any(|t| stem(t) == stem(&ot.verb)) {
        let idf = idf_of(idx, &ot.verb);
        total += VERB_BOOST * idf * idf * W_ID;
    }

    if total == 0.0 {
        return 0.0;
    }
    total * (1.0 + COVERAGE_BONUS * covered as f64)
}

/// The canonical lowercase name of a side-effect class (matches the IR's
/// `serde(rename_all = "lowercase")`), used for the `side_effect` filter without
/// pulling serde into the hot path.
fn side_effect_str(se: SideEffect) -> &'static str {
    match se {
        SideEffect::Read => "read",
        SideEffect::Write => "write",
        SideEffect::Destructive => "destructive",
        SideEffect::Send => "send",
    }
}

fn passes_filters(op: &OperationIr, f: &SearchFilters) -> bool {
    if !f.tags.is_empty()
        && !f
            .tags
            .iter()
            .any(|t| op.tags.iter().any(|ot| ot.eq_ignore_ascii_case(t)))
    {
        return false;
    }
    if let Some(se) = &f.side_effect
        && !se.eq_ignore_ascii_case(side_effect_str(op.side_effect))
    {
        return false;
    }
    if let Some(m) = &f.method
        && !op.method.eq_ignore_ascii_case(m)
    {
        return false;
    }
    if let Some(d) = &f.domain
        && !op.domain.eq_ignore_ascii_case(d)
    {
        return false;
    }
    true
}

/// Rank `query` against the registry's operations, applying `filters`, and return
/// hits in descending score (ties broken by `id` ascending for determinism).
///
/// An empty/whitespace query behaves as a pure filter/browse listing (every
/// candidate passing the filters scores `1.0`). The single ranking implementation
/// shared by the CLI `sendgrid search` subcommand and the MCP `search_operations`
/// meta-tool — call this from both so a query ranks identically on either surface.
pub fn search<'a>(reg: &'a Registry, query: &str, filters: &SearchFilters) -> Vec<SearchHit<'a>> {
    let idx = index();

    let terms: Vec<String> = tokenize(query.trim())
        .into_iter()
        .filter(|t| !is_stopword(t))
        .collect();

    let ops = reg.operations();
    let mut hits: Vec<(f64, usize)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        if op.hidden && !filters.include_legacy {
            continue;
        }
        if !passes_filters(op, filters) {
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
    if let Some(limit) = filters.limit {
        hits.truncate(limit);
    }

    hits.into_iter()
        .map(|(score, i)| SearchHit { op: &ops[i], score })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tokenizer / stemmer (moved from the former mcp `text` module) ----------

    #[test]
    fn camel_case_split() {
        assert_eq!(tokenize("SendMail"), vec!["send", "mail"]);
        assert_eq!(
            tokenize("sg_marketing_lists_CreateMarketingList"),
            vec!["sg", "marketing", "lists", "create", "marketing", "list"]
        );
        assert_eq!(
            tokenize("/v3/marketing/contacts/count"),
            vec!["v3", "marketing", "contacts", "count"]
        );
    }

    #[test]
    fn stem_folds_inflected_variants_to_one_stem() {
        // The pairs the search match step must collide (review-agent-ux F2).
        for group in [
            vec!["bounce", "bounced", "bounces"],
            vec!["email", "emails"],
            vec!["contact", "contacts"],
            vec!["address", "addresses"],
            vec!["suppress", "suppression", "suppressions"],
            vec!["send", "sends", "sending"],
            vec!["create", "creates", "created"],
            vec!["validate", "validates", "validation"],
            vec!["domain", "domains"],
            vec!["list", "lists", "listed"],
        ] {
            let stems: Vec<String> = group.iter().map(|w| stem(w)).collect();
            assert!(
                stems.windows(2).all(|w| w[0] == w[1]),
                "expected {group:?} to share a stem, got {stems:?}"
            );
        }
    }

    #[test]
    fn stem_preserves_precision_no_over_stemming() {
        // Distinct concepts must STAY distinct (the trailing-e / -ion folds are the
        // aggressive pieces — guard against them over-merging).
        assert_eq!(stem("region"), stem("regions"), "region/regions collide");
        assert_ne!(stem("region"), "reg", "must NOT strip region → reg");
        assert_ne!(stem("contact"), stem("account"), "contact != account");
        assert_ne!(stem("domain"), stem("template"), "domain != template");
        // Short verb strings the synonym map + verb classifier key on are untouched.
        for kw in ["get", "add", "new", "id"] {
            assert_eq!(stem(kw), kw, "short keyword `{kw}` must be preserved");
        }
        // Non-plural sibilant/latinate endings are not mis-stemmed.
        assert_eq!(stem("address"), "address");
        assert_eq!(stem("status"), "status");
    }

    // --- ranking (moved from the former mcp `search` module) --------------------

    /// Returns the ranked ids for a query (helper for the sanity cases).
    fn ranked_ids(query: &str, include_legacy: bool) -> Vec<String> {
        let filters = SearchFilters {
            // Large limit so we can find positions of both ops.
            limit: Some(MAX_LIMIT),
            include_legacy,
            ..Default::default()
        };
        search(Registry::global(), query, &filters)
            .into_iter()
            .map(|h| h.op.id.clone())
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

    /// Domain of the op with this id (helper for the suppressions-domain assertion).
    fn domain_of(id: &str) -> String {
        Registry::global()
            .by_id(id)
            .map(|o| o.domain.clone())
            .unwrap_or_default()
    }

    // --- Reviewer's tricky queries (review-agent-ux F2/F3/F4) -------------------
    // Each query previously returned a wrong-domain #1 or nothing in the top 100.
    // We assert the *fix*, not an arbitrary slot: the right op is surfaced near the
    // top and the specific regression op is demoted. Where pure lexical ranking
    // can't make the ideal op #1 (it isn't named for the query's verb, or a rival
    // matches more query tokens), the assertion captures the achievable, stable win.

    #[test]
    fn tricky_suppress_an_email_address() {
        // F2: stemming makes `suppress` reach `suppression(s)`. Was: not in top 100,
        // #1 a validation-email job. Now: top is a suppressions-domain op and the
        // global-suppression op (the "add an address to suppressions" op) is surfaced.
        let ids = ranked_ids("suppress an email address", false);
        assert_eq!(
            domain_of(&ids[0]),
            "suppressions",
            "top hit should be in the suppressions domain, got {}",
            ids[0]
        );
        let p = pos(&ids, "sg_suppressions_CreateGlobalSuppression");
        assert!(
            p < 8,
            "CreateGlobalSuppression should be surfaced near the top, was at {p}"
        );
    }

    #[test]
    fn tricky_verify_my_sending_domain() {
        // F2 + verify→validate synonym. The domain-authentication validate op should
        // rank in the top 2 (it's the SendGrid "verify a domain" action).
        let ids = ranked_ids("verify my sending domain", false);
        let p = pos(&ids, "sg_branding_domain_ValidateAuthenticatedDomain");
        assert!(
            p < 2,
            "ValidateAuthenticatedDomain should be top-2, was at {p}"
        );
    }

    #[test]
    fn tricky_list_bounced_emails() {
        // F2: `bounced` stems to match `bounces`. Was: not in top 100 (only the exact
        // plural "bounces" matched). Now ListSuppressionBounces is top-ranked (it ties
        // a couple of bounce-settings list ops, broken alphabetically).
        let ids = ranked_ids("list bounced emails", false);
        let p = pos(&ids, "sg_suppressions_ListSuppressionBounces");
        assert!(p < 4, "ListSuppressionBounces should be top-4, was at {p}");
    }

    #[test]
    fn tricky_create_a_contact() {
        // F3 (the named regression): the contact op is `UpdateContact` ("Add or Update
        // a Contact"). The gated verb boost must keep unrelated `Create*` ops (account
        // /sso/...) BELOW it; previously they flooded the top (CreateAccount was #1,
        // UpdateContact #26).
        let ids = ranked_ids("create a contact", false);
        let contact = pos(&ids, "sg_marketing_contacts_UpdateContact");
        assert!(
            contact < 3,
            "UpdateContact should be top-3, was at {contact}"
        );
        for unrelated in [
            "sg_account_provisioning_CreateAccount",
            "sg_account_sso_CreateSsoIntegration",
            "sg_account_subusers_CreateSubuser",
        ] {
            assert!(
                contact < pos(&ids, unrelated),
                "UpdateContact ({contact}) must outrank the unrelated Create op {unrelated} \
                 ({})",
                pos(&ids, unrelated)
            );
        }
    }

    #[test]
    fn tricky_send_a_campaign() {
        // F4: `campaign` is a curated search_keyword on the modern Single Sends ops.
        // Was: #1 = transactional SendMail; Single Sends never appeared. Now a
        // singlesends op tops the list and outranks SendMail.
        let ids = ranked_ids("send a campaign", false);
        assert!(
            ids[0].contains("singlesends"),
            "top hit should be a Single Sends op, got {}",
            ids[0]
        );
        let single = pos(&ids, &ids[0].clone());
        let sendmail = pos(&ids, "sg_mail_send_SendMail");
        assert!(
            single < sendmail,
            "a Single Sends op ({single}) must outrank transactional SendMail ({sendmail})"
        );
    }

    #[test]
    fn filters_apply() {
        let filters = SearchFilters {
            method: Some("POST".into()),
            limit: Some(MAX_LIMIT),
            ..Default::default()
        };
        let hits = search(Registry::global(), "list", &filters);
        assert!(!hits.is_empty());
        for hit in &hits {
            assert_eq!(hit.op.method, "POST");
        }
    }
}
