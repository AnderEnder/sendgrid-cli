//! Top-level (root) global flags and the [`RuntimeConfig`]/[`McpServerConfig`]
//! they build.
//!
//! Global flags are defined on the **root** command only (not `global(true)`):
//! several operations declare params whose names collide with global flag longs
//! (`limit`, `offset`, `query`, `region` as query params; `on-behalf-of` as a
//! header). A `global(true)` arg would be an arg-id collision (clap build-time
//! panic) and would shadow the leaf param. Keeping globals root-only means they
//! are parsed *before* the subcommand and never clash with a leaf's own flags.

use anyhow::{Context, bail};
use clap::{Arg, ArgAction, ArgMatches, parser::ValueSource, value_parser};
use sendgrid_core::ir::SideEffect;
use sendgrid_core::runtime::auth::resolve_api_key;
use sendgrid_core::{ApiKey, Policy, Region, RuntimeConfig};
use sendgrid_mcp::McpServerConfig;

/// Output rendering format for the success `data` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// The full `ExecuteResult` envelope as JSON (pretty on a TTY, else compact).
    Json,
    /// A column grid over `data`.
    Table,
    /// RFC-4180 CSV over `data`.
    Csv,
    /// One JSON value per line over `data` (streams elements under `--all`).
    Ndjson,
}

impl OutputFormat {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(OutputFormat::Json),
            "table" => Some(OutputFormat::Table),
            "csv" => Some(OutputFormat::Csv),
            "ndjson" => Some(OutputFormat::Ndjson),
            _ => None,
        }
    }
}

/// Parsed root-level global flags.
#[derive(Debug, Clone)]
pub struct GlobalOpts {
    pub region: Region,
    pub output: OutputFormat,
    pub query: Option<String>,
    pub dry_run: bool,
    pub all: bool,
    pub limit: Option<usize>,
    pub offset: Option<String>,
    pub page_token: Option<String>,
    pub include_legacy: bool,
    pub allow: Option<String>,
    /// Whether `--allow` was passed explicitly on the command line (vs. absent).
    /// Drives the `mcp` subcommand's read-only default: an unsupervised server with
    /// no explicit policy locks down to Read, while direct CLI use stays allow-all.
    pub allow_explicit: bool,
    pub allow_bulk: bool,
    pub on_behalf_of: Option<String>,
    pub api_key: Option<String>,
}

/// Attach every root-level global flag to `cmd`.
pub fn with_global_flags(cmd: clap::Command) -> clap::Command {
    cmd.arg(
        Arg::new("region")
            .long("region")
            .value_name("global|eu")
            .value_parser(["global", "eu", "us"])
            .help("Data region to route to (default: global)"),
    )
    .arg(
        // `global(true)`: accepted before OR after the subcommand. Safe because no API
        // operation declares an `output` param, so there is no leaf-flag collision.
        Arg::new("output")
            .long("output")
            .global(true)
            .value_name("json|table|csv|ndjson")
            .value_parser(["json", "table", "csv", "ndjson"])
            .default_value("json")
            .help("Output format for the response data"),
    )
    .arg(
        Arg::new("query")
            .long("query")
            .value_name("PATH")
            .help("jq-lite field selector over `data` (e.g. result[].id)"),
    )
    .arg(
        // `global(true)`: agents naturally place --dry-run next to the operation (after
        // the subcommand). Globalizing it accepts both positions. No API op declares a
        // `dry-run` param, so there is no leaf-flag collision.
        Arg::new("dry-run")
            .long("dry-run")
            .global(true)
            .action(ArgAction::SetTrue)
            .help("Build and preview the request without sending it (accepted anywhere)"),
    )
    .arg(
        // `global(true)`: no API op declares an `all` param — safe to accept anywhere.
        Arg::new("all")
            .long("all")
            .global(true)
            .action(ArgAction::SetTrue)
            .help("Auto-paginate: follow pages up to --limit / page caps"),
    )
    .arg(
        Arg::new("limit")
            .long("limit")
            .value_name("N")
            .value_parser(value_parser!(usize))
            .help("Under --all, cap the total number of accumulated items"),
    )
    .arg(
        Arg::new("offset")
            .long("offset")
            .value_name("N")
            .help("Starting offset (injected into the query for offset-paginated ops)"),
    )
    .arg(
        // `global(true)`: the long-name is `page-token` (hyphen) while the API param is
        // `page_token` (underscore), so there is no leaf-flag collision.
        Arg::new("page-token")
            .long("page-token")
            .global(true)
            .value_name("TOKEN")
            .help("Page token (injected into the query for page-token ops)"),
    )
    .arg(
        // `global(true)`: long-name `include-legacy` (hyphen); no API param matches.
        Arg::new("include-legacy")
            .long("include-legacy")
            .global(true)
            .action(ArgAction::SetTrue)
            .help("Expose hidden/legacy operations in the tree and search"),
    )
    .arg(
        Arg::new("allow")
            .long("allow")
            .value_name("CLASSES")
            .help("Comma-list of allowed side-effect classes: read,write,destructive,send,bulk (default: all)"),
    )
    .arg(
        Arg::new("allow-bulk")
            .long("allow-bulk")
            .action(ArgAction::SetTrue)
            .help("Permit operations whose bulk triggers fire"),
    )
    .arg(
        Arg::new("on-behalf-of")
            .long("on-behalf-of")
            .value_name("SUBUSER")
            .help("Governed impersonation subuser (authorized by being passed here)"),
    )
    .arg(
        // `global(true)`: long-name `api-key` (hyphen); no API param matches.
        Arg::new("api-key")
            .long("api-key")
            .global(true)
            .value_name("KEY")
            .help("API key (discouraged — prefer the SENDGRID_API_KEY env var)"),
    )
}

