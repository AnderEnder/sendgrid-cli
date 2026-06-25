# Architecture

A short tour of the four ideas that hold the project together: the **IR as the
single contract**, the **data-driven generator** (the bake-off outcome), the
**xtask codegen + drift** pipeline, and the **single `execute()` chokepoint**.

```
twilio/sendgrid-oai specs (vendored, SHA-pinned)
        │  cargo xtask codegen   (parse + curated tables)
        ▼
generated/ir.json  +  generated/schemas.json     ← the contract, committed
        │  include_str!  (compile-time embed)
        ▼
sendgrid-core::Registry  (391 × OperationIr, &'static)
        │
        ├──────────────► sendgrid-core::execute()  ◄── the one chokepoint
        │                        ▲          ▲
   crates/cli (clap tree)  ──────┘          └────── crates/mcp (meta-tools + resources/prompts)
```

## IR as the contract

Everything downstream is driven by one data structure: a `Vec<OperationIr>` — one
entry per operation, carrying its HTTP method/path, parameters (with location),
body schema reference, CLI taxonomy slot, side-effect class, pagination style,
region availability, async-job kind, and secret-field lists.

The IR is produced offline by `cargo xtask codegen` and committed as
`crates/core/generated/ir.json` (~600 KB, 391 entries) plus
`schemas.json` (~178 KB of per-op JSON Schemas). `sendgrid-core` embeds both at
**compile time** via `include_str!` and parses them once into a `&'static Registry`
(`OnceLock`). So the contract ships *inside* the binary — no sidecar files, no
runtime spec fetch.

Because the IR is the single source of truth, the CLI command tree, the MCP tool
schemas, body validation, the safety classification, pagination, and region
routing are all **the same data viewed differently**. Adding/removing an operation
is a regeneration, never hand-edited parallel lists.

## The generator bake-off: data-driven (Backend D) won

Two backends were prototyped against the same specs:

- **Backend T (typed)** — `progenitor`-generated typed clients per spec.
- **Backend D (data-driven)** — parse specs into the IR; one generic
  `build_request` drives every op from raw `serde_json::Value`.

**D was adopted.** The decision (see `.research-notes/generator-decision.md`) rested
on three ranked criteria, all favoring D:

| Criterion | Backend D | Backend T |
|---|---|---|
| **Coverage** | **391/391** ops build | ~380 (3 specs never generate → 11 ops lost; progenitor fails the whole document when one op trips) |
| **Per-spec hacks / drift-robustness** | **0** — raw `Value` parse, no spec rewriting | 2 global JSON transforms + 2 schema renames + 3 bespoke specs, all maintained against upstream drift |
| **Feeds the runtime IR** | **D *is* the IR** — one parse → one generic dispatcher | contributes nothing to the IR; *adds* ~386 per-op dispatch shims + lossy typed→raw re-serialization |

Two findings were decisive beyond the table:

- **Round-trip fidelity.** Real SendGrid responses routinely carry fields the specs
  omit. D returns the response **verbatim**; T's typed structs silently dropped
  undocumented fields and omitted documented-null fields (measured: 5 fields lost on
  a single paginated response). For an agent that self-corrects from the response,
  verbatim matters.
- **Error fidelity.** D returns the SendGrid error body verbatim
  (`errors[].field/message/help`); T collapsed per-status error schemas into one
  shape or left some ops fully untyped.

T's one genuine win — compile-time body typing — is inert here: the consumer is an
agent sending JSON at runtime, so correctness comes from runtime JSON-Schema
validation plus cross-field constraints (`constraints.toml`) either way.

## xtask codegen + drift

Specs are **vendored** under `specs/` with `specs.lock` pinning the upstream SHA.
Regeneration is an explicit developer/CI step, not a build step:

```sh
cargo xtask codegen   # parse specs + curated tables → generated/ir.json + schemas.json
```

Design choices (see `.research-notes/r6-regen-pipeline-testing.md`):

- **Commit the generated artifact.** The generated-code diff in a regen PR *is* the
  drift-review surface and the changelog — no hidden build.rs codegen, no network or
  generator deps leaking into consumer builds or `docs.rs`.
- **Idempotence gate.** Re-running `codegen` on unchanged specs must produce a
  byte-identical artifact; otherwise every drift diff is noise.
- **Semantic drift, not raw diff.** Upstream is itself machine-regenerated (cosmetic
  churn is expected), so drift detection canonicalizes specs and compares the
  **operation-set** — the meaningful change surface — rather than raw text.

> Status: `cargo xtask codegen` is implemented and is the regeneration entrypoint.
> The scheduled semantic-drift detection job is part of the regen-pipeline design
> (the changelog/idempotence gating described above); it lives outside this crate's
> runtime.

## The single `execute()` chokepoint

Both surfaces — the CLI's clap tree and the MCP server's `invoke_operation` — build
a uniform `{path, query, header, body}` envelope and call **one** function,
`sendgrid_core::execute()`. That function runs a fixed pipeline:

```
coerce → sanitize-headers → govern-OBO → validate → policy → bulk → region
       → build → [dry-run preview] → send (with retry) → [paginate if --all] → envelope
```

with **always-on secret redaction** applied to results and previews (field-level,
plus a final belt-and-suspenders scrub).

Consequences:

- **Security is enforced once.** The side-effect policy, header sanitization,
  governed impersonation, and redaction live here — there is no second code path to
  forget to harden. (See [safety.md](safety.md).)
- **Behavior is identical across surfaces.** A human running a CLI command and an
  agent calling an MCP tool hit the same validation, the same policy gate, the same
  pagination and retry logic, and get the same result envelope.
- **The transport is a seam.** `execute()` talks to an `OperationDispatcher` trait;
  the production impl (`ReqwestDispatcher`, "Backend D") uses a pooled `reqwest`
  client with **ring TLS + bundled webpki roots** and no auto-redirect. The seam is
  backend-blind (provider-neutral `http` types), which is what lets the test suite
  drive the real pipeline against a localhost mock.

## See also

- [README](../README.md) · [Safety model](safety.md) · [Known limitations](limitations.md)
