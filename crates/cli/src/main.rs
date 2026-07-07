//! `sendgrid` — the agent-facing CLI (and `sendgrid mcp` server host).
//!
//! The command tree is built **at runtime** from `sendgrid_core::Registry`
//! (clap builder API, no derive). Every operation becomes a leaf command; global
//! flags are defined at the root, and several (e.g. `--dry-run`, `--output`) are
//! accepted anywhere via clap `global(true)`; `execute()` in `sendgrid-core` is the
//! single dispatch chokepoint. See the module docs for the `cli_path` → command convention.

mod auth;
mod envelope;
mod globals;
mod jobs;
mod output;
mod resolve;
mod search;
mod tree;

use clap::ArgMatches;
use globals::GlobalOpts;
use sendgrid_core::ir::OperationIr;

#[tokio::main]
async fn main() {
    let code = run().await;
    std::process::exit(code);
}

/// Root globals that are NOT `global(true)` because their long-name collides with
/// real leaf params (38 ops declare `limit`, 27 `offset`, etc.). clap only accepts
/// them *before* the subcommand, so an agent that writes them after the operation
/// hits `unexpected argument`. Each takes a value.
const HOISTABLE_GLOBALS: &[&str] = &["query", "limit", "offset", "region", "on-behalf-of"];

/// Root global longs that consume a following value token — used only to step over
/// leading root flags while locating the subcommand. Boolean root flags (`dry-run`,
/// `all`, `include-legacy`, `allow-bulk`) and `--help`/`--version` take no value.
fn root_flag_takes_value(long: &str) -> bool {
    matches!(
        long,
        "region"
            | "output"
            | "query"
            | "limit"
            | "offset"
            | "page-token"
            | "allow"
            | "on-behalf-of"
            | "api-key"
    )
}

/// Locate the subcommand chain in `argv`: the `[sub_start, chain_end)` range of the
/// contiguous non-flag tokens that name the command (group tokens + hyphen-joined
/// leaf), skipping the program name and any leading root flags (and the value each
/// value-taking flag consumes). `argv[sub_start..chain_end].join(" ")` is the
/// resolve-map key. Shared by [`hoist_globals`] and the `--explain` pre-scan.
fn locate_subcommand(argv: &[String]) -> (usize, usize) {
    let mut sub_start = 1;
    while sub_start < argv.len() {
        let tok = &argv[sub_start];
        let Some(long) = tok.strip_prefix("--") else {
            if tok.starts_with('-') {
                // A short flag (none of ours are, but be conservative): skip one.
                sub_start += 1;
                continue;
            }
            break; // first non-flag token = start of the subcommand chain
        };
        if long.contains('=') {
            sub_start += 1;
        } else if root_flag_takes_value(long) {
            sub_start += 2;
        } else {
            sub_start += 1;
        }
    }

    let mut chain_end = sub_start;
    while chain_end < argv.len() && !argv[chain_end].starts_with('-') {
        chain_end += 1;
    }
    (sub_start, chain_end)
}

/// Canonicalize an inferred top-level group token in place.
///
/// clap `.infer_subcommands(true)` lets the user type a unique prefix / singular of a
/// top-level group name (e.g. `suppression` for `suppressions`). Our pre-scans
/// ([`hoist_globals`] and the `--explain` lookup) key the resolve-map off CANONICAL
/// `cli_path` tokens, so an inferred token misses the lookup — and the leaf's own
/// `--limit`/`--offset` query param would be wrongly hoisted to the pagination-cap
/// global and silently dropped. Inference applies only to the top-level command, so
/// only the FIRST subcommand token can diverge; rewrite it to the group clap would
/// infer. Exact matches are left untouched; ambiguous / no match is left as-is so
/// clap emits its normal error. clap accepts the canonical form too, so mutating the
/// argv vector clap ultimately parses is safe.
fn canonicalize_inferred_group(argv: &mut [String], group_names: &[String]) {
    let (sub_start, _chain_end) = locate_subcommand(argv);
    if sub_start >= argv.len() {
        return;
    }
    let tok = &argv[sub_start];
    // Already canonical (exact match): nothing to do.
    if group_names.iter().any(|g| g == tok) {
        return;
    }
    // Unique-prefix match against the inferable group names.
    let mut hits = group_names.iter().filter(|g| g.starts_with(tok.as_str()));
    if let Some(canonical) = hits.next()
        && hits.next().is_none()
    {
        argv[sub_start] = canonical.clone();
    }
}

