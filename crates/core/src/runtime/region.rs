//! Region / base-URL selection with **fail-closed** EU data-residency (r4 §2).

use crate::ir::OperationIr;

/// SendGrid data region. There are exactly two servers, no templating (r4 F4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Region {
    /// `https://api.sendgrid.com`
    #[default]
    Global,
    /// `https://api.eu.sendgrid.com`
    Eu,
}

impl Region {
    /// The base URL for this region (no trailing slash).
    pub fn base_url(self) -> &'static str {
        match self {
            Region::Global => "https://api.sendgrid.com",
            Region::Eu => "https://api.eu.sendgrid.com",
        }
    }

    /// Parse from a config/flag string (`global` | `eu`, case-insensitive).
    pub fn parse(s: &str) -> Option<Region> {
        match s.trim().to_ascii_lowercase().as_str() {
            "global" | "us" => Some(Region::Global),
            "eu" => Some(Region::Eu),
            _ => None,
        }
    }
}

/// Outcome of resolving the base URL for an operation in a region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionDecision {
    /// Use this base URL.
    Route { base_url: String },
    /// Use this base URL, but warn (residency not guaranteed).
    RouteWithFallbackWarning { base_url: String, warning: String },
    /// Refuse: routing would move data out of the EU region.
    Unavailable { message: String },
}

/// Decide the base URL for `op` in `region` (r4 §2).
///
/// - `region == Global` → always global.
/// - `region == Eu`, op has an EU endpoint → EU.
/// - `region == Eu`, op is `region_global_only` → **fail closed** unless
///   `allow_region_fallback`, in which case route to global with a warning.
/// - `base_url_override` (e.g. a proxy/test host) overrides everything.
pub fn resolve_base_url(
    op: &OperationIr,
    region: Region,
    allow_region_fallback: bool,
    base_url_override: Option<&str>,
) -> RegionDecision {
    if let Some(base) = base_url_override {
        return RegionDecision::Route {
            base_url: base.trim_end_matches('/').to_string(),
        };
    }
    match region {
        Region::Global => RegionDecision::Route {
            base_url: Region::Global.base_url().to_string(),
        },
        Region::Eu if !op.region_global_only => RegionDecision::Route {
            base_url: Region::Eu.base_url().to_string(),
        },
        // EU requested for a global-only op.
        Region::Eu if allow_region_fallback => RegionDecision::RouteWithFallbackWarning {
            base_url: Region::Global.base_url().to_string(),
            warning: format!(
                "region fallback to global for `{}` — EU data residency not guaranteed",
                op.id
            ),
        },
        Region::Eu => RegionDecision::Unavailable {
            message: format!(
                "operation `{}` has no EU endpoint; routing to global would move data out of the \
                 EU region. Re-run with region=global or set allow_region_fallback to override.",
                op.id
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Registry;

    #[test]
    fn parse_and_base_urls() {
        assert_eq!(Region::parse("EU"), Some(Region::Eu));
        assert_eq!(Region::parse("global"), Some(Region::Global));
        assert_eq!(Region::parse("us"), Some(Region::Global));
        assert_eq!(Region::parse("mars"), None);
        assert_eq!(Region::Eu.base_url(), "https://api.eu.sendgrid.com");
    }

    #[test]
    fn eu_global_only_fails_closed() {
        let r = Registry::global();
        let op = r
            .operations()
            .iter()
            .find(|o| o.region_global_only)
            .expect("a global-only op exists");
        match resolve_base_url(op, Region::Eu, false, None) {
            RegionDecision::Unavailable { .. } => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
        // With the override flag it routes to global, but warns.
        match resolve_base_url(op, Region::Eu, true, None) {
            RegionDecision::RouteWithFallbackWarning { base_url, .. } => {
                assert_eq!(base_url, "https://api.sendgrid.com");
            }
            other => panic!("expected fallback warning, got {other:?}"),
        }
    }

    #[test]
    fn eu_regional_op_routes_eu() {
        let r = Registry::global();
        let op = r
            .operations()
            .iter()
            .find(|o| !o.region_global_only)
            .expect("a regional op exists");
        assert_eq!(
            resolve_base_url(op, Region::Eu, false, None),
            RegionDecision::Route {
                base_url: "https://api.eu.sendgrid.com".into()
            }
        );
    }
}
