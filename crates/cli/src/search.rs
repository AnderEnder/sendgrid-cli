//! `sendgrid search <terms>` — lexical search over the registry (the human
//! mirror of the MCP `search_operations` meta-tool).
//!
//! Ranking, stemming, and filtering live in [`sendgrid_core::search`] so this
//! subcommand ranks **identically** to the MCP surface (P5 unification): the same
//! query surfaces the same op whichever interface an agent uses. This module only
//! owns the human-readable presentation.

use sendgrid_core::Registry;
use sendgrid_core::ir::OperationIr;
use sendgrid_core::search::{SearchFilters, search};

/// Max hits printed before the "… N more" footer.
const MAX: usize = 50;

/// Run a search and print matches (id, summary, method+path) to stdout. Hidden
/// ops are included only when `include_legacy` is set.
pub fn run(terms: &[String], include_legacy: bool) -> i32 {
    let query = terms.join(" ");
    // Clap requires >=1 term, but guard the empty case so a stray call preserves
    // the "no match" exit path rather than browse-listing every op.
    if query.trim().is_empty() {
        eprintln!("no operations match: {query}");
        return 1;
    }

    let ops = ranked(&query, include_legacy);
    if ops.is_empty() {
        eprintln!("no operations match: {query}");
        return 1;
    }

    let shown = ops.len().min(MAX);
    for op in ops.iter().take(MAX) {
        let summary = op.summary.as_deref().unwrap_or("");
        let cli = op.cli_path.join(" ");
        println!("{}  [{} {}]", op.id, op.method, op.path);
        if !summary.is_empty() {
            println!("    {summary}");
        }
        println!("    cli: sendgrid {cli}");
    }
    if ops.len() > MAX {
        eprintln!("… {} more (showing top {MAX})", ops.len() - shown);
    }
    0
}

/// Ranked ops for `query` (full result set, descending score) via the shared core
/// ranking. `limit: None` returns every hit so `run` can render its own top-N.
fn ranked(query: &str, include_legacy: bool) -> Vec<&'static OperationIr> {
    let filters = SearchFilters {
        include_legacy,
        ..Default::default()
    };
    search(Registry::global(), query, &filters)
        .into_iter()
        .map(|hit| hit.op)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_send_mail() {
        // Smoke: searching for an obvious term yields a successful exit.
        assert_eq!(run(&["send".to_string(), "mail".to_string()], false), 0);
    }

    #[test]
    fn no_match_returns_nonzero() {
        assert_eq!(run(&["qzxwvk".to_string()], false), 1);
    }

    #[test]
    fn send_a_campaign_surfaces_singlesends() {
        // Parity with the MCP `search_operations` tool: the failing smoke from P5
        // ("send a campaign" returned nothing on the old AND-match CLI search) now
        // surfaces a Single Sends op at the top via the shared ranking.
        let ops = ranked("send a campaign", false);
        assert!(!ops.is_empty(), "query must now return hits");
        assert!(
            ops[0].id.contains("singlesends"),
            "top hit should be a Single Sends op, got {}",
            ops[0].id
        );
    }

    #[test]
    fn hidden_excluded_without_include_legacy() {
        // The hidden legacy SendCampaign op is excluded by default, present with
        // --include-legacy — same gate as the MCP surface.
        let default = ranked("send campaign", false);
        assert!(
            !default
                .iter()
                .any(|op| op.id == "sg_legacy_campaigns_SendCampaign"),
            "hidden op must be excluded by default"
        );
        let legacy = ranked("send campaign", true);
        assert!(
            legacy
                .iter()
                .any(|op| op.id == "sg_legacy_campaigns_SendCampaign"),
            "hidden op must appear with include_legacy"
        );
    }
}
