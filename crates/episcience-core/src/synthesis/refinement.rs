//! Refinement temperature. SciLink's "simulated-annealing agentic pipelines"
//! hold priors strict at first, then progressively thaw as iterations fail.
//! Episcience adopts the same pattern: the first refinement starts cool
//! (small deltas to the original traversal config); each subsequent
//! refinement thaws further (wider depth, lower relevance threshold).

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RefinementTemperature {
    /// Hops added on top of the parent's traversal config. Bounded at 3.
    pub depth_delta: u8,
    /// Multiplier on `relevance_prune` (smaller → keeps more neighbours).
    /// Bounded at 0.4 floor (a relevance threshold below 0.22ish becomes
    /// noise).
    pub relevance_prune_relax: f32,
    /// True after the first reject — the verifier may downgrade strict
    /// rubrics (e.g., "every member must be cited" becomes "at least 50%
    /// of members must be cited"). The default rubric does NOT honor
    /// this knob today; future skill rubrics may.
    pub allow_soft_verifier: bool,
}

impl Default for RefinementTemperature {
    fn default() -> Self {
        Self {
            depth_delta: 0,
            relevance_prune_relax: 1.0,
            allow_soft_verifier: false,
        }
    }
}

impl RefinementTemperature {
    /// Anneal one step. Bounded by the hard ceiling `depth_delta <= 3`.
    pub fn anneal(self) -> Self {
        Self {
            depth_delta: self.depth_delta.saturating_add(1).min(3),
            relevance_prune_relax: (self.relevance_prune_relax * 0.8).max(0.4),
            allow_soft_verifier: true,
        }
    }

    /// True if this temperature has hit the refinement ceiling and no
    /// further annealing should spawn a child.
    pub fn at_ceiling(self) -> bool {
        self.depth_delta >= 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_cold() {
        let t = RefinementTemperature::default();
        assert_eq!(t.depth_delta, 0);
        assert_eq!(t.relevance_prune_relax, 1.0);
        assert!(!t.allow_soft_verifier);
        assert!(!t.at_ceiling());
    }

    #[test]
    fn anneal_progresses_monotonically() {
        let t0 = RefinementTemperature::default();
        let t1 = t0.anneal();
        assert_eq!(t1.depth_delta, 1);
        assert!(t1.relevance_prune_relax < t0.relevance_prune_relax);
        assert!(t1.allow_soft_verifier);
        assert!(!t1.at_ceiling());

        let t2 = t1.anneal();
        assert_eq!(t2.depth_delta, 2);
        assert!(t2.relevance_prune_relax < t1.relevance_prune_relax);

        let t3 = t2.anneal();
        assert_eq!(t3.depth_delta, 3);
        assert!(t3.at_ceiling());
    }

    #[test]
    fn anneal_caps_at_3() {
        let mut t = RefinementTemperature::default();
        for _ in 0..10 {
            t = t.anneal();
        }
        assert_eq!(t.depth_delta, 3);
        assert!(t.at_ceiling());
        // relevance_prune_relax has a floor of 0.4.
        assert!((t.relevance_prune_relax - 0.4).abs() < 1e-6);
    }
}
