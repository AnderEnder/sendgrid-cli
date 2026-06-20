//! `sendgrid` — the agent-facing CLI (and `sendgrid mcp` server host).
//!
//! The command tree is built **at runtime** from `sendgrid_core::Registry`
//! (clap builder API, no derive). Every operation becomes a leaf command; global
//! flags are root-level; `execute()` in `sendgrid-core` is the single dispatch
//! chokepoint. See the module docs for the `cli_path` → command convention.

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

async fn run() -> i32 {
    // `--include-legacy` decides the *shape* of the tree (whether hidden ops and
    // the all-hidden `legacy` group exist), so it must be known before the tree
    // is built. A cheap argv pre-scan resolves it; the parsed flag still governs
    // all runtime behavior.
    let include_legacy = std::env::args().any(|a| a == "--include-legacy");
    let (command, resolve_map) = tree::build(include_legacy);

    // clap handles `--help`/`--version`/parse errors itself (exiting as it sees
    // fit); we only reach here with a valid parse.
    let matches = command.get_matches();

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
        let (command, resolve_map) = tree::build(false);
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

        let (command, resolve_map) = tree::build(false);
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
        let (command, _resolve) = tree::build(false);
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
    fn obo_op_envelope_builds_without_leaf_flag() {
        // Regression: 219 ops carry an `on-behalf-of` header param whose leaf flag
        // is suppressed. `envelope::build` must NOT query that arg id (clap panics
        // on an unregistered id) and must emit no `on-behalf-of` header.
        let (command, resolve_map) = tree::build(false);
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
        let (command, _resolve) = tree::build(false);
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
        let (command, _resolve) = tree::build(false);
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
        let (command, _resolve) = tree::build(false);
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
        let (command, _resolve) = tree::build(false);
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
            allow_bulk: false,
            on_behalf_of: None,
            api_key: Some(DUMMY_KEY.to_string()),
        }
    }
}
