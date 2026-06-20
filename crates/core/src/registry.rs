//! The operation **Registry** — the parsed, in-memory view of the committed IR
//! artifact (`generated/ir.json` + `generated/schemas.json`), embedded into this
//! crate at compile time via `include_str!`.
//!
//! Everything downstream (CLI tree, MCP meta-tools, the dispatch chokepoint)
//! reads operations from here. The artifact is produced by `xtask codegen`; this
//! module never parses OpenAPI specs.

use crate::ir::OperationIr;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Raw embedded IR (the `Vec<OperationIr>` as deterministic JSON).
const IR_JSON: &str = include_str!("../generated/ir.json");
/// Raw embedded schema map (`body_schema_id` → normalized 2020-12 input schema).
const SCHEMAS_JSON: &str = include_str!("../generated/schemas.json");

/// Indexed, immutable view over all SendGrid operations + their input schemas.
#[derive(Debug)]
pub struct Registry {
    ops: Vec<OperationIr>,
    by_id: HashMap<String, usize>,
    by_cli_path: HashMap<String, usize>,
    schemas: HashMap<String, Value>,
}

impl Registry {
    /// Parse the embedded artifact. Panics only on a malformed artifact, which
    /// is a build-time (codegen) bug, never a runtime input — so a panic here is
    /// the correct, loud failure mode.
    fn load() -> Registry {
        let ops: Vec<OperationIr> =
            serde_json::from_str(IR_JSON).expect("embedded generated/ir.json is valid IR");
        let schemas: HashMap<String, Value> = serde_json::from_str(SCHEMAS_JSON)
            .expect("embedded generated/schemas.json is a valid schema map");

        let mut by_id = HashMap::with_capacity(ops.len() * 2);
        let mut by_cli_path = HashMap::with_capacity(ops.len());
        for (i, op) in ops.iter().enumerate() {
            // Primary id + the curated alias both resolve to the same op.
            if by_id.insert(op.id.clone(), i).is_some() {
                panic!("duplicate operation id in artifact: {}", op.id);
            }
            if let Some(alias) = &op.id_alias
                && by_id.insert(alias.clone(), i).is_some()
            {
                panic!("id_alias collides with an existing id: {alias}");
            }
            let cli = op.cli_path.join(" ");
            if by_cli_path.insert(cli.clone(), i).is_some() {
                panic!("duplicate cli_path in artifact: {cli}");
            }
        }
        Registry {
            ops,
            by_id,
            by_cli_path,
            schemas,
        }
    }

    /// The process-wide registry, parsed once on first access.
    pub fn global() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(Registry::load)
    }

    /// All operations, in deterministic (id-sorted) order.
    pub fn operations(&self) -> &[OperationIr] {
        &self.ops
    }

    /// Number of operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the registry is empty (true only for the bootstrap placeholder).
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Look up an op by canonical `id` or by its curated `id_alias`.
    pub fn by_id(&self, id: &str) -> Option<&OperationIr> {
        self.by_id.get(id).map(|&i| &self.ops[i])
    }

    /// Look up an op by its rendered CLI path (the `cli_path` tokens joined by spaces).
    pub fn by_cli_path(&self, path: &str) -> Option<&OperationIr> {
        self.by_cli_path.get(path).map(|&i| &self.ops[i])
    }

    /// The normalized (JSON Schema 2020-12) request-body schema for `body_schema_id`.
    pub fn schema(&self, body_schema_id: &str) -> Option<&Value> {
        self.schemas.get(body_schema_id)
    }

    /// The normalized request-body schema for an operation, if it has one.
    pub fn schema_for(&self, op: &OperationIr) -> Option<&Value> {
        op.body_schema_id.as_deref().and_then(|id| self.schema(id))
    }

    /// Number of embedded input schemas.
    pub fn schema_count(&self) -> usize {
        self.schemas.len()
    }
}
