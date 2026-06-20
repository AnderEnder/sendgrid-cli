//! Async-job flag handlers: `--await` (poll a submitted job to terminal),
//! `--upload-file` (PUT a file to a presigned URL), and `--download` (GET
//! presigned URL(s) to disk).
//!
//! These wrap the frozen `sendgrid_core` job helpers. Two non-obvious contracts
//! drive the design:
//! - **await reports success on FAILURE.** `await_job` returns
//!   `is_success() == true` whenever the *poll* got a 2xx, even if the job's own
//!   terminal `status` is `failure`. We re-inspect the job status and force a
//!   non-zero exit on a failed terminal state.
//! - **the job id is not always in the submit response.** Email-activity
//!   `RequestCsv` returns `{status,message}` and delivers the `download_uuid` via
//!   webhook; we detect "no extractable id" and print actionable guidance instead
//!   of polling blind.
//!
//! Presigned transfers go through `build_client()` (no SendGrid bearer ever
//! reaches the upload/download host) and a fresh `ReqwestDispatcher`.

use crate::globals::GlobalOpts;
use crate::output;
use clap::ArgMatches;
use sendgrid_core::ir::{AsyncJob, Location, OperationIr};
use sendgrid_core::runtime::envelope::exit_code_for_status;
use sendgrid_core::runtime::http::build_client;
use sendgrid_core::{
    ExecuteResult, PollConfig, Registry, ReqwestDispatcher, await_job, execute, external_download,
    external_upload,
};
use serde_json::{Map, Value, json};
use std::path::Path;

/// Which async flag (if any) the parsed leaf selected. The `async_job`-gated
/// queries live here because clap panics on `get_flag`/`get_one` for an arg id
/// that was never registered — so we only ask for a flag on the op kind that has it.
pub enum AsyncAction {
    Await,
    Upload(String),
    Download(String),
}

/// Inspect the parsed leaf for an async flag, gated by the op's `async_job` kind.
pub fn selected_async(op: &OperationIr, leaf: &ArgMatches) -> Option<AsyncAction> {
    match op.async_job {
        AsyncJob::Poll => leaf.get_flag("await").then_some(AsyncAction::Await),
        AsyncJob::ExternalUpload => leaf
            .get_one::<String>("upload-file")
            .map(|s| AsyncAction::Upload(s.clone())),
        AsyncJob::ExternalDownload => leaf
            .get_one::<String>("download")
            .map(|s| AsyncAction::Download(s.clone())),
        AsyncJob::FireAndForget | AsyncJob::None => None,
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

// ---- --await -----------------------------------------------------------------

/// Submit a `Poll` op, then poll its companion status op until terminal.
pub async fn run_await(op: &OperationIr, args: Value, globals: &GlobalOpts) -> i32 {
    let cfg = match globals.runtime_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };

    // dry-run: show the submit preview; never poll.
    if globals.dry_run {
        let result = execute(&cfg, op, args).await;
        return output::render(&result, globals);
    }

    let initial = execute(&cfg, op, args).await;
    if !initial.is_success() {
        return output::render(&initial, globals);
    }

    let Some(status_id) = op.async_status_op.as_deref() else {
        eprintln!(
            "warning: `{}` is a poll job but declares no companion status op; printing the submit \
             response.",
            op.id
        );
        return output::render(&initial, globals);
    };
    let Some(status_op) = Registry::global().by_id(status_id) else {
        eprintln!("error: companion status op `{status_id}` is not in the registry");
        return 70;
    };

    let data = initial.data().cloned().unwrap_or(Value::Null);
    let status_args = match build_status_args(status_op, &data) {
        Ok(a) => a,
        Err(msg) => {
            // Webhook-delivered-id case (e.g. RequestCsv): print the submit response
            // and tell the operator how to continue.
            eprintln!("note: {msg}");
            return output::render(&initial, globals);
        }
    };

    let dispatcher = ReqwestDispatcher::new();
    let poll = poll_config_for(status_op);
    let result = await_job(&cfg, status_op, status_args, &dispatcher, &poll).await;
    let code = output::render(&result, globals);

    // The await-success-on-FAILURE gotcha: a 2xx poll can still carry a terminal
    // `failure` status. Branch on the job's own status for the exit code.
    if result.is_success()
        && let Some(status) = job_status(&result, &poll)
        && is_failure_status(&status)
    {
        eprintln!(
            "error: job `{}` reached terminal status `{status}` — treating as failure",
            status_op.id
        );
        return 1;
    }
    code
}

/// Build the status op's args (`{path:{<param>:<id>}}`) from the submit response.
/// Errors (with actionable guidance) when no id for the status op's path param can
/// be found — the signal that the id is delivered out-of-band.
fn build_status_args(status_op: &OperationIr, data: &Value) -> Result<Value, String> {
    let path_params: Vec<&str> = status_op
        .params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.name.as_str())
        .collect();
    if path_params.is_empty() {
        return Ok(json!({}));
    }
    let obj = data.as_object();
    let mut path = Map::new();
    for pname in &path_params {
        match find_id(obj, pname) {
            Some(v) => {
                path.insert((*pname).to_string(), v);
            }
            None => {
                let fields: Vec<String> =
                    obj.map(|m| m.keys().cloned().collect()).unwrap_or_default();
                return Err(format!(
                    "could not find a value for the status op's `{pname}` path param in the submit \
                     response (response fields: {fields:?}). Some jobs deliver the id out-of-band \
                     via webhook rather than in the response (e.g. email-activity RequestCsv → \
                     DownloadCsv); once you have the id, call the status op directly with \
                     `--{pname} <value>`."
                ));
            }
        }
    }
    Ok(json!({ "path": Value::Object(path) }))
}

