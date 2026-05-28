//! `CodeReviewSkill` — synthesis tuned for the nightly-bug-fix pipeline.
//!
//! Output is a PR-body-shaped Markdown narrative. Verifier inherits
//! the default citation discipline AND adds a check: every PR number
//! (#NNNN) mentioned in the narrative must appear within 120 chars of
//! a `[<claim_id>]` citation. Strictness is appropriate here because
//! PR-body narratives can become merge gates (Phase 8).

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};
use crate::synthesis::verifier::{
    default_citation_rubric, VerificationContext, VerificationOutcome, VerificationReason,
};

#[derive(Debug, Default)]
pub struct CodeReviewSkill;

#[async_trait::async_trait]
impl SynthesisSkill for CodeReviewSkill {
    fn name(&self) -> &'static str {
        "code_review"
    }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview => {
                "Summarise a code-change run: which files changed, what \
                 invariants were tested, which PRs opened."
            }
            SynthesisStage::Narration => {
                "For each cluster, write a PR-body-shaped 3-5 sentence \
                 summary. Cite every claim with `[<claim_id>]`. Cite PRs \
                 as `#<number>` and commits as `` `<sha>` `` (7-char \
                 abbreviation acceptable). Do not invent any."
            }
            SynthesisStage::Composition => {
                "Compose the per-cluster summaries into a Markdown \
                 narrative organised as `## Summary` / `## Files changed` \
                 / `## Test plan` (standard PR shape). Keep the \
                 `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim."
            }
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        Some(TraversalConfig {
            max_hops: 2,
            edge_types: vec![EdgeType::Supports, EdgeType::Methodology],
            relevance_prune: 0.6,
            ..TraversalConfig::default()
        })
    }

    async fn verify(&self, ctx: &VerificationContext<'_>) -> VerificationOutcome {
        // Run the default citation rubric first.
        let baseline = default_citation_rubric(ctx);
        if let VerificationOutcome::Reject { .. } = baseline {
            return baseline;
        }
        // Additional check: every #NNNN in the narrative must have a
        // `[<claim_id>]` citation within ~120 chars on either side.
        // The DB-side check that the cited claim actually carries a
        // pr_number property belongs in Phase 8 (review bot); the
        // verifier only has the narrative + member ids.
        let pr_re = regex::Regex::new(r"#(\d{1,6})\b").expect("static");
        for caps in pr_re.captures_iter(ctx.narrative) {
            let pr_num = &caps[1];
            let pos = caps.get(0).unwrap().start();
            let window_start = pos.saturating_sub(120);
            let window_end = (pos + 120).min(ctx.narrative.len());
            let window = &ctx.narrative[window_start..window_end];
            let has_citation = window.contains('[') && window.contains(']');
            if !has_citation {
                return VerificationOutcome::Reject {
                    rubric: "code_review_pr_citation".into(),
                    reason: VerificationReason::SkillRejection {
                        detail: format!(
                            "PR #{pr_num} mentioned without a nearby `[<claim_id>]` citation"
                        ),
                    },
                    evidence: serde_json::json!({
                        "pr_number": pr_num,
                        "window_chars": 120,
                    }),
                };
            }
        }
        baseline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn code_review_overrides_three_stages_and_traversal() {
        let s = CodeReviewSkill;
        assert_eq!(s.name(), "code_review");
        let narration = s.section(SynthesisStage::Narration).unwrap();
        assert!(narration.contains("PR"));
        assert!(narration.contains("#<number>"));
        let composition = s.section(SynthesisStage::Composition).unwrap();
        assert!(composition.contains("Summary"));
        assert!(composition.contains("Files changed"));
        let cfg = s.traversal_config().unwrap();
        assert_eq!(cfg.max_hops, 2);
        assert_eq!(cfg.edge_types.len(), 2);
    }

    #[tokio::test]
    async fn code_review_verifier_accepts_pr_with_nearby_citation() {
        let s = CodeReviewSkill;
        let a = Uuid::new_v4();
        let narrative = format!("Fixed bug in [{a}] — opened PR #1234.");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a],
        };
        match s.verify(&ctx).await {
            VerificationOutcome::Accept { .. } => {}
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn code_review_verifier_rejects_pr_without_nearby_citation() {
        let s = CodeReviewSkill;
        let a = Uuid::new_v4();
        // PR mentioned >120 chars after the only citation.
        let narrative = format!(
            "Fixed bug in [{a}]. {} Opened PR #1234 separately.",
            "x".repeat(300)
        );
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a],
        };
        match s.verify(&ctx).await {
            VerificationOutcome::Reject {
                rubric,
                reason: VerificationReason::SkillRejection { detail },
                ..
            } => {
                assert_eq!(rubric, "code_review_pr_citation");
                assert!(detail.contains("#1234"));
            }
            other => panic!("expected SkillRejection for PR #1234, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn code_review_verifier_inherits_baseline_reject() {
        let s = CodeReviewSkill;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Only `a` cited; `b` is uncited. Baseline rubric should reject
        // BEFORE the PR check runs.
        let narrative = format!("Saw [{a}] only.");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a, b],
        };
        match s.verify(&ctx).await {
            VerificationOutcome::Reject {
                reason: VerificationReason::UncitedMember { claim_id },
                ..
            } => assert_eq!(claim_id, b),
            other => panic!("expected baseline UncitedMember reject, got {other:?}"),
        }
    }
}
