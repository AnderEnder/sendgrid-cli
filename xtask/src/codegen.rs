//! Codegen orchestration: parse → build → L1 self-gates → deterministic emit.
//!
//! The same `pipeline()` the binary runs is what the L1 tests drive, so the gates
//! enforced here (every `$ref` resolves, every emitted INPUT schema is valid
//! 2020-12, openapiv3 parses 46/46) are exactly what the test suite re-asserts.

use crate::build::{self, BuildOutput};
use crate::tables::Tables;
use crate::{data_dir, emit, generated_dir, specs, specs_dir};
use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Parse all specs + load all tables + build the IR. No file writes, no gates —
/// the pure transform, shared by the binary and the tests.
pub fn pipeline() -> Result<BuildOutput> {
    let mut stats = specs::Stats::default();
    let files = specs::parse_all(&specs_dir(), &mut stats)?;
    let tables = Tables::load(&data_dir())?;
    let mut out = build::build(&files, &tables)?;
    // Fold the parse-time resolution stats (param/requestBody $ref misses) into the
    // build stats (which already carries the deep schema-resolution misses).
    out.stats.unresolved_refs.extend(stats.unresolved_refs);
    out.stats.cycles.extend(stats.cycles);
    Ok(out)
}

/// Compile one schema under JSON Schema draft 2020-12; `Err` carries a message.
pub fn compile_schema(schema: &Value) -> std::result::Result<(), String> {
    jsonschema::draft202012::options()
        .build(schema)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Assert every emitted INPUT schema is self-contained (no residual `$ref`) and
/// compiles under draft 2020-12.
pub fn gate_schemas(out: &BuildOutput) -> Result<()> {
    for (id, schema) in &out.schemas {
        let txt = schema.to_string();
        if txt.contains("\"$ref\"") {
            bail!("schema {id:?} still contains a $ref after resolution (not standalone)");
        }
        if let Err(e) = compile_schema(schema) {
            bail!("schema {id:?} is not valid JSON Schema 2020-12: {e}");
        }
    }
    Ok(())
}

/// All the L1 gates that don't require re-reading the artifact from disk.
pub fn gate_all(out: &BuildOutput) -> Result<()> {
    // (1) every $ref resolved during parse + schema inlining.
    if !out.stats.unresolved_refs.is_empty() {
        let mut u = out.stats.unresolved_refs.clone();
        u.sort();
        u.dedup();
        bail!("unresolved $refs: {u:?}");
    }
    // (2) no schema cycles (would force $defs bundling instead of inlining).
    if !out.stats.cycles.is_empty() {
        bail!("schema $ref cycles detected: {:?}", out.stats.cycles);
    }
    // (3) every emitted input schema valid 2020-12 + standalone.
    gate_schemas(out)?;
    Ok(())
}

/// Binary entry point: run the pipeline, all gates, the openapiv3 validity gate,
/// then write the deterministic artifacts and print a report.
pub fn run() -> Result<()> {
    // openapiv3 L1 validity gate (the parser backend T consumes).
    let (ok, total, first_err) = specs::openapiv3_parse_count(&specs_dir())?;
    if ok != total {
        bail!("openapiv3 parsed {ok}/{total} specs; first error: {first_err:?}");
    }

    let out = pipeline().context("build IR")?;
    gate_all(&out)?;

    for w in &out.warnings {
        eprintln!("warning: {w}");
    }

    let (wrote_ir, wrote_schemas) =
        emit::write_artifacts(&generated_dir(), &out.ops, &out.schemas)?;

    // Per-class / per-kind tallies for the report.
    use sendgrid_core::ir::{AsyncJob, PaginationKind, SideEffect};
    let mut se = std::collections::BTreeMap::<&str, usize>::new();
    let mut pg = std::collections::BTreeMap::<&str, usize>::new();
    let mut aj = std::collections::BTreeMap::<&str, usize>::new();
    let mut region_only = 0usize;
    let mut array_bodies = 0usize;
    let mut data_keys = 0usize;
    let mut with_constraints = 0usize;
    let mut with_search_keywords = 0usize;
    let mut with_response_schema = 0usize;
    let mut with_reveal_fields = 0usize;
    for op in &out.ops {
        *se.entry(match op.side_effect {
            SideEffect::Read => "read",
            SideEffect::Write => "write",
            SideEffect::Destructive => "destructive",
            SideEffect::Send => "send",
        })
        .or_default() += 1;
        *pg.entry(match op.pagination.kind {
            PaginationKind::None => "none",
            PaginationKind::Offset => "offset",
            PaginationKind::PageNumber => "page_number",
            PaginationKind::PageToken => "page_token",
            PaginationKind::CursorKey => "cursor_key",
            PaginationKind::CursorRecord => "cursor_record",
            PaginationKind::CappedSingle => "capped_single",
        })
        .or_default() += 1;
        *aj.entry(match op.async_job {
            AsyncJob::None => "none",
            AsyncJob::Poll => "poll",
            AsyncJob::FireAndForget => "fire_and_forget",
            AsyncJob::ExternalUpload => "external_upload",
            AsyncJob::ExternalDownload => "external_download",
        })
        .or_default() += 1;
        if op.region_global_only {
            region_only += 1;
        }
        if op.body_is_array {
            array_bodies += 1;
        }
        if op.pagination.data_key.is_some() {
            data_keys += 1;
        }
        if !op.constraints.is_empty() {
            with_constraints += 1;
        }
        if !op.search_keywords.is_empty() {
            with_search_keywords += 1;
        }
        if op.response_schema_id.is_some() {
            with_response_schema += 1;
        }
        if !op.reveal_response_fields.is_empty() {
            with_reveal_fields += 1;
        }
    }

    println!(
        "xtask codegen: {} ops, {} input schemas",
        out.ops.len(),
        out.schemas.len()
    );
    println!("  openapiv3 gate: {ok}/{total} parsed");
    println!("  side_effect:    {se:?}");
    println!("  pagination:     {pg:?}");
    println!("  async_job:      {aj:?}");
    println!("  region_global_only: {region_only}");
    println!("  top-level-array bodies: {array_bodies}");
    println!("  pagination data_key: {data_keys}");
    println!("  ops with constraints: {with_constraints}");
    println!("  ops with search_keywords: {with_search_keywords}");
    println!("  ops with response_schema_id: {with_response_schema}");
    println!("  ops with reveal_response_fields: {with_reveal_fields}");
    println!(
        "  wrote ir.json: {wrote_ir}, schemas.json: {wrote_schemas} (false = already up to date)"
    );
    Ok(())
}
