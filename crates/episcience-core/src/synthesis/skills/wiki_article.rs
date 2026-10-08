//! `WikiArticleSkill` — an encyclopedic article about one theme, for the wiki
//! (plan 2026-10-08-wiki-phase-b-articles.md). Seeds are theme-anchored and
//! de-duplicated upstream (Task 3); this skill shapes the prose.
//!
//! Differs from baseline:
//! - narration writes encyclopedic paragraphs, states a repeated fact once
//!   (citing every claim that repeats it) and flags disagreement explicitly
//! - composition frames the cluster blocks as an article (title, lede,
//!   `##` headings, `## Contested`, `## Gaps`) while leaving every
//!   `<<<CLUSTER:…>>>` block verbatim — `stage5_compose` rejects any edit
//!   inside a block (`ComposeAnchorViolation`)
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
                "Write an encyclopedia article around the cluster blocks: a `# Title` \
                 line, then a lede paragraph that defines the topic in its first \
                 sentence, then the cluster blocks in a logical teaching order, each \
                 preceded by a `##` heading you write. Your own text goes only between \
                 blocks; every `<<<CLUSTER:{id}:BEGIN/END>>>` block stays verbatim, \
                 unchanged inside. If any cluster reported disagreement, add \
                 `## Contested` after the last block, listing each disagreement with \
                 citations to both sides. End with `## Gaps` naming what the cited \
                 claims do not cover."
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
        for must in ["lede", "## Contested", "## Gaps", "<<<CLUSTER:", "verbatim"] {
            assert!(comp.contains(must), "composition prompt lacks {must:?}");
        }
        // stage5_compose rejects any edit inside a cluster block (ComposeAnchorViolation), so the
        // composition guidance must never ask to merge, reorder-within, or rewrite cluster text.
        let lc = comp.to_lowercase();
        for banned in ["merge", "rewrite", "paraphrase", "do not restate"] {
            assert!(
                !lc.contains(banned),
                "composition prompt asks for {banned:?}, which breaks the anchor validator"
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
