//! The `instructions` string returned at `initialize`. This is the always-loaded core —
//! deliberately lean. The full usage skill and the reference tables (regions, the
//! side-effect model, the async-job catalog) live in MCP **resources** that the agent
//! pulls on demand (`resources/list` + `resources/read`, or the `read_doc` tool), so this
//! string stays small while the depth is one read away.

pub const INSTRUCTIONS: &str = r#"SendGrid MCP server — 391 v3 API operations behind a few meta-tools.

LEARN MORE (pull on demand; this string is just the essentials):
  - sendgrid://skill/using-the-server   full how-to: searching, building calls, shaping, errors, async
  - sendgrid://reference/side-effects   the read/write/destructive/send policy model
  - sendgrid://reference/regions        the 14 global-only API groups + EU fail-close
  - sendgrid://reference/async-jobs     every export/import/poll flow
  Read any of these via resources/read, or the read_doc tool: read_doc { "uri": "..." }
  (read_doc with no args lists them).

WORKFLOW (search → describe → invoke):
  1. search_operations { query, [tags], [side_effect], [method], [domain], [limit] }
     → ranked metadata-only hits: { id, summary, method, path, side_effect, tags }.
       Start here; queries are free-text (e.g. "create a contact list", "send email").
       Ranking is lexical: if the top hit looks off, retry with the modern term
       (SendGrid renamed concepts): "campaign"/"newsletter" → "single send";
       "verify a domain" → "validate"; "suppress" → "suppression". Narrow with a
       `domain` or `tags` filter. Lower `limit` (e.g. 5) to spend fewer tokens.
  2. describe_operation { id, [expand: minimal|full] }
     → minimal (default): params, required fields, a STRUCTURALLY-VALID body example
       (passes schema + cross-field rules; swap placeholder values for real ones before
       a live call), constraint notes, and a compact RESPONSE field-menu for chaining.
       full: the complete request + response JSON Schemas (large; opt in — e.g. for a
       polymorphic oneOf/anyOf body the minimal example shows only the first alternative).
  3. invoke_operation { id, [path_params], [query], [headers], [body], [dry_run], [confirm],
                         [fields], [max_items], [await] }
     → executes the op and returns the uniform result envelope (below). isError on the
       tool result is the source of truth: it is set whenever the call failed (policy
       denial, validation, region, a 4xx/5xx, or a failed async job).
       OUTPUT SHAPING (optional, to keep large responses out of context):
         fields: jq-lite dotted paths to keep from success `data`, e.g.
                 ["result[].id","result[].name"] (a path crossing an array projects each
                 element). Echoed back as `shaped`.
         max_items: cap the result array; a `truncated` note is added when it bites.

OPERATION IDS: `sg_<domain>_<subgroup>_<operationId>` (subgroup dropped when it equals the
domain), e.g. `sg_mail_send_SendMail`, `sg_marketing_lists_CreateMarketingList`. A few ids
have a curated alias that also resolves.

RESULT ENVELOPE (from invoke_operation; also delivered as structuredContent):
  { status, side_effect, exit_code, code?, request_preview?, next?, warnings?, data | error }
  - status: HTTP status (0 = nothing sent: dry-run or a pre-flight failure).
  - side_effect: read | write | destructive | send (the op's class).
  - data on success; error (verbatim SendGrid body, or {code,message}) on failure.
  - request_preview: the redacted request (always on dry_run).
  - next: continuation cursor/params when a paginated run stopped at a cap — pass it back.

SAFETY / POLICY (enforced server-side; you cannot bypass it — full model in the
sendgrid://reference/side-effects resource):
  - Each op is read | write | destructive | send. read is always allowed; the others only
    if the server's policy permits that class. A denied call returns code E_POLICY_DENIED:
    a CONFIGURATION limit, not a transient error — do NOT retry; tell the user.
  - dry_run:true builds request_preview WITHOUT sending — use it before any write/destructive/send.
  - `confirm` is acknowledgement only; NOT a security control, never bypasses policy.
  - Bulk actions (e.g. delete_all=true) are denied unless enabled (E_BULK_NOT_ALLOWED).
  - Secrets are always redacted; caller-supplied on-behalf-of/authorization headers are stripped.

REGIONS: 14 of 46 API groups are global-only and fail closed (E_REGION_UNAVAILABLE) on an
EU-configured server — not retryable as-is. See sendgrid://reference/regions.

ASYNC JOBS / EXPORTS: a handful of ops are multi-step; describe + the invoke result carry an
`async` block. Pass "await":true to a Poll-class op to submit then poll to a terminal state
(a FAILED job → isError + E_ASYNC_JOB_FAILED). External downloads surface `download_urls`.
Full catalog + the RequestCsv webhook caveat: sendgrid://reference/async-jobs.

STARTUP: this server reports its implementation name as `sendgrid` and fails closed at
startup on a malformed API key (E_BAD_KEY_FORMAT) — a dead pipe at initialize means fix the
credentials on the host, not retry.

TIP: if --expose-tags/--expose-op were set, some operations also appear as first-class tools
(named by their id, carrying MCP behavior annotations) that route through the same safety
pipeline and accept the same fields/max_items/await controls where relevant.
"#;
