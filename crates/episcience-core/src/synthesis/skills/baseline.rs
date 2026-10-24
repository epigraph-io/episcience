//! `BaselineSkill` — the default synthesis specialisation.
//!
//! Encodes the prompt content the pre-skill `SynthesisPipeline` carried
//! inline. Loaded when a synthesis row does not specify a skill, so the
//! refactor is behaviour-preserving.

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};

#[derive(Debug, Default)]
pub struct BaselineSkill;

#[async_trait::async_trait]
impl SynthesisSkill for BaselineSkill {
    fn name(&self) -> &'static str {
        "baseline"
    }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview => {
                "Summarise the cluster of related claims. Cite each cluster \
                 member exactly once with `[<claim_id>]`."
            }
            SynthesisStage::Narration => {
                "Produce a short title and a 1-3 paragraph summary. Cite \
                 every cluster member claim at least once by its full id as \
                 `[<claim_id>]`; never shorten an id. Do not introduce facts \
                 not present in the supplied claim contents."
            }
            SynthesisStage::Composition => {
                "Weave the per-cluster summaries into one Markdown narrative. \
                 Place each cluster's `{{CLUSTER_N}}` placeholder exactly once \
                 where its summary belongs; the pipeline replaces each \
                 placeholder with that summary, so write only the text \
                 around them."
            }
            SynthesisStage::Verification => {
                "Accept a narrative iff every cluster member appears in a \
                 citation and no citation refers to a claim outside the cluster."
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_provides_narration_and_composition() {
        let s = BaselineSkill;
        assert_eq!(s.name(), "baseline");
        assert!(s.section(SynthesisStage::Narration).is_some());
        assert!(s.section(SynthesisStage::Composition).is_some());
        assert!(s.section(SynthesisStage::Overview).is_some());
        assert!(s.section(SynthesisStage::Verification).is_some());
        // Stages without baseline content return None.
        assert!(s.section(SynthesisStage::Traversal).is_none());
        assert!(s.section(SynthesisStage::Clustering).is_none());
        assert!(s.section(SynthesisStage::Novelty).is_none());
        assert!(s.section(SynthesisStage::Planning).is_none());
        // Default traversal_config from the trait impl: None.
        assert!(s.traversal_config().is_none());
    }

    /// Narration guidance matches what stage 4 enforces and what the narrate
    /// prompt asks for: every member by its full id, and a 1-3 paragraph
    /// summary (not a 2-4 sentence one, which pushed the model to compress
    /// and drop or shorten citations).
    #[test]
    fn baseline_narration_asks_for_every_member_by_full_id() {
        let n = BaselineSkill.section(SynthesisStage::Narration).unwrap();
        assert!(
            n.contains("Cite every cluster member claim at least once by its full id"),
            "got: {n}"
        );
        assert!(n.contains("never shorten an id"), "got: {n}");
        assert!(n.contains("1-3 paragraph summary"), "got: {n}");
        assert!(!n.contains("2-4 sentence"), "got: {n}");
    }
}
