//! Build — merge the raw parsed specs with the curated tables into the final
//! `Vec<OperationIr>` + the `body_schema_id → normalized schema` map.
//!
//! All curated lookups key on the canonical op key `namespace.OperationId`. Every
//! curated entry that targets a field/param (bulk triggers, comma-join) is verified
//! against the actual op and a miss is a hard error, so the tables can't silently rot.

use crate::schema;
use crate::specs::{SpecFile, Stats};
use crate::tables::{ConstraintEntry, Tables};
use anyhow::{Result, bail};
use sendgrid_core::ir::{
    AsyncJob, BulkLocation, BulkTrigger, Constraint, Location, OperationIr, Pagination,
    PaginationKind, ParamDefault, ParamIr, SideEffect,
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

/// The `#/components/schemas/X` component name a raw response node points at (when
/// it is a direct `$ref`), used to dedup response schemas into the shared schema map
/// (a request + response that reference the same component collapse to one entry).
fn response_ref_name(node: &Value) -> Option<String> {
    node.get("$ref")
        .and_then(Value::as_str)
        .filter(|r| r.contains("/components/schemas/"))
        .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
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

/// Follow a `$ref` chain (cycle-guarded) to the concrete node it points at.
fn deref_node<'a>(root: &'a Value, node: &'a Value, seen: &mut Vec<String>) -> Option<&'a Value> {
    if let Some(r) = node.get("$ref").and_then(Value::as_str) {
        if seen.iter().any(|s| s == r) {
            return None; // cycle — give up
        }
        seen.push(r.to_string());
        let target = r.strip_prefix('#').and_then(|p| root.pointer(p))?;
        return deref_node(root, target, seen);
    }
    Some(node)
}

/// Collect the TOP-LEVEL properties of a (possibly `$ref`/`allOf`) object schema,
/// shallowly — enough to detect the result-array key and verify async uri fields.
/// No deep resolution, no build-stats pollution.
fn response_top_props(root: &Value, node: &Value) -> BTreeMap<String, Value> {
    fn walk(root: &Value, node: &Value, out: &mut BTreeMap<String, Value>, seen: &mut Vec<String>) {
        let Some(resolved) = deref_node(root, node, seen) else {
            return;
        };
        if let Some(props) = resolved.get("properties").and_then(Value::as_object) {
            for (k, v) in props {
                out.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        if let Some(all_of) = resolved.get("allOf").and_then(Value::as_array) {
            for sub in all_of {
                walk(root, sub, out, seen);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, node, &mut out, &mut Vec::new());
    out
}

/// True when a (possibly `$ref`-ed) property schema's type is `array`.
fn prop_is_array(root: &Value, prop: &Value) -> bool {
    let resolved = deref_node(root, prop, &mut Vec::new()).unwrap_or(prop);
    match resolved.get("type") {
        Some(Value::String(s)) => s == "array",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("array")),
        _ => false,
    }
}

/// Derive `pagination.data_key`: the SINGLE top-level array property of the 2xx
/// response schema. `None` when there is no response schema, zero arrays (root-array
/// envelopes handled by the runtime fallback), or more than one (ambiguous → leave
/// to a curated override).
fn derive_data_key(root: &Value, response_2xx: Option<&Value>) -> Option<String> {
    let node = response_2xx?;
    let props = response_top_props(root, node);
    let arrays: Vec<&String> = props
        .iter()
        .filter(|(_, v)| prop_is_array(root, v))
        .map(|(k, _)| k)
        .collect();
    match arrays.as_slice() {
        [only] => Some((*only).clone()),
        _ => None,
    }
}

/// Convert a curated [`ConstraintEntry`] into the IR [`Constraint`], verifying every
/// referenced field is a real top-level body property (table-rot guard).
fn build_constraint(
    e: &ConstraintEntry,
    op_key: &str,
    body_props: &[String],
) -> Result<Constraint> {
    let check = |f: &str| -> Result<()> {
        if !body_props.iter().any(|p| p == f) {
            bail!(
                "constraint {op_key} ({}): body field {f:?} is not a top-level property of the op",
                e.rule
            );
        }
        Ok(())
    };
    Ok(match e.rule.as_str() {
        "requires_one_of" => {
            if e.fields.len() < 2 {
                bail!("constraint {op_key} requires_one_of needs >=2 fields");
            }
            for f in &e.fields {
                check(f)?;
            }
            Constraint::RequiresOneOf {
                fields: e.fields.clone(),
                message: e.message.clone(),
            }
        }
        "mutually_exclusive" => {
            if e.fields.len() < 2 {
                bail!("constraint {op_key} mutually_exclusive needs >=2 fields");
            }
            for f in &e.fields {
                check(f)?;
            }
            Constraint::MutuallyExclusive {
                fields: e.fields.clone(),
                message: e.message.clone(),
            }
        }
        "required_unless_present" => {
            let Some(field) = e.field.clone() else {
                bail!("constraint {op_key} required_unless_present needs `field`");
            };
            let Some(unless_present) = e.unless_present.clone() else {
                bail!("constraint {op_key} required_unless_present needs `unless_present`");
            };
            check(&field)?;
            check(&unless_present)?;
            if let Some(arr) = &e.or_each_in {
                check(arr)?;
            }
            Constraint::RequiredUnlessPresent {
                field,
                unless_present,
                or_each_in: e.or_each_in.clone(),
                message: e.message.clone(),
            }
        }
        other => bail!("constraint {op_key}: unknown rule {other:?}"),
    })
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
    let mut applied_constraints: BTreeSet<String> = BTreeSet::new();
    let mut applied_async: BTreeSet<String> = BTreeSet::new();
    let mut applied_defaults: BTreeSet<String> = BTreeSet::new();
    // (op_key, computed companion status-op id) — verified to exist post-loop.
    let mut async_status_targets: Vec<(String, String)> = Vec::new();

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

            // --- primary success-response schema: resolve + normalize + embed (P6
            //     item 7). Reuses the request-style resolver/normalizer and the same
            //     schema map, so a request + response sharing a component collapse to
            //     one entry. A degenerate result (204, or a cycle/unresolved that
            //     collapsed to `{}`) is not embedded. ---
            let response_schema_id = match &raw.response_2xx {
                None => None,
                Some(node) => {
                    let normalized = schema::resolve_and_normalize(&spec.root, node, &mut stats);
                    if schema::is_empty_schema(&normalized) {
                        None
                    } else {
                        let key = match response_ref_name(node) {
                            Some(name) => format!("{ns}.{name}"),
                            None => format!("{ns}.{}.response", raw.operation_id),
                        };
                        match schemas.get(&key) {
                            Some(existing) if existing != &normalized => {
                                bail!("response schema {key:?} maps to two different schemas");
                            }
                            Some(_) => {}
                            None => {
                                schemas.insert(key.clone(), normalized);
                            }
                        }
                        Some(key)
                    }
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
            let reveal_response_fields = tables
                .reveal_response_by_op
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
            let mut pagination = if let Some(ov) = tables.pagination_override_by_op.get(&op_key) {
                Pagination {
                    kind: parse_kind(&ov.kind)?,
                    cursor_path: ov.cursor_path.clone(),
                    inject_param: ov.inject_param.clone(),
                    data_key: ov.data_key.clone(),
                }
            } else {
                derive_pagination(&query_names)
            };
            // data_key: the single top-level array of the 2xx response, derived
            // offline (M5). A curated override (if any) wins; otherwise fill it for
            // every paginating op so `--all` unwraps real records (not envelopes).
            if pagination.kind != PaginationKind::None && pagination.data_key.is_none() {
                pagination.data_key = derive_data_key(&spec.root, raw.response_2xx.as_ref());
            }

            // --- cross-field constraints (M1, data/constraints.toml) ---
            let mut constraints: Vec<Constraint> = Vec::new();
            if let Some(entries) = tables.constraints_by_op.get(&op_key) {
                for e in entries {
                    constraints.push(build_constraint(e, &op_key, &body_props)?);
                }
                applied_constraints.insert(op_key.clone());
            }

            // --- curated client-side param defaults (data/defaults.toml) ---
            let mut param_defaults: Vec<ParamDefault> = Vec::new();
            if let Some(entries) = tables.defaults_by_op.get(&op_key) {
                for e in entries {
                    let location = match e.location.as_str() {
                        "query" => Location::Query,
                        "header" => Location::Header,
                        other => bail!(
                            "defaults {op_key}: unsupported location {other:?} (only query/header)"
                        ),
                    };
                    // Rot-guard: the op must actually declare this param.
                    if !params
                        .iter()
                        .any(|p| p.location == location && p.name == e.name)
                    {
                        bail!(
                            "defaults {op_key}: op has no {} param {:?}",
                            e.location,
                            e.name
                        );
                    }
                    param_defaults.push(ParamDefault {
                        location,
                        name: e.name.clone(),
                        value: e.value.clone(),
                    });
                    applied_defaults.insert(format!("{op_key}.{}.{}", e.location, e.name));
                }
            }

            // --- async job classification (data/async_jobs.toml) ---
            let response_props = raw
                .response_2xx
                .as_ref()
                .map(|n| response_top_props(&spec.root, n))
                .unwrap_or_default();
            let (async_job, async_status_op, async_uri_field) = if let Some(j) =
                tables.async_job_by_op.get(&op_key)
            {
                applied_async.insert(op_key.clone());
                let kind = match j.kind.as_str() {
                    "poll" => AsyncJob::Poll,
                    "fire_and_forget" => AsyncJob::FireAndForget,
                    "external_upload" => AsyncJob::ExternalUpload,
                    "external_download" => AsyncJob::ExternalDownload,
                    other => bail!("async_jobs {op_key}: unknown kind {other:?}"),
                };
                // Companion status op is same-namespace → reuse this op's slug rules.
                let status_op = j.status_operation_id.as_ref().map(|sid| {
                    let id = build_id(&domain_slug, &subgroup_slug, collapse, sid);
                    async_status_targets.push((op_key.clone(), id.clone()));
                    id
                });
                // uri_field must be a real 2xx response property (rot-guard).
                if let Some(uf) = &j.uri_field
                    && !response_props.contains_key(uf)
                {
                    bail!("async_jobs {op_key}: uri_field {uf:?} is not a 2xx response property");
                }
                (kind, status_op, j.uri_field.clone())
            } else {
                (AsyncJob::default(), None, None)
            };

            // --- search keywords (taxonomy.toml, by namespace) ---
            let search_keywords = tables
                .taxonomy
                .search_keywords
                .get(ns)
                .cloned()
                .unwrap_or_default();

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
                response_schema_id,
                side_effect,
                secret_response_fields,
                reveal_response_fields,
                secret_request_fields,
                bulk_triggers,
                pagination,
                async_job,
                async_status_op,
                async_uri_field,
                search_keywords,
                constraints,
                param_defaults,
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
    let want_constraints: BTreeSet<String> = tables.constraints_by_op.keys().cloned().collect();
    let missing_constraints: Vec<_> = want_constraints
        .difference(&applied_constraints)
        .cloned()
        .collect();
    if !missing_constraints.is_empty() {
        bail!("constraints never applied (op not found?): {missing_constraints:?}");
    }
    let want_async: BTreeSet<String> = tables.async_job_by_op.keys().cloned().collect();
    let missing_async: Vec<_> = want_async.difference(&applied_async).cloned().collect();
    if !missing_async.is_empty() {
        bail!("async_jobs entries never applied (op not found?): {missing_async:?}");
    }
    let want_defaults: BTreeSet<String> = tables
        .defaults
        .default
        .iter()
        .map(|d| format!("{}.{}.{}", d.op, d.location, d.name))
        .collect();
    let missing_defaults: Vec<_> = want_defaults.difference(&applied_defaults).cloned().collect();
    if !missing_defaults.is_empty() {
        bail!("defaults never applied (op/param not found?): {missing_defaults:?}");
    }
    // Every curated search_keywords namespace must be a real namespace.
    for ns in tables.taxonomy.search_keywords.keys() {
        if !tables.taxonomy.namespaces.contains_key(ns) {
            bail!("search_keywords targets unknown namespace {ns:?}");
        }
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
    for s in &tables.safety.reveal_response_fields {
        if !op_keys.contains(&s.op) {
            bail!("reveal_response_fields targets unknown op {:?}", s.op);
        }
    }
    for k in tables.destructive_set.iter().chain(tables.send_set.iter()) {
        if !op_keys.contains(k) {
            bail!("safety override targets unknown op {k:?}");
        }
    }
    // Every computed async companion status-op id must resolve to a real op.
    let all_ids: BTreeSet<&str> = ops.iter().map(|o| o.id.as_str()).collect();
    for (op_key, status_id) in &async_status_targets {
        if !all_ids.contains(status_id.as_str()) {
            bail!(
                "async_jobs {op_key}: companion status op id {status_id:?} does not exist \
                 (bad status_operation_id?)"
            );
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
