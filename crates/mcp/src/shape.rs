//! Output-shaping helpers for `invoke_operation` results: a jq-lite field
//! **projector** (`fields`) and an array **cap** (`max_items`), plus the small
//! dotted-path primitive both they and the async/export surfacing share.
//!
//! All three operate purely on JSON **structure** — there is no secret-field or
//! redaction logic here (that lives in `sendgrid_core::execute`, and the
//! `CreateApiKey` reveal must NOT be re-broken). Shaping is opt-in and applied only
//! to a real success `data` payload (see `invoke`), so a default call returns the
//! body verbatim.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Resolve a dotted path against `data`, descending **object keys only** (no array
/// indexing). Returns the value at the path, or `None` if a segment is missing or a
/// non-object is encountered mid-path. Used for async presigned-URL fields
/// (`upload_uri`, `presigned_url`, `urls`) and the await status-id lookup.
pub fn get_path<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = data;
    for seg in path.split('.').map(str::trim).filter(|s| !s.is_empty()) {
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
}

/// Collect URL string(s) at `field`, handling the non-uniform async response shape:
/// a STRING (`upload_uri`/`presigned_url`) OR an ARRAY of strings (`urls`). Mirrors
/// the CLI's `all_uris`.
pub fn collect_uris(data: &Value, field: &str) -> Vec<String> {
    match get_path(data, field) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// A prefix trie of path segments. A node flagged `leaf` terminates a requested
/// path: from there the whole subtree is kept.
#[derive(Default)]
struct Trie {
    children: BTreeMap<String, Trie>,
    leaf: bool,
}

impl Trie {
    fn insert(&mut self, segs: &[String]) {
        match segs.split_first() {
            None => self.leaf = true,
            Some((head, rest)) => self.children.entry(head.clone()).or_default().insert(rest),
        }
    }
}

/// Project `data` down to the union of `paths` (dotted, jq-lite). Objects are
/// pruned to the requested keys; an array along a path is projected **element-wise**
/// (so `result.id`, or the lenient `result[].id`, keeps `id` from every element,
/// preserving per-item pairing). A path terminating at a node keeps that whole
/// subtree. Paths that don't resolve simply contribute nothing. With no usable
/// paths, `data` is returned unchanged.
pub fn project(data: &Value, paths: &[String]) -> Value {
    let mut trie = Trie::default();
    for p in paths {
        let segs: Vec<String> = p
            .split('.')
            .map(|s| s.trim().trim_end_matches("[]").to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !segs.is_empty() {
            trie.insert(&segs);
        }
    }
    if trie.children.is_empty() {
        return data.clone();
    }
    prune(data, &trie)
}

fn prune(node: &Value, trie: &Trie) -> Value {
    // A terminating path (or a node with no further constraints) keeps the subtree.
    if trie.leaf || trie.children.is_empty() {
        return node.clone();
    }
    match node {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in &trie.children {
                if let Some(v) = map.get(key) {
                    out.insert(key.clone(), prune(v, child));
                }
            }
            Value::Object(out)
        }
        // Apply the same trie to each element: dotted segments after an array
        // descend into every item.
        Value::Array(items) => Value::Array(items.iter().map(|it| prune(it, trie)).collect()),
        // A scalar where the path expects to descend → the path doesn't resolve.
        _ => Value::Null,
    }
}

/// Describes a `max_items` truncation, surfaced as a note in the result envelope.
pub struct Truncation {
    /// Items kept after the cap.
    pub kept: usize,
    /// Items present before the cap.
    pub total: usize,
    /// The object key whose array was capped (`None` when `data` itself is the array).
    pub at: Option<String>,
}

impl Truncation {
    /// Render as a JSON note for the envelope's `truncated` field.
    pub fn to_note(&self) -> Value {
        let mut m = Map::new();
        m.insert("kept".into(), Value::from(self.kept));
        m.insert("total".into(), Value::from(self.total));
        if let Some(at) = &self.at {
            m.insert("field".into(), Value::from(at.clone()));
        }
        m.insert(
            "note".into(),
            Value::from(format!(
                "result truncated to {} of {} item(s) by max_items; \
                 re-invoke with pagination or a larger max_items for more",
                self.kept, self.total
            )),
        );
        Value::Object(m)
    }
}

/// Cap the op's **result array** at `max` items, returning a [`Truncation`] when it
/// shortened anything. Caps only the top level: `data` itself when it is an array,
/// otherwise the `data_key` array field when the op declares one (its paginated
/// result key). Never recurses into per-item sub-arrays (an item's own `scopes`
/// etc. are left intact).
pub fn cap_result(data: &mut Value, max: usize, data_key: Option<&str>) -> Option<Truncation> {
    fn cap(arr: &mut Vec<Value>, max: usize, at: Option<String>) -> Option<Truncation> {
        let total = arr.len();
        (total > max).then(|| {
            arr.truncate(max);
            Truncation {
                kept: max,
                total,
                at,
            }
        })
    }
    match data {
        Value::Array(arr) => cap(arr, max, None),
        Value::Object(map) => match map.get_mut(data_key?) {
            Some(Value::Array(arr)) => cap(arr, max, data_key.map(str::to_string)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_path_descends_objects_only() {
        let v = json!({ "a": { "b": { "c": 7 } }, "list": [1, 2] });
        assert_eq!(get_path(&v, "a.b.c"), Some(&json!(7)));
        assert_eq!(get_path(&v, "list"), Some(&json!([1, 2])));
        assert_eq!(get_path(&v, "a.missing"), None);
        // No array indexing: stop at the array.
        assert_eq!(get_path(&v, "list.0"), None);
    }

    #[test]
    fn collect_uris_handles_string_and_array() {
        assert_eq!(
            collect_uris(
                &json!({ "presigned_url": "https://x/y.csv" }),
                "presigned_url"
            ),
            vec!["https://x/y.csv".to_string()]
        );
        assert_eq!(
            collect_uris(&json!({ "urls": ["https://a/1", "https://b/2"] }), "urls"),
            vec!["https://a/1".to_string(), "https://b/2".to_string()]
        );
        assert!(collect_uris(&json!({ "other": 1 }), "urls").is_empty());
    }

    #[test]
    fn project_prunes_object_keys() {
        let data = json!({ "id": "x", "name": "n", "secret": "s" });
        let out = project(&data, &["id".into(), "name".into()]);
        assert_eq!(out, json!({ "id": "x", "name": "n" }));
        assert!(out.get("secret").is_none());
    }

    #[test]
    fn project_descends_into_arrays_elementwise() {
        // A list response: project id+name from each element, preserving pairing.
        let data = json!({
            "result": [
                { "id": 1, "name": "a", "extra": true },
                { "id": 2, "name": "b", "extra": false }
            ],
            "_metadata": { "count": 2 }
        });
        // Lenient `[]` accepted and stripped.
        let out = project(&data, &["result[].id".into(), "result.name".into()]);
        assert_eq!(
            out,
            json!({ "result": [ { "id": 1, "name": "a" }, { "id": 2, "name": "b" } ] })
        );
        assert!(out.get("_metadata").is_none());
    }

    #[test]
    fn project_keeps_whole_subtree_at_terminating_path() {
        let data = json!({ "result": { "id": 1, "deep": { "x": 2 } }, "other": 9 });
        let out = project(&data, &["result".into()]);
        assert_eq!(out, json!({ "result": { "id": 1, "deep": { "x": 2 } } }));
    }

    #[test]
    fn project_no_usable_paths_returns_unchanged() {
        let data = json!({ "a": 1 });
        assert_eq!(project(&data, &["".into()]), data);
    }

    #[test]
    fn cap_result_top_level_array() {
        let mut data = json!([1, 2, 3, 4, 5]);
        let t = cap_result(&mut data, 2, None).expect("truncated");
        assert_eq!(data, json!([1, 2]));
        assert_eq!((t.kept, t.total), (2, 5));
        assert!(t.at.is_none());
    }

    #[test]
    fn cap_result_data_key_array() {
        let mut data = json!({ "result": [1, 2, 3], "_metadata": {} });
        let t = cap_result(&mut data, 1, Some("result")).expect("truncated");
        assert_eq!(data["result"], json!([1]));
        assert_eq!(t.at.as_deref(), Some("result"));
        // Untouched when already under the cap.
        let mut small = json!({ "result": [1] });
        assert!(cap_result(&mut small, 5, Some("result")).is_none());
    }

    #[test]
    fn cap_result_does_not_touch_item_subarrays() {
        // data_key=None and data is an object → no cap (don't guess / don't recurse).
        let mut data = json!({ "item": { "scopes": [1, 2, 3, 4] } });
        assert!(cap_result(&mut data, 1, None).is_none());
        assert_eq!(data["item"]["scopes"], json!([1, 2, 3, 4]));
    }
}
