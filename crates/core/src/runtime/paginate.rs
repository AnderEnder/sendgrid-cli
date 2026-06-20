//! The generic `--all` pagination engine (r5 §1, brief item 10). One loop drives
//! every pattern via the op's `pagination` IR (`kind` / `cursor_path` /
//! `inject_param`). Extract → inject → terminate per r5 §1.3, with safety caps
//! (`max_items` 1000, `max_pages` 50) and a continuation hint on cap-stop.
//!
//! The engine is backend-blind: it rebuilds each page's request and sends it
//! through [`send_with_retry`], reading the cursor from the dotted `cursor_path`
//! (e.g. `_metadata.next_params.after_key`). It re-issues against the configured
//! base URL (never the response `next` host), preserving auth + region residency.

use super::auth::ApiKey;
use super::build::build_request;
use super::dispatch::{DispatchError, OperationDispatcher};
use super::retry::{RetryConfig, send_with_retry};
use crate::ir::{OperationIr, PaginationKind};
use serde_json::{Map, Value};

/// Outcome of an auto-paginate run.
pub(crate) enum PaginateOutcome {
    /// Accumulated items across pages.
    Collected {
        items: Vec<Value>,
        last_status: u16,
        /// Continuation hint when stopped at a cap (else `None`).
        next: Option<Value>,
        /// Non-fatal warnings accumulated across pages (e.g. a page's result array
        /// could not be located, or a cursor op found no continuation cursor). Each
        /// distinct warning is emitted at most once.
        warnings: Vec<String>,
    },
    /// A page returned a non-2xx status — stop and surface it verbatim.
    HttpError { status: u16, body: Value },
    /// A transport failure.
    Network(DispatchError),
    /// A request could not be built (should not happen post-validation).
    Build(String),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn paginate_all<D: OperationDispatcher>(
    dispatcher: &D,
    client: &reqwest::Client,
    op: &OperationIr,
    mut args: Value,
    api_key: &ApiKey,
    base_url: &str,
    governed_obo: Option<&str>,
    retry: &RetryConfig,
    max_items: usize,
    max_pages: usize,
) -> PaginateOutcome {
    let kind = op.pagination.kind;
    let inject = op.pagination.inject_param.as_deref();
    let cursor_path = op.pagination.cursor_path.as_deref();

    // Read starting offset/page/limit from the caller's query (already coerced).
    let q = || args.get("query").and_then(Value::as_object);
    let limit = q()
        .and_then(|m| read_u64(m, "limit"))
        .or_else(|| q().and_then(|m| read_u64(m, "page_size")));
    let mut offset = q().and_then(|m| read_u64(m, "offset")).unwrap_or(0);
    // The caller's STARTING offset — fixed for the run. The next-page hint is computed
    // relative to it (`start_offset + items_collected`), not the mutating `offset`.
    let start_offset = offset;
    let mut page = q().and_then(|m| read_u64(m, "page")).unwrap_or(1);
    // Cursor token may be pre-seeded (resume from a continuation hint).
    let mut cursor: Option<String> =
        inject.and_then(|p| q().and_then(|m| m.get(p)).map(value_to_string));

    let mut items: Vec<Value> = Vec::new();
    let mut pages = 0usize;
    // Non-fatal warnings accumulated across pages (each emitted at most once).
    let mut warnings: Vec<String> = Vec::new();
    let mut warned_extract = false;

    loop {
        // Inject the current cursor/offset/page into the query bucket.
        match kind {
            PaginationKind::Offset => {
                set_query(&mut args, inject.unwrap_or("offset"), Value::from(offset))
            }
            PaginationKind::PageNumber => {
                set_query(&mut args, inject.unwrap_or("page"), Value::from(page))
            }
            PaginationKind::PageToken
            | PaginationKind::CursorKey
            | PaginationKind::CursorRecord => {
                if let (Some(param), Some(tok)) = (inject, cursor.as_ref()) {
                    set_query(&mut args, param, Value::from(tok.clone()));
                }
            }
            PaginationKind::CappedSingle | PaginationKind::None => {}
        }

        // Build + send this page.
        let built = match build_request(client, op, &args, api_key, base_url, governed_obo) {
            Ok(b) => b,
            Err(e) => return PaginateOutcome::Build(e.to_string()),
        };
        let resp = match send_with_retry(dispatcher, op, retry, built.request).await {
            Ok(r) => r,
            Err(e) => return PaginateOutcome::Network(e),
        };
        let last_status = resp.status.as_u16();
        if !resp.status.is_success() {
            return PaginateOutcome::HttpError {
                status: last_status,
                body: resp.body,
            };
        }

        // Locate the page's result array. `None` = no array key matched at all →
        // collect nothing for this page and emit a single visible warning (never
        // silently wrap the whole envelope as one item). An empty array is `Some`.
        let data_key = op.pagination.data_key.as_deref();
        let page_items = match extract_items(&resp.body, data_key) {
            Some(arr) => arr,
            None => {
                if !warned_extract {
                    warned_extract = true;
                    warnings.push(format!(
                        "--all: could not locate a result array (data_key={:?}) in a page of \
                         `{}`; collected 0 items for it — the response shape may have changed",
                        data_key, op.id
                    ));
                }
                Vec::new()
            }
        };
        let page_len = page_items.len();
        items.extend(page_items);
        pages += 1;

        // Caps: stop with a continuation hint.
        if items.len() >= max_items || pages >= max_pages {
            truncate(&mut items, max_items);
            // The hint must point at the NEXT uncovered page, not the one just
            // fetched (else a resume re-fetches/overlaps it). For offset, the next
            // uncovered offset is `start_offset + items_collected`: pages continue
            // only while full, so collected items map 1:1 onto covered offsets — and
            // this stays correct even when truncation drops the tail of the last page.
            // For page numbers it is `last_page + 1`.
            let next_offset = start_offset + items.len() as u64;
            let next_page = page + 1;
            let next = next_hint(
                kind,
                inject,
                next_offset,
                limit,
                next_page,
                &resp.body,
                cursor_path,
                data_key,
            );
            return PaginateOutcome::Collected {
                items,
                last_status,
                next,
                warnings,
            };
        }

        // Terminate / advance per kind.
        let done = match kind {
            PaginationKind::CappedSingle | PaginationKind::None => true,
            PaginationKind::Offset => {
                offset += limit.unwrap_or(page_len as u64).max(1);
                page_len == 0 || limit.is_some_and(|l| (page_len as u64) < l)
            }
            PaginationKind::PageNumber => {
                page += 1;
                page_len == 0
            }
            PaginationKind::CursorRecord => {
                cursor = last_record_id(&resp.body, op.pagination.data_key.as_deref());
                page_len == 0 || cursor.is_none() || limit.is_some_and(|l| (page_len as u64) < l)
            }
            PaginationKind::PageToken => {
                cursor = extract_page_token(&resp.body, inject.unwrap_or("page_token"));
                cursor.is_none()
            }
            PaginationKind::CursorKey => {
                cursor = cursor_path
                    .and_then(|p| dotted(&resp.body, p))
                    .map(|v| value_to_string(&v));
                // Visible warning (not silent under-fetch) when we stop because no
                // continuation cursor was found on a NON-EMPTY page AND the cursor
                // envelope is absent — i.e. the response doesn't paginate the way the
                // IR expects (the 2 `seq` engagement-quality ops have schema-less,
                // undocumented responses). A legitimate last page of a real cursor op
                // (the IP ops) carries the envelope (`_metadata`) with no `after_key`,
                // so it does NOT warn. Strictly additive: never changes `done`.
                if cursor.is_none()
                    && page_len > 0
                    && !cursor_envelope_present(&resp.body, cursor_path)
                {
                    warnings.push(format!(
                        "--all: `{}` is configured for cursor pagination but no continuation \
                         cursor was found (looked for `{}`); stopped after {} item(s) — results \
                         may be incomplete if the endpoint paginates differently than documented",
                        op.id,
                        cursor_path.unwrap_or("<unset>"),
                        items.len()
                    ));
                }
                cursor.is_none()
            }
        };
        if done {
            return PaginateOutcome::Collected {
                items,
                last_status,
                next: None,
                warnings,
            };
        }
    }
}

/// Extract the result array from a page body. When `data_key` is known it is
/// authoritative (no fallback); otherwise try a small set of common envelope keys,
/// then a bare top-level array. Returns `None` when NO array could be located — the
/// caller then collects nothing + warns, rather than wrapping the whole page
/// envelope as one bogus "item" (the old silent under-fetch). An empty result array
/// is `Some(vec![])` (a legitimate last page → clean termination).
fn extract_items(body: &Value, data_key: Option<&str>) -> Option<Vec<Value>> {
    if let Some(key) = data_key {
        return body.get(key).and_then(Value::as_array).cloned();
    }
    for key in ["result", "results", "contacts", "data"] {
        if let Some(arr) = body.get(key).and_then(Value::as_array) {
            return Some(arr.clone());
        }
    }
    body.as_array().cloned()
}

fn last_record_id(body: &Value, data_key: Option<&str>) -> Option<String> {
    extract_items(body, data_key)?
        .last()
        .and_then(|v| v.get("id"))
        .map(value_to_string)
}

/// Parse the `page_token` (or named param) out of the `_metadata.next` URL.
fn extract_page_token(body: &Value, param: &str) -> Option<String> {
    let next = body.pointer("/_metadata/next").and_then(Value::as_str)?;
    let (_, query) = next.split_once('?')?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == param
        {
            return Some(v.to_string());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn next_hint(
    kind: PaginationKind,
    inject: Option<&str>,
    offset: u64,
    limit: Option<u64>,
    page: u64,
    body: &Value,
    cursor_path: Option<&str>,
    data_key: Option<&str>,
) -> Option<Value> {
    let mut hint = Map::new();
    match kind {
        PaginationKind::Offset => {
            hint.insert(inject.unwrap_or("offset").to_string(), Value::from(offset));
            if let Some(l) = limit {
                hint.insert("limit".to_string(), Value::from(l));
            }
        }
        PaginationKind::PageNumber => {
            hint.insert(inject.unwrap_or("page").to_string(), Value::from(page));
        }
        PaginationKind::PageToken => {
            let tok = extract_page_token(body, inject.unwrap_or("page_token"))?;
            hint.insert(inject.unwrap_or("page_token").to_string(), Value::from(tok));
        }
        PaginationKind::CursorKey => {
            let tok = cursor_path.and_then(|p| dotted(body, p))?;
            hint.insert(inject.unwrap_or("after_key").to_string(), tok);
        }
        PaginationKind::CursorRecord => {
            let tok = last_record_id(body, data_key)?;
            hint.insert(
                inject.unwrap_or("after_subuser_id").to_string(),
                Value::from(tok),
            );
        }
        PaginationKind::CappedSingle | PaginationKind::None => return None,
    }
    Some(Value::Object(hint))
}

fn set_query(args: &mut Value, key: &str, value: Value) {
    let obj = args.as_object_mut().expect("args is an object envelope");
    let q = obj
        .entry("query")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(m) = q.as_object_mut() {
        m.insert(key.to_string(), value);
    }
}

fn read_u64(m: &Map<String, Value>, key: &str) -> Option<u64> {
    match m.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Whether the cursor envelope is present in a response body — i.e. the FIRST
/// segment of the dotted `cursor_path` (e.g. `_metadata`) exists. A real cursor op
/// emits this envelope on every page (the next-cursor is simply absent on the last);
/// an op that emits NO such container isn't paginating the documented way, so the
/// engine warns rather than under-fetch silently. `None` cursor_path ⇒ no envelope.
fn cursor_envelope_present(body: &Value, cursor_path: Option<&str>) -> bool {
    match cursor_path.and_then(|p| p.split('.').next()) {
        Some(seg) => body.get(seg).is_some(),
        None => false,
    }
}

/// Resolve a dotted path (`a.b.c`) into a body, returning the leaf value.
fn dotted(body: &Value, path: &str) -> Option<Value> {
    let mut cur = body;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    if cur.is_null() {
        None
    } else {
        Some(cur.clone())
    }
}

fn truncate(items: &mut Vec<Value>, max: usize) {
    if items.len() > max {
        items.truncate(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dotted_path_reads_cursor() {
        let body = json!({ "_metadata": { "next_params": { "after_key": "abc" } } });
        assert_eq!(
            dotted(&body, "_metadata.next_params.after_key"),
            Some(json!("abc"))
        );
        let nullbody = json!({ "_metadata": { "next_params": { "after_key": null } } });
        assert_eq!(dotted(&nullbody, "_metadata.next_params.after_key"), None);
    }

    #[test]
    fn page_token_extracted_from_next_url() {
        let body = json!({ "_metadata": { "next": "https://api.sendgrid.com/v3/x?page_size=10&page_token=TOK99" } });
        assert_eq!(
            extract_page_token(&body, "page_token"),
            Some("TOK99".into())
        );
    }

    #[test]
    fn extract_items_prefers_data_key_then_common() {
        assert_eq!(
            extract_items(&json!({"result":[1,2]}), None).unwrap().len(),
            2
        );
        assert_eq!(
            extract_items(&json!({"contacts":[1]}), None).unwrap().len(),
            1
        );
        assert_eq!(
            extract_items(&json!({"things":[1,2,3]}), Some("things"))
                .unwrap()
                .len(),
            3
        );
        assert_eq!(extract_items(&json!([9, 8]), None).unwrap().len(), 2);
    }

    #[test]
    fn extract_items_data_key_is_authoritative_and_empty_is_some() {
        // data_key set + present empty array → Some(vec![]) (clean termination).
        assert_eq!(
            extract_items(&json!({"stats":[]}), Some("stats")),
            Some(vec![])
        );
        // data_key set but the key is absent → None (no silent envelope-wrap).
        assert_eq!(extract_items(&json!({"date":"2026"}), Some("stats")), None);
        // No data_key and no known array key anywhere → None (warn, collect nothing).
        assert_eq!(extract_items(&json!({"foo":{"bar":1}}), None), None);
    }
}
