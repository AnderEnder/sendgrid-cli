//! Rendering an [`ExecuteResult`] to stdout/stderr and computing the process
//! exit code.
//!
//! - **json** (default): the full envelope; pretty on a TTY, compact otherwise.
//!   With `--query`, the `data` field is narrowed to the selection.
//! - **table / csv / ndjson**: operate over `data` (post-`--query`); envelope
//!   metadata is dropped (use json for that).
//! - **errors**: the verbatim `error` body goes to **stderr**; the exit code is
//!   `result.exit_code`.
//! - **--dry-run**: prints the `request_preview`.

use crate::globals::{GlobalOpts, OutputFormat};
use sendgrid_core::ExecuteResult;
use serde_json::Value;
use std::io::{IsTerminal, Write};

/// Render `result` per the global options and return the process exit code.
pub fn render(result: &ExecuteResult, globals: &GlobalOpts) -> i32 {
    let stdout_tty = std::io::stdout().is_terminal();

    // Non-fatal warnings always go to stderr.
    for w in &result.warnings {
        eprintln!("warning: {w}");
    }

    // Errors: verbatim error body to stderr.
    if !result.is_success() {
        if let Some(err) = result.error() {
            let _ = writeln!(std::io::stderr(), "{}", to_json_string(err, true));
        }
        return result.exit_code;
    }

    // Dry-run: show the constructed request preview.
    if globals.dry_run {
        match &result.request_preview {
            Some(preview) => println!("{}", to_json_string(preview, stdout_tty)),
            None => println!("{}", to_json_string(&envelope_value(result), stdout_tty)),
        }
        return result.exit_code;
    }

    // Success: render `data` (post --query) in the requested format.
    let mut data = result.data().cloned().unwrap_or(Value::Null);
    if let Some(q) = &globals.query {
        data = select(&data, q);
    }

    match globals.output {
        OutputFormat::Json => {
            let out = if globals.query.is_some() {
                let mut env = envelope_value(result);
                if let Value::Object(map) = &mut env {
                    map.insert("data".into(), data);
                }
                env
            } else {
                envelope_value(result)
            };
            println!("{}", to_json_string(&out, stdout_tty));
        }
        OutputFormat::Table => print!("{}", render_table(&data)),
        OutputFormat::Csv => print!("{}", render_csv(&data)),
        OutputFormat::Ndjson => print!("{}", render_ndjson(&data)),
    }

    result.exit_code
}

fn envelope_value(result: &ExecuteResult) -> Value {
    serde_json::to_value(result).unwrap_or(Value::Null)
}

fn to_json_string(v: &Value, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    } else {
        v.to_string()
    }
}

// ---- jq-lite field selection over `data` -----------------------------------

/// Select a sub-value from `data` using a dotted path. Supports object keys,
/// numeric array indices, and `[]`/`*` array wildcards (`result[].id`,
/// `result.0.name`, `*.email`).
pub fn select(value: &Value, expr: &str) -> Value {
    let expr = expr.trim().trim_start_matches('.');
    if expr.is_empty() {
        return value.clone();
    }
    let tokens: Vec<&str> = expr.split('.').filter(|t| !t.is_empty()).collect();
    select_tokens(value, &tokens)
}

fn select_tokens(value: &Value, tokens: &[&str]) -> Value {
    let Some((head, rest)) = tokens.split_first() else {
        return value.clone();
    };

    // Bare wildcard: map the remaining path over an array.
    if *head == "[]" || *head == "*" {
        return match value {
            Value::Array(arr) => Value::Array(arr.iter().map(|v| select_tokens(v, rest)).collect()),
            _ => Value::Null,
        };
    }

    // Numeric index into an array.
    if let Ok(idx) = head.parse::<usize>()
        && let Value::Array(arr) = value
    {
        return arr
            .get(idx)
            .map(|v| select_tokens(v, rest))
            .unwrap_or(Value::Null);
    }

    // Object key, with an optional trailing `[]` wildcard (`foo[]`).
    let (key, wild) = match head.strip_suffix("[]") {
        Some(k) => (k, true),
        None => (*head, false),
    };
    let next = match value {
        Value::Object(map) => map.get(key).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    if wild {
        return match &next {
            Value::Array(arr) => Value::Array(arr.iter().map(|v| select_tokens(v, rest)).collect()),
            _ => Value::Null,
        };
    }
    select_tokens(&next, rest)
}

// ---- tabular formatters -----------------------------------------------------

/// A scalar cell's string form; complex values become compact JSON.
fn cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Ordered union of object keys across an array of objects.
fn union_columns(rows: &[Value]) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    for row in rows {
        if let Value::Object(map) = row {
            for k in map.keys() {
                if !cols.iter().any(|c| c == k) {
                    cols.push(k.clone());
                }
            }
        }
    }
    cols
}