/// Recover trailing collision-prone root globals: move any of [`HOISTABLE_GLOBALS`]
/// that appear *after* the subcommand to *before* it, but ONLY when the resolved
/// leaf op does not itself declare a usable flag of that name (`leaf_declares`).
/// This is pure and unit-tested; `run` calls it before building matches. `argv`
/// includes the program name at index 0.
fn hoist_globals(argv: Vec<String>, leaf_declares: impl Fn(&str, &str) -> bool) -> Vec<String> {
    // 1 & 2. Locate the subcommand chain; its space-join is the resolve-map key.
    let (sub_start, chain_end) = locate_subcommand(&argv);
    let chain_key = argv[sub_start..chain_end].join(" ");

    // 3. Walk the tail (leaf args + any trailing globals); pull out hoistable
    //    globals the leaf does not declare, preserving everything else in place.
    let mut hoisted: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let mut i = chain_end;
    while i < argv.len() {
        let tok = &argv[i];
        if let Some(long) = tok.strip_prefix("--") {
            let (name, inline_value) = match long.split_once('=') {
                Some((n, _)) => (n, true),
                None => (long, false),
            };
            if HOISTABLE_GLOBALS.contains(&name) && !leaf_declares(&chain_key, name) {
                hoisted.push(tok.clone());
                if !inline_value && i + 1 < argv.len() {
                    hoisted.push(argv[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
        }
        tail.push(tok.clone());
        i += 1;
    }

    if hoisted.is_empty() {
        return argv; // nothing moved: preserve argv exactly
    }

    // 4. Reassemble: [prog + leading globals] ++ [hoisted] ++ [subcommand chain] ++ [tail].
    let mut out = Vec::with_capacity(argv.len());
    out.extend_from_slice(&argv[..sub_start]);
    out.append(&mut hoisted);
    out.extend_from_slice(&argv[sub_start..chain_end]);
    out.append(&mut tail);
    out
}

/// Render the human-formatted `--explain` view of an operation: its identity and
/// summary, the parameter menu, a copy-paste-ready example command line, and the
/// response field-menu — the same building blocks the MCP `describe_operation` tool
/// surfaces (via `sendgrid_core::describe`), formatted for a terminal. Returned as a
/// `String` (trailing newline included) for the caller to print; nothing is dispatched.
fn render_explain(op: &OperationIr) -> String {
    use std::fmt::Write as _;
    let d = sendgrid_core::describe::describe(op);
    let mut s = String::new();

    let _ = writeln!(s, "{}  ({} {})", op.id, op.method, op.path);
    if let Some(summary) = &op.summary {
        let _ = writeln!(s, "{summary}");
    }

    // Parameters — skip the `on-behalf-of` header: the leaf CLI suppresses that flag
    // (impersonation is routed through the governed global `--on-behalf-of`), so
    // showing it as a leaf param here would misrepresent the actual surface.
    let params: Vec<&serde_json::Value> = d
        .params
        .iter()
        .filter(|p| {
            p.get("name")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|n| !n.eq_ignore_ascii_case("on-behalf-of"))
        })
        .collect();
    if !params.is_empty() {
        let _ = writeln!(s, "\nParameters:");
        for p in params {
            let get = |k| p.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
            let name = get("name");
            let loc = get("in");
            let ty = get("type");
            let req = p
                .get("required")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let req = if req { ", required" } else { "" };
            let _ = writeln!(s, "  --{name}  [{loc}{req}] {ty}");
        }
    }

    // A runnable example command line: the full chain + required-param placeholders
    // (mirroring the group `--help` examples), plus the synthesized body when the op
    // takes one.
    let mut cmd = format!("sendgrid {}", tree::chain_key(op));
    for p in &op.params {
        if p.required && !p.name.eq_ignore_ascii_case("on-behalf-of") {
            let _ = write!(cmd, " --{} <{}>", p.name, p.name);
        }
    }
    if op.has_body {
        if d.example.is_null() {
            cmd.push_str(" --body '{…}'");
        } else {
            let _ = write!(cmd, " --body '{}'", d.example);
        }
    }
    let _ = writeln!(s, "\nExample:\n  {cmd}");

    // Response field-menu (names→types; the chaining case shows one level into a
    // result array's element) — the fix for query-root guessing.
    let _ = writeln!(s, "\nResponse fields:");
    let fields = render_response_fields(&d.response_fields);
    if fields.is_empty() {
        let _ = writeln!(s, "  (no documented response schema)");
    } else {
        s.push_str(&fields);
    }

    s
}

/// Format the response field-menu produced by `sendgrid_core::describe` (see
/// `response_menu`) into indented `name: type` lines. Returns an empty string when
/// no schema is embedded (`Value::Null`).
fn render_response_fields(menu: &serde_json::Value) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();

    // Top-level array response: `{is_array, item_fields}`.
    if menu.get("is_array").and_then(serde_json::Value::as_bool) == Some(true) {
        let _ = writeln!(s, "  [array of objects]");
        if let Some(item_fields) = menu
            .get("item_fields")
            .and_then(serde_json::Value::as_object)
        {
            for (name, ty) in item_fields {
                let _ = writeln!(s, "    {name}: {}", ty.as_str().unwrap_or(""));
            }
        }
        return s;
    }

    // Object response: `{fields, items?}` where `items` descends one level into
    // array-of-object fields (the chaining case, e.g. `versions[]`).
    if let Some(fields) = menu.get("fields").and_then(serde_json::Value::as_object) {
        let items = menu.get("items").and_then(serde_json::Value::as_object);
        for (name, ty) in fields {
            let _ = writeln!(s, "  {name}: {}", ty.as_str().unwrap_or(""));
            if let Some(sub) = items
                .and_then(|it| it.get(name))
                .and_then(serde_json::Value::as_object)
            {
                for (sname, sty) in sub {
                    let _ = writeln!(s, "    {name}[].{sname}: {}", sty.as_str().unwrap_or(""));
                }
            }
        }
        return s;
    }

    // Scalar response: `{type}`.
    if let Some(ty) = menu.get("type").and_then(serde_json::Value::as_str) {
        let _ = writeln!(s, "  {ty}");
    }
    s
}

async fn run() -> i32 {
    // `--include-legacy` decides the *shape* of the tree (whether hidden ops and
    // the all-hidden `legacy` group exist), so it must be known before the tree
    // is built. A cheap argv pre-scan resolves it; the parsed flag still governs
    // all runtime behavior.
    let argv: Vec<String> = std::env::args().collect();
    let include_legacy = argv.iter().any(|a| a == "--include-legacy");
    // `--all` likewise decides the *shape* of the tree: it relaxes clap's `required`
    // on a page-size/limit param so the runtime can inject the per-page default. Like
    // `--include-legacy`, this must be known before the tree is built, so a cheap argv
    // pre-scan resolves it; the parsed flag still governs all runtime behavior.
    let paginate_all = argv.iter().any(|a| a == "--all");
    let (command, resolve_map) = tree::build(include_legacy, paginate_all);

    // Canonicalize a clap-inferred top-level group token (e.g. singular `suppression`
    // → `suppressions`) BEFORE the argv pre-scans run, so the resolve-map lookups they
    // key off CANONICAL `cli_path` tokens see the canonical name. Otherwise an inferred
    // group misses the lookup and the leaf's own `--limit`/`--offset` is wrongly hoisted
    // and silently dropped. The inferable names are exactly the built top-level
    // subcommands (already reflecting `--include-legacy`).
    let mut argv = argv;
    let group_names: Vec<String> = command
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    canonicalize_inferred_group(&mut argv, &group_names);

    // Recover trailing collision-prone globals (Task 3.2): hoist any that the
    // resolved leaf does not declare, so `... --query X` after the subcommand works.
    let argv = hoist_globals(argv, |chain, long| {
        resolve_map
            .get(chain)
            .is_some_and(|op| tree::leaf_declares_flag(op, long))
    });

    // `--explain` short-circuit (Task 5.2): describe the target op and exit 0 without
    // dispatching. This must run BEFORE `try_get_matches_from`, because clap enforces
    // required leaf params first — so `sendgrid templates get-template --explain`
    // (no `--template_id`) would otherwise error out before we could explain it.
    // Resolution uses the full chain key; a singular inferred prefix (Task 4.1) won't
    // resolve here and falls through to clap (a minor, accepted gap).
    if argv.iter().any(|a| a == "--explain") {
        let (sub_start, chain_end) = locate_subcommand(&argv);
        let key = argv[sub_start..chain_end].join(" ");
        if let Some(op) = resolve_map.get(key.as_str()).copied() {
            print!("{}", render_explain(op));
            return 0;
        }
        // No resolvable op (e.g. bare `--explain`, or on a group): fall through so
        // clap emits its usual help/error.
    }

    // clap handles `--help`/`--version`/parse errors itself (exiting as it sees
    // fit); we only reach here with a valid parse. On a residual unknown arg the
    // pre-scan could not recover, this falls through to clap's normal error.
    let matches = match command.try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => e.exit(),
    };

    let globals = match GlobalOpts::from_matches(&matches) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };

    match matches.subcommand() {
        Some(("search", sub)) => {
            let terms: Vec<String> = sub
                .get_many::<String>("query")
                .map(|v| v.cloned().collect())
                .unwrap_or_default();
            search::run(&terms, globals.include_legacy)
        }
        Some(("mcp", sub)) => run_mcp(sub, &globals).await,
        Some(("auth", sub)) => run_auth(sub, &globals).await,
        Some(_) => run_operation(&matches, &resolve_map, &globals).await,
        None => {
            // Unreachable: the root sets `subcommand_required(true)`.
            64
        }
    }
}

