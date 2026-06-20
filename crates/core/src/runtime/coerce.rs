//! Type coercion (r4/PLAN): CLI passes every arg as a string. Before validation
//! we coerce each `path`/`query`/`header` value to the IR param's declared `ty`
//! so that `--limit "50"` satisfies an `integer` param, `--flag "true"` a boolean,
//! and `--ids "a,b,c"` an `array`. Dates and unknown types pass through as strings.
//!
//! Coercion is **scoped to the param buckets** (`path`/`query`/`header`). The
//! `body` is JSON the caller already structured; it is never coerced here.
//! Coercion is also **best-effort and non-destructive**: a value that cannot be
//! coerced (e.g. `"abc"` for an integer) is left as the original string so the
//! validation layer reports a precise, actionable error rather than this layer
//! swallowing it.

use crate::ir::{Location, OperationIr};
use serde_json::Value;

/// Coerce the `path`/`query`/`header` buckets of an args envelope in place,
/// using each op param's declared `ty`/`item_ty`. Only *string* values are
/// touched (MCP callers may already pass typed JSON, which is left as-is).
pub fn coerce_args(op: &OperationIr, args: &mut Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };

    for (bucket_key, loc) in [
        ("path", Location::Path),
        ("query", Location::Query),
        ("header", Location::Header),
    ] {
        let Some(Value::Object(bucket)) = obj.get_mut(bucket_key) else {
            continue;
        };
        for (name, val) in bucket.iter_mut() {
            // Param count per op is small; a linear find avoids needing `Hash`
            // on `Location` (keeps ir.rs untouched).
            let Some(param) = op
                .params
                .iter()
                .find(|p| p.location == loc && p.name == *name)
            else {
                continue; // undeclared param: leave untouched, validation flags it.
            };
            coerce_value(
                val,
                &param.ty,
                param.item_ty.as_deref(),
                param.format.as_deref(),
            );
        }
    }
}

/// Coerce a single value toward `ty`. Returns nothing; mutates `val`.
fn coerce_value(val: &mut Value, ty: &str, item_ty: Option<&str>, format: Option<&str>) {
    // Dates are carried as strings even though `ty` may say "string"; nothing to do.
    let _ = format;
    match ty {
        "integer" => {
            if let Value::String(s) = val
                && let Ok(n) = s.trim().parse::<i64>()
            {
                *val = Value::from(n);
            }
        }
        "number" => {
            if let Value::String(s) = val
                && let Ok(n) = s.trim().parse::<f64>()
                && let Some(num) = serde_json::Number::from_f64(n)
            {
                *val = Value::Number(num);
            }
        }
        "boolean" => {
            if let Value::String(s) = val {
                match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => *val = Value::Bool(true),
                    "false" | "0" | "no" => *val = Value::Bool(false),
                    _ => {}
                }
            }
        }
        "array" => {
            if let Value::String(s) = val {
                let items = s
                    .split(',')
                    .map(|piece| {
                        let mut elem = Value::String(piece.trim().to_string());
                        if let Some(it) = item_ty {
                            coerce_value(&mut elem, it, None, None);
                        }
                        elem
                    })
                    .collect();
                *val = Value::Array(items);
            } else if let Value::Array(items) = val {
                // Already an array (MCP): coerce each element by item type.
                if let Some(it) = item_ty {
                    for elem in items.iter_mut() {
                        coerce_value(elem, it, None, None);
                    }
                }
            }
        }
        // "string", "object", and unknown types pass through unchanged.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;
    use serde_json::json;

    #[test]
    fn limit_string_becomes_integer() {
        let r = Registry::global();
        let op = r.by_id("sg_stats_global_ListBrowserStat").expect("op");
        let mut args = json!({
            "query": { "start_date": "2026-06-01", "limit": "50", "offset": "100" }
        });
        coerce_args(op, &mut args);
        assert_eq!(args["query"]["limit"], json!(50));
        assert_eq!(args["query"]["offset"], json!(100));
        // A date-shaped string stays a string.
        assert_eq!(args["query"]["start_date"], json!("2026-06-01"));
    }

    #[test]
    fn boolean_and_array_coercion() {
        // Use ListSegment: `no_parent_list_id` boolean, `ids` array<string>.
        let r = Registry::global();
        let op = r
            .by_id("sg_marketing_segments_v1_ListSegment")
            .expect("ListSegment");
        let mut args = json!({
            "query": { "no_parent_list_id": "true", "ids": "a,b,c" }
        });
        coerce_args(op, &mut args);
        assert_eq!(args["query"]["no_parent_list_id"], json!(true));
        assert_eq!(args["query"]["ids"], json!(["a", "b", "c"]));
    }

    #[test]
    fn uncoercible_integer_left_as_string() {
        let r = Registry::global();
        let op = r.by_id("sg_stats_global_ListBrowserStat").expect("op");
        let mut args = json!({ "query": { "start_date": "x", "limit": "abc" } });
        coerce_args(op, &mut args);
        // Left for the validator to reject precisely, not silently dropped.
        assert_eq!(args["query"]["limit"], json!("abc"));
    }
}
