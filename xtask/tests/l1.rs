//! L1 invariant tests — assert the codegen pipeline holds the contract the whole
//! tool depends on. These drive the *same* `xtask` pipeline the binary runs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use xtask::specs::Stats;
use xtask::{build, codegen, emit, specs, tables};

const EXPECTED_SPECS: usize = 46;
const EXPECTED_OPS: usize = 391;

fn parse_specs() -> Vec<specs::SpecFile> {
    let mut stats = Stats::default();
    specs::parse_all(&xtask::specs_dir(), &mut stats).expect("parse all specs")
}

fn build_ir() -> build::BuildOutput {
    codegen::pipeline().expect("run codegen pipeline")
}

#[test]
fn parses_46_with_both_parsers() {
    // Value parser (IR-construction path).
    let files = parse_specs();
    assert_eq!(files.len(), EXPECTED_SPECS, "Value parser file count");

    // openapiv3 validity gate (backend T's parser).
    let (ok, total, first_err) =
        specs::openapiv3_parse_count(&xtask::specs_dir()).expect("openapiv3 scan");
    assert_eq!(total, EXPECTED_SPECS, "spec file count for openapiv3");
    assert_eq!(
        ok, EXPECTED_SPECS,
        "openapiv3 parsed all; first error: {first_err:?}"
    );
}

#[test]
fn total_ops_is_391() {
    assert_eq!(build_ir().ops.len(), EXPECTED_OPS);
}

#[test]
fn all_refs_resolve_no_cycles() {
    let out = build_ir();
    assert!(
        out.stats.unresolved_refs.is_empty(),
        "unresolved $refs: {:?}",
        out.stats.unresolved_refs
    );
    assert!(
        out.stats.cycles.is_empty(),
        "schema $ref cycles: {:?}",
        out.stats.cycles
    );
}

#[test]
fn operation_id_unique_within_each_file() {
    for f in parse_specs() {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for op in &f.ops {
            *seen.entry(op.operation_id.as_str()).or_default() += 1;
        }
        let dups: Vec<_> = seen
            .iter()
            .filter(|(_, c)| **c > 1)
            .map(|(k, _)| *k)
            .collect();
        assert!(
            dups.is_empty(),
            "{}: duplicate operationIds {dups:?}",
            f.namespace
        );
    }
}

