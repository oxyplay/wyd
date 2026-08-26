// ponytail: phase-2 seam; dead until wired into the collector (attribution
// pass + store persistence).
#![allow(dead_code)]

//! Deterministic ownership resolver for resources whose exact ancestry is
//! unavailable — re-parented, detached, or never directly observed.
//!
//! A session becomes a candidate only through an **anchor** (project and
//! timing alone never create ownership). Anchors score; capped evidence
//! families add support; hard contradictions reject. The result is a scored
//! candidate set plus a verdict, never a probability.

use crate::model::session::RuntimeSessionId;

use super::rules::{RESOLVER_VERSION, ResolverRules};

/// Why a session is a candidate owner. Never project/timing alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// Future vendor registration. Strongest.
    Explicit,
    /// Direct child of the session root.
    DirectChild,
    /// Observed descendant of the session root.
    Descendant,
    /// Ownership persisted from a previous observation.
    PersistedPrevious,
    /// Inherited through an already-attributed owned parent.
    Propagated,
}

/// A hard contradiction. Rejects the candidate — it never subtracts points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The resource started before the session; the session cannot have
    /// spawned it.
    PredatesSession,
    /// Observed ancestry reaches another agent session.
    ReachesOtherAgent,
}

/// Per-session evidence fed to the resolver for one resource.
#[derive(Debug, Clone)]
pub struct CandidateInput {
    pub session: RuntimeSessionId,
    pub anchor: Option<AnchorKind>,
    /// Score for `PersistedPrevious` / `Propagated` anchors.
    pub anchor_score: Option<u8>,
    /// Cap from owned-parent propagation: a downstream node can never exceed
    /// the score of the chain that established its ownership.
    pub propagate_cap: Option<u8>,

    // Project family
    pub exact_cwd: bool,
    pub same_git_root: bool,
    // Temporal family (seconds after the session started)
    pub start_delta_secs: Option<u64>,
    // Tool relationship family
    pub tool_relationship: bool,
    // Contradictions
    pub predates_session: bool,
    pub reaches_other_agent: bool,
}

/// One scored candidate for a resource, with its rejected reason if any.
#[derive(Debug, Clone)]
pub struct AttributionCandidate {
    pub session: RuntimeSessionId,
    pub anchor: AnchorKind,
    pub anchor_score: u8,
    pub project_support: u8,
    pub temporal_support: u8,
    pub relationship_support: u8,
    pub total: u8,
    pub rejected: Option<Rejection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Owned,
    Ambiguous,
    Unknown,
}

/// The full decision for one resource: verdict, winner, and the complete
/// candidate set (for reproducibility and `wyd why`).
#[derive(Debug, Clone)]
pub struct AttributionDecision {
    pub resolver_version: u32,
    pub verdict: Verdict,
    pub winner: Option<RuntimeSessionId>,
    pub candidates: Vec<AttributionCandidate>,
}

pub fn resolve(rules: &ResolverRules, inputs: Vec<CandidateInput>) -> AttributionDecision {
    let mut candidates: Vec<AttributionCandidate> = Vec::new();

    for input in inputs {
        // Project/timing alone is never a candidate (§7).
        let Some(anchor) = input.anchor else { continue };

        let anchor_score = match anchor {
            AnchorKind::Explicit => rules.explicit_anchor,
            AnchorKind::DirectChild => rules.direct_parent_anchor,
            AnchorKind::Descendant => rules.descendant_anchor,
            AnchorKind::PersistedPrevious | AnchorKind::Propagated => {
                input.anchor_score.unwrap_or(rules.descendant_anchor)
            }
        };

        let rejected = if input.predates_session {
            Some(Rejection::PredatesSession)
        } else if input.reaches_other_agent {
            Some(Rejection::ReachesOtherAgent)
        } else {
            None
        };

        let project_support = project_support(rules, &input);
        let temporal_support = temporal_support(rules, input.start_delta_secs);
        let relationship_support = if input.tool_relationship {
            rules.relationship_support
        } else {
            0
        };

        let total = anchor_score
            .saturating_add(project_support)
            .saturating_add(temporal_support)
            .saturating_add(relationship_support)
            .min(100);

        // Propagated ownership can never exceed its parent's score (§11).
        let total = input.propagate_cap.map_or(total, |cap| total.min(cap));

        candidates.push(AttributionCandidate {
            session: input.session,
            anchor,
            anchor_score,
            project_support,
            temporal_support,
            relationship_support,
            total,
            rejected,
        });
    }

    select(rules, candidates)
}

fn project_support(rules: &ResolverRules, input: &CandidateInput) -> u8 {
    if input.exact_cwd {
        rules.exact_cwd
    } else if input.same_git_root {
        rules.same_git_root
    } else {
        0
    }
    .min(rules.project_cap)
}

fn temporal_support(rules: &ResolverRules, delta: Option<u64>) -> u8 {
    let Some(d) = delta else { return 0 };
    let points = if d <= 10 {
        rules.temporal_10s
    } else if d <= 60 {
        rules.temporal_60s
    } else if d <= 5 * 60 {
        rules.temporal_5m
    } else {
        0
    };
    points.min(rules.temporal_cap)
}

