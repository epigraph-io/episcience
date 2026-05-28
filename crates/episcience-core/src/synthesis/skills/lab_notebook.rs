//! `LabNotebookSkill` — synthesis tuned for ELN narrative summaries.
//!
//! Differs from baseline in three ways:
//! - prefers a traversal config with a shallower depth and a narrower set
//!   of edge types (focused on observational lineage)
//! - narration cites protocols and samples by id alongside claims
//! - composition produces a chronological narrative not a thematic one

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};

#[derive(Debug, Default)]
pub struct LabNotebookSkill;

#[async_trait::async_trait]
impl SynthesisSkill for LabNotebookSkill {
    fn name(&self) -> &'static str { "lab_notebook" }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Narration =>
                "For each cluster, write a chronological 2-4 sentence \
                 summary mentioning the protocol used and the samples \
                 observed. Cite every claim with `[<claim_id>]`. Cite \
                 protocols as `(protocol:<title>@v<version>)` and samples \
                 as `(sample:<name>)` when relevant. Do not invent any.",
            SynthesisStage::Composition =>
                "Compose the per-cluster summaries into a chronologically \
                 ordered Markdown narrative (oldest first). Keep the \
                 `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.",
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        // Lab-notebook synthesis prefers observational lineage over thematic
        // coverage: shallow hops, two edge types that carry narrative-relevant
        // signal (Supports = downstream observation, Corroborates = repeat
        // observation of the same phenomenon). Other variants (Contradicts,
        // Supersedes, Methodology) are intentionally excluded — they widen
        // into argumentative or methodological lineage we don't want here.
        Some(TraversalConfig {
            max_hops: 2,
            edge_types: vec![EdgeType::Supports, EdgeType::Corroborates],
            relevance_prune: 0.55,
            ..TraversalConfig::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_notebook_overrides_narration_composition_traversal() {
        let s = LabNotebookSkill;
        assert_eq!(s.name(), "lab_notebook");
        // Narration section mentions protocol and sample (its differentiator).
        let narration = s.section(SynthesisStage::Narration).unwrap();
        assert!(narration.to_lowercase().contains("protocol"));
        assert!(narration.to_lowercase().contains("sample"));
        // Composition section mentions chronological (its differentiator).
        assert!(s.section(SynthesisStage::Composition).unwrap()
            .to_lowercase().contains("chronolog"));
        // Other stages still return None (no override).
        assert!(s.section(SynthesisStage::Overview).is_none());
        assert!(s.section(SynthesisStage::Verification).is_none());
        // Traversal config IS set — proves the skill narrows traversal.
        let cfg = s.traversal_config().expect("lab_notebook sets traversal_config");
        assert_eq!(cfg.max_hops, 2);
        assert!(!cfg.edge_types.is_empty(), "should narrow to at least one edge type");
    }
}
