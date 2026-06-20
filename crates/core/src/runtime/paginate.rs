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
    let mut page = q().and_then(|m| read_u64(m, "page")).unwrap_or(1);
    // Cursor token may be pre-seeded (resume from a continuation hint).
    let mut cursor: Option<String> =
        inject.and_then(|p| q().and_then(|m| m.get(p)).map(value_to_string));

    let mut items: Vec<Value> = Vec::new();
    let mut pages = 0usize;

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

        let page_items = extract_items(&resp.body, op.pagination.data_key.as_deref());
        let page_len = page_items.len();
        items.extend(page_items);
        pages += 1;

        // Caps: stop with a continuation hint.
        if items.len() >= max_items || pages >= max_pages {
            truncate(&mut items, max_items);
            let next = next_hint(kind, inject, offset, limit, page, &resp.body, cursor_path);
            return PaginateOutcome::Collected {
                items,
                last_status,
                next,
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
                cursor.is_none()
            }
        };
        if done {
            return PaginateOutcome::Collected {
                items,
                last_status,
                next: None,
            };
        }
    }
}

/// Extract the result array from a page body: the op's `data_key` if known, else
/// a small set of common envelope keys, else the body itself if it is an array.
fn extract_items(body: &Value, data_key: Option<&str>) -> Vec<Value> {
    if let Some(key) = data_key
        && let Some(arr) = body.get(key).and_then(Value::as_array)
    {
        return arr.clone();
    }
    for key in ["result", "results", "contacts", "data"] {
        if let Some(arr) = body.get(key).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    if let Some(arr) = body.as_array() {
        return arr.clone();
    }
    // Single-object page: treat as one item so nothing is silently dropped.
    vec![body.clone()]
}

fn last_record_id(body: &Value, data_key: Option<&str>) -> Option<String> {
    let items = extract_items(body, data_key);
    items.last().and_then(|v| v.get("id")).map(value_to_string)
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

fn next_hint(
    kind: PaginationKind,
    inject: Option<&str>,
    offset: u64,
    limit: Option<u64>,
    page: u64,
    body: &Value,
    cursor_path: Option<&str>,
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
            let tok = last_record_id(body, None)?;
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
        assert_eq!(extract_items(&json!({"result":[1,2]}), None).len(), 2);
        assert_eq!(extract_items(&json!({"contacts":[1]}), None).len(), 1);
        assert_eq!(
            extract_items(&json!({"things":[1,2,3]}), Some("things")).len(),
            3
        );
        assert_eq!(extract_items(&json!([9, 8]), None).len(), 2);
    }
}
