//! Walking parsed clap matches back to the subcommand chain.

use clap::ArgMatches;

/// The deepest (leaf) `ArgMatches` and the chain of subcommand names that
/// reached it. For `sendgrid mail send send-mail ...` the chain is
/// `["mail", "send", "send-mail"]`.
///
/// The leaf name is the hyphen-joined last two `cli_path` tokens, and some tokens
/// themselves contain hyphens (`api-keys`, `segments-v1`), so the chain must be
/// read from clap's actual subcommand names — never reconstructed by splitting a
/// leaf string.
pub fn leaf_matches(matches: &ArgMatches) -> (Vec<String>, &ArgMatches) {
    let mut chain = Vec::new();
    let mut cur = matches;
    while let Some((name, sub)) = cur.subcommand() {
        chain.push(name.to_string());
        cur = sub;
    }
    (chain, cur)
}