async fn run_mcp(sub: &ArgMatches, globals: &GlobalOpts) -> i32 {
    let expose_tags: Vec<String> = sub
        .get_many::<String>("expose-tag")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();
    let expose_ops: Vec<String> = sub
        .get_many::<String>("expose-op")
        .map(|v| v.cloned().collect())
        .unwrap_or_default();

    let cfg = match globals.mcp_config(expose_tags, expose_ops) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };
    match sendgrid_mcp::run_stdio(cfg).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mcp error: {e:#}");
            1
        }
    }
}

async fn run_auth(sub: &ArgMatches, globals: &GlobalOpts) -> i32 {
    match sub.subcommand() {
        Some(("scopes", _)) => auth::scopes(globals).await,
        Some(("whoami", _)) => auth::whoami(globals).await,
        Some(("doctor", _)) => auth::doctor(globals).await,
        // Unreachable: the `auth` group sets subcommand_required(true).
        _ => 64,
    }
}

async fn run_operation(
    matches: &ArgMatches,
    resolve_map: &std::collections::BTreeMap<String, &'static OperationIr>,
    globals: &GlobalOpts,
) -> i32 {
    let (chain, leaf) = resolve::leaf_matches(matches);
    let key = chain.join(" ");

    let Some(op) = resolve_map.get(key.as_str()).copied() else {
        eprintln!("error: unknown operation `{key}`");
        return 64;
    };

    // Defensive gate (the tree already omits hidden ops without the flag).
    if op.hidden && !globals.include_legacy {
        eprintln!("error: `{key}` is a hidden/legacy operation; re-run with --include-legacy");
        return 64;
    }

    // `--explain` also short-circuits here: the argv pre-scan catches the flag when
    // required leaf params are omitted, but when they *are* supplied (e.g. a Task-4.1
    // inferred prefix that the pre-scan's full-name lookup missed) the parse succeeds
    // and we intercept off the parsed flag instead — describe and exit, never dispatch.
    if globals.explain {
        print!("{}", render_explain(op));
        return 0;
    }

    let args = match envelope::build(op, leaf, globals) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };

    // Async-job flags take over the call (each builds its own runtime config). The
    // query is gated by the op's `async_job` kind inside `selected_async`, since
    // clap panics on `get_flag`/`get_one` for an unregistered arg id.
    match jobs::selected_async(op, leaf) {
        Some(jobs::AsyncAction::Await) => return jobs::run_await(op, args, globals).await,
        Some(jobs::AsyncAction::Upload(path)) => {
            return jobs::run_upload(op, args, globals, &path).await;
        }
        Some(jobs::AsyncAction::Download(dest)) => {
            return jobs::run_download(op, args, globals, &dest).await;
        }
        None => {}
    }

    let cfg = match globals.runtime_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };

    let result = sendgrid_core::execute(&cfg, op, args).await;
    output::render(&result, globals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendgrid_core::{ApiKey, RuntimeConfig};
    use serde_json::Value;

    /// A syntactically well-formed dummy key (`SG.<22>.<43>`) for dry-run tests —
    /// nothing is ever sent.
    const DUMMY_KEY: &str =
        "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

    fn valid_sendmail_body() -> &'static str {
        r#"{"personalizations":[{"to":[{"email":"to@example.com"}]}],"from":{"email":"from@example.com"},"subject":"hi","content":[{"type":"text/plain","value":"hello"}]}"#
    }

    #[test]
    fn parsed_send_mail_inline_body_builds_envelope_and_resolves() {
        let (command, resolve_map) = tree::build(false, false);
        let matches = command
            .try_get_matches_from([
                "sendgrid",
                "mail",
                "send",
                "send-mail",
                "--body",
                valid_sendmail_body(),
            ])
            .expect("parses");

        let (chain, leaf) = resolve::leaf_matches(&matches);
        assert_eq!(chain, vec!["mail", "send", "send-mail"]);
        let op = resolve_map
            .get(&chain.join(" "))
            .copied()
            .expect("resolves");
        assert_eq!(op.operation_id, "SendMail");

        let globals = test_globals();
        let env = envelope::build(op, leaf, &globals).expect("envelope");
        assert_eq!(
            env["body"]["from"]["email"],
            Value::String("from@example.com".into())
        );
        assert!(env["path"].is_object() && env["query"].is_object());
    }

    #[test]
    fn send_mail_body_from_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("sendgrid_cli_test_body.json");
        std::fs::write(&path, valid_sendmail_body()).unwrap();

        let (command, resolve_map) = tree::build(false, false);
        let body_arg = format!("@{}", path.display());
        let matches = command
            .try_get_matches_from(["sendgrid", "mail", "send", "send-mail", "--body", &body_arg])
            .expect("parses");
        let (chain, leaf) = resolve::leaf_matches(&matches);
        let op = resolve_map
            .get(&chain.join(" "))
            .copied()
            .expect("resolves");
        let env = envelope::build(op, leaf, &test_globals()).expect("envelope");
        assert_eq!(env["body"]["subject"], Value::String("hi".into()));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn dry_run_send_mail_yields_request_preview() {
        let op = sendgrid_core::Registry::global()
            .by_id("sg_mail_send_SendMail")
            .expect("SendMail");
        let args: Value =
            serde_json::from_str(&format!(r#"{{"body":{}}}"#, valid_sendmail_body())).unwrap();

        let mut cfg = RuntimeConfig::new(ApiKey::new(DUMMY_KEY));
        cfg.dry_run = true;
        let result = sendgrid_core::execute(&cfg, op, args).await;

        assert!(result.is_success(), "dry-run should succeed: {result:?}");
        let preview = result.request_preview.expect("dry-run yields a preview");
        assert_eq!(preview["method"], Value::String("POST".into()));
        assert!(
            preview["url"].as_str().unwrap().ends_with("/v3/mail/send"),
            "preview url: {}",
            preview["url"]
        );
    }

    #[test]
    fn global_flags_parse_before_subcommand() {
        let (command, _resolve) = tree::build(false, false);
        let matches = command
            .try_get_matches_from([
                "sendgrid",
                "--region",
                "eu",
                "--dry-run",
                "--output",
                "table",
                "mail",
                "send",
                "send-mail",
                "--body",
                "{}",
            ])
            .expect("global flags before subcommand parse");
        let globals = GlobalOpts::from_matches(&matches).expect("globals");
        assert!(globals.dry_run);
        assert_eq!(globals.output, globals::OutputFormat::Table);
    }

    #[test]
    fn global_flags_parse_after_subcommand() {
        // Agents naturally place --dry-run/--output next to the operation (after the
        // subcommand). With `global(true)` these are accepted there AND still read from
        // the root matches. Building the FULL tree (include_legacy=true) also makes this
        // a collision detector: clap panics on a duplicate long-name across all ops, so
        // a clean parse proves none of the globalized flags collide with a leaf param.
        let (command, _resolve) = tree::build(true, false);
        let matches = command
            .try_get_matches_from([
                "sendgrid",
                "mail",
                "send",
                "send-mail",
                "--body",
                "{}",
                "--dry-run",
                "--output",
                "table",
            ])
            .expect("global flags after the subcommand parse");
        let globals = GlobalOpts::from_matches(&matches).expect("globals");
        assert!(
            globals.dry_run,
            "--dry-run after the subcommand must be visible from the root matches"
        );
        assert_eq!(globals.output, globals::OutputFormat::Table);
    }

    #[test]
    fn allow_explicit_still_detected() {
        // `--allow` is `global(true)` (Task 3.1) yet still gates the mcp read-only
        // default via value_source == CommandLine. Guard that --allow before the
        // subcommand is still detected as an explicit choice.
        let (command, _resolve) = tree::build(false, false);
        let matches = command
            .try_get_matches_from([
                "sendgrid",
                "--allow",
                "read",
                "mail",
                "send",
                "send-mail",
                "--body",
                "{}",
            ])
            .expect("parse with --allow");
        let globals = GlobalOpts::from_matches(&matches).expect("globals");
        assert!(globals.allow_explicit, "--allow must register as explicit");
    }

    #[test]
    fn obo_op_envelope_builds_without_leaf_flag() {
        // Regression: 219 ops carry an `on-behalf-of` header param whose leaf flag
        // is suppressed. `envelope::build` must NOT query that arg id (clap panics
        // on an unregistered id) and must emit no `on-behalf-of` header.
        let (command, resolve_map) = tree::build(false, false);
        let matches = command
            .try_get_matches_from([
                "sendgrid",
                "account",
                "teammates",
                "get-teammate",
                "--username",
                "alice",
            ])
            .expect("parses without --on-behalf-of");
        let (chain, leaf) = resolve::leaf_matches(&matches);
        let op = resolve_map
            .get(&chain.join(" "))
            .copied()
            .expect("resolves");
        assert!(
            op.params.iter().any(|p| p.name == "on-behalf-of"),
            "GetTeammate is expected to carry an on-behalf-of header param"
        );
        let env = envelope::build(op, leaf, &test_globals()).expect("envelope builds");
        assert_eq!(env["path"]["username"], Value::String("alice".into()));
        assert!(
            env["header"]
                .as_object()
                .map(|m| m.is_empty())
                .unwrap_or(true),
            "no on-behalf-of (or any) header should be emitted from the leaf"
        );
    }

    #[test]
    fn obo_leaf_flag_is_rejected() {
        // The suppressed leaf flag must not exist — passing it is an unknown-arg error.
        let (command, _resolve) = tree::build(false, false);
        let err = command.try_get_matches_from([
            "sendgrid",
            "account",
            "teammates",
            "get-teammate",
            "--username",
            "alice",
            "--on-behalf-of",
            "subuser",
        ]);
        assert!(err.is_err(), "leaf --on-behalf-of should be rejected");
    }

    #[test]
    fn global_on_behalf_of_still_parses() {
        // Impersonation is routed only through the governed global flag (root-level).
        let (command, _resolve) = tree::build(false, false);
        let matches = command
            .try_get_matches_from([
                "sendgrid",
                "--on-behalf-of",
                "subuser",
                "account",
                "teammates",
                "get-teammate",
                "--username",
                "alice",
            ])
            .expect("global --on-behalf-of parses before the subcommand");
        let globals = GlobalOpts::from_matches(&matches).expect("globals");
        assert_eq!(globals.on_behalf_of.as_deref(), Some("subuser"));
    }

    #[test]
    fn auth_subcommands_parse() {
        let (command, _resolve) = tree::build(false, false);
        for sub in ["scopes", "whoami", "doctor"] {
            command
                .clone()
                .try_get_matches_from(["sendgrid", "auth", sub])
                .unwrap_or_else(|e| panic!("auth {sub} parses: {e}"));
        }
        // The group requires a subcommand.
        assert!(
            command.try_get_matches_from(["sendgrid", "auth"]).is_err(),
            "bare `auth` requires a subcommand"
        );
    }

    #[test]
    fn async_ops_expose_the_right_flags() {
        let (command, _resolve) = tree::build(false, false);
        // Poll op → --await.
        command
            .clone()
            .try_get_matches_from([
                "sendgrid",
                "marketing",
                "contacts",
                "export-contact",
                "--await",
            ])
            .expect("export-contact --await");
        // ExternalUpload op → --upload-file.
        command
            .clone()
            .try_get_matches_from([
                "sendgrid",
                "marketing",
                "contacts",
                "import-contact",
                "--upload-file",
                "/tmp/x.csv",
            ])
            .expect("import-contact --upload-file");
        // A non-async op must reject --await.
        assert!(
            command
                .try_get_matches_from(["sendgrid", "mail", "send", "send-mail", "--await"])
                .is_err(),
            "--await should not exist on a non-poll op"
        );
    }

    #[test]
    fn hoist_globals_moves_trailing_query_but_not_leaf_limit() {
        let (_cmd, resolve) = tree::build(false, false);
        let declares = |chain: &str, long: &str| {
            resolve
                .get(chain)
                .is_some_and(|op| tree::leaf_declares_flag(op, long))
        };

        // `--query` after the subcommand is hoisted (list-template has no `query`
        // param), landing before the subcommand with its value intact.
        let argv = [
            "sendgrid",
            "templates",
            "list-template",
            "--query",
            "result",
        ]
        .map(String::from)
        .to_vec();
        let hoisted = hoist_globals(argv, declares);
        let q = hoisted.iter().position(|t| t == "--query").expect("kept");
        let sub = hoisted.iter().position(|t| t == "templates").expect("kept");
        assert!(
            q < sub,
            "--query should be hoisted before the subcommand: {hoisted:?}"
        );
        assert_eq!(hoisted[q + 1], "result", "value follows the hoisted flag");

        // A NESTED op (3-token chain) that declares its OWN `--limit` must keep it as
        // a leaf value — the chain-key lookup must match the resolve map exactly.
        let argv = [
            "sendgrid",
            "account",
            "subusers",
            "list-subuser",
            "--limit",
            "5",
        ]
        .map(String::from)
        .to_vec();
        let hoisted = hoist_globals(argv, declares);
        let lim = hoisted.iter().position(|t| t == "--limit").expect("kept");
        let sub = hoisted
            .iter()
            .position(|t| t == "list-subuser")
            .expect("kept");
        assert!(
            lim > sub,
            "leaf --limit must stay after the subcommand: {hoisted:?}"
        );
    }

    fn test_globals() -> GlobalOpts {
        GlobalOpts {
            region: sendgrid_core::Region::Global,
            output: globals::OutputFormat::Json,
            query: None,
            dry_run: false,
            all: false,
            limit: None,
            offset: None,
            page_token: None,
            include_legacy: false,
            allow: None,
            allow_explicit: false,
            allow_bulk: false,
            on_behalf_of: None,
            api_key: Some(DUMMY_KEY.to_string()),
            api_key_stdin: false,
            explain: false,
        }
    }
}