impl GlobalOpts {
    /// Extract the global flags from the **root** `ArgMatches`.
    pub fn from_matches(m: &ArgMatches) -> anyhow::Result<Self> {
        let region = match m.get_one::<String>("region") {
            Some(s) => Region::parse(s).with_context(|| format!("invalid --region `{s}`"))?,
            None => Region::Global,
        };
        let output = m
            .get_one::<String>("output")
            .and_then(|s| OutputFormat::parse(s))
            .unwrap_or(OutputFormat::Json);
        Ok(GlobalOpts {
            region,
            output,
            query: m.get_one::<String>("query").cloned(),
            dry_run: m.get_flag("dry-run"),
            all: m.get_flag("all"),
            limit: m.get_one::<usize>("limit").copied(),
            offset: m.get_one::<String>("offset").cloned(),
            page_token: m.get_one::<String>("page-token").cloned(),
            include_legacy: m.get_flag("include-legacy"),
            allow: m.get_one::<String>("allow").cloned(),
            // CommandLine (not the absent `None` or a future default/env) means the
            // operator made an explicit policy choice — honored as-is everywhere.
            allow_explicit: matches!(m.value_source("allow"), Some(ValueSource::CommandLine)),
            allow_bulk: m.get_flag("allow-bulk"),
            on_behalf_of: m.get_one::<String>("on-behalf-of").cloned(),
            api_key: m.get_one::<String>("api-key").cloned(),
        })
    }

