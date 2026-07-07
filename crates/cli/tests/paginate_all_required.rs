//! Integration tests for `--all` relaxing a REQUIRED page-size/limit param.
//!
//! An op like `templates list-template` declares `page_size` as REQUIRED. clap
//! would reject the command without it — even though under `--all` the runtime
//! injects a default page size. So `--all` must make that param non-required at the
//! clap layer, letting the runtime injection (`apply_defaults::inject_page_size`)
//! fill it in. Without `--all` the param stays required.

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
fn list_template_with_all_and_no_page_size_succeeds() {
    // Under `--all`, the required `page_size` param is relaxed at the clap layer, so
    // the command parses without it and the runtime injects the default page size.
    let out = sendgrid(&[
        "templates",
        "list-template",
        "--all",
        "--dry-run",
        "--output",
        "json",
    ]);
    assert!(
        out.status.success(),
        "expected success under --all without --page_size; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("page_size="),
        "dry-run preview URL should carry an injected page_size= query param; stdout: {stdout}"
    );
}

#[test]
fn list_template_without_all_still_requires_page_size() {
    // Without `--all` we do NOT relax anything: the required `page_size` param is
    // still enforced by clap, so omitting it is a hard missing-required-arg error.
    let out = sendgrid(&[
        "templates",
        "list-template",
        "--dry-run",
        "--output",
        "json",
    ]);
    assert!(
        !out.status.success(),
        "omitting --page_size without --all must still fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--page_size") && stderr.contains("required arguments were not provided"),
        "expected the missing-required-arg error for --page_size; stderr: {stderr}"
    );
}
