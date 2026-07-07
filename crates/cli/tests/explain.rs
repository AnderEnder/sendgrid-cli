//! Integration test for the global `--explain` flag (Task 5.2): on an operation
//! leaf it prints the op's human-formatted description (a runnable example plus the
//! response field-menu) and exits 0 *without dispatching* — so a bare `--explain`
//! works even when required leaf params are omitted, and no network call is made.

use assert_cmd::Command;

/// A syntactically well-formed dummy key (`SG.<22>.<43>`). `--explain` never
/// dispatches, so nothing is sent — but key resolution machinery expects a shape.
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
fn explain_prints_example_and_response_fields() {
    // No `--template_id` (a required leaf param): `--explain` must short-circuit
    // before clap enforces required args, and before any dispatch.
    let out = sendgrid(&["templates", "get-template", "--explain"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("Example:") && s.contains("Response fields:"),
        "explain output should carry an example + response field-menu; got:\n{s}"
    );
    assert!(
        out.status.success(),
        "explain must exit 0 without dispatching; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
