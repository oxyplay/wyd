// ponytail: phase-2 seam; dead until wired into the collector.
#![allow(dead_code)]

//! Deterministic resolver rule table (v1).
//!
//! These are deterministic v1 heuristic weights, **not** statistically
//! calibrated probabilities. Fixtures freeze v1 behavior; changing any number
//! here is a deliberate change to the resolver contract, so bump
//! `RESOLVER_VERSION` and re-run the fixtures.

pub const RESOLVER_VERSION: u32 = 1;

/// The whole v1 rule table in one place — no magic numbers in scattered code.
#[derive(Debug, Clone, Copy)]
pub struct ResolverRules {
    pub descendant_anchor: u8,
    pub direct_parent_anchor: u8,
    pub explicit_anchor: u8,

    pub exact_cwd: u8,
    pub same_git_root: u8,
    pub project_cap: u8,

    pub temporal_10s: u8,
    pub temporal_60s: u8,
    pub temporal_5m: u8,
    pub temporal_cap: u8,

    pub relationship_support: u8,
    pub relationship_cap: u8,

    pub ownership_threshold: u8,
    pub ambiguity_margin: u8,
}

impl ResolverRules {
    pub const fn v1() -> Self {
        Self {
            descendant_anchor: 75,
            direct_parent_anchor: 85,
            explicit_anchor: 100,

            exact_cwd: 10,
            same_git_root: 8,
            project_cap: 10,

            temporal_10s: 5,
            temporal_60s: 3,
            temporal_5m: 1,
            temporal_cap: 5,

            relationship_support: 5,
            relationship_cap: 5,

            ownership_threshold: 75,
            ambiguity_margin: 15,
        }
    }
}
