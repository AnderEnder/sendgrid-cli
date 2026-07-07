//! Integration tests for global-flag positioning: globals that are `global(true)`
//! must be accepted both before AND after the operation subcommand. Task 3.1
//! globalizes `--allow` (the collision-free policy flag).

use assert_cmd::Command;

/// A syntactically well-formed dummy key (`SG.<22>.<43>`) — nothing is sent under
/// `--dry-run`, but key resolution still runs, so a valid shape is required.
const DUMMY_KEY: &str = "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

/// Run the built `sendgrid` binary with a dummy key in the environment.
fn sendgrid(args: &[&str]) -> std::process::Output {
    Command::cargo_bin("sendgrid")
        .expect("sendgrid binary builds")
        .env("SENDGRID_API_KEY", DUMMY_KEY)
        .args(args)
        .output()
        .expect("binary runs")
}

#[test]
fn allow_after_subcommand_is_accepted() {
    let out = sendgrid(&[
        "templates",
        "list-template",
        "--page_size",
        "1",
        "--allow",
        "read",
        "--dry-run",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn query_after_subcommand_is_hoisted_and_works() {
    // `--query` is root-only (it collides with the `query` param 2 ops declare), so
    // clap rejects it after the subcommand. `list-template` has no `query` param, so
    // the argv pre-scan hoists the trailing `--query` to before the subcommand.
    let out = sendgrid(&[
        "templates",
        "list-template",
        "--page_size",
        "1",
        "--query",
        "result",
        "--dry-run",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn inferred_group_keeps_leaf_limit_as_query_param() {
    // clap `.infer_subcommands(true)` lets an agent type a unique prefix / singular
    // of a top-level group (`suppression` for `suppressions`). The argv pre-scan keys
    // off CANONICAL group names, so the inferred token must be canonicalized before
    // flag-hoisting — otherwise `global-suppression`'s OWN `--limit` query param is
    // wrongly hoisted to the pagination-cap global and SILENTLY DROPPED.
    let out = sendgrid(&[
        "suppression", // inferred singular of `suppressions`
        "list",
        "global-suppression",
        "--limit",
        "7",
        "--dry-run",
        "--output",
        "json",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("limit=7"),
        "inferred-group leaf --limit must reach the request query; stdout: {stdout}"
    );

    // The exact-name form must still work identically.
    let out = sendgrid(&[
        "suppressions",
        "list",
        "global-suppression",
        "--limit",
        "7",
        "--dry-run",
        "--output",
        "json",
    ]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("limit=7"),
        "exact-name leaf --limit must reach the request query"
    );
}

#[test]
fn leaf_param_named_like_a_global_is_not_hoisted() {
    // `account subusers list-subuser` declares its OWN `--limit` query param. A
    // trailing `--limit` must bind to the leaf (landing in the request query), NOT be
    // hoisted to the pagination-cap global (which would drop it from the request).
    let out = sendgrid(&[
        "account",
        "subusers",
        "list-subuser",
        "--limit",
        "5",
        "--dry-run",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("limit=5"),
        "leaf --limit must reach the request query; stdout: {stdout}"
    );
}
