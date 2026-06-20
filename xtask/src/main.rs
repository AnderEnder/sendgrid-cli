//! `cargo xtask` — codegen + spec-drift tooling (unshipped).
//!
//! Subcommands (P0 implements `codegen`):
//!   codegen  — parse vendored specs + curated tables → deterministic IR artifact
//!   drift    — (P5) compare upstream specs vs specs.lock, emit op-set changelog

fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "codegen" => xtask::codegen::run(),
        other => {
            eprintln!("usage: cargo xtask <codegen>");
            anyhow::bail!("unknown xtask subcommand: {other:?}");
        }
    }
}