fn is_mcp_safe(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[test]
fn ids_and_aliases_unique_charset_and_length() {
    let out = build_ir();
    let real_ids: HashSet<&str> = out.ops.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(real_ids.len(), out.ops.len(), "duplicate `id` across ops");

    // Aliases share the same rules and must not shadow a real id.
    let mut all_keys: HashSet<String> = real_ids.iter().map(|s| s.to_string()).collect();
    for op in &out.ops {
        for id in std::iter::once(&op.id).chain(op.id_alias.iter()) {
            assert!(is_mcp_safe(id), "id {id:?} violates ^[A-Za-z0-9_]+$");
            assert!(id.len() <= 64, "id {id:?} exceeds 64 chars ({})", id.len());
        }
        if let Some(alias) = &op.id_alias {
            assert!(
                !real_ids.contains(alias.as_str()),
                "alias {alias:?} shadows a real id"
            );
            assert!(
                all_keys.insert(alias.clone()),
                "alias {alias:?} collides with another key"
            );
        }
    }
}

#[test]
fn cli_paths_globally_unique() {
    let out = build_ir();
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for op in &out.ops {
        let path = op.cli_path.join(" ");
        if let Some(prev) = seen.insert(path.clone(), &op.id) {
            panic!("cli_path collision {path:?}: {prev} and {}", op.id);
        }
    }
    assert_eq!(seen.len(), out.ops.len());
}

#[test]
fn every_input_schema_is_valid_2020_12_and_standalone() {
    let out = build_ir();
    assert!(!out.schemas.is_empty(), "expected embedded input schemas");
    for (id, schema) in &out.schemas {
        assert!(
            !schema.to_string().contains("\"$ref\""),
            "schema {id:?} still contains a $ref (not standalone)"
        );
        codegen::compile_schema(schema)
            .unwrap_or_else(|e| panic!("schema {id:?} invalid under draft 2020-12: {e}"));
    }
}

#[test]
fn param_style_and_explode_survive_into_ir() {
    let out = build_ir();
    let find = |ns: &str, oid: &str| {
        out.ops
            .iter()
            .find(|o| o.namespace == ns && o.operation_id == oid)
            .unwrap_or_else(|| panic!("op {ns}.{oid} not found"))
    };

    // Declared `style=form, explode=false` array query param (comma-joined wire form).
    let export = find("mc_stats", "ExportSingleSendStat");
    let ids = export
        .params
        .iter()
        .find(|p| p.name == "ids")
        .expect("ids param");
    assert_eq!(ids.ty, "array");
    assert_eq!(ids.item_ty.as_deref(), Some("string"));
    assert_eq!(ids.style.as_deref(), Some("form"));
    assert_eq!(ids.explode, Some(false));

    // Curated comma-join override forces explode=false where the spec under-declares.
    for (ns, oid) in [
        ("integrations", "DeleteIntegration"),
        ("mc_segments", "ListSegment"),
        ("mc_segments_2.0", "ListSegment"),
    ] {
        let op = find(ns, oid);
        let p = op
            .params
            .iter()
            .find(|p| p.name == "ids")
            .expect("ids param");
        assert_eq!(
            p.explode,
            Some(false),
            "{ns}.{oid} ids should be comma-joined"
        );
    }

    // Sanity: many params carry explode=false; at least one declares style=form.
    let explode_false = out
        .ops
        .iter()
        .flat_map(|o| &o.params)
        .filter(|p| p.explode == Some(false))
        .count();
    assert!(
        explode_false >= 14,
        "expected >=14 explode=false params, got {explode_false}"
    );
    assert!(
        out.ops
            .iter()
            .flat_map(|o| &o.params)
            .any(|p| p.style.as_deref() == Some("form")),
        "expected at least one style=form param"
    );
}

#[test]
fn codegen_is_idempotent_and_committed_artifact_is_current() {
    // Two independent in-process builds must render byte-identical output.
    let a = build_ir();
    let b = build_ir();
    let ir_a = emit::render_ir(&a.ops).unwrap();
    let ir_b = emit::render_ir(&b.ops).unwrap();
    assert_eq!(ir_a, ir_b, "ir.json render is non-deterministic");
    let sc_a = emit::render_schemas(&a.schemas).unwrap();
    let sc_b = emit::render_schemas(&b.schemas).unwrap();
    assert_eq!(sc_a, sc_b, "schemas.json render is non-deterministic");

    // The committed artifact must equal a fresh render (regen → empty diff).
    let gen_dir = xtask::generated_dir();
    let on_disk_ir = std::fs::read_to_string(gen_dir.join("ir.json")).unwrap();
    let on_disk_sc = std::fs::read_to_string(gen_dir.join("schemas.json")).unwrap();
    assert_eq!(
        on_disk_ir, ir_a,
        "committed ir.json is stale — run `cargo run -p xtask -- codegen`"
    );
    assert_eq!(
        on_disk_sc, sc_a,
        "committed schemas.json is stale — run `cargo run -p xtask -- codegen`"
    );
}

#[test]
fn region_global_only_matches_servers() {
    // Cross-check the curated region.toml against each spec's parsed servers[].
    let files = parse_specs();
    let mut derived_global: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        let servers = f.root.get("servers").and_then(|v| v.as_array());
        let has_eu = servers
            .map(|arr| {
                arr.iter().any(|s| {
                    s.get("url")
                        .and_then(|u| u.as_str())
                        .map(|u| u.contains("eu.sendgrid.com"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if !has_eu {
            derived_global.insert(f.namespace.clone());
        }
    }
    assert_eq!(
        derived_global.len(),
        14,
        "expected 14 global-only specs, got {derived_global:?}"
    );

    // Every op's flag must agree with its namespace's server-derived value.
    let out = build_ir();
    for op in &out.ops {
        let expected = derived_global.contains(&op.namespace);
        assert_eq!(
            op.region_global_only, expected,
            "{}: region_global_only={} but servers say {expected}",
            op.id, op.region_global_only
        );
    }
}

#[test]
fn tables_load_and_cover_all_46_namespaces() {
    let t = tables::Tables::load(&xtask::data_dir()).expect("load tables");
    let files = parse_specs();
    for f in &files {
        assert!(
            t.taxonomy.namespaces.contains_key(&f.namespace),
            "taxonomy.toml missing namespace {}",
            f.namespace
        );
    }
    assert_eq!(t.taxonomy.namespaces.len(), EXPECTED_SPECS);
}