/// Find a job-id value for `pname`: exact key first, then common id field names.
fn find_id(obj: Option<&Map<String, Value>>, pname: &str) -> Option<Value> {
    let obj = obj?;
    if let Some(v) = obj.get(pname) {
        return Some(v.clone());
    }
    const FALLBACKS: [&str; 4] = ["id", "job_id", "download_uuid", "uuid"];
    FALLBACKS.iter().find_map(|k| obj.get(*k).cloned())
}

/// Poll settings per status op. Download-link status ops (the link IS the payload)
/// return 404 until the artifact lands, so 404 means "keep polling" for them.
fn poll_config_for(status_op: &OperationIr) -> PollConfig {
    let mut p = PollConfig::default();
    if status_op.async_job == AsyncJob::ExternalDownload {
        p.pending_http_statuses = vec![404];
    }
    p
}

fn job_status(result: &ExecuteResult, poll: &PollConfig) -> Option<String> {
    result
        .data()?
        .pointer(&poll.status_pointer)?
        .as_str()
        .map(str::to_string)
}

fn is_failure_status(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "failure" | "failed" | "error" | "errored" | "canceled" | "cancelled" | "expired"
    )
}

// ---- --upload-file -----------------------------------------------------------

/// Submit an `ExternalUpload` op, then PUT the file's bytes to the returned URL.
pub async fn run_upload(op: &OperationIr, args: Value, globals: &GlobalOpts, file: &str) -> i32 {
    let cfg = match globals.runtime_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };

    if globals.dry_run {
        eprintln!("note: --dry-run shows the submit request preview only; no file is uploaded.");
        let result = execute(&cfg, op, args).await;
        return output::render(&result, globals);
    }

    // Read the file before submitting — fail fast on a bad path.
    let body = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading upload file `{file}`: {e}");
            return 66;
        }
    };
    let bytes = body.len();

    let initial = execute(&cfg, op, args).await;
    if !initial.is_success() {
        return output::render(&initial, globals);
    }
    let data = initial.data().cloned().unwrap_or(Value::Null);
    let uri_field = op.async_uri_field.as_deref().unwrap_or("upload_uri");
    let Some(url) = first_uri(&data, uri_field) else {
        eprintln!("error: submit response carried no upload URL at `{uri_field}`");
        println!("{}", pretty(&json!({ "operation": op.id, "job": data })));
        return 70;
    };

    let (content_type, unforwarded) = upload_content_type(&data, Path::new(file));
    if !unforwarded.is_empty() {
        eprintln!(
            "warning: the upload also requires header(s) {unforwarded:?} that this client cannot \
             forward (only Content-Type is sent); the upload host may reject the request."
        );
    }

    let client = build_client();
    let dispatcher = ReqwestDispatcher::with_client(client.clone());
    match external_upload(&dispatcher, &client, &url, content_type.as_deref(), body).await {
        Ok(resp) => {
            let code = resp.status.as_u16();
            let report = json!({
                "operation": op.id,
                "job": data,
                "upload": {
                    "status": code,
                    "ok": resp.status.is_success(),
                    "bytes": bytes,
                    "content_type": content_type,
                },
            });
            println!("{}", pretty(&report));
            if resp.status.is_success() {
                0
            } else {
                exit_code_for_status(code)
            }
        }
        Err(e) => {
            eprintln!("error: upload transfer failed: {e}");
            8
        }
    }
}

