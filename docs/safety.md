# Safety model

Every call — CLI or MCP — passes through one `execute()` chokepoint in
`sendgrid-core`. The safety controls below are enforced there, so there is no
second path that can skip them.

## The side-effect model

Each of the 391 operations is classified into exactly one **side-effect class**:

| Class | Meaning | How it's assigned |
|---|---|---|
| `read` | No state change (a `GET`). | Default for `GET`. |
| `write` | Creates or modifies state. | Default for non-`GET`/`DELETE`. |
| `destructive` | Irreversibly removes data. | Default for `DELETE`; plus curated overrides (e.g. recipient data erasure, `:batchDelete`/`erasejob` paths). |
| `send` | Dispatches real outbound email or spends money/quota. | Curated list (e.g. `SendMail`, schedule/send single-send, send-test, invite-teammate, DNS-record email). |

The class is **semantic, not just method-derived**. For example
`EraseRecipientEmailData` is a `POST` but is classed `destructive`, and
`InviteTeammate` is classed `send` because it emails an arbitrary recipient — so a
read-only or no-send policy genuinely blocks them.

Separately, a small set of operations carry **bulk triggers**: a specific
field/value (e.g. `delete_all=true`, `delete_all_contacts=true`,
`delete_contacts=true`) that promotes an otherwise-ordinary call to a mass
mutation. These are gated independently of the side-effect class.

## The policy gate

A deployment permits a set of side-effect classes. A call whose class is not
permitted is refused **before anything is sent**, with a stable machine code:

- `E_POLICY_DENIED` — the op's class is not in the allowed set.
- `E_BULK_NOT_ALLOWED` — a bulk trigger fired and bulk is not enabled.

These are **configuration limits, not transient errors** — do not retry them.

### Default policy: ALLOWS ALL

> **The default policy allows every class (`read`, `write`, `destructive`,
> `send`).** This is a deliberate project decision (the mechanism for locking down
> is fully intact; it is simply not the default).

Restrict it with `--allow` (a comma-list) on either surface:

```sh
# CLI: allow only reads and writes for this invocation
sendgrid --allow read,write marketing contacts update-contact --body @c.json

# MCP: launch the server read-only
sendgrid --allow read mcp
```

Accepted tokens: `read`, `write`, `destructive`, `send`, `bulk`.

> **Important — `--allow` sets the *exact* allowed set; it is not additive over a
> base.** If you pass `--allow write`, then `read` operations are **also denied**.
> Always include `read` in any allow-list you want read operations to work under
> (e.g. `--allow read,write`). Bulk is enabled either by the `bulk` token in
> `--allow` or by the standalone `--allow-bulk` flag.

`--dry-run` (CLI) / `dry_run: true` (MCP) **bypasses the policy gate** because
nothing is sent — it builds and returns the exact request preview. Use it to
inspect a destructive/send call before granting the class.

## Recommended hardened preset (autonomous agents)

For an agent you do not want sending mail or deleting data on its own:

1. **Run the MCP server read-only:**

   ```jsonc
   {
     "mcpServers": {
       "sendgrid": {
         "command": "sendgrid",
         "args": ["--allow", "read", "mcp"],
         "env": { "SENDGRID_API_KEY": "SG.xxxxxxxx.yyyyyyyy" }
       }
     }
   }
   ```

   Write/destructive/send calls return `E_POLICY_DENIED`; the agent is steered
   toward `dry_run: true` to see what a call *would* do.

2. **Grant the minimum, never the default.** If the workflow legitimately needs to
   create/update, widen to `--allow read,write` — and **keep `destructive` and
   `send` off** unless the task truly requires them. Add `destructive`/`send`
   per-workflow, not globally.

3. **Preview first.** Have the agent run `dry_run: true` (or `--dry-run`) before
   any class-elevated call, and read the returned `request_preview`.

4. **Leave bulk disabled.** Do not pass `--allow-bulk` / the `bulk` token unless a
   mass operation is the explicit intent.

5. **Pin the region** if you have data-residency requirements (`--region eu`); see
   [limitations](limitations.md#eu-region-fails-closed-for-global-only-groups) for
   the global-only groups.

> Use the **most restrictive** preset that lets the task complete. `--allow read`
> is the safe baseline; escalate deliberately and narrowly.

## Always-on invariants (cannot be disabled by policy)

These hold regardless of the configured policy — they are not gated, they are
unconditional:

- **API keys are never logged, echoed, or returned.** The credential type is
  non-serializable, redacts itself in `Debug`, and zeroizes on drop. This includes
  the **live key in a `CreateApiKey` response** — that `api_key` field is redacted
  from the returned `data`. `auth whoami` shows only a non-reversible FNV
  fingerprint, never key bytes. A defense-in-depth regex scrub for the canonical
  `SG.…` key format runs over outputs as a final backstop.

- **On-behalf-of (impersonation) cannot be spoofed.** Any caller-supplied
  `on-behalf-of` or `authorization` header in the request args is **stripped**
  unconditionally. Impersonation is set **only** through the governed path
  (the root `--on-behalf-of` flag / server config) and the value must be in the
  configured allow-list, or the call fails with `E_IMPERSONATION_NOT_ALLOWED`.
  An empty allow-list means impersonation is disabled.

- **Secret fields are redacted from results and previews.** Request-side secret
  fields (`password`, `oauth_client_secret`, `client_secret`) are redacted from
  any `request_preview`, and response-side secret fields are redacted from the
  returned `data`. Redaction is deep (walks nested objects/arrays) and name-based,
  applied to every body-bearing op.

## `confirm` is **not** a security control

The MCP `invoke_operation` tool accepts a `confirm` parameter. It is
**acknowledgement only** — an interactive-human affordance. An autonomous agent
can set it on itself, so it provides **no protection** and **never bypasses the
policy gate**. The CLI deliberately has **no `--confirm` flag** at all.

> The side-effect policy (`--allow` / server config) is the **sole effective
> gate**. Do not rely on `confirm` to stop an agent from doing anything.

## See also

- [README — MCP setup & hardening](../README.md#hardening-the-mcp-server)
- [Architecture — the single execute() chokepoint](architecture.md#the-single-execute-chokepoint)
- [Known limitations](limitations.md)
