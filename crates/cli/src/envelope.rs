//! Turning a parsed leaf command into the `{path,query,header,body}` args
//! envelope the runtime [`execute`](sendgrid_core::execute) consumes.
//!
//! Every param value is carried as a **string**; the runtime coerces by the IR
//! `ty`. The body is parsed JSON (object *or* top-level array), passed verbatim.

use crate::globals::GlobalOpts;
use anyhow::{Context, bail};
use clap::ArgMatches;
use sendgrid_core::ir::{Location, OperationIr};
use serde_json::{Map, Value};
use std::io::Read;

/// Build the args envelope for `op` from its parsed leaf `ArgMatches` and the
/// global pagination conveniences.
pub fn build(op: &OperationIr, m: &ArgMatches, globals: &GlobalOpts) -> anyhow::Result<Value> {
    let mut path = Map::new();
    let mut query = Map::new();
    let mut header = Map::new();

    for p in &op.params {
        // The `on-behalf-of` leaf flag is intentionally NOT generated (impersonation
        // is governed only through the global `--on-behalf-of`). Querying clap for an
        // unregistered arg id panics, so this skip MUST mirror `tree::leaf_command`.
        if p.name.eq_ignore_ascii_case("on-behalf-of") {
            continue;
        }
        if let Some(v) = m.get_one::<String>(&p.name) {
            let bucket = match p.location {
                Location::Path => &mut path,
                Location::Query => &mut query,
                Location::Header => &mut header,
            };
            // Verbatim spec name as the key — the runtime looks params up by it.
            bucket.insert(p.name.clone(), Value::String(v.clone()));
        }
    }

    // Global pagination conveniences: inject only when the op actually declares
    // the matching query param (avoids sending unsupported params).
    if let Some(offset) = &globals.offset
        && has_query_param(op, "offset")
        && !query.contains_key("offset")
    {
        query.insert("offset".into(), Value::String(offset.clone()));
    }
    if let Some(token) = &globals.page_token {
        let key = page_token_param(op);
        if let Some(key) = key
            && !query.contains_key(key)
        {
            query.insert(key.to_string(), Value::String(token.clone()));
        } else if key.is_none() {
            eprintln!(
                "warning: --page-token ignored for `{}` (no page-token query param)",
                op.id
            );
        }
    }

    let mut envelope = Map::new();
    envelope.insert("path".into(), Value::Object(path));
    envelope.insert("query".into(), Value::Object(query));
    envelope.insert("header".into(), Value::Object(header));

    if op.has_body
        && let Some(raw) = m.get_one::<String>("body")
    {
        envelope.insert("body".into(), parse_body(raw)?);
    }

    Ok(Value::Object(envelope))
}

fn has_query_param(op: &OperationIr, name: &str) -> bool {
    op.params
        .iter()
        .any(|p| p.location == Location::Query && p.name == name)
}

/// The query param name to inject `--page-token` into, if the op has one.
fn page_token_param(op: &OperationIr) -> Option<&'static str> {
    const CANDIDATES: [&str; 2] = ["page_token", "page-token"];
    CANDIDATES.into_iter().find(|cand| {
        op.params
            .iter()
            .any(|p| p.location == Location::Query && p.name == *cand)
    })
}

/// Resolve a `--body` value: `@file`, `-` (stdin), or inline JSON. Accepts any
/// JSON value (objects and top-level arrays alike).
fn parse_body(raw: &str) -> anyhow::Result<Value> {
    let text = if raw == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading request body from stdin")?;
        buf
    } else if let Some(path) = raw.strip_prefix('@') {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading request body from file `{path}`"))?
    } else {
        raw.to_string()
    };

    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Ok(v),
        Err(e) => bail!("--body is not valid JSON: {e}"),
    }
}
