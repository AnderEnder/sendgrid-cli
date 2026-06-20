//! `cargo xtask` — codegen + spec-drift tooling (unshipped).
//!
//! Subcommands:
//!   codegen  — parse vendored specs + curated tables → deterministic IR artifact
//!   drift    — compare freshly-fetched upstream specs vs vendored `specs/`,
//!              emit a semantic operation-set changelog
//!              (usage: `cargo xtask drift --upstream <dir> [--vendored <dir>] [--json]`)
//!
//! `drift` uses `diff`-style exit codes so CI can tell "drift" apart from "the tool
//! broke": `0` = no drift, `1` = drift detected, `2` = tool error (bad path,
//! malformed upstream JSON, …). Without this split a transient upstream error would
//! masquerade as drift and file a blank-changelog issue while the job stayed green.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or_default();
    match cmd {
        "codegen" => xtask::codegen::run(),
        "drift" => match xtask::drift::run(&args[2..]) {
            Ok(code) => std::process::exit(code), // 0 = no drift, 1 = drift
            Err(e) => {
                eprintln!("Error: {e:?}");
                std::process::exit(2); // 2 = tool error (distinct from drift)
            }
        },
        other => {
            eprintln!("usage: cargo xtask <codegen|drift>");
            eprintln!("  drift --upstream <dir> [--vendored <dir>] [--json]");
            anyhow::bail!("unknown xtask subcommand: {other:?}");
        }
    }
}
