# Reference: async / multi-step jobs

Most operations are a single request/response. A handful are multi-step jobs. For these,
`describe_operation` and the `invoke_operation` result carry an `async` block naming the
job `kind` and the next step. There are four kinds.

## `poll` — submit, then poll a status op

The op returns HTTP `202` plus a job handle. Pass **`await: true`** to `invoke_operation`
and the server submits, then polls the companion status op to a terminal state (bounded;
on timeout it adds a warning and you can re-invoke or poll the status op yourself with the
returned id). A job that ends in a FAILED terminal state is reported as an error
(`isError`, `code: E_ASYNC_JOB_FAILED`), with the job data kept intact.

- **Marketing contact export** — `ExportContact` → polls `GetExportContact`, whose
  terminal response carries the presigned download `urls`.

## `external_download` — fetch a presigned URL

The response carries presigned URL(s) on a non-SendGrid host. `invoke` surfaces them as
`download_urls` for you to GET directly. (Binary streaming over MCP is out of scope; fetch
the URL yourself.)

- `GetExportContact` (`urls`), `DownloadCsv` (`presigned_url`).

## `external_upload` — PUT to an upload URL, then poll

The op returns an `upload_uri` (+ headers). PUT your CSV/JSON to it, then poll the status
op. Binary upload is driven via the CLI, not MCP.

- **Marketing contact import** — `ImportContact` → PUT to `upload_uri` → poll
  `GetImportContact`.
- **Bulk email verification** — `ListEmailJobForVerification` → PUT upload → poll
  `GetEmailJobForVerification`.

## `fire_and_forget` — `202`, no status endpoint

Submitted and accepted; there is no status op to poll.

- **Recipient data erasure** — `EraseRecipientEmailData` returns `202 {job_id}`.

## The one flow that cannot be fully auto-awaited

**Email Activity CSV export** (`RequestCsv` → `DownloadCsv`): SendGrid delivers the
download UUID **out-of-band via webhook**, not in the `RequestCsv` 202 body. Because the
id never appears in the submit response, the chain cannot be auto-`await`ed. Workflow:
submit `RequestCsv`, capture the UUID from your webhook endpoint, then call `DownloadCsv`
with it. When you `await` it, the result is returned with this guidance instead of hanging.
