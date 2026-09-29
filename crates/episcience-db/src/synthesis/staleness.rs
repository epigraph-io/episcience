//! The staleness recheck (E1f): compare a completed synthesis' recorded
//! belief intervals with the kernel's current ones, AS the synthesis' acting
//! principal. It replaces the retired event poll (`belief.updated` over the
//! kernel's HTTP events API with a service credential): the worker drives it
//! from the `staleness_check` worklist instead.

use epigraph_db::Viewer;
use epigraph_engine::belief_query::BeliefQueryError;
use sqlx::PgPool;
use uuid::Uuid;

use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::SubgraphSnapshot;

/// The drift threshold on the pignistic probability (the retired event
/// poll's value).
pub const DRIFT_EPSILON: f64 = 0.10;

/// The outcome of one recheck.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Recheck {
    /// Claims whose current BetP differs from the recorded one by more than
    /// the epsilon, in snapshot order.
    pub drifted: Vec<Uuid>,
    /// Claims the engine could not see for this viewer (or that are gone):
    /// UNKNOWN, never drift. A claim narrowed out of the owner's reach is not
    /// evidence that its belief moved.
    pub unknown: Vec<Uuid>,
}

impl Recheck {
    /// Whether the synthesis should be marked stale.
    pub fn is_stale(&self) -> bool {
        !self.drifted.is_empty()
    }
}

/// Whether `current` drifted from `recorded` by more than `epsilon`.
pub fn drifted(recorded: f64, current: f64, epsilon: f64) -> bool {
    (recorded - current).abs() > epsilon
}

/// Recheck every recorded interval of `snapshot` with
/// `epigraph_engine::belief_query::get_belief` on `engine_pool` AS `viewer`.
///
/// # Errors
/// [`SynthesisError::Db`] on any engine failure other than "claim not
/// found": the caller then skips the item WITHOUT advancing its
/// `staleness_checked_at`, so a transient failure is retried next period
/// rather than recorded as "checked, fine".
pub async fn recheck_beliefs(
    engine_pool: &PgPool,
    viewer: &Viewer,
    snapshot: &SubgraphSnapshot,
    epsilon: f64,
) -> Result<Recheck, SynthesisError> {
    let mut out = Recheck::default();
    for bi in &snapshot.belief_intervals {
        match epigraph_engine::belief_query::get_belief(engine_pool, viewer, bi.claim_id, None)
            .await
        {
            Ok(now) => {
                if drifted(bi.pignistic_prob, now.pignistic_prob, epsilon) {
                    out.drifted.push(bi.claim_id);
                }
            }
            Err(BeliefQueryError::ClaimNotFound(_)) => out.unknown.push(bi.claim_id),
            Err(e) => return Err(SynthesisError::Db(e.to_string())),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary is strict: exactly epsilon is not drift, anything past it
    /// is, in either direction. Kills: `>=` for `>`, and a one-sided check.
    #[test]
    fn drift_is_strictly_beyond_epsilon_in_either_direction() {
        assert!(!drifted(0.5, 0.6, 0.1 + 1e-12));
        assert!(drifted(0.5, 0.62, 0.1));
        assert!(drifted(0.62, 0.5, 0.1));
        assert!(!drifted(0.5, 0.55, 0.1));
    }

    /// Unknown claims never make a synthesis stale; only drifted ones do.
    #[test]
    fn unknown_claims_alone_are_not_staleness() {
        let r = Recheck {
            drifted: vec![],
            unknown: vec![Uuid::new_v4()],
        };
        assert!(!r.is_stale());
        let r = Recheck {
            drifted: vec![Uuid::new_v4()],
            unknown: vec![],
        };
        assert!(r.is_stale());
    }
}
