//! Display helper(s) for MCP output rendering.
//!
//! The lexical tokenizer + stemmer that used to live here moved to
//! [`sendgrid_core::search`] in P5 so the CLI and MCP share one ranking
//! implementation (see that module). What remains is the char-safe truncator,
//! used by both [`crate::search`] (hit summaries) and [`crate::describe`].

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
    fn truncate_is_char_safe() {
        assert_eq!(truncate("short", 80), "short");
        let t = truncate("aaaaaaaaaa", 5);
        assert_eq!(t.chars().count(), 5);
        assert!(t.ends_with('…'));
    }
}