fn select(rules: &ResolverRules, candidates: Vec<AttributionCandidate>) -> AttributionDecision {
    let mut eligible: Vec<&AttributionCandidate> =
        candidates.iter().filter(|c| c.rejected.is_none()).collect();
    eligible.sort_by(|a, b| b.total.cmp(&a.total));

    let verdict = match eligible.first() {
        None => Verdict::Unknown,
        Some(top) if top.total < rules.ownership_threshold => Verdict::Unknown,
        Some(top) => {
            let clear = eligible.len() == 1
                || top.total.saturating_sub(eligible[1].total) >= rules.ambiguity_margin;
            if clear {
                Verdict::Owned
            } else {
                Verdict::Ambiguous
            }
        }
    };

    let winner = match verdict {
        Verdict::Owned => eligible.first().map(|c| c.session),
        _ => None,
    };

    AttributionDecision {
        resolver_version: RESOLVER_VERSION,
        verdict,
        winner,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ResolverRules {
        ResolverRules::v1()
    }

    fn session(id: u64) -> RuntimeSessionId {
        RuntimeSessionId::from_u64(id)
    }

    fn base(sid: RuntimeSessionId) -> CandidateInput {
        CandidateInput {
            session: sid,
            anchor: Some(AnchorKind::Descendant),
            anchor_score: None,
            propagate_cap: None,
            exact_cwd: false,
            same_git_root: false,
            start_delta_secs: None,
            tool_relationship: false,
            predates_session: false,
            reaches_other_agent: false,
        }
    }

    // The worked example (§8): Claude → npm → Vite.
    #[test]
    fn worked_example_scores_95_and_owns() {
        let mut c = base(session(1));
        c.anchor = Some(AnchorKind::Descendant);
        c.exact_cwd = true; // exact cwd match
        c.start_delta_secs = Some(7); // started 7s after session
        c.tool_relationship = true; // agent → pkg runner → dev server

        let d = resolve(&rules(), vec![c]);
        assert_eq!(d.verdict, Verdict::Owned);
        assert_eq!(d.winner, Some(session(1)));
        assert_eq!(d.candidates[0].total, 95); // 75 + 10 + 5 + 5
    }

    // The §8 twist: a second session in the same repo, but no anchor.
    #[test]
    fn no_anchor_means_not_a_candidate() {
        let mut claude = base(session(1));
        claude.exact_cwd = true;
        claude.start_delta_secs = Some(7);
        claude.tool_relationship = true;

        // Codex has matching project/timing but NO anchor → not a candidate.
        let mut codex = base(session(2));
        codex.anchor = None;
        codex.exact_cwd = true;

        let d = resolve(&rules(), vec![claude, codex]);
        assert_eq!(d.verdict, Verdict::Owned);
        assert_eq!(d.winner, Some(session(1)));
        assert_eq!(d.candidates.len(), 1, "Codex without an anchor is absent");
        assert_eq!(d.candidates[0].total, 95);
    }

    #[test]
    fn close_candidates_are_ambiguous() {
        let mut a = base(session(1));
        a.exact_cwd = true;
        a.start_delta_secs = Some(7);
        let mut b = base(session(2));
        b.same_git_root = true;

        // Both anchored with 85 and 83 — margin 2 < 15 → ambiguous.
        let d = resolve(&rules(), vec![a, b]);
        assert_eq!(d.verdict, Verdict::Ambiguous);
        assert_eq!(d.winner, None);
    }

    #[test]
    fn weak_winner_is_never_selected() {
        // Anchor 75, no support → total 75 < threshold? 75 >= 75 threshold.
        // Single candidate at exactly threshold → owned. Use a low anchor.
        let mut c = base(session(1));
        c.anchor = Some(AnchorKind::Propagated);
        c.anchor_score = Some(40); // weak propagated anchor
        let d = resolve(&rules(), vec![c]);
        assert_eq!(
            d.verdict,
            Verdict::Unknown,
            "weak winner must not be selected"
        );
    }

    #[test]
    fn contradiction_rejects_the_candidate() {
        let mut c = base(session(1));
        c.exact_cwd = true;
        c.start_delta_secs = Some(7);
        c.predates_session = true; // resource started before the session
        let d = resolve(&rules(), vec![c]);
        assert_eq!(d.verdict, Verdict::Unknown);
        assert!(d.candidates[0].rejected.is_some());
    }

    #[test]
    fn propagation_never_exceeds_parent_score() {
        let mut c = base(session(1));
        c.exact_cwd = true;
        c.start_delta_secs = Some(7);
        c.tool_relationship = true;
        c.propagate_cap = Some(90); // parent ownership was 90
        let d = resolve(&rules(), vec![c]);
        assert_eq!(d.candidates[0].total, 90, "capped by parent, not 95");
        assert_eq!(d.verdict, Verdict::Owned);
    }

    #[test]
    fn project_cap_is_independent_of_temporal_and_relationship() {
        // exact_cwd + same_git_root both true → only one counts (cap 10).
        let mut c = base(session(1));
        c.exact_cwd = true;
        c.same_git_root = true;
        c.start_delta_secs = Some(3);
        c.tool_relationship = true;
        let d = resolve(&rules(), vec![c]);
        let cand = &d.candidates[0];
        assert_eq!(cand.project_support, 10, "family capped: not +18");
        assert_eq!(cand.total, 75 + 10 + 5 + 5);
    }
}
