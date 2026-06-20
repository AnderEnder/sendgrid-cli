//! Curated table loading — deserialize `data/*.toml` into typed structures and
//! provide fast lookups keyed by the canonical `namespace.OperationId` (op key),
//! which disambiguates the cross-file operationId collisions (DeleteSegment ×3,
//! DeleteContact ×2).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

// --- taxonomy.toml ---

#[derive(Debug, Deserialize)]
pub struct Taxonomy {
    pub namespaces: BTreeMap<String, NsEntry>,
    pub leaf: LeafCfg,
    #[serde(default)]
    pub id_alias: Vec<IdAlias>,
    /// Curated search aliases keyed by namespace (applied to every op in it).
    #[serde(default)]
    pub search_keywords: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct NsEntry {
    pub domain: String,
    pub subgroup: String,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Deserialize)]
pub struct LeafCfg {
    pub known_verbs: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct IdAlias {
    pub namespace: String,
    pub operation_id: String,
    pub alias: String,
}

// --- safety.toml ---

#[derive(Debug, Deserialize)]
pub struct Safety {
    pub send_list: Vec<String>,
    pub destructive_overrides: Vec<String>,
    pub destructive_path_substrings: Vec<String>,
    pub secret_request_fields_global: Vec<String>,
    #[serde(default)]
    pub secret_response_fields: Vec<SecretResponse>,
    /// Intended-output response fields exempted from the generic SG-key pattern scrub.
    #[serde(default)]
    pub reveal_response_fields: Vec<SecretResponse>,
    #[serde(default)]
    pub bulk_triggers: Vec<BulkTriggerEntry>,
}

#[derive(Debug, Deserialize)]
pub struct SecretResponse {
    pub op: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct BulkTriggerEntry {
    pub op: String,
    pub field: String,
    pub location: String, // "query" | "body"
    pub value: String,
}

// --- region.toml ---

#[derive(Debug, Deserialize)]
pub struct Region {
    pub global_only: Vec<String>,
}

// --- pagination.toml ---

#[derive(Debug, Deserialize)]
pub struct Pagination {
    #[serde(default)]
    pub overrides: Vec<PaginationOverride>,
    #[serde(default)]
    pub comma_join: Vec<CommaJoin>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PaginationOverride {
    pub op: String,
    pub kind: String,
    #[serde(default)]
    pub cursor_path: Option<String>,
    #[serde(default)]
    pub inject_param: Option<String>,
    #[serde(default)]
    pub data_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommaJoin {
    pub op: String,
    pub param: String,
}

// --- constraints.toml ---

#[derive(Debug, Deserialize, Default)]
pub struct Constraints {
    #[serde(default)]
    pub constraint: Vec<ConstraintEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ConstraintEntry {
    pub op: String,
    /// "requires_one_of" | "mutually_exclusive" | "required_unless_present".
    pub rule: String,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub unless_present: Option<String>,
    /// For `required_unless_present`: array body field whose every element carrying
    /// `field` also satisfies the rule (e.g. per-personalization `subject`).
    #[serde(default)]
    pub or_each_in: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

// --- async_jobs.toml ---

#[derive(Debug, Deserialize, Default)]
pub struct AsyncJobs {
    #[serde(default)]
    pub job: Vec<AsyncJobEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AsyncJobEntry {
    pub op: String,
    /// "poll" | "external_upload" | "external_download" | "fire_and_forget".
    pub kind: String,
    /// Same-namespace operationId of the companion status op (codegen computes its id).
    #[serde(default)]
    pub status_operation_id: Option<String>,
    /// Dotted path to the presigned-URL field in the 2xx response.
    #[serde(default)]
    pub uri_field: Option<String>,
}

/// All curated tables, loaded once with derived fast-lookup indexes.
pub struct Tables {
    pub taxonomy: Taxonomy,
    pub safety: Safety,
    pub region: Region,
    pub pagination: Pagination,
    pub constraints: Constraints,
    pub async_jobs: AsyncJobs,

    // Derived indexes (built in `load`).
    pub send_set: BTreeSet<String>,
    pub destructive_set: BTreeSet<String>,
    pub global_only_set: BTreeSet<String>,
    pub known_verbs: BTreeSet<String>,
    pub secret_response_by_op: HashMap<String, Vec<String>>,
    pub reveal_response_by_op: HashMap<String, Vec<String>>,
    pub pagination_override_by_op: HashMap<String, PaginationOverride>,
    pub comma_join_by_op: HashMap<String, BTreeSet<String>>,
    pub bulk_by_op: HashMap<String, Vec<BulkTriggerEntry>>,
    pub alias_by_op: HashMap<String, String>,
    pub constraints_by_op: HashMap<String, Vec<ConstraintEntry>>,
    pub async_job_by_op: HashMap<String, AsyncJobEntry>,
}

fn load_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse TOML {}", path.display()))
}

impl Tables {
    pub fn load(data_dir: &Path) -> Result<Tables> {
        let taxonomy: Taxonomy = load_toml(&data_dir.join("taxonomy.toml"))?;
        let safety: Safety = load_toml(&data_dir.join("safety.toml"))?;
        let region: Region = load_toml(&data_dir.join("region.toml"))?;
        let pagination: Pagination = load_toml(&data_dir.join("pagination.toml"))?;
        let constraints: Constraints = load_toml(&data_dir.join("constraints.toml"))?;
        let async_jobs: AsyncJobs = load_toml(&data_dir.join("async_jobs.toml"))?;

        let send_set = safety.send_list.iter().cloned().collect();
        let destructive_set = safety.destructive_overrides.iter().cloned().collect();
        let global_only_set = region.global_only.iter().cloned().collect();
        let known_verbs = taxonomy
            .leaf
            .known_verbs
            .iter()
            .map(|v| v.to_lowercase())
            .collect();

        let secret_response_by_op = safety
            .secret_response_fields
            .iter()
            .map(|s| (s.op.clone(), s.fields.clone()))
            .collect();

        let reveal_response_by_op = safety
            .reveal_response_fields
            .iter()
            .map(|s| (s.op.clone(), s.fields.clone()))
            .collect();

        let pagination_override_by_op = pagination
            .overrides
            .iter()
            .map(|o| (o.op.clone(), o.clone()))
            .collect();

        let mut comma_join_by_op: HashMap<String, BTreeSet<String>> = HashMap::new();
        for cj in &pagination.comma_join {
            comma_join_by_op
                .entry(cj.op.clone())
                .or_default()
                .insert(cj.param.clone());
        }

        let mut bulk_by_op: HashMap<String, Vec<BulkTriggerEntry>> = HashMap::new();
        for b in &safety.bulk_triggers {
            bulk_by_op
                .entry(b.op.clone())
                .or_default()
                .push(BulkTriggerEntry {
                    op: b.op.clone(),
                    field: b.field.clone(),
                    location: b.location.clone(),
                    value: b.value.clone(),
                });
        }

        let alias_by_op = taxonomy
            .id_alias
            .iter()
            .map(|a| {
                (
                    format!("{}.{}", a.namespace, a.operation_id),
                    a.alias.clone(),
                )
            })
            .collect();

        let mut constraints_by_op: HashMap<String, Vec<ConstraintEntry>> = HashMap::new();
        for c in &constraints.constraint {
            constraints_by_op
                .entry(c.op.clone())
                .or_default()
                .push(c.clone());
        }

        let async_job_by_op = async_jobs
            .job
            .iter()
            .map(|j| (j.op.clone(), j.clone()))
            .collect();

        Ok(Tables {
            taxonomy,
            safety,
            region,
            pagination,
            constraints,
            async_jobs,
            send_set,
            destructive_set,
            global_only_set,
            known_verbs,
            secret_response_by_op,
            reveal_response_by_op,
            pagination_override_by_op,
            comma_join_by_op,
            bulk_by_op,
            alias_by_op,
            constraints_by_op,
            async_job_by_op,
        })
    }
}
