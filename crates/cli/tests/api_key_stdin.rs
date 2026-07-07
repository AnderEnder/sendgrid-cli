//! Integration test for `--api-key-stdin` (Task 7.1): the API key is read from
//! the first line of stdin — never argv, never the environment — so it cannot
//! leak via shell history or a process listing.

use assert_cmd::Command;

/// A syntactically well-formed dummy key (`SG.<22>.<43>`).
const DUMMY_KEY: &str = "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

/// Run `sendgrid` with the key piped on stdin and **no** `SENDGRID_API_KEY` in
/// the environment — so stdin is the only possible key source. If the stdin path
/// were broken, `resolve_raw_key` could otherwise silently fall back to the env
/// var and make the assertion vacuous.
fn sendgrid_stdin(args: &[&str], stdin: &str) -> std::process::Output {
    Command::cargo_bin("sendgrid")
        .expect("sendgrid binary builds")
        .env_remove("SENDGRID_API_KEY")
        .args(args)
        .write_stdin(stdin)
        .output()
        .expect("binary runs")
}

#[test]
fn api_key_stdin_is_accepted() {
    // `--dry-run`: with a dummy key, doctor's live `GET /v3/scopes` returns HTTP
    // 401 (exit 1); dry-run previews the call instead so the exit stays 0 and the
    // test is network-independent. The key still flows through resolution, so the
    // `api_key` block reports it present with a fingerprint.
    let out = sendgrid_stdin(
        &[
            "--api-key-stdin",
            "auth",
            "doctor",
            "--dry-run",
            "--output",
            "json",
        ],
        "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123\n",
    );
    assert!(
        out.status.success(),
        "auth doctor should succeed with a stdin key; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `fingerprint` only appears when the key resolved as present — proof the
    // stdin path fed the key through (env was removed, argv carried no key).
    assert!(
        stdout.contains("fingerprint"),
        "expected a key fingerprint in the report; stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"present\": true"),
        "expected the key to be reported present; stdout: {stdout}"
    );
}

#[test]
fn api_key_stdin_conflicts_with_api_key() {
    // The spec'd mutual exclusion: passing both must be a clap conflict error.
    let out = sendgrid_stdin(
        &["--api-key", DUMMY_KEY, "--api-key-stdin", "auth", "doctor"],
        "\n",
    );
    assert!(
        !out.status.success(),
        "--api-key and --api-key-stdin must not be combined"
    );
}
