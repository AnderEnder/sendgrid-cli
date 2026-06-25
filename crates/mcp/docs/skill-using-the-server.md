# Skill: driving the SendGrid MCP server

This server exposes **all 391 SendGrid v3 API operations** behind a tiny meta-tool
surface instead of 391 tool definitions. This guide teaches you to drive it well.
Read it once before non-trivial work; the `instructions` you got at startup are the
condensed version of what follows.

## The core loop: search → describe → invoke

1. **`search_operations { query, [domain], [tags], [side_effect], [method], [limit] }`**
   Free-text, ranked, metadata-only hits (`id, summary, method, path, side_effect,
   tags`). Cheap — start here. Lower `limit` (e.g. 5) to spend fewer tokens when you
   only need the top hit.
2. **`describe_operation { id, [expand: minimal|full] }`**
   `minimal` (default) gives params, required fields, a **validated** body example,
   cross-field constraints in plain English, and a compact response field-menu for
   chaining — enough to build a correct call cheaply. `full` adds the complete request
   and response JSON Schemas (can be large; opt in only when you need every field or
   when a body is polymorphic — see below).
3. **`invoke_operation { id, [path_params], [query], [headers], [body], [dry_run],
   [fields], [max_items], [await] }`**
   Executes and returns the uniform result envelope. `isError` on the tool result is
   the source of truth for success/failure.

You generally do not need `full`: the `minimal` example already passes the same
validator `invoke` uses. Spend a `full` only when `describe` tells you a field is
`oneOf`/`anyOf` (the minimal example materializes just the first alternative).

## Searching well

Ranking is **lexical, not semantic** — match the API's vocabulary, not a paraphrase.
SendGrid renamed several concepts; if the top hit looks off, retry with the modern term:

| You might say | Search for |
|---|---|
| campaign / newsletter / broadcast | **single send** |
| verify a domain | **validate** / **authenticate** (domain) |
| suppress an address | **suppression** |
| whitelabel | **branding** / authenticated domain |

Narrow with filters rather than longer prose: `domain:"suppressions"`,
`side_effect:"send"`, `method:"POST"`.

## Building an invoke call

`invoke_operation` buckets inputs explicitly — put each value where it belongs:

- `path_params` — values that substitute into the URL path (`{id}` etc.).
- `query` — query-string params.
- `headers` — header values. Note: `on-behalf-of` / `authorization` are **stripped**;
  impersonation is set only from governed server config, never by you.
- `body` — the request body (object, or array for the few array-body ops).

Always `dry_run: true` first for any `write` / `destructive` / `send` op to see the
redacted `request_preview` before committing.

## Reading the result envelope

```
{ status, side_effect, exit_code, code?, request_preview?, next?, warnings?, data | error }
```

- `status` — HTTP status; `0` means nothing was sent (dry-run or a pre-flight failure).
- `data` on success, `error` on failure (verbatim SendGrid body or `{code,message}`).
- **`isError` on the tool result** is set whenever the call failed; trust it over guessing
  from the body. A success envelope is never reported as an error, and vice-versa.

## Keeping responses small

List/get ops can return large payloads. Trim them at the source:

- `fields`: jq-lite dotted paths to keep from success `data`, e.g.
  `["result[].id","result[].name"]`. A path crossing an array projects each element.
- `max_items`: cap the result array; a `truncated` note is added when it bites.

Use these whenever you only need a few fields or rows — it keeps large responses out of
your context.

## Failure modes that are NOT retryable

- `E_POLICY_DENIED` — the server's side-effect policy forbids this class (write/
  destructive/send). This is a **configuration limit, not a transient error.** Do not
  retry; tell the user the server is configured read-only (or without that class).
- `E_BULK_NOT_ALLOWED` — a bulk action (e.g. `delete_all=true`) is disabled on this
  server. Not retryable.
- `E_REGION_UNAVAILABLE` — the op's API group has no host in the configured region
  (14 groups are global-only). Not retryable as-is; the account/server must use the
  right region. See `sendgrid://reference/regions`.
- `E_BAD_KEY_FORMAT` — the server fails closed at startup on a malformed key, so a dead
  pipe at initialize means fix the credentials on the host, not retry.

Retryable: ordinary `5xx`/network blips. A `4xx` validation error means fix the request
(the envelope points at the offending field), not retry verbatim.

## Async jobs (exports, imports, bulk validation, IP warmup)

Some ops are multi-step. `describe` and the invoke result carry an `async` block naming
the job `kind` and next step. Pass `await: true` to a Poll-class op to submit then poll
its companion status op to a terminal state (bounded). A job that ends FAILED is
reported as an error (`E_ASYNC_JOB_FAILED`) with the job data intact. For external
downloads the result carries `download_urls` to fetch yourself. Full catalog:
`sendgrid://reference/async-jobs`.

## Pagination

List ops paginate. When a capped auto-follow stops early, the envelope's `next` field
carries the continuation cursor/params to pass back on the next call.

## More reference

- `sendgrid://reference/side-effects` — the read/write/destructive/send model in full.
- `sendgrid://reference/regions` — the global-only groups and EU fail-closed behavior.
- `sendgrid://reference/async-jobs` — every async/export flow and how to drive it.

All three are also readable via the `read_doc` tool if your client doesn't surface
resources to you directly.