/// Extract a Content-Type (and the names of any other required headers we cannot
/// forward) from the response's `upload_headers`, falling back to file extension.
fn upload_content_type(data: &Value, path: &Path) -> (Option<String>, Vec<String>) {
    let mut content_type = None;
    let mut other = Vec::new();
    match data.get("upload_headers") {
        Some(Value::Array(arr)) => {
            for h in arr {
                let name = h.get("header").and_then(Value::as_str);
                let val = h.get("value").and_then(Value::as_str);
                if let (Some(name), Some(val)) = (name, val) {
                    if name.eq_ignore_ascii_case("content-type") {
                        content_type = Some(val.to_string());
                    } else {
                        other.push(name.to_string());
                    }
                }
            }
        }
        Some(Value::Object(map)) => {
            for (k, v) in map {
                if k.eq_ignore_ascii_case("content-type") {
                    if let Some(s) = v.as_str() {
                        content_type = Some(s.to_string());
                    }
                } else {
                    other.push(k.clone());
                }
            }
        }
        _ => {}
    }
    if content_type.is_none() {
        content_type = infer_content_type(path);
    }
    (content_type, other)
}

fn infer_content_type(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("csv") => Some("text/csv".into()),
        Some("json") => Some("application/json".into()),
        Some("gz" | "gzip") => Some("application/gzip".into()),
        Some("zip") => Some("application/zip".into()),
        _ => None,
    }
}

// ---- --download --------------------------------------------------------------

