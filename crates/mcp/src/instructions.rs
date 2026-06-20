//! The `instructions` string returned at `initialize`. This is the entire onboarding
//! an agent gets — it sees only 3 tools, so this must teach the full workflow.

pub const INSTRUCTIONS: &str = r#"SendGrid MCP server — 391 v3 API operations behind 3 tools.

WORKFLOW (search → describe → invoke):
  1. search_operations { query, [tags], [side_effect], [method], [domain], [limit] }
     → ranked metadata-only hits: { id, summary, method, path, side_effect, tags }.
       Start here; queries are free-text (e.g. "create a contact list", "send email").
       If the top hit looks off-target or nothing fits, retry with a synonym or the
       modern term (SendGrid renamed several concepts): "campaign"/"newsletter" →
       "single send"; "verify a domain" → "validate"; "suppress" → "suppression".
       You can also narrow with a `domain` or `tags` filter (e.g. domain:"suppressions").
  2. describe_operation { id, [expand: minimal|full] }
     → minimal (default): params, required fields, a compact body EXAMPLE, cross-field
       constraints, and a compact RESPONSE field-menu (top-level names→types, one level
       into a result array's element) so you can chain calls — enough to build a valid
       call cheaply. The example is STRUCTURALLY valid (passes schema + cross-field rules;
       placeholders match each field's kind) but values are NOT guaranteed semantically
       sendable — swap `user@example.com` etc. for real values before a live call.
       full: the complete request-body AND response-body JSON Schemas (large; opt in).
  3. invoke_operation { id, [path_params], [query], [headers], [body], [dry_run], [confirm],
                         [fields], [max_items], [await] }
     → executes the op and returns a uniform result envelope (below).
       isError is set on the tool result whenever the call failed (policy denial,
       validation, region, a 4xx/5xx, or a failed async job) — a success envelope is
       never reported as an error and vice-versa.
       OUTPUT SHAPING (optional, to keep large responses out of context):
         fields: jq-lite dotted paths to keep from the success `data`, e.g.
                 ["result.id","result.name"]. A path crossing an array projects each
                 element (so `result[].id` keeps every item's id). Echoed back as `shaped`.
         max_items: cap the result array (top-level array, or the op's paginated result
                 array); when it truncates, a `truncated` note is added. Combine with
                 pagination to fetch more. Shaping touches only success data, never
                 errors/previews/secret handling.

OPERATION IDS: `sg_<domain>_<subgroup>_<operationId>` (the subgroup is dropped when
it equals the domain), e.g. `sg_mail_send_SendMail`, `sg_marketing_lists_CreateMarketingList`.
A few ids have a curated alias that also resolves.

RESULT ENVELOPE (from invoke_operation):
  { status, side_effect, exit_code, code?, request_preview?, next?, warnings?, data | error }
  - status: HTTP status (0 = nothing sent: dry-run or a pre-flight failure).
  - side_effect: read | write | destructive | send (the op's class).
  - data on success; error (verbatim SendGrid body, or {code,message}) on failure.
  - request_preview: the redacted request (always on dry_run).
  - next: continuation hint when a paginated --all run stopped at a cap.

SAFETY / POLICY MODEL (enforced server-side; you cannot bypass it):
  - Every op is classed read | write | destructive | send. read is always allowed;
    write/destructive/send are permitted only if the server's policy allows that class.
    A denied call returns code E_POLICY_DENIED — it is a configuration limit, not a
    transient error; do not retry it.
  - dry_run:true builds and returns request_preview WITHOUT sending — use it to confirm
    a destructive/send call before committing.
  - `confirm` is acknowledgement only; it is NOT a security control and never bypasses policy.
  - Bulk-class actions (e.g. delete_all=true) are denied unless the server enables them
    (code E_BULK_NOT_ALLOWED).
  - Secrets (api keys, passwords) are always redacted from results and previews.
  - Impersonation (on-behalf-of) is set only from governed server config; caller-supplied
    on-behalf-of/authorization headers are stripped.

PAGINATION: list ops paginate. The server can auto-follow pages up to capped limits; when
it stops early, the `next` field carries the continuation cursor/params to pass back.

REGIONS: some ops are global-only and will fail closed on an EU-configured server unless
fallback is enabled (code E_REGION_UNAVAILABLE).

ASYNC JOBS / EXPORTS: a handful of ops are multi-step. describe_operation and the invoke
result carry an `async` block naming the job `kind` and the `next` step:
  - poll: returns HTTP 202 + a job. Pass "await": true to invoke_operation to submit then
    poll the companion status op to a terminal state (bounded; on timeout a warning is
    added and you can re-invoke or call the status op yourself with the returned id). A
    job that finishes in a FAILED terminal state is reported as an error (isError + code
    E_ASYNC_JOB_FAILED), with the job data kept intact.
  - external_download: the response carries presigned URL(s); invoke surfaces them as
    `download_urls` for you to fetch (binary streaming over MCP is out of scope).
  - external_upload / fire_and_forget: described for legibility; uploads are driven via
    the CLI (binary upload is out of MCP scope).

SERVER IDENTITY / STARTUP: this server reports its implementation name as `sendgrid`.
It fails closed at startup if the configured API key is not a well-formed SendGrid key
(E_BAD_KEY_FORMAT) — if that happens the process never serves a single tool, so a dead
pipe at initialize means the key/credentials need fixing on the host, not a retry.

TIP: if --expose-tags/--expose-op were set, some operations also appear as first-class
tools (named by their id) that take the op's parameters directly and route through the
same safety pipeline (and accept the same fields/max_items/await controls where relevant).
"#;
