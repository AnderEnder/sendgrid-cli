//! `xtask` library — the codegen pipeline that turns the 46 vendored SendGrid
//! OpenAPI specs + the curated `data/*.toml` tables into the deterministic IR
//! artifact embedded by `sendgrid-core`.
//!
//! Split into:
//!   - [`specs`]  — `serde_json::Value` parser + `$ref` resolver (HTTP shape).
//!   - [`schema`] — recursive resolution + 3.0→2020-12 normalization (input schemas).
//!   - [`tables`] — load/deserialize the curated `data/*.toml`.
//!   - [`build`]  — merge specs + tables → `Vec<OperationIr>` + schema map.
//!   - [`emit`]   — deterministic (recursively key-sorted) JSON emit + write.
//!   - [`codegen`]— orchestrate: parse → build → openapiv3/jsonschema gates → emit.

pub mod build;
pub mod codegen;
pub mod emit;
pub mod schema;
pub mod specs;
pub mod tables;

use std::path::{Path, PathBuf};

/// Workspace root, computed from this crate's manifest dir (xtask is `<root>/xtask`).
/// Used so both `cargo run` and `cargo test` resolve `specs/`, `data/`, and the
/// `generated/` output regardless of the process's current working directory.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest dir has a parent (the workspace root)")
        .to_path_buf()
}

/// `<root>/specs` — the 46 vendored OpenAPI specs.
pub fn specs_dir() -> PathBuf {
    workspace_root().join("specs")
}

/// `<root>/data` — the curated TOML tables.
pub fn data_dir() -> PathBuf {
    workspace_root().join("data")
}

/// `<root>/crates/core/generated` — the committed IR artifact destination.
pub fn generated_dir() -> PathBuf {
    workspace_root().join("crates/core/generated")
}
