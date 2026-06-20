//! `sendgrid-core` — the Operation IR contract + (later) runtime core.
//!
//! P0 exposes the IR types and the `Registry` that parses the committed,
//! `xtask codegen`-produced artifact embedded via `include_str!`. The runtime
//! modules (dispatch chokepoint, region engine, etc.) are added by later phases.

pub mod ir;
pub mod registry;

pub use registry::Registry;
