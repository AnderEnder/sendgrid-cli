//! Static documentation embedded at compile time and served two ways: as MCP
//! **resources** (`resources/list` + `resources/read`) and via the model-controlled
//! **`read_doc`** tool. This is the "modern MCP skills" surface — a `using-the-server`
//! skill plus reference docs the agent pulls on demand, so the always-loaded
//! `instructions` string can stay lean.

use rmcp::model::{AnnotateAble, RawResource, ReadResourceResult, Resource, ResourceContents};

/// All embedded docs render as markdown.
const MIME: &str = "text/markdown";

/// One embedded document.
struct Doc {
    /// Stable resource URI (also the `read_doc` key).
    uri: &'static str,
    /// Short machine name (the MCP resource `name`).
    name: &'static str,
    /// Human-readable title.
    title: &'static str,
    /// One-line summary shown in `resources/list` and the `read_doc` index.
    description: &'static str,
    /// The markdown body.
    body: &'static str,
}

/// The doc catalog. The flagship skill first, then reference docs.
const DOCS: &[Doc] = &[
    Doc {
        uri: "sendgrid://skill/using-the-server",
        name: "using-the-server",
        title: "Skill: driving the SendGrid MCP server",
        description: "How to use search/describe/invoke well: searching, building calls, \
                      the result envelope, output shaping, non-retryable errors, async jobs.",
        body: include_str!("../docs/skill-using-the-server.md"),
    },
    Doc {
        uri: "sendgrid://reference/side-effects",
        name: "reference-side-effects",
        title: "Reference: the side-effect / safety model",
        description: "The read/write/destructive/send classes and how the policy gate, \
                      dry_run, bulk denial, secret redaction, and impersonation work.",
        body: include_str!("../docs/reference-side-effects.md"),
    },
    Doc {
        uri: "sendgrid://reference/regions",
        name: "reference-regions",
        title: "Reference: regions and EU fail-closed behavior",
        description: "The 14 global-only API groups and E_REGION_UNAVAILABLE fail-closed \
                      behavior on an EU-configured server.",
        body: include_str!("../docs/reference-regions.md"),
    },
    Doc {
        uri: "sendgrid://reference/async-jobs",
        name: "reference-async-jobs",
        title: "Reference: async / multi-step jobs",
        description: "Every poll / external_download / external_upload / fire_and_forget \
                      flow and how to drive it (await, download_urls, the RequestCsv caveat).",
        body: include_str!("../docs/reference-async-jobs.md"),
    },
];

/// Every doc as an MCP resource (for `resources/list`).
pub fn list() -> Vec<Resource> {
    DOCS.iter()
        .map(|d| {
            RawResource::new(d.uri, d.name)
                .with_title(d.title)
                .with_description(d.description)
                .with_mime_type(MIME)
                .no_annotation()
        })
        .collect()
}

/// Read one doc by URI (for `resources/read`). `None` if the URI is unknown.
pub fn read(uri: &str) -> Option<ReadResourceResult> {
    DOCS.iter().find(|d| d.uri == uri).map(|d| {
        ReadResourceResult::new(vec![
            ResourceContents::text(d.body, d.uri).with_mime_type(MIME),
        ])
    })
}

/// Body of the `read_doc` tool. With a `uri`, returns that doc's markdown; without one,
/// returns a readable index so the agent can pick. Unknown URI → an error naming the
/// valid URIs.
pub fn read_doc(uri: Option<&str>) -> Result<String, String> {
    match uri.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            let mut out =
                String::from("SendGrid MCP docs — call read_doc { uri } with one of:\n\n");
            for d in DOCS {
                out.push_str(&format!("- {}\n  {} — {}\n", d.uri, d.title, d.description));
            }
            Ok(out)
        }
        Some(u) => DOCS
            .iter()
            .find(|d| d.uri == u)
            .map(|d| d.body.to_string())
            .ok_or_else(|| {
                let uris: Vec<&str> = DOCS.iter().map(|d| d.uri).collect();
                format!("unknown doc uri `{u}`. Available: {}", uris.join(", "))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_is_first_and_uris_unique() {
        assert_eq!(DOCS[0].uri, "sendgrid://skill/using-the-server");
        let mut uris: Vec<&str> = DOCS.iter().map(|d| d.uri).collect();
        let n = uris.len();
        uris.sort_unstable();
        uris.dedup();
        assert_eq!(uris.len(), n, "duplicate doc uri");
    }

    #[test]
    fn list_exposes_every_doc_as_markdown_resource() {
        let res = list();
        assert_eq!(res.len(), DOCS.len());
        for r in &res {
            assert_eq!(r.mime_type.as_deref(), Some(MIME));
            assert!(r.title.is_some());
        }
    }

    #[test]
    fn read_known_returns_body_unknown_returns_none() {
        let ok = read("sendgrid://skill/using-the-server").expect("known uri");
        assert_eq!(ok.contents.len(), 1);
        assert!(read("sendgrid://nope").is_none());
    }

    #[test]
    fn read_doc_lists_then_reads_then_errors() {
        // No uri → an index naming every doc.
        let index = read_doc(None).unwrap();
        for d in DOCS {
            assert!(index.contains(d.uri), "index missing {}", d.uri);
        }
        // A known uri → that doc's body.
        let body = read_doc(Some("sendgrid://reference/regions")).unwrap();
        assert!(body.contains("global-only"));
        // An unknown uri → an error listing the valid ones.
        let err = read_doc(Some("sendgrid://bogus")).unwrap_err();
        assert!(err.contains("sendgrid://skill/using-the-server"));
    }

    // The reference docs restate two SPEC-DERIVED facts (the async-job op set and the
    // global-only API groups) as prose. The repo runs drift CI so spec-derived facts
    // can't silently rot — so these tests pin the docs to the embedded IR: a spec
    // re-gen that adds/renames an async op or a global-only group fails the build until
    // the doc is updated.

    #[test]
    fn async_jobs_doc_mentions_every_async_op() {
        use sendgrid_core::Registry;
        use sendgrid_core::ir::AsyncJob;
        let doc = include_str!("../docs/reference-async-jobs.md");
        for op in Registry::global().operations() {
            if op.async_job != AsyncJob::None {
                assert!(
                    doc.contains(&op.operation_id),
                    "reference-async-jobs.md does not mention async op `{}` ({})",
                    op.operation_id,
                    op.id,
                );
            }
        }
    }

    #[test]
    fn regions_doc_lists_every_global_only_group() {
        use sendgrid_core::Registry;
        use std::collections::BTreeSet;
        let doc = include_str!("../docs/reference-regions.md");
        let global_only: BTreeSet<&str> = Registry::global()
            .operations()
            .iter()
            .filter(|o| o.region_global_only)
            .map(|o| o.namespace.as_str())
            .collect();
        for ns in &global_only {
            assert!(
                doc.contains(*ns),
                "reference-regions.md is missing global-only API group `{ns}`",
            );
        }
        // Keep the doc's stated count honest, too.
        assert!(
            doc.contains(&global_only.len().to_string()),
            "reference-regions.md count is stale: the IR has {} global-only groups",
            global_only.len(),
        );
    }
}
