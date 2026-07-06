//! Integration tests for command discovery ergonomics. Task 4.1 enables clap's
//! `infer_subcommands`, so a unique prefix of a group name (e.g. the singular
//! `template` for the `templates` group) routes to that group instead of erroring
//! with `unrecognized subcommand`.

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
fn singular_noun_is_inferred() {
    // `template` is a unique prefix of the `templates` group (and not itself a full
    // subcommand name), so subcommand inference routes it to `templates`.
    let out = sendgrid(&["template", "list-template", "--page_size", "1", "--dry-run"]);
    assert!(
        out.status.success(),
        "`template` should infer to `templates`; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
