//! Build — merge the raw parsed specs with the curated tables into the final
//! `Vec<OperationIr>` + the `body_schema_id → normalized schema` map.
//!
//! All curated lookups key on the canonical op key `namespace.OperationId`. Every
//! curated entry that targets a field/param (bulk triggers, comma-join) is verified
//! against the actual op and a miss is a hard error, so the tables can't silently rot.

use crate::schema;
use crate::specs::{SpecFile, Stats};
use crate::tables::Tables;
use anyhow::{Result, bail};
use sendgrid_core::ir::{
    BulkLocation, BulkTrigger, Location, OperationIr, Pagination, PaginationKind, ParamIr,
    SideEffect,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub struct BuildOutput {
    pub ops: Vec<OperationIr>,
    pub schemas: BTreeMap<String, Value>,
    pub stats: Stats,
    pub warnings: Vec<String>,
}

/// Lowercase + map every non-`[a-z0-9]` char to `_` (the MCP-id slug rule).
fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() { c } else { '_' }
        })
        .collect()
}

/// Split a PascalCase operationId into tokens, emulating the r2 regex
/// `[A-Z]+(?=[A-Z][a-z])|[A-Z][a-z0-9]*|[0-9]+` (preserves acronym/digit runs).
fn split_op_id(op_id: &str) -> Vec<String> {
    let chars: Vec<char> = op_id.chars().collect();
    let n = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c.is_ascii_uppercase() {
            // Length of the uppercase run starting at i.
            let mut j = i;
            while j < n && chars[j].is_ascii_uppercase() {
                j += 1;
            }
            if (j - i) >= 2 && j < n && chars[j].is_ascii_lowercase() {
                // alt1: `[A-Z]+(?=[A-Z][a-z])` — caps run minus the last cap (it
                // begins the next word, e.g. "APIKeys" -> "API","Keys").
                tokens.push(chars[i..j - 1].iter().collect());
                i = j - 1;
            } else {
                // alt2: `[A-Z][a-z0-9]*`.
                let mut k = i + 1;
                while k < n && (chars[k].is_ascii_lowercase() || chars[k].is_ascii_digit()) {
                    k += 1;
                }
                tokens.push(chars[i..k].iter().collect());
                i = k;
            }
        } else if c.is_ascii_digit() {
            let mut k = i;
            while k < n && chars[k].is_ascii_digit() {
                k += 1;
            }
            tokens.push(chars[i..k].iter().collect());
            i = k;
        } else {
            // Not expected in PascalCase ids; consume a lower/digit run defensively.
            let mut k = i;
            while k < n && (chars[k].is_ascii_lowercase() || chars[k].is_ascii_digit()) {
                k += 1;
            }
            tokens.push(chars[i..k.max(i + 1)].iter().collect());
            i = k.max(i + 1);
        }
    }
    tokens
}

/// `sg_<domain>[_<subgroup>]_<operationId>` with the collapse rule.
fn build_id(domain_slug: &str, subgroup_slug: &str, collapse: bool, op_id: &str) -> String {
    if collapse {
        format!("sg_{domain_slug}_{op_id}")
    } else {
        format!("sg_{domain_slug}_{subgroup_slug}_{op_id}")
    }
}

fn parse_kind(s: &str) -> Result<PaginationKind> {
    Ok(match s {
        "none" => PaginationKind::None,
        "offset" => PaginationKind::Offset,
        "page_number" => PaginationKind::PageNumber,
        "page_token" => PaginationKind::PageToken,
        "cursor_key" => PaginationKind::CursorKey,
        "cursor_record" => PaginationKind::CursorRecord,
        "capped_single" => PaginationKind::CappedSingle,
        other => bail!("unknown pagination kind in override: {other:?}"),
    })
}

/// Derive pagination from query param names (r5 §1.2 ordered precedence).
fn derive_pagination(query_names: &[String]) -> Pagination {
    let has = |name: &str| query_names.iter().any(|q| q == name);
    let mut p = Pagination::default();
    if has("page_token") {
        p.kind = PaginationKind::PageToken;
        p.inject_param = Some("page_token".into());
    } else if has("after_key") {
        p.kind = PaginationKind::CursorKey;
        p.cursor_path = Some("_metadata.after_key".into());
        p.inject_param = Some("after_key".into());
    } else if let Some(rec) = query_names
        .iter()
        .find(|q| q.starts_with("after_") && q.ends_with("_id"))
    {
        p.kind = PaginationKind::CursorRecord;
        p.inject_param = Some(rec.clone());
    } else if has("page") && has("page_size") {
        p.kind = PaginationKind::PageNumber;
        p.inject_param = Some("page".into());
    } else if has("offset") {
        p.kind = PaginationKind::Offset;
        p.inject_param = Some("offset".into());
    } else if has("limit") {
        p.kind = PaginationKind::CappedSingle;
    }
    p
}

