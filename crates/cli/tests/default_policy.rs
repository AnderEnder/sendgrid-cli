//! Integration test for the fail-closed default policy (Task 6.1): with no
//! `--allow`, the CLI defaults to READ-ONLY, so a write op is refused at the
//! policy gate — before any request is dispatched.

use assert_cmd::Command;

/// A syntactically well-formed dummy key (`SG.<22>.<43>`). The policy gate fires
/// before dispatch, so no request is sent — the key just needs a valid shape for
/// resolution to succeed.
const DUMMY_KEY: &str = "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

fn sendgrid(args: &[&str]) -> std::process::Output {
    Command::cargo_bin("sendgrid")
        .expect("sendgrid binary builds")
        .env("SENDGRID_API_KEY", DUMMY_KEY)
        .args(args)
        .output()
        .expect("binary runs")
}

#[test]
fn write_op_blocked_without_allow() {
    // `update-password` is a `write` op with a full body → validation passes and the
    // call reaches the policy gate, which refuses it under the read-only default.
    let out = sendgrid(&[
        "account",
        "user",
        "update-password",
        "--body",
        r#"{"new_password":"aa","old_password":"bb"}"#,
    ]);
    assert!(
        !out.status.success(),
        "a write must be blocked without --allow; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("E_POLICY_DENIED") || stderr.contains("does not allow"),
        "expected a policy denial; stderr: {stderr}"
    );
}
