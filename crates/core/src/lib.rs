//! `sendgrid-core` — the Operation IR contract + the shared runtime core.
//!
//! P0 exposes the IR types and the [`Registry`] that parses the committed,
//! `xtask codegen`-produced artifact embedded via `include_str!`. The [`runtime`]
//! module adds the data-driven dispatcher (**Backend D**) and the single
//! [`runtime::execute`] chokepoint the CLI and MCP server consume.

pub mod ir;
pub mod registry;
pub mod runtime;

pub use registry::Registry;

// The frozen runtime entrypoint + its contract types, re-exported at the crate
// root for ergonomic consumption by the CLI/MCP crates.
pub use runtime::{
    ApiKey, AuthError, DispatchError, DispatchResponse, ExecuteResult, OperationDispatcher,
    Payload, Policy, Region, ReqwestDispatcher, RuntimeConfig, execute, execute_with,
};
