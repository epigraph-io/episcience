//! Synthesis skill registry.
//!
//! Skills are static — registered at compile time. Adding a new skill is
//! a deliberate change: the impl goes in a sibling module and the lookup
//! arm goes into [`load_by_name`].

pub mod baseline;
pub mod code_review;
pub mod lab_notebook;
pub mod literature;
pub mod registry_diff;
pub mod wiki_article;

use std::sync::Arc;

use crate::synthesis::skill::SynthesisSkill;

/// A registry entry's constructor. Non-capturing closures coerce to this.
type SkillCtor = fn() -> Arc<dyn SynthesisSkill>;

/// Every registered skill, keyed by its stable name. The single source of
/// truth for [`load_by_name`] and [`registered_names`]: a skill added here is
/// enumerable, so the database test that every registered name is accepted by
/// the `syntheses_skill_name_known` CHECK
/// (`episcience-db/tests/synthesis_repo_test.rs`) covers it automatically and
/// a skill can no longer ship without its CHECK migration.
const REGISTRY: &[(&str, SkillCtor)] = &[
    ("baseline", || Arc::new(baseline::BaselineSkill)),
    ("lab_notebook", || Arc::new(lab_notebook::LabNotebookSkill)),
    ("literature", || Arc::new(literature::LiteratureSkill)),
    ("code_review", || Arc::new(code_review::CodeReviewSkill)),
    ("registry_diff", || {
        Arc::new(registry_diff::RegistryDiffSkill)
    }),
    (crate::wiki::WIKI_SKILL_NAME, || {
        Arc::new(wiki_article::WikiArticleSkill)
    }),
];

/// Look up a skill by its stable name. Unknown names return `None` so the
/// caller can decide whether to error or fall back to baseline.
pub fn load_by_name(name: &str) -> Option<Arc<dyn SynthesisSkill>> {
    REGISTRY
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, ctor)| ctor())
}

/// The stable names of every registered skill, in registration order. Each
/// one must be a value `public.syntheses.skill_name` accepts.
pub fn registered_names() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|(n, _)| *n)
}

/// The skill used when a synthesis row does not specify one.
pub fn default_skill() -> Arc<dyn SynthesisSkill> {
    Arc::new(baseline::BaselineSkill)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_by_name_returns_baseline() {
        let s = load_by_name("baseline").expect("baseline must be registered");
        assert_eq!(s.name(), "baseline");
    }

    #[test]
    fn load_by_name_returns_none_for_unknown() {
        assert!(load_by_name("does_not_exist").is_none());
    }

    #[test]
    fn load_by_name_returns_wiki_article() {
        assert_eq!(load_by_name("wiki_article").unwrap().name(), "wiki_article");
    }

    /// Every registered name resolves to a skill reporting that same name, and
    /// no name is registered twice. Kills: a registry row whose key and
    /// `SynthesisSkill::name` disagree (the row would persist one name and
    /// run another skill), or a duplicate key shadowing a later skill.
    #[test]
    fn registered_names_resolve_to_themselves_and_are_unique() {
        let names: Vec<&str> = registered_names().collect();
        assert!(names.contains(&"baseline") && names.contains(&"wiki_article"));
        for n in &names {
            assert_eq!(load_by_name(n).expect("registered").name(), *n);
        }
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate registry key in {names:?}"
        );
    }

    #[test]
    fn default_skill_is_baseline() {
        assert_eq!(default_skill().name(), "baseline");
    }
}