    /// Resolve `(Policy, allow_bulk)` from `--allow` / `--allow-bulk`.
    ///
    /// No `--allow` ⇒ `Policy::all()` (the project default). A `bulk` token in
    /// `--allow` also flips `allow_bulk` (there is no `SideEffect::Bulk`).
    fn policy(&self) -> anyhow::Result<(Policy, bool)> {
        let Some(raw) = self.allow.as_deref() else {
            return Ok((Policy::all(), self.allow_bulk));
        };
        let mut classes = Vec::new();
        let mut allow_bulk = self.allow_bulk;
        for tok in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match tok.to_ascii_lowercase().as_str() {
                "read" => classes.push(SideEffect::Read),
                "write" => classes.push(SideEffect::Write),
                "destructive" => classes.push(SideEffect::Destructive),
                "send" => classes.push(SideEffect::Send),
                "bulk" => allow_bulk = true,
                other => bail!(
                    "unknown --allow class `{other}` (expected: read,write,destructive,send,bulk)"
                ),
            }
        }
        Ok((Policy::from_classes(classes), allow_bulk))
    }

    /// Resolve the API key (explicit `--api-key` beats `SENDGRID_API_KEY`).
    /// Emits a discouragement warning when `--api-key` is used.
    fn api_key(&self) -> anyhow::Result<ApiKey> {
        if self.api_key.is_some() {
            eprintln!(
                "warning: --api-key is discouraged (it can leak via shell history / process \
                 listings); prefer the SENDGRID_API_KEY environment variable"
            );
        }
        resolve_api_key(self.api_key.clone()).map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    /// Build the [`RuntimeConfig`] for an operation call.
    pub fn runtime_config(&self) -> anyhow::Result<RuntimeConfig> {
        let (policy, allow_bulk) = self.policy()?;
        let mut cfg = RuntimeConfig::new(self.api_key()?);
        cfg.region = self.region;
        cfg.policy = policy;
        cfg.allow_bulk = allow_bulk;
        cfg.dry_run = self.dry_run;
        cfg.paginate_all = self.all;
        if let Some(limit) = self.limit {
            cfg.max_items = limit;
        }
        // An explicit --on-behalf-of is itself the operator's authorization, so we
        // add it to the governed allow-list the runtime validates against.
        if let Some(obo) = self.on_behalf_of.clone() {
            cfg.allowed_subusers = vec![obo.clone()];
            cfg.on_behalf_of = Some(obo);
        }
        Ok(cfg)
    }

    /// Build the [`McpServerConfig`] for `sendgrid mcp`.
    ///
    /// The MCP server is the **unsupervised** surface, so it defaults to READ-ONLY:
    /// with no explicit `--allow`, the policy locks down to `{Read}` (overriding the
    /// allow-all default that `runtime_config` hands direct CLI use). An explicit
    /// `--allow` is honored verbatim (with F1's implied-Read). This is the *only*
    /// place the default flips — direct op invocation stays allow-all.
    pub fn mcp_config(
        &self,
        expose_tags: Vec<String>,
        expose_ops: Vec<String>,
    ) -> anyhow::Result<McpServerConfig> {
        let mut runtime = self.runtime_config()?;
        if !self.allow_explicit {
            runtime.policy = Policy::read_only();
        }
        Ok(McpServerConfig {
            runtime,
            include_legacy: self.include_legacy,
            expose_tags,
            expose_ops,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree;
    use sendgrid_core::ir::SideEffect;

    /// A syntactically valid dummy key so `runtime_config`/`mcp_config` resolve a
    /// key (passed via `--api-key` so no env var / ordering is relied on).
    const DUMMY_KEY: &str =
        "SG.0123456789abcdefghABCD.0123456789abcdefghABCDEFGHIJKLMNOPqrstuvwxyz123";

    /// Parse argv through the real command tree and extract the root globals — the
    /// only path that populates clap's `value_source` for `--allow`.
    fn globals_from(argv: &[&str]) -> GlobalOpts {
        let (command, _resolve) = tree::build(false);
        let matches = command.try_get_matches_from(argv).expect("argv parses");
        GlobalOpts::from_matches(&matches).expect("globals")
    }

    #[test]
    fn mcp_defaults_to_read_only_without_allow() {
        // The unsupervised MCP surface locks down to {Read} when no policy is given.
        let g = globals_from(&["sendgrid", "--api-key", DUMMY_KEY, "mcp"]);
        assert!(!g.allow_explicit, "no --allow was passed");
        let cfg = g.mcp_config(vec![], vec![]).expect("mcp config");
        let p = cfg.runtime.policy;
        assert!(p.allows(SideEffect::Read), "read is allowed");
        assert!(
            !p.allows(SideEffect::Write),
            "write denied under read-only default"
        );
        assert!(!p.allows(SideEffect::Destructive), "destructive denied");
        assert!(!p.allows(SideEffect::Send), "send denied");
    }

    #[test]
    fn mcp_honors_explicit_allow() {
        let g = globals_from(&["sendgrid", "--api-key", DUMMY_KEY, "--allow", "send", "mcp"]);
        assert!(g.allow_explicit, "--allow was passed");
        let cfg = g.mcp_config(vec![], vec![]).expect("mcp config");
        let p = cfg.runtime.policy;
        // F1 contract: `from_classes` implies Read, so `--allow send` → {Read, Send}.
        assert!(p.allows(SideEffect::Read), "read is implied");
        assert!(p.allows(SideEffect::Send), "send is allowed");
        assert!(!p.allows(SideEffect::Write), "write not granted");
        assert!(
            !p.allows(SideEffect::Destructive),
            "destructive not granted"
        );
    }

    #[test]
    fn direct_cli_op_stays_allow_all_without_allow() {
        // The read-only default is MCP-only: a plain op with no --allow keeps all
        // four classes (the supervised CLI's allow-all default is unchanged).
        let g = globals_from(&[
            "sendgrid",
            "--api-key",
            DUMMY_KEY,
            "mail",
            "send",
            "send-mail",
            "--body",
            "{}",
        ]);
        assert!(!g.allow_explicit);
        let cfg = g.runtime_config().expect("runtime config");
        let p = cfg.policy;
        for e in [
            SideEffect::Read,
            SideEffect::Write,
            SideEffect::Destructive,
            SideEffect::Send,
        ] {
            assert!(
                p.allows(e),
                "{e:?} should be allowed under the allow-all default"
            );
        }
    }
}
