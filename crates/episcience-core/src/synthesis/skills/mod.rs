//! Synthesis skill registry.
//!
//! Skills are static — registered at compile time. Adding a new skill is
//! a deliberate change: the impl goes in a sibling module and the lookup
//! arm goes into [`load_by_name`].

pub mod baseline;

use std::sync::Arc;

use crate::synthesis::skill::SynthesisSkill;

/// Look up a skill by its stable name. Unknown names return `None` so the
/// caller can decide whether to error or fall back to baseline.
pub fn load_by_name(name: &str) -> Option<Arc<dyn SynthesisSkill>> {
    match name {
        "baseline" => Some(Arc::new(baseline::BaselineSkill)),
        _ => None,
    }
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
    fn default_skill_is_baseline() {
        assert_eq!(default_skill().name(), "baseline");
    }
}
