# Known limitations

Honest boundaries of the current build. None are silent: each surfaces a clear
message, a printed artifact, or an error code at the point it matters.

## Search ranking is lexical, not semantic

`sendgrid search <terms>` and the MCP `search_operations` tool rank by **lexical**
matching over each operation's id, summary, tags, domain, and HTTP path (plus a
small set of curated keyword aliases — e.g. "campaign"/"newsletter"/"broadcast"
fold onto the modern Single Sends ops). There is **no embedding/semantic ranking**;
that is future work.

Practical consequences and how to work around them:

- If the top hit looks off-target, **retry with a synonym or the modern term**.
  SendGrid renamed several concepts: "campaign" → "single send", "verify a domain"
  → "validate", "suppress" → "suppression".
- **Narrow with filters** rather than longer prose: the MCP tool accepts `domain`,
  `tags`, `side_effect`, and `method` filters; on the CLI, add a domain word or the
  HTTP verb/path fragment to your terms.
- When in doubt, browse the tree with `--help` (`sendgrid <domain> --help`).

## `--download` may not round-trip binary / gzipped exports

For external-download jobs (e.g. contact exports, email-activity CSV), `--download`
fetches the presigned URL(s) and writes them to a destination. **Binary or
gzip-compressed payloads may not round-trip faithfully** through the JSON transfer
layer.

**Mitigation (always available):** the resolved **presigned URL(s) are always
printed**, and that URL is the reliable artifact — fetch it directly with `curl`/
`wget` if `--download` produces anything unexpected. The URL is pre-authorized, so
no SendGrid credentials are sent to the storage host.

## `RequestCsv` → `DownloadCsv` cannot be fully auto-`--await`ed

The email-activity CSV export is a multi-step job, and SendGrid delivers the
download UUID **out-of-band via webhook**, not in the `RequestCsv` 202 response
body. Because the id never appears in the submit response, the CLI **cannot chain
`RequestCsv` straight into `DownloadCsv` with `--await`**.

When you `--await` such an op, the response is printed with guidance instead of
silently hanging. Workflow: submit `RequestCsv`, capture the UUID from your webhook
endpoint, then call `DownloadCsv` with it (optionally `--download`). Other async
jobs whose id *is* in the submit response (contact export/import, bulk email
validation) do auto-`--await` normally.

## EU region fails closed for global-only groups

SendGrid exposes only two data regions (`global`, `eu`). **14 of the 46 API groups
declare no EU host** — they are global-only:

> `email_activity`, `email_validation`, `lmc_campaigns`, `lmc_contactdb`,
> `lmc_senders`, `mc_contacts`, `mc_custom_fields`, `mc_lists`, `mc_segments`,
> `mc_segments_2.0`, `mc_senders`, `mc_singlesends`, `mc_stats`, `mc_test`.

With `--region eu`, calling any operation in these groups **fails closed**
(`E_REGION_UNAVAILABLE`) rather than silently routing your data to the global
region. This is intentional data-residency protection. If routing to global is
acceptable for that call, re-run with `--region global`.

## Config-file profiles are not implemented

Authentication and settings are configured today via **environment variables and
flags only**:

- API key: `SENDGRID_API_KEY` (preferred) or `--api-key`.
- Region: `--region global|eu`.
- Side-effect policy: `--allow` / `--allow-bulk`.
- Impersonation: `--on-behalf-of`.

A full **config-file profile system** — multiple named accounts in
`~/.config/sendgrid/config.toml`, alternate key sources (`key_command`, OS
keychain), per-profile region/impersonation defaults, `--profile`/`--config`/
`SENDGRID_PROFILE`/`SENDGRID_REGION` resolution — is **designed but not yet
implemented**. Use the env/flag inputs above for now.

## See also

- [Safety model](safety.md)
- [Architecture](architecture.md)
