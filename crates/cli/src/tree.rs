//! The dynamic clap command tree, built at runtime from the operation registry.
//!
//! `cli_path` convention: all tokens except the last two are nested subcommand
//! groups; the last two, joined by `-`, are the leaf command. So
//! `["mail","send","send","mail"]` → `sendgrid mail send send-mail`.
//!
//! `Registry::global()` returns a `&'static Registry`, so every spec-derived
//! string (group tokens, param names, summaries) is already `'static` and is
//! borrowed directly. clap 4.x's `Str`/`Id` only accept `&'static str` (not
//! `String`), so the few **computed** strings (the hyphen-joined leaf name and
//! the composed help text) are interned once via [`intern`] (spike-c).

use crate::globals;
use clap::{Arg, ArgAction, Command};
use sendgrid_core::Registry;
use sendgrid_core::ir::{Location, OperationIr};
use std::collections::BTreeMap;

/// Leak a computed string to `'static` so clap can hold it. Called a bounded
/// number of times at startup (once per leaf name / composed help string); the
/// allocations live for the process lifetime, which is correct for an arg tree.
fn intern(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// The clap subcommand-chain (e.g. `mail send send-mail`) → operation.
type ResolveMap = BTreeMap<String, &'static OperationIr>;

/// Compute the leaf command name for an op: the last two `cli_path` tokens joined
/// by `-`. (e.g. `send-mail`.)
fn leaf_name(op: &OperationIr) -> String {
    let p = &op.cli_path;
    let n = p.len();
    format!("{}-{}", p[n - 2], p[n - 1])
}

/// The full clap subcommand chain for an op (group tokens + leaf), space-joined —
/// the key the parsed tree resolves through.
fn chain_key(op: &OperationIr) -> String {
    let p = &op.cli_path;
    let n = p.len();
    let mut parts: Vec<&str> = p[..n - 2].iter().map(String::as_str).collect();
    let leaf = leaf_name(op);
    parts.push(&leaf);
    parts.join(" ")
}

/// Intermediate trie: group token → subtree, plus leaf name → op at this node.
#[derive(Default)]
struct Node {
    groups: BTreeMap<&'static str, Node>,
    leaves: BTreeMap<String, &'static OperationIr>,
}

impl Node {
    fn insert(&mut self, op: &'static OperationIr) {
        let p = &op.cli_path;
        let n = p.len();
        let mut node = self;
        for tok in &p[..n - 2] {
            node = node.groups.entry(tok.as_str()).or_default();
        }
        node.leaves.insert(leaf_name(op), op);
    }
}

/// Build a leaf command for an operation: a `--<name>` flag per path/query/header
/// param (string-valued; the runtime coerces), plus `--body` for body ops.
fn leaf_command(op: &'static OperationIr) -> Command {
    let mut cmd = Command::new(intern(leaf_name(op)));
    if let Some(summary) = &op.summary {
        cmd = cmd.about(summary.as_str());
    }
    cmd = cmd.long_about(intern(format!(
        "{} {}\noperation: {}",
        op.method, op.path, op.id
    )));

    for p in &op.params {
        let loc = match p.location {
            Location::Path => "path",
            Location::Query => "query",
            Location::Header => "header",
        };
        let help = match &p.description {
            Some(d) if !d.is_empty() => intern(format!("[{loc}] {d}")),
            _ => intern(format!("[{loc}] {} parameter", p.ty)),
        };
        // Arg id == long == exact param name (the verbatim envelope key the
        // runtime looks params up by). Names like `Content-Encoding`/`accountID`
        // are kept as-is so the envelope key matches the spec exactly. These are
        // `&'static str` borrowed from the global registry.
        let arg = Arg::new(p.name.as_str())
            .long(p.name.as_str())
            .value_name(p.ty.as_str())
            .action(ArgAction::Set)
            .required(p.required)
            .help(help);
        cmd = cmd.arg(arg);
    }

    if op.has_body {
        cmd = cmd.arg(
            Arg::new("body")
                .long("body")
                .value_name("JSON|@FILE|-")
                .action(ArgAction::Set)
                .help("Request body: inline JSON, @path/to/file, or - for stdin"),
        );
    }
    cmd
}

/// Recursively convert a trie node into a clap subcommand named `name`.
/// `name` is a `&'static str` group token borrowed from the global registry.
fn node_to_command(name: &'static str, node: &Node) -> Command {
    let mut cmd = Command::new(name)
        .subcommand_required(true)
        .arg_required_else_help(true);
    for (gname, child) in &node.groups {
        cmd = cmd.subcommand(node_to_command(gname, child));
    }
    for op in node.leaves.values() {
        cmd = cmd.subcommand(leaf_command(op));
    }
    cmd
}

/// The `sendgrid search` subcommand.
fn search_command() -> Command {
    Command::new("search")
        .about("Search operations by id, summary, tags, domain, or HTTP path")
        .arg(
            Arg::new("query")
                .num_args(1..)
                .required(true)
                .value_name("TERMS")
                .help("One or more search terms (matched case-insensitively)"),
        )
}

/// The `sendgrid mcp` subcommand.
fn mcp_command() -> Command {
    Command::new("mcp")
        .about("Run the SendGrid MCP server over stdio")
        .arg(
            Arg::new("expose-tag")
                .long("expose-tag")
                .action(ArgAction::Append)
                .value_name("TAG")
                .help("Promote operations carrying this tag to first-class MCP tools (repeatable)"),
        )
        .arg(
            Arg::new("expose-op")
                .long("expose-op")
                .action(ArgAction::Append)
                .value_name("OP_ID")
                .help("Promote this operation id to a first-class MCP tool (repeatable)"),
        )
}

/// Build the complete root command and the chain→op resolution map.
///
/// When `include_legacy` is false, hidden ops are omitted entirely, and any group
/// that would become empty (e.g. the all-hidden `legacy` group) is never created.
pub fn build(include_legacy: bool) -> (Command, ResolveMap) {
    let registry = Registry::global();

    let mut root_node = Node::default();
    let mut resolve = ResolveMap::new();
    for op in registry.operations() {
        if op.hidden && !include_legacy {
            continue;
        }
        root_node.insert(op);
        resolve.insert(chain_key(op), op);
    }

    let mut root = globals::with_global_flags(
        Command::new("sendgrid")
            .about("Agent-facing CLI for the SendGrid v3 API (dynamically generated)")
            .version(env!("CARGO_PKG_VERSION"))
            .subcommand_required(true)
            .arg_required_else_help(true),
    );

    // Domain groups, in deterministic order. (Every op has ≥3 `cli_path` tokens,
    // so the shortest path `[a,b,c]` is group `a` + leaf `b-c`; there are never
    // leaves directly at the root, hence `root_node.leaves` is always empty.)
    for (gname, child) in &root_node.groups {
        root = root.subcommand(node_to_command(gname, child));
    }

    // Reserved built-in subcommands (verified not to collide with any group token).
    root = root.subcommand(search_command()).subcommand(mcp_command());

    (root, resolve)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_all_visible_ops_without_panic() {
        let registry = Registry::global();
        let visible = registry.operations().iter().filter(|o| !o.hidden).count();
        let (cmd, resolve) = build(false);
        // Every visible op resolves through the tree.
        assert_eq!(resolve.len(), visible);
        // The command tree builds (clap's debug_assert catches id/name collisions).
        cmd.clone().debug_assert();
    }

    #[test]
    fn full_tree_registers_all_391_without_collision() {
        let registry = Registry::global();
        let (cmd, resolve) = build(true);
        assert_eq!(resolve.len(), registry.operations().len());
        assert_eq!(resolve.len(), 391);
        // Asserts the *full* tree (incl. all 56 hidden ops, which are not all
        // under `legacy`) is free of arg-id/subcommand-name collisions.
        cmd.debug_assert();
    }

    #[test]
    fn send_mail_resolves_at_expected_path() {
        let (_cmd, resolve) = build(false);
        let op = resolve
            .get("mail send send-mail")
            .expect("SendMail at `mail send send-mail`");
        assert_eq!(op.operation_id, "SendMail");
        assert_eq!(op.id, "sg_mail_send_SendMail");
    }

    #[test]
    fn hidden_op_absent_without_flag_present_with_it() {
        let registry = Registry::global();
        let hidden = registry
            .operations()
            .iter()
            .find(|o| o.hidden)
            .expect("a hidden op exists");
        let key = chain_key(hidden);

        let (_c1, without) = build(false);
        assert!(
            !without.contains_key(&key),
            "hidden op should be absent without --include-legacy"
        );
        let (_c2, with) = build(true);
        assert!(
            with.contains_key(&key),
            "hidden op should be present with --include-legacy"
        );
    }

    #[test]
    fn parsed_command_resolves_to_operation() {
        let (cmd, resolve) = build(false);
        let m = cmd
            .try_get_matches_from(["sendgrid", "mail", "send", "send-mail", "--body", "{}"])
            .expect("parse SendMail");
        let (chain, _leaf) = crate::resolve::leaf_matches(&m);
        assert_eq!(chain, vec!["mail", "send", "send-mail"]);
        let op = resolve.get(&chain.join(" ")).expect("resolves");
        assert_eq!(op.operation_id, "SendMail");
    }
}
