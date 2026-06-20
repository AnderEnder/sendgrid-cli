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
    fn truncate_is_char_safe() {
        assert_eq!(truncate("short", 80), "short");
        let t = truncate("aaaaaaaaaa", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
    }
}
