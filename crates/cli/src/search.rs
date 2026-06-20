//! `sendgrid search <terms>` — lexical search over the registry (the human
//! mirror of the MCP `search_operations` meta-tool).

use sendgrid_core::Registry;
use sendgrid_core::ir::OperationIr;

/// Run a search and print matches (id, summary, method+path) to stdout. Hidden
/// ops are included only when `include_legacy` is set.
pub fn run(terms: &[String], include_legacy: bool) -> i32 {
    let needles: Vec<String> = terms.iter().map(|t| t.to_ascii_lowercase()).collect();
    let registry = Registry::global();

    let mut hits: Vec<(u32, &OperationIr)> = registry
        .operations()
        .iter()
        .filter(|op| include_legacy || !op.hidden)
        .filter_map(|op| {
            let score = score(op, &needles);
            (score > 0).then_some((score, op))
        })
        .collect();

    // Highest score first, then stable by id for determinism.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));

    if hits.is_empty() {
        eprintln!("no operations match: {}", terms.join(" "));
        return 1;
    }

    const MAX: usize = 50;
    let shown = hits.len().min(MAX);
    for (_score, op) in hits.iter().take(MAX) {
        let summary = op.summary.as_deref().unwrap_or("");
        let cli = op.cli_path.join(" ");
        println!("{}  [{} {}]", op.id, op.method, op.path);
        if !summary.is_empty() {
            println!("    {summary}");
        }
        println!("    cli: sendgrid {cli}");
    }
    if hits.len() > MAX {
        eprintln!("… {} more (showing top {MAX})", hits.len() - shown);
    }
    0
}

/// Score an op against the lowercased needles. Every needle must hit some
/// haystack field (AND semantics); id/summary hits weigh more than path hits.
fn score(op: &OperationIr, needles: &[String]) -> u32 {
    let id = op.id.to_ascii_lowercase();
    let alias = op.id_alias.as_deref().unwrap_or("").to_ascii_lowercase();
    let op_id = op.operation_id.to_ascii_lowercase();
    let summary = op.summary.as_deref().unwrap_or("").to_ascii_lowercase();
    let path = op.path.to_ascii_lowercase();
    let domain = op.domain.to_ascii_lowercase();
    let subgroup = op.subgroup.to_ascii_lowercase();
    let cli = op.cli_path.join(" ").to_ascii_lowercase();
    let tags = op.tags.join(" ").to_ascii_lowercase();

    let mut total = 0u32;
    for needle in needles {
        let mut best = 0u32;
        if id.contains(needle) || op_id.contains(needle) || alias.contains(needle) {
            best = best.max(5);
        }
        if summary.contains(needle) {
            best = best.max(4);
        }
        if tags.contains(needle) || domain.contains(needle) || subgroup.contains(needle) {
            best = best.max(3);
        }
        if cli.contains(needle) {
            best = best.max(2);
        }
        if path.contains(needle) {
            best = best.max(1);
        }
        if best == 0 {
            return 0; // this needle matched nothing → not a hit (AND).
        }
        total += best;
    }
    total
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
        assert_eq!(run(&["zzz_no_such_op_xyz".to_string()], false), 1);
    }
}
