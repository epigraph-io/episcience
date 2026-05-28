//! Synthesis-stage section vocabulary and the [`SynthesisSkill`] trait.
//!
//! SciLink's foundation-agent pattern (see SciLink `CLAUDE.md`, "Foundation
//! agents") defines a fixed *section vocabulary* per modality and pluggable
//! *skills* that contribute per-section content. Episcience adopts that
//! pattern for the synthesis worker: [`SynthesisStage`] is the section
//! vocabulary, [`SynthesisSkill`] is the contract a skill implements.

use crate::synthesis::traversal::TraversalConfig;
use crate::synthesis::verifier::{VerificationContext, VerificationOutcome};

/// The fixed section vocabulary the synthesis pipeline knows how to splice
/// skill-provided content into. The enum is **closed** — adding a new
/// variant is a deliberate pipeline change.
///
/// The naming mirrors SciLink's `overview / planning / implementation /
/// interpretation / validation` set, extended with the stages specific to
/// graph-clustering synthesis (`traversal`, `clustering`, `composition`,
/// `novelty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SynthesisStage {
    Overview,
    Planning,
    Traversal,
    Clustering,
    Narration,
    Composition,
    Verification,
    Novelty,
}

impl SynthesisStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Planning => "planning",
            Self::Traversal => "traversal",
            Self::Clustering => "clustering",
            Self::Narration => "narration",
            Self::Composition => "composition",
            Self::Verification => "verification",
            Self::Novelty => "novelty",
        }
    }

    // Inherent `from_str` returning `Option<Self>` (no parse error).
    // Implementing `std::str::FromStr` would require choosing an error
    // type and propagates through callers; the inherent method captures
    // the intent ("unknown stage -> None") more directly.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "overview" => Self::Overview,
            "planning" => Self::Planning,
            "traversal" => Self::Traversal,
            "clustering" => Self::Clustering,
            "narration" => Self::Narration,
            "composition" => Self::Composition,
            "verification" => Self::Verification,
            "novelty" => Self::Novelty,
            _ => return None,
        })
    }
}

/// A pluggable synthesis specialisation. Implementations contribute
/// per-stage prompt sections, optional traversal-config defaults, and
/// optional verification rubrics. The default-method bodies encode the
/// "no opinion" answer — callers fall back to baseline behaviour.
///
/// Trait-object safe: pipelines hold `Arc<dyn SynthesisSkill>`.
#[async_trait::async_trait]
pub trait SynthesisSkill: Send + Sync + std::fmt::Debug {
    /// Stable identifier persisted in `syntheses.skill_name`.
    /// Lowercase snake_case. Must match the registry key (see
    /// `crate::synthesis::skills::load_by_name`, added in Task 1.3).
    fn name(&self) -> &'static str;

    /// Returns the skill-specific prompt section for `stage`, or `None`
    /// to fall back to the pipeline's baseline prompt. Implementations
    /// return short, focused content — multi-paragraph sections belong
    /// in the sibling markdown reference, not in code.
    fn section(&self, stage: SynthesisStage) -> Option<&str>;

    /// Default traversal config override. `None` means "use the caller's
    /// supplied config or the schema default". Skills with strong domain
    /// opinions (e.g. lab-notebook synthesis wants depth=2, edge_types
    /// limited to `derived_from`+`refutes`) override this.
    fn traversal_config(&self) -> Option<TraversalConfig> {
        None
    }

    /// Verify a generated narrative against the cluster + the kernel state.
    /// The default impl runs the citation-discipline rubric (see
    /// [`crate::synthesis::verifier::default_citation_rubric`]): every
    /// member must be cited, no citation may hallucinate.
    ///
    /// Skills with stricter checks override.
    async fn verify(&self, ctx: &VerificationContext<'_>) -> VerificationOutcome {
        crate::synthesis::verifier::default_citation_rubric(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesis_stage_round_trips_through_str() {
        for s in [
            SynthesisStage::Overview,
            SynthesisStage::Planning,
            SynthesisStage::Traversal,
            SynthesisStage::Clustering,
            SynthesisStage::Narration,
            SynthesisStage::Composition,
            SynthesisStage::Verification,
            SynthesisStage::Novelty,
        ] {
            let serialized = s.as_str();
            let parsed = SynthesisStage::from_str(serialized)
                .unwrap_or_else(|| panic!("could not parse {serialized}"));
            assert_eq!(parsed, s, "round-trip failed for {serialized}");
        }
    }

    #[test]
    fn synthesis_stage_rejects_unknown_strings() {
        assert!(SynthesisStage::from_str("not_a_stage").is_none());
        assert!(SynthesisStage::from_str("").is_none());
    }

    #[derive(Debug)]
    struct StubSkill;

    #[async_trait::async_trait]
    impl SynthesisSkill for StubSkill {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn section(&self, stage: SynthesisStage) -> Option<&str> {
            match stage {
                SynthesisStage::Overview => Some("stub overview"),
                _ => None,
            }
        }
    }

    #[test]
    fn stub_skill_returns_overview_only() {
        let s = StubSkill;
        assert_eq!(s.name(), "stub");
        assert_eq!(s.section(SynthesisStage::Overview), Some("stub overview"));
        assert_eq!(s.section(SynthesisStage::Narration), None);
        assert!(s.traversal_config().is_none());
    }
}
