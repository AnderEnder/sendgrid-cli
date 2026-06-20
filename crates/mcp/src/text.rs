//! Shared lexical helpers: a camelCase-aware tokenizer and a char-safe truncator.
//!
//! The tokenizer must match the offline ranking prototype exactly: insert a word
//! boundary between a lowercase/digit and an uppercase letter (`SendMail` →
//! `send mail`), then split on any non-alphanumeric run and lowercase. This is the
//! single source of truth for both corpus IDF and per-query scoring.

/// Split `s` into lowercased alphanumeric tokens, breaking camelCase boundaries.
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
///     — the exact verb strings `get`/`add` that [`crate::search`]'s synonym map and
///     verb classifier key on);
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

/// Truncate a string to at most `max` chars (on a char boundary), appending `…`
/// when truncated. Used to keep search hits and descriptions token-bounded.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("short", 80), "short");
        let t = truncate("aaaaaaaaaa", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
    }
}
