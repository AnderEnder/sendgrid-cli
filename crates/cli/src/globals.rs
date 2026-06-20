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
use clap::{Arg, ArgAction, ArgMatches, value_parser};
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
        Arg::new("output")
            .long("output")
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
        Arg::new("dry-run")
            .long("dry-run")
            .action(ArgAction::SetTrue)
            .help("Build and preview the request without sending it"),
    )
    .arg(
        Arg::new("all")
            .long("all")
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
        Arg::new("page-token")
            .long("page-token")
            .value_name("TOKEN")
            .help("Page token (injected into the query for page-token ops)"),
    )
    .arg(
        Arg::new("include-legacy")
            .long("include-legacy")
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
        Arg::new("api-key")
            .long("api-key")
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
    pub fn mcp_config(
        &self,
        expose_tags: Vec<String>,
        expose_ops: Vec<String>,
    ) -> anyhow::Result<McpServerConfig> {
        Ok(McpServerConfig {
            runtime: self.runtime_config()?,
            include_legacy: self.include_legacy,
            expose_tags,
            expose_ops,
        })
    }
}
