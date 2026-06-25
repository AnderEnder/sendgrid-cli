# sendgrid-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) (MCP) server that
exposes **all 391 SendGrid v3 API operations** to AI agents through a small,
self-describing meta-tool surface — so an agent works the entire API without
391 individual tool definitions bloating its context.

Built on [`rmcp`](https://crates.io/crates/rmcp) (stdio transport) over the
[`sendgrid-core`](https://crates.io/crates/sendgrid-core) runtime.

## The surface

- **Tools** — `search_operations` → `describe_operation` → `invoke_operation`
  (plus `read_doc`), with tool **annotations** (read-only / destructive hints)
  and **structured output**.
- **Resources** — an on-demand "skill" (`sendgrid://skill/using-the-server`) and
  reference docs (side-effects, regions, async jobs) the agent pulls when it needs them.
- **Prompts** — `find_operation` and `safe_invoke` workflow templates.

## Running it

The server ships inside the [`sendgrid-cli`](https://crates.io/crates/sendgrid-cli) binary:

```sh
cargo install sendgrid-cli
sendgrid mcp          # speak MCP over stdio
```

Or embed the handler in your own binary as a library dependency:

```toml
[dependencies]
sendgrid-mcp = "0.1"
```

See [docs.rs](https://docs.rs/sendgrid-mcp) for the handler API.

## License

Licensed under either of [Apache License, Version 2.0](../../LICENSE-APACHE) or
[MIT license](../../LICENSE-MIT) at your option.

> Unofficial. "SendGrid" is a trademark of Twilio Inc.; this project is not
> affiliated with or endorsed by Twilio.
