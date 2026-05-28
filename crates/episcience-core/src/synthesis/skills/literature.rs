//! `LiteratureSkill` — synthesis tuned for the arxiv research-scan
//! workflow.
//!
//! Differs from baseline:
//! - traversal narrows to Supports + Methodology + Corroborates edges
//!   (the citation-discipline trio for literature work) at max_hops=3
//! - narration explicitly demands DOI / arxiv citation formatting
//! - verifier inherits the default citation rubric (every cluster
//!   member cited, no hallucinations) — no override

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};

#[derive(Debug, Default)]
pub struct LiteratureSkill;

#[async_trait::async_trait]
impl SynthesisSkill for LiteratureSkill {
    fn name(&self) -> &'static str {
        "literature"
    }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview => {
                "Summarise a literature-scan run: which papers were found, \
                 which were already known, which contributed novel findings."
            }
            SynthesisStage::Narration => {
                "For each cluster, list the papers it covers. Cite each \
                 with `[<claim_id>]` and ALSO with the paper's DOI in \
                 parentheses: `(doi:10.xxx/yyy)`. If a paper has no DOI, \
                 use `(arxiv:NNNN.NNNNN)`. Group by methodology or topic. \
                 Do not invent identifiers."
            }
            SynthesisStage::Composition => {
                "Compose the per-cluster summaries into one Markdown \
                 narrative, ordered by methodology family then publication \
                 date. Keep the `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels \
                 verbatim."
            }
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        Some(TraversalConfig {
            max_hops: 3,
            edge_types: vec![
                EdgeType::Supports,
                EdgeType::Methodology,
                EdgeType::Corroborates,
            ],
            relevance_prune: 0.5,
            ..TraversalConfig::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literature_skill_overrides_three_stages_and_traversal() {
        let s = LiteratureSkill;
        assert_eq!(s.name(), "literature");

        // Narration demands DOI + arxiv formatting.
        let narration = s.section(SynthesisStage::Narration).unwrap();
        assert!(narration.contains("DOI"));
        assert!(narration.contains("arxiv"));

        // Composition mentions methodology ordering.
        let composition = s.section(SynthesisStage::Composition).unwrap();
        assert!(composition.to_lowercase().contains("methodology"));

        // Verification inherits default rubric — no override.
        assert!(s.section(SynthesisStage::Verification).is_none());

        // Traversal is opinionated.
        let cfg = s.traversal_config().expect("literature sets traversal");
        assert_eq!(cfg.max_hops, 3);
        assert_eq!(cfg.edge_types.len(), 3);
        assert!(cfg.edge_types.contains(&EdgeType::Supports));
        assert!(cfg.edge_types.contains(&EdgeType::Methodology));
        assert!(cfg.edge_types.contains(&EdgeType::Corroborates));
        assert!((cfg.relevance_prune - 0.5).abs() < 1e-9);
    }
}
