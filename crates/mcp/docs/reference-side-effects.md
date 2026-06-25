# Reference: the side-effect / safety model

Every one of the 391 operations is classified into exactly one **side-effect class**,
and the server enforces a policy over those classes in one place (the `execute()`
chokepoint). You cannot bypass it from the tool surface.

## The four classes

| Class | Meaning | Examples |
|---|---|---|
| `read` | No state change. **Always allowed.** | list/get ops, GET requests |
| `write` | Creates or updates state, non-destructive. | create a list, update a template |
| `destructive` | Deletes or irreversibly changes state. | delete a list, erase recipient data |
| `send` | Emits a message to the outside world. | send mail, send a single send |

`search_operations` returns each op's class, and `describe_operation` repeats it, so you
always know the class before you invoke.

## How the policy gate works

- `read` is always permitted.
- `write`, `destructive`, and `send` are permitted **only if the server's policy allows
  that class.** A denied call returns `code: E_POLICY_DENIED` — a configuration limit,
  not a transient error. Do not retry it; report that the server is configured without
  that class.
- **Bulk** actions (e.g. `delete_all=true`) are denied unless the server explicitly
  enables them: `code: E_BULK_NOT_ALLOWED`.

## Controls you do have

- `dry_run: true` builds and returns the redacted `request_preview` **without sending.**
  Use it to confirm any write/destructive/send call before committing.
- `confirm` is acknowledgement only. It is **not** a security control and never bypasses
  policy.

## Always-on protections

- **Secret redaction.** API keys, passwords, and similar are redacted from every result
  and preview.
- **Impersonation governance.** `on-behalf-of` is set only from governed server config;
  any caller-supplied `on-behalf-of` / `authorization` header is stripped.
