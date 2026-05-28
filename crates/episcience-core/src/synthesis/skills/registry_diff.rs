//! `RegistryDiffSkill` — synthesis tuned for the weekly-capability-audit
//! workflow.
//!
//! Differs from baseline:
//! - traversal narrows to `Supersedes` edges only (tool versions chain
//!   via supersedes) at max_hops=1
//! - narration formatted as added/removed/drifted markers per claim
//! - composition produces three Markdown tables (Added / Removed / Drifted)
//! - verifier inherits default citation rubric (no override; the
//!   "Removed claim should carry epigraph_edge_id" check belongs at the
//!   review-bot tier, not the verifier — the verifier only has the
//!   narrative + member ids, not claim properties)

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};

#[derive(Debug, Default)]
pub struct RegistryDiffSkill;

#[async_trait::async_trait]
impl SynthesisSkill for RegistryDiffSkill {
    fn name(&self) -> &'static str {
        "registry_diff"
    }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview => {
                "Summarise a capability-audit run: tools added, tools \
                 removed, tools whose schemas drifted."
            }
            SynthesisStage::Narration => {
                "For each cluster, list the capability changes it covers. \
                 Mark added tools with `+`, removed with `-`, drifted with \
                 `~`. Cite every claim with `[<claim_id>]`. Do not invent \
                 capability names."
            }
            SynthesisStage::Composition => {
                "Compose the per-cluster summaries into a Markdown narrative \
                 organised as three tables: `## Added` / `## Removed` / \
                 `## Drifted`. Each table has columns: Tool, Version, \
                 Notes, [<claim_id>]. Keep the `<<<CLUSTER:{id}:BEGIN/END>>>` \
                 sentinels verbatim."
            }
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        Some(TraversalConfig {
            max_hops: 1,
            edge_types: vec![EdgeType::Supersedes],
            relevance_prune: 0.6,
            ..TraversalConfig::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_diff_overrides_three_stages_and_traversal() {
        let s = RegistryDiffSkill;
        assert_eq!(s.name(), "registry_diff");

        // Narration mentions the +/-/~ markers.
        let narration = s.section(SynthesisStage::Narration).unwrap();
        assert!(narration.contains("`+`"));
        assert!(narration.contains("`-`"));
        assert!(narration.contains("`~`"));

        // Composition mentions all 3 tables.
        let composition = s.section(SynthesisStage::Composition).unwrap();
        assert!(composition.contains("Added"));
        assert!(composition.contains("Removed"));
        assert!(composition.contains("Drifted"));

        // Verification inherits default rubric — no override.
        assert!(s.section(SynthesisStage::Verification).is_none());

        // Traversal is opinionated: shallow + Supersedes only.
        let cfg = s.traversal_config().unwrap();
        assert_eq!(cfg.max_hops, 1);
        assert_eq!(cfg.edge_types.len(), 1);
        assert_eq!(cfg.edge_types[0], EdgeType::Supersedes);
    }
}