/// Submit an `ExternalDownload` op, then GET the returned presigned URL(s) to disk.
pub async fn run_download(op: &OperationIr, args: Value, globals: &GlobalOpts, dest: &str) -> i32 {
    let cfg = match globals.runtime_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 64;
        }
    };

    if globals.dry_run {
        eprintln!("note: --dry-run shows the submit request preview only; nothing is downloaded.");
        let result = execute(&cfg, op, args).await;
        return output::render(&result, globals);
    }

    let initial = execute(&cfg, op, args).await;
    if !initial.is_success() {
        return output::render(&initial, globals);
    }
    let data = initial.data().cloned().unwrap_or(Value::Null);
    let uri_field = op.async_uri_field.as_deref().unwrap_or("presigned_url");
    let urls = all_uris(&data, uri_field);
    if urls.is_empty() {
        eprintln!(
            "error: submit response carried no download URL(s) at `{uri_field}` — the job may not \
             be ready yet (run the submit op with --await first), or the link is delivered via \
             webhook."
        );
        let _ = output::render(&initial, globals);
        return 70;
    }

    // The resolved URLs are the reliable artifact (binary/compressed payloads may
    // not round-trip the JSON transfer layer), so always surface them.
    for u in &urls {
        eprintln!("download url: {u}");
    }
    eprintln!(
        "note: compressed/binary payloads may not round-trip the JSON transfer layer; if a written \
         file looks corrupt, fetch the URL(s) above directly."
    );

    let multi = urls.len() > 1;
    let dest_path = Path::new(dest);
    let dir_mode = multi || dest_path.is_dir();
    if dir_mode {
        if let Err(e) = std::fs::create_dir_all(dest_path) {
            eprintln!("error: creating destination directory `{dest}`: {e}");
            return 73;
        }
    } else if let Some(parent) = dest_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        let _ = std::fs::create_dir_all(parent);
    }

    let client = build_client();
    let dispatcher = ReqwestDispatcher::with_client(client.clone());
    let mut files = Vec::new();
    let mut any_fail = false;
    for (i, url) in urls.iter().enumerate() {
        match external_download(&dispatcher, &client, url).await {
            Ok(resp) => {
                let code = resp.status.as_u16();
                if !resp.status.is_success() {
                    any_fail = true;
                    files.push(json!({ "index": i, "status": code, "ok": false }));
                    continue;
                }
                let out = if dir_mode {
                    dest_path.join(filename_from_url(url).unwrap_or_else(|| format!("part-{i}")))
                } else {
                    dest_path.to_path_buf()
                };
                let payload = body_bytes(&resp.body);
                match std::fs::write(&out, &payload) {
                    Ok(()) => files.push(json!({
                        "index": i, "status": code, "ok": true,
                        "path": out.display().to_string(), "bytes": payload.len(),
                    })),
                    Err(e) => {
                        any_fail = true;
                        eprintln!("error: writing `{}`: {e}", out.display());
                        files.push(json!({ "index": i, "status": code, "ok": false, "error": e.to_string() }));
                    }
                }
            }
            Err(e) => {
                any_fail = true;
                eprintln!("error: download transfer failed: {e}");
                files.push(json!({ "index": i, "ok": false, "error": e.to_string() }));
            }
        }
    }

    let report = json!({ "operation": op.id, "downloaded": files, "urls": urls });
    println!("{}", pretty(&report));
    if any_fail { 8 } else { 0 }
}

