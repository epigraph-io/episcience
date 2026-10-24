//! `WikiArticleSkill` — an encyclopedic article about one theme, for the wiki
//! (plan 2026-10-08-wiki-phase-b-articles.md). Seeds are theme-anchored and
//! de-duplicated upstream (Task 3); this skill shapes the prose.
//!
//! Differs from baseline:
//! - narration writes encyclopedic paragraphs, states a repeated fact once
//!   (citing every claim that repeats it) and flags disagreement explicitly
//! - composition frames the cluster placeholders as an article (title,
//!   lede, `##` headings, `## Contested`, `## Gaps`); `stage5_compose`
//!   splices each stored summary in at its `{{CLUSTER_N}}` placeholder and
//!   rejects a missing or repeated one (`ComposeAnchorViolation`)
//! - no traversal opinion: production runs with no edge provider
//! - verification inherits the default citation rubric (every member cited)
//!
//! Human-readable reference: `markdown/wiki_article.md`.
use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};

#[derive(Debug, Default)]
pub struct WikiArticleSkill;

#[async_trait::async_trait]
impl SynthesisSkill for WikiArticleSkill {
    fn name(&self) -> &'static str {
        crate::wiki::WIKI_SKILL_NAME
    }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview => {
                "Write an encyclopedia article about one topic for readers who know \
                 nothing about it. Neutral, factual tone; no first person."
            }
            SynthesisStage::Narration => {
                "Summarise this cluster as one or two encyclopedic paragraphs. Cite \
                 every claim as `[<claim_id>]`. If claims disagree or qualify each \
                 other, say so explicitly and cite both sides. Within this cluster, \
                 state a fact once even if several claims repeat it; cite all of them \
                 on that sentence."
            }
            SynthesisStage::Composition => {
                "Write an encyclopedia article around the cluster placeholders: a \
                 `# Title` line, then a lede paragraph that defines the topic in its \
                 first sentence, then each cluster's `{{CLUSTER_N}}` placeholder, \
                 exactly once, in a logical teaching order, each preceded by a `##` \
                 heading you write. Your own text goes only between placeholders; \
                 the pipeline replaces each placeholder with that cluster's summary, \
                 unchanged. If any cluster reported disagreement, add `## Contested` \
                 after the last placeholder, listing each disagreement with citations \
                 to both sides. End with `## Gaps` naming what the cited claims do \
                 not cover."
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_article_skill_shapes_an_encyclopedic_article() {
        let s = WikiArticleSkill;
        assert_eq!(s.name(), crate::wiki::WIKI_SKILL_NAME);
        let comp = s.section(SynthesisStage::Composition).unwrap();
        for must in [
            "lede",
            "## Contested",
            "## Gaps",
            "`{{CLUSTER_N}}` placeholder",
            "exactly once",
        ] {
            assert!(comp.contains(must), "composition prompt lacks {must:?}");
        }
        assert!(
            !comp.contains("<<<CLUSTER:") && !comp.to_lowercase().contains("verbatim"),
            "composition prompt still asks for sentinel blocks copied verbatim: {comp}"
        );
        // stage5_compose splices each stored summary in at its placeholder, so the composition
        // guidance must never ask to merge, reorder-within, or rewrite cluster text.
        let lc = comp.to_lowercase();
        for banned in ["merge", "rewrite", "paraphrase", "do not restate"] {
            assert!(
                !lc.contains(banned),
                "composition prompt asks for {banned:?}, which the placeholder splice cannot honour"
            );
        }
        let narr = s.section(SynthesisStage::Narration).unwrap();
        assert!(narr.contains("[<claim_id>]"));
        assert!(narr.to_lowercase().contains("disagree"));
        // No traversal opinion: production has no edge provider (plan, "Spike evidence").
        assert!(s.traversal_config().is_none());
        assert!(s.section(SynthesisStage::Verification).is_none());
    }
}