fn render_table(data: &Value) -> String {
    match data {
        Value::Array(arr) if arr.is_empty() => "(no rows)\n".to_string(),
        Value::Array(arr) if arr.iter().all(Value::is_object) => {
            let cols = union_columns(arr);
            let rows: Vec<Vec<String>> = arr
                .iter()
                .map(|row| cols.iter().map(|c| cell(row.get(c))).collect())
                .collect();
            grid(&cols, &rows)
        }
        Value::Array(arr) => {
            let rows: Vec<Vec<String>> = arr.iter().map(|v| vec![cell(Some(v))]).collect();
            grid(&["value".to_string()], &rows)
        }
        Value::Object(map) => {
            let rows: Vec<Vec<String>> = map
                .iter()
                .map(|(k, v)| vec![k.clone(), cell(Some(v))])
                .collect();
            grid(&["field".to_string(), "value".to_string()], &rows)
        }
        other => format!("{}\n", cell(Some(other))),
    }
}

/// A fixed-width column grid with a header underline.
fn grid(cols: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    for row in rows {
        for (i, c) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let mut out = String::new();
    let header: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| pad(c, widths[i]))
        .collect();
    out.push_str(header.join("  ").trim_end());
    out.push('\n');
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push_str(sep.join("  ").trim_end());
    out.push('\n');
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, *widths.get(i).unwrap_or(&0)))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

fn render_csv(data: &Value) -> String {
    match data {
        Value::Array(arr) if arr.iter().all(Value::is_object) && !arr.is_empty() => {
            let cols = union_columns(arr);
            let mut out = String::new();
            out.push_str(&csv_row(&cols));
            for row in arr {
                let fields: Vec<String> = cols.iter().map(|c| cell(row.get(c))).collect();
                out.push_str(&csv_row(&fields));
            }
            out
        }
        Value::Object(map) => {
            let cols: Vec<String> = map.keys().cloned().collect();
            let fields: Vec<String> = map.values().map(|v| cell(Some(v))).collect();
            let mut out = csv_row(&cols);
            out.push_str(&csv_row(&fields));
            out
        }
        Value::Array(arr) => {
            let mut out = csv_row(&["value".to_string()]);
            for v in arr {
                out.push_str(&csv_row(&[cell(Some(v))]));
            }
            out
        }
        other => csv_row(&[cell(Some(other))]),
    }
}

/// One RFC-4180 CSV record (CRLF-free `\n` line terminator).
fn csv_row(fields: &[String]) -> String {
    let escaped: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
    format!("{}\n", escaped.join(","))
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn render_ndjson(data: &Value) -> String {
    match data {
        Value::Array(arr) => {
            let mut out = String::new();
            for v in arr {
                out.push_str(&v.to_string());
                out.push('\n');
            }
            out
        }
        other => format!("{}\n", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn select_dotted_and_wildcard() {
        let data = json!({"result": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]});
        assert_eq!(select(&data, "result[].id"), json!([1, 2]));
        assert_eq!(select(&data, ".result.0.name"), json!("a"));
        assert_eq!(select(&data, "missing"), json!(null));
    }

    #[test]
    fn table_over_array_of_objects() {
        let data = json!([{"id": 1, "name": "alice"}, {"id": 2, "name": "bob"}]);
        let t = render_table(&data);
        assert!(t.contains("id"));
        assert!(t.contains("name"));
        assert!(t.contains("alice"));
    }

    #[test]
    fn csv_quotes_special_chars() {
        let data = json!([{"a": "x,y", "b": "he said \"hi\""}]);
        let c = render_csv(&data);
        assert!(c.contains("\"x,y\""));
        assert!(c.contains("\"he said \"\"hi\"\"\""));
    }

    #[test]
    fn ndjson_streams_array_elements() {
        let data = json!([{"id": 1}, {"id": 2}]);
        let n = render_ndjson(&data);
        assert_eq!(n.lines().count(), 2);
    }
}
