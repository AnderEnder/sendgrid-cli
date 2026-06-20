//! Smoke test for the *consumption* path P1–P3 build on: parse the embedded
//! `generated/ir.json` + `schemas.json` via `Registry::global()`, build the
//! indexes, and resolve by id / alias / cli_path / schema. `cargo build` only
//! checks the embedded files are UTF-8; this is the only thing that actually runs
//! `Registry::load()` (serde round-trip + the dup/alias-shadow invariant panics).

use sendgrid_core::Registry;
use sendgrid_core::ir::SideEffect;

#[test]
fn embedded_artifact_loads_into_a_working_registry() {
    let r = Registry::global();
    assert_eq!(r.len(), 391, "embedded ir.json op count");
    assert!(r.schema_count() >= 130, "embedded schema count");

    // by_id resolves the canonical id, and the op is well-formed.
    let send = r.by_id("sg_mail_send_SendMail").expect("by_id canonical");
    assert_eq!(send.method, "POST");
    assert_eq!(send.side_effect, SideEffect::Send);
    assert!(send.has_body);

    // The curated alias resolves to the real (typo) op.
    let aliased = r
        .by_id("sg_suppressions_CreateAsmGroup")
        .expect("alias resolves");
    assert_eq!(aliased.operation_id, "CreatAsmGroup");

    // cli_path index (literal space-join of the cli_path tokens).
    assert!(
        r.by_cli_path("mail send send mail").is_some(),
        "cli_path index"
    );

    // body_schema_id → embedded normalized schema.
    let schema = r.schema_for(send).expect("schema_for body op");
    assert!(
        schema.get("properties").is_some(),
        "SendMail schema has properties"
    );
    assert_eq!(
        r.schema(send.body_schema_id.as_deref().unwrap()),
        Some(schema)
    );

    // A non-body read op has no schema.
    let read = r
        .operations()
        .iter()
        .find(|o| !o.has_body)
        .expect("a body-less op");
    assert!(r.schema_for(read).is_none());
}
