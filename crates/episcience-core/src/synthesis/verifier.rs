//! Verifier types used by Stage 6 (Phase 4).
//!
//! A [`SynthesisSkill`] runs its [`verify`] method against a generated
//! narrative + cluster context and returns a [`VerificationOutcome`].
//! The pipeline routes Accept → `status = 'complete'`; Reject → either
//! refinement (Task 7.1) or `status = 'rejected'`.
//!
//! [`SynthesisSkill`]: crate::synthesis::skill::SynthesisSkill
//! [`verify`]: crate::synthesis::skill::SynthesisSkill::verify

use uuid::Uuid;

/// The outcome of running a skill's verifier rubric. Persisted on the
/// row (Task 4.2 schema) so post-hoc inspection can see *why* a synthesis
/// was rejected.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationOutcome {
    Accept {
        /// The rubric name that produced this outcome (e.g. "default_citation").
        rubric: String,
        /// Free-form structured evidence (e.g. `{"cited_count": 7}`).
        evidence: serde_json::Value,
    },
    Reject {
        rubric: String,
        reason: VerificationReason,
        evidence: serde_json::Value,
    },
}

/// Why a verifier rejected the narrative. Each variant carries enough
/// identifying detail to point at the offending claim or cluster.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationReason {
    /// A claim in the cluster was not cited anywhere in the narrative.
    UncitedMember { claim_id: Uuid },
    /// A citation referred to a claim outside the cluster.
    HallucinatedCitation { claim_id: Uuid },
    /// The narrative contradicts a kernel claim it should respect.
    /// (Reserved for future use — the default rubric does not check this.)
    KernelContradiction { claim_id: Uuid },
    /// Skill-specific veto with free-form detail.
    SkillRejection { detail: String },
}

/// Inputs to a verification pass.
#[derive(Debug)]
pub struct VerificationContext<'a> {
    pub synthesis_id: Uuid,
    pub query: &'a str,
    pub narrative: &'a str,
    pub cluster_member_ids: &'a [Uuid],
}

/// The default verifier rubric: every cluster member must appear as a
/// citation `[<claim_id>]` in the narrative, and no citation may refer
/// to a claim outside the cluster.
///
/// This is the rubric `BaselineSkill::verify` delegates to (via the
/// trait default). Skills with stricter checks override.
pub fn default_citation_rubric(ctx: &VerificationContext<'_>) -> VerificationOutcome {
    let cite_re = regex::Regex::new(r"\[([0-9a-f-]{36})\]").expect("static regex");
    let cited: std::collections::HashSet<Uuid> = cite_re
        .captures_iter(ctx.narrative)
        .filter_map(|c| c[1].parse().ok())
        .collect();

    // 1. Every member must be cited at least once.
    for m in ctx.cluster_member_ids {
        if !cited.contains(m) {
            return VerificationOutcome::Reject {
                rubric: "default_citation".into(),
                reason: VerificationReason::UncitedMember { claim_id: *m },
                evidence: serde_json::json!({
                    "cited": cited.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                }),
            };
        }
    }

    // 2. No citation may refer to a claim outside the cluster.
    let members: std::collections::HashSet<Uuid> = ctx.cluster_member_ids.iter().copied().collect();
    for c in &cited {
        if !members.contains(c) {
            return VerificationOutcome::Reject {
                rubric: "default_citation".into(),
                reason: VerificationReason::HallucinatedCitation { claim_id: *c },
                evidence: serde_json::json!({
                    "members": ctx.cluster_member_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                }),
            };
        }
    }

    VerificationOutcome::Accept {
        rubric: "default_citation".into(),
        evidence: serde_json::json!({ "cited_count": cited.len() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_when_every_member_is_cited_and_no_hallucinations() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let narrative = format!("Saw [{a}] and [{b}].");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a, b],
        };
        match default_citation_rubric(&ctx) {
            VerificationOutcome::Accept { rubric, evidence } => {
                assert_eq!(rubric, "default_citation");
                assert_eq!(evidence["cited_count"], 2);
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn rejects_when_member_is_uncited() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let narrative = format!("Only saw [{a}].");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a, b],
        };
        match default_citation_rubric(&ctx) {
            VerificationOutcome::Reject {
                rubric,
                reason: VerificationReason::UncitedMember { claim_id },
                ..
            } => {
                assert_eq!(rubric, "default_citation");
                assert_eq!(claim_id, b);
            }
            other => panic!("expected UncitedMember reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_when_citation_is_hallucinated() {
        let a = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let narrative = format!("Saw [{a}] and [{intruder}].");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a],
        };
        match default_citation_rubric(&ctx) {
            VerificationOutcome::Reject {
                rubric,
                reason: VerificationReason::HallucinatedCitation { claim_id },
                ..
            } => {
                assert_eq!(rubric, "default_citation");
                assert_eq!(claim_id, intruder);
            }
            other => panic!("expected HallucinatedCitation reject, got {other:?}"),
        }
    }
}
