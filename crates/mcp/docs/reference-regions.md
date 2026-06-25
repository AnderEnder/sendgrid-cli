# Reference: regions and EU fail-closed behavior

SendGrid exposes two data regions: `global` and `eu`. The server routes every call to
the region it was configured with.

## Global-only API groups

**14 of the 46 API groups declare no EU host** — they are global-only:

> `email_activity`, `email_validation`, `lmc_campaigns`, `lmc_contactdb`,
> `lmc_senders`, `mc_contacts`, `mc_custom_fields`, `mc_lists`, `mc_segments`,
> `mc_segments_2.0`, `mc_senders`, `mc_singlesends`, `mc_stats`, `mc_test`.

## What happens with `--region eu`

Calling any operation in a global-only group on an EU-configured server **fails closed**
with `code: E_REGION_UNAVAILABLE` — it does **not** silently route your data to the
global region. This is intentional data-residency protection.

This is not retryable as-is. If routing that call to the global region is acceptable for
your use case, the server must be run with `--region global`; otherwise the operation is
simply unavailable in EU.

## Practical note

Most Marketing Campaigns and contact (`mc_*` / `lmc_*`) operations are global-only. If
you are on an EU server and a contacts/marketing call returns `E_REGION_UNAVAILABLE`,
that group has no EU host — it is not a transient failure.
