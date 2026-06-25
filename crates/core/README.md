# sendgrid-core

Typed, validated Rust client for the **SendGrid v3 API** — all **391 operations**
across 46 API groups, generated from the official
[`twilio/sendgrid-oai`](https://github.com/twilio/sendgrid-oai) OpenAPI specs.

This is the runtime engine behind the [`sendgrid-cli`](https://crates.io/crates/sendgrid-cli)
agent CLI and the [`sendgrid-mcp`](https://crates.io/crates/sendgrid-mcp) MCP server.
It provides:

- an **operation registry** ([`Registry`]) embedded at compile time from the codegen artifact;
- a **request builder** with JSON-Schema (2020-12) body validation;
- a **safety / side-effect model** (read / write / destructive / send) and policy gate;
- **secret redaction**, **retry/backoff**, **pagination**, and **async-job** helpers;
- a single [`execute`] dispatch chokepoint over `reqwest` (pure-Rust rustls/ring TLS).

## Usage

```toml
[dependencies]
sendgrid-core = "0.1"
```

```rust
use sendgrid_core::Registry;

let reg = Registry::global();
println!("{} SendGrid operations available", reg.operations().len());

// Look up an operation by its stable id.
let op = reg.by_id("sg_mail_send_mail").expect("operation exists");
println!("{} {}", op.method, op.id);
```

The async dispatch path (`execute` / `execute_with`, `RuntimeConfig`, `ApiKey`,
`ReqwestDispatcher`) is documented on [docs.rs](https://docs.rs/sendgrid-core).
Consumers own the async runtime; `core` is runtime-agnostic.

## License

Licensed under either of [Apache License, Version 2.0](../../LICENSE-APACHE) or
[MIT license](../../LICENSE-MIT) at your option.

> Unofficial. "SendGrid" is a trademark of Twilio Inc.; this project is not
> affiliated with or endorsed by Twilio.