/// All URL strings at `field`, handling the non-uniform shape: a STRING
/// (`upload_uri`/`presigned_url`) OR an ARRAY of strings (`urls`).
fn all_uris(data: &Value, field: &str) -> Vec<String> {
    match output::select(data, field) {
        Value::String(s) => vec![s],
        Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn first_uri(data: &Value, field: &str) -> Option<String> {
    all_uris(data, field).into_iter().next()
}

fn filename_from_url(url: &str) -> Option<String> {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let seg = no_query.rsplit('/').next().unwrap_or("");
    (!seg.is_empty()).then(|| seg.to_string())
}

fn body_bytes(body: &Value) -> Vec<u8> {
    match body {
        Value::String(s) => s.clone().into_bytes(),
        Value::Null => Vec::new(),
        other => serde_json::to_vec_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{resolve, tree};

    fn op_by_id(id: &str) -> &'static OperationIr {
        Registry::global().by_id(id).expect("op present")
    }

    #[test]
    fn export_contact_status_args_from_id() {
        let status = op_by_id("sg_marketing_contacts_GetExportContact");
        let data = json!({ "_metadata": {}, "id": "exp_123" });
        let args = build_status_args(status, &data).expect("id extracted");
        assert_eq!(args, json!({ "path": { "id": "exp_123" } }));
    }

    #[test]
    fn request_csv_has_no_id_in_response() {
        // DownloadCsv needs `download_uuid`; the RequestCsv 202 body has none.
        let status = op_by_id("sg_stats_activity_DownloadCsv");
        let data = json!({ "status": "pending", "message": "queued" });
        let err = build_status_args(status, &data).expect_err("no extractable id");
        assert!(err.contains("download_uuid"));
        assert!(err.contains("webhook"));
    }

    #[test]
    fn uris_handle_string_and_array() {
        // presigned_url is a string; urls is an array.
        assert_eq!(
            all_uris(
                &json!({ "presigned_url": "https://x/y.csv" }),
                "presigned_url"
            ),
            vec!["https://x/y.csv".to_string()]
        );
        assert_eq!(
            all_uris(&json!({ "urls": ["https://a/1", "https://b/2"] }), "urls"),
            vec!["https://a/1".to_string(), "https://b/2".to_string()]
        );
        assert!(all_uris(&json!({ "other": 1 }), "urls").is_empty());
    }

    #[test]
    fn failure_status_classification() {
        for s in ["failure", "FAILED", "error", "canceled", "Expired"] {
            assert!(is_failure_status(s), "{s} should be failure");
        }
        for s in ["ready", "complete", "succeeded", "done"] {
            assert!(!is_failure_status(s), "{s} should be success");
        }
    }

    #[test]
    fn content_type_from_upload_headers_and_extension() {
        let data = json!({ "upload_headers": [
            { "header": "Content-Type", "value": "text/csv" },
            { "header": "x-amz-server-side-encryption", "value": "AES256" },
        ]});
        let (ct, other) = upload_content_type(&data, Path::new("contacts.csv"));
        assert_eq!(ct.as_deref(), Some("text/csv"));
        assert_eq!(other, vec!["x-amz-server-side-encryption".to_string()]);

        // No headers → infer from extension.
        let (ct2, other2) = upload_content_type(&json!({}), Path::new("data.json"));
        assert_eq!(ct2.as_deref(), Some("application/json"));
        assert!(other2.is_empty());
    }

    #[test]
    fn filename_from_url_strips_query() {
        assert_eq!(
            filename_from_url("https://h/path/export-1.csv.gz?sig=abc"),
            Some("export-1.csv.gz".to_string())
        );
        assert_eq!(filename_from_url("https://h/"), None);
    }

    #[test]
    fn selected_async_is_gated_by_async_job() {
        let (cmd, resolve_map) = tree::build(false);

        // Poll op with --await → Await.
        let m = cmd
            .clone()
            .try_get_matches_from([
                "sendgrid",
                "marketing",
                "contacts",
                "export-contact",
                "--await",
            ])
            .expect("parse export --await");
        let (chain, leaf) = resolve::leaf_matches(&m);
        let op = resolve_map
            .get(&chain.join(" "))
            .copied()
            .expect("resolves");
        assert!(matches!(selected_async(op, leaf), Some(AsyncAction::Await)));

        // ExternalDownload op with --download → Download(dest).
        let m = cmd
            .clone()
            .try_get_matches_from([
                "sendgrid",
                "stats",
                "activity",
                "download-csv",
                "--download_uuid",
                "u1",
                "--download",
                "/tmp/out.csv",
            ])
            .expect("parse download-csv --download");
        let (chain, leaf) = resolve::leaf_matches(&m);
        let op = resolve_map
            .get(&chain.join(" "))
            .copied()
            .expect("resolves");
        assert!(matches!(
            selected_async(op, leaf),
            Some(AsyncAction::Download(_))
        ));

        // A non-async op never trips the gated queries (no panic, None).
        let m = cmd
            .try_get_matches_from(["sendgrid", "mail", "send", "send-mail", "--body", "{}"])
            .expect("parse send-mail");
        let (chain, leaf) = resolve::leaf_matches(&m);
        let op = resolve_map
            .get(&chain.join(" "))
            .copied()
            .expect("resolves");
        assert!(selected_async(op, leaf).is_none());
    }
}