pub fn build(specs: &[SpecFile], tables: &Tables) -> Result<BuildOutput> {
    let mut ops: Vec<OperationIr> = Vec::new();
    let mut schemas: BTreeMap<String, Value> = BTreeMap::new();
    let mut stats = Stats::default();
    let mut warnings: Vec<String> = Vec::new();

    // Track which curated field-targeting entries were actually applied, so an
    // entry that targets a non-existent op/field is caught.
    let mut applied_bulk: BTreeSet<String> = BTreeSet::new();
    let mut applied_comma_join: BTreeSet<String> = BTreeSet::new();
    let mut applied_alias: BTreeSet<String> = BTreeSet::new();

    let sorted_global_secrets: Vec<String> = {
        let mut v = tables.safety.secret_request_fields_global.clone();
        v.sort();
        v.dedup();
        v
    };

    for spec in specs {
        let ns = &spec.namespace;
        let nsentry = tables.taxonomy.namespaces.get(ns).ok_or_else(|| {
            anyhow::anyhow!(
                "taxonomy.toml has no entry for namespace {ns:?} (spec {})",
                spec.stem
            )
        })?;
        let domain = &nsentry.domain;
        let subgroup = &nsentry.subgroup;
        let hidden = nsentry.hidden;
        let collapse = subgroup == domain;
        let domain_slug = slugify(domain);
        let subgroup_slug = slugify(subgroup);
        let region_global_only = tables.global_only_set.contains(ns);

        for raw in &spec.ops {
            let op_key = format!("{ns}.{}", raw.operation_id);

            // --- HTTP shape + comma-join param override ---
            let mut params: Vec<ParamIr> = raw.params.clone();
            if let Some(cj_params) = tables.comma_join_by_op.get(&op_key) {
                for target in cj_params {
                    let mut found = false;
                    for p in params.iter_mut() {
                        if p.location == Location::Query && &p.name == target {
                            p.explode = Some(false);
                            found = true;
                        }
                    }
                    if !found {
                        bail!(
                            "comma_join override targets missing query param {target:?} on {op_key}"
                        );
                    }
                    applied_comma_join.insert(format!("{op_key}.{target}"));
                }
            }
            let query_names: Vec<String> = params
                .iter()
                .filter(|p| p.location == Location::Query)
                .map(|p| p.name.clone())
                .collect();

            // --- request body: resolve + normalize, index into schema map ---
            let mut body_props: Vec<String> = Vec::new();
            let (has_body, body_schema_id, body_is_array) = match &raw.request_body {
                None => (false, None, false),
                Some(rb) => {
                    if rb.content_type != "application/json" {
                        warnings.push(format!(
                            "{op_key}: non-JSON request body content-type {:?}",
                            rb.content_type
                        ));
                    }
                    let normalized =
                        schema::resolve_and_normalize(&spec.root, &rb.schema_node, &mut stats);
                    let is_array = schema::is_array_schema(&normalized);
                    body_props = schema::top_level_property_names(&normalized);
                    let key = match &rb.schema_ref_name {
                        Some(name) => format!("{ns}.{name}"),
                        None => format!("{ns}.{}", raw.operation_id),
                    };
                    match schemas.get(&key) {
                        Some(existing) if existing != &normalized => {
                            bail!("body_schema_id {key:?} maps to two different schemas");
                        }
                        Some(_) => {}
                        None => {
                            schemas.insert(key.clone(), normalized);
                        }
                    }
                    (true, Some(key), is_array)
                }
            };

            // --- taxonomy: id, id_alias, cli_path ---
            let id = build_id(&domain_slug, &subgroup_slug, collapse, &raw.operation_id);
            let id_alias = tables.alias_by_op.get(&op_key).map(|alias_op| {
                applied_alias.insert(op_key.clone());
                build_id(&domain_slug, &subgroup_slug, collapse, alias_op)
            });

            let leaf = split_op_id(&raw.operation_id);
            if leaf.is_empty() {
                bail!("{op_key}: operationId split to zero tokens");
            }
            let verb = leaf[0].to_lowercase();
            if !tables.known_verbs.contains(&verb) {
                warnings.push(format!(
                    "{op_key}: leading verb {verb:?} not in known_verbs lexicon"
                ));
            }
            let mut cli_path = vec![domain.clone()];
            if !collapse {
                cli_path.push(subgroup.clone());
            }
            cli_path.push(verb);
            cli_path.extend(leaf[1..].iter().map(|t| t.to_lowercase()));

            // --- safety: side_effect ---
            let mut side_effect = SideEffect::from_method(&raw.method);
            let destructive_override = tables.destructive_set.contains(&op_key)
                || tables
                    .safety
                    .destructive_path_substrings
                    .iter()
                    .any(|sub| raw.path.contains(sub));
            if destructive_override {
                side_effect = SideEffect::Destructive;
            }
            if tables.send_set.contains(&op_key) {
                side_effect = SideEffect::Send;
            }

            // --- safety: secret fields ---
            let secret_response_fields = tables
                .secret_response_by_op
                .get(&op_key)
                .cloned()
                .unwrap_or_default();
            let secret_request_fields = if has_body {
                sorted_global_secrets.clone()
            } else {
                Vec::new()
            };

            // --- safety: bulk triggers (verify the field exists) ---
            let mut bulk_triggers: Vec<BulkTrigger> = Vec::new();
            if let Some(entries) = tables.bulk_by_op.get(&op_key) {
                for e in entries {
                    let location = match e.location.as_str() {
                        "query" => BulkLocation::Query,
                        "body" => BulkLocation::Body,
                        other => bail!("bulk trigger {op_key}: bad location {other:?}"),
                    };
                    let exists = match location {
                        BulkLocation::Query => query_names.iter().any(|q| q == &e.field),
                        BulkLocation::Body => body_props.iter().any(|p| p == &e.field),
                    };
                    if !exists {
                        bail!(
                            "bulk trigger {op_key}: field {:?} ({}) not present on op",
                            e.field,
                            e.location
                        );
                    }
                    bulk_triggers.push(BulkTrigger {
                        field: e.field.clone(),
                        location,
                        value: e.value.clone(),
                    });
                    applied_bulk.insert(format!("{op_key}.{}.{}", e.location, e.field));
                }
            }

            // --- pagination ---
            let pagination = if let Some(ov) = tables.pagination_override_by_op.get(&op_key) {
                Pagination {
                    kind: parse_kind(&ov.kind)?,
                    cursor_path: ov.cursor_path.clone(),
                    inject_param: ov.inject_param.clone(),
                    data_key: ov.data_key.clone(),
                }
            } else {
                derive_pagination(&query_names)
            };

            // --- retry policy (r5 §4.2) ---
            let method_idempotent = matches!(raw.method.as_str(), "GET" | "PUT" | "DELETE");
            let post_read = raw.method == "POST"
                && (raw.path.ends_with("/search") || raw.path.ends_with("/count"));
            let retry_safe_5xx = method_idempotent || post_read;

            ops.push(OperationIr {
                id,
                id_alias,
                operation_id: raw.operation_id.clone(),
                namespace: ns.clone(),
                domain: domain.clone(),
                subgroup: subgroup.clone(),
                cli_path,
                hidden,
                method: raw.method.clone(),
                path: raw.path.clone(),
                tags: raw.tags.clone(),
                summary: raw.summary.clone(),
                params,
                body_schema_id,
                has_body,
                body_is_array,
                side_effect,
                secret_response_fields,
                secret_request_fields,
                bulk_triggers,
                pagination,
                async_job: Default::default(),
                region_global_only,
                retry_safe_5xx,
                retry_safe_429: true,
            });
        }
    }

    // Every curated targeted entry must have been applied (table-rot guard).
    let want_bulk: BTreeSet<String> = tables
        .safety
        .bulk_triggers
        .iter()
        .map(|b| format!("{}.{}.{}", b.op, b.location, b.field))
        .collect();
    let missing_bulk: Vec<_> = want_bulk.difference(&applied_bulk).cloned().collect();
    if !missing_bulk.is_empty() {
        bail!("bulk triggers never applied (op not found?): {missing_bulk:?}");
    }
    let want_cj: BTreeSet<String> = tables
        .pagination
        .comma_join
        .iter()
        .map(|c| format!("{}.{}", c.op, c.param))
        .collect();
    let missing_cj: Vec<_> = want_cj.difference(&applied_comma_join).cloned().collect();
    if !missing_cj.is_empty() {
        bail!("comma_join overrides never applied: {missing_cj:?}");
    }
    let want_alias: BTreeSet<String> = tables.alias_by_op.keys().cloned().collect();
    let missing_alias: Vec<_> = want_alias.difference(&applied_alias).cloned().collect();
    if !missing_alias.is_empty() {
        bail!("id_alias entries never applied (op not found?): {missing_alias:?}");
    }
    // Pagination overrides + secret-response + destructive-override + send-list
    // entries must all reference real ops too.
    let op_keys: BTreeSet<String> = ops
        .iter()
        .map(|o| format!("{}.{}", o.namespace, o.operation_id))
        .collect();
    for ov in &tables.pagination.overrides {
        if !op_keys.contains(&ov.op) {
            bail!("pagination override targets unknown op {:?}", ov.op);
        }
    }
    for s in &tables.safety.secret_response_fields {
        if !op_keys.contains(&s.op) {
            bail!("secret_response_fields targets unknown op {:?}", s.op);
        }
    }
    for k in tables.destructive_set.iter().chain(tables.send_set.iter()) {
        if !op_keys.contains(k) {
            bail!("safety override targets unknown op {k:?}");
        }
    }

    // Deterministic op order: by globally-unique id.
    ops.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(BuildOutput {
        ops,
        schemas,
        stats,
        warnings,
    })
}
