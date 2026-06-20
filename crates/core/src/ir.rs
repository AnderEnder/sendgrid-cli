//! The Operation IR — the **stable contract** that drives the CLI, the MCP
//! server, and all cross-cutting runtime behavior.
//!
//! The IR is generated once (by `xtask codegen`) from the 46 vendored OpenAPI
//! specs, merged with the curated tables in `data/`, and committed as a
//! deterministic artifact embedded into this crate. Nothing downstream depends
//! on *how* requests are executed (the generator backend) — only on this IR.

use serde::{Deserialize, Serialize};

/// Where a parameter is carried in the HTTP request.
///
/// Only `path`, `query`, and `header` occur in the SendGrid specs (verified:
/// zero `cookie` params across all 46 files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    Path,
    Query,
    Header,
}

/// Side-effect classification — the spine of the safety model. Derived from the
/// HTTP method, then overridden semantically by `data/safety.toml` (e.g. a POST
/// that erases PII is `Destructive`, not `Write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideEffect {
    /// GET — safe to call freely.
    Read,
    /// POST/PUT/PATCH that mutate config/resources reversibly.
    Write,
    /// DELETE, plus curated POSTs that irreversibly remove data (erase/purge/batchDelete).
    Destructive,
    /// Operations that send real email / spend money (mail/send, campaigns, invites).
    Send,
}

impl SideEffect {
    /// Method-only default, before curated overrides are applied.
    pub fn from_method(method: &str) -> Self {
        match method {
            "GET" => SideEffect::Read,
            "DELETE" => SideEffect::Destructive,
            _ => SideEffect::Write,
        }
    }
}

/// How an operation paginates. Detected offline from query params + 200 schema,
/// with curated overrides in `data/pagination.toml` for the cases that can't be
/// derived statically (e.g. the two `seq` ops whose responses have no schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaginationKind {
    /// Not a paginated list (single-shot).
    #[default]
    None,
    /// `limit` + `offset` query params.
    Offset,
    /// `page` + `page_size` query params.
    PageNumber,
    /// `page_token` query param; next token from the response envelope.
    PageToken,
    /// `after_key` cursor — in the query and/or `_metadata.next_params.after_key`.
    CursorKey,
    /// `after_subuser_id`-style record cursor.
    CursorRecord,
    /// Only a `limit` (no continuation) — single page, capped.
    CappedSingle,
}

/// Multi-step async job shape. Most ops are `None`; a handful return 202 + a job
/// to poll, or a presigned URL to upload-to / download-from an out-of-band host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AsyncJob {
    #[default]
    None,
    /// 202 + a status endpoint to poll until terminal.
    Poll,
    /// 202 with no status endpoint (e.g. recipient data-erase).
    FireAndForget,
    /// Returns `upload_uri` (+ headers) for a follow-up PUT to a non-SendGrid host.
    ExternalUpload,
    /// Returns a presigned URL to download from a non-SendGrid host.
    ExternalDownload,
}

/// A single request parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamIr {
    pub name: String,
    pub location: Location,
    pub required: bool,
    /// JSON-schema type: string/integer/number/boolean/array/object.
    pub ty: String,
    /// Element type when `ty == "array"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_ty: Option<String>,
    /// `format` (e.g. `date`, `date-time`) — drives CLI coercion + validation hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// OpenAPI `style` (only `form` occurs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// OpenAPI `explode`. SendGrid sets `false` on most array query params,
    /// meaning comma-joined (`ids=a,b,c`), not repeated keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Pagination metadata for an operation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pagination {
    pub kind: PaginationKind,
    /// Dotted path into the response where the next cursor lives,
    /// e.g. `_metadata.next_params.after_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_path: Option<String>,
    /// Query param to inject the next cursor into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_param: Option<String>,
    /// Key in the response holding the result array (for unwrapping/`--all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_key: Option<String>,
}

/// One SendGrid operation — the unit the whole tool is built around.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationIr {
    /// Globally-unique, MCP-safe id: `sg_<domain>_<subgroup>_<operationId>`
    /// (subgroup dropped when it equals the domain). Underscore-only, ≤64 chars.
    pub id: String,
    /// Alternate id that also resolves to this op via the registry. Used for the
    /// one well-spelled alias of a spec typo (`...CreateAsmGroup` →
    /// `...CreatAsmGroup`). Same charset/length/uniqueness rules as `id`; must not
    /// shadow any real `id`. Curated in `data/taxonomy.toml` (`id_alias`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_alias: Option<String>,
    /// The raw OpenAPI operationId (unique within a file, may collide across files).
    pub operation_id: String,
    /// Spec namespace derived from filename, e.g. `mc_contacts`.
    pub namespace: String,

    // --- Taxonomy (from data/taxonomy.toml) ---
    pub domain: String,
    pub subgroup: String,
    /// CLI command path: `["mail","send","send","mail"]` → `sendgrid mail send send-mail`.
    /// (subgroup collapsed when it equals the domain).
    pub cli_path: Vec<String>,
    /// Hidden from default `--help`/MCP search unless `--include-legacy`.
    #[serde(default)]
    pub hidden: bool,

    // --- HTTP shape ---
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub params: Vec<ParamIr>,
    /// Key into the embedded schema map for the request body (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_schema_id: Option<String>,
    #[serde(default)]
    pub has_body: bool,
    /// True when the request body is a top-level JSON array (4 such ops exist).
    #[serde(default)]
    pub body_is_array: bool,

    // --- Safety (from data/safety.toml + method default) ---
    pub side_effect: SideEffect,
    /// Response fields whose values are secrets to redact (e.g. `api_key`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_response_fields: Vec<String>,
    /// Request fields whose values are secrets to redact in previews (`password`, `*_secret`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_request_fields: Vec<String>,
    /// (param-or-body-field, value) pairs that, if present, promote the call to
    /// `bulk` (hard-deny unless `--allow-bulk`), e.g. `delete_all=true`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bulk_triggers: Vec<BulkTrigger>,

    // --- Cross-cutting behavior ---
    #[serde(default)]
    pub pagination: Pagination,
    #[serde(default)]
    pub async_job: AsyncJob,
    /// True when the spec declares no EU server for this op (14 global-only specs).
    #[serde(default)]
    pub region_global_only: bool,
    /// Safe to retry on a 5xx/network error (idempotent). Defaults from method.
    #[serde(default)]
    pub retry_safe_5xx: bool,
    /// Safe to retry on 429 (true for all methods — rejected before processing).
    #[serde(default = "default_true")]
    pub retry_safe_429: bool,
}

fn default_true() -> bool {
    true
}

/// A condition that promotes an operation to the `bulk` class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkTrigger {
    /// Field/param name, e.g. `delete_all`, `delete_contacts`.
    pub field: String,
    /// Where it appears.
    pub location: BulkLocation,
    /// The value that triggers bulk (matched stringly, so `"true"` covers bool+string).
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BulkLocation {
    Query,
    Body,
}

impl OperationIr {
    /// Required params for a given location.
    pub fn required_params(&self, loc: Location) -> impl Iterator<Item = &ParamIr> {
        self.params
            .iter()
            .filter(move |p| p.location == loc && p.required)
    }
}
