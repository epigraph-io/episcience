//! StalenessWorker — drains `belief.updated` events from upstream, identifies
//! cited syntheses whose recorded BetP for the affected claim has drifted by
//! more than `drift_epsilon`, records a staleness event, and marks the
//! synthesis stale.
//!
//! ## Phase 4 v1 scope
//!
//! Only the `belief_drift` trigger is implemented. The other triggers
//! (`new_contradiction`, `claim_superseded`, `frame_changed`, `edge_revoked`)
//! depend on event types that are NOT reliably persisted by upstream as of
//! 2026-04-28 (see `docs/superpowers/plans/p3-status.md`):
//!
//! - `edge.added` / `edge.deleted` / `claim.superseded` — emitted to the
//!   in-memory `EventStore` only, not the Postgres `events` table that
//!   `GET /api/v1/events` reads from when `feature = "db"` (the production
//!   default). They are therefore not visible to this worker over HTTP.
//! - `frame.changed` — not emitted at all (no hypothesis-set mutation
//!   endpoint exists yet).
//!
//! Tasks 4.3 and 4.4 will land the remaining triggers once upstream
//! dual-writes those event types.
//!
//! ## Watermark dedup
//!
//! Upstream's `since` filter is "created_at >= since" semantics. To avoid
//! re-processing the same boundary event each tick, we advance the watermark
//! to `last.created_at + 1µs`. This is fine because upstream timestamps have
//! sub-microsecond resolution from `Utc::now()` and we only need monotonic
//! progress, not exact replay.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::clients::epigraph_events::{EpigraphEventsClient, GraphEvent};
use episcience_core::synthesis::SubgraphSnapshot;
use episcience_db::{
    SynthesisMembershipRepository, SynthesisRepository, SynthesisStalenessRepository,
    WorkerStateRepository,
};

/// Persistent identifier for this worker in `episcience_worker_state`.
pub const STALENESS_WORKER_NAME: &str = "staleness_worker";

/// Default tick cadence for `run_forever`.
const DEFAULT_DRAIN_INTERVAL_SECS: u64 = 15;

/// Default belief-drift threshold. A `belief.updated` event whose
/// `pignistic_prob` differs from the synthesis's recorded `pignistic_prob` for
/// the same claim by more than this value triggers `belief_drift`. The
/// epsilon is tunable per-instance (e.g. tests use 0.10, production may
/// recalibrate from observed false-positive rates).
const DEFAULT_DRIFT_EPSILON: f64 = 0.10;

/// Cap on how many events to drain per tick. Bounds blast radius if the
/// worker has been offline a long time.
const POLL_PAGE_LIMIT: usize = 500;

pub struct StalenessWorker {
    pub pool: PgPool,
    pub events_client: Arc<EpigraphEventsClient>,
    pub drain_interval: Duration,
    pub drift_epsilon: f64,
}

impl StalenessWorker {
    pub fn new(pool: PgPool, events_client: Arc<EpigraphEventsClient>) -> Self {
        Self {
            pool,
            events_client,
            drain_interval: Duration::from_secs(DEFAULT_DRAIN_INTERVAL_SECS),
            drift_epsilon: DEFAULT_DRIFT_EPSILON,
        }
    }

    /// Long-running drain loop. Reads the persisted watermark on startup,
    /// then ticks forever. Errors are logged and swallowed — the worker
    /// keeps running so transient upstream outages don't kill the loop.
    pub async fn run_forever(self) {
        let mut watermark = match WorkerStateRepository::get(&self.pool, STALENESS_WORKER_NAME)
            .await
        {
            Ok(Some(state)) => state.last_event_ts,
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load worker_state; starting from None");
                None
            }
        };
        loop {
            if let Err(e) = self.tick(&mut watermark).await {
                tracing::error!(error = %e, "staleness worker tick failed");
            }
            tokio::time::sleep(self.drain_interval).await;
        }
    }

    /// One drain pass. Public for testability.
    ///
    /// `watermark` is updated in place: if events were drained, it advances
    /// past the last event's timestamp (plus 1µs to avoid re-processing).
    pub async fn tick(
        &self,
        watermark: &mut Option<DateTime<Utc>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // `ApiError` doesn't implement `std::error::Error` (it's an axum
        // response enum). Convert via display string before boxing.
        let events = self
            .events_client
            .poll_since(*watermark, &["belief.updated"], POLL_PAGE_LIMIT)
            .await
            .map_err(|e| {
                let msg = match e {
                    crate::errors::ApiError::ServiceUnavailable(m) => {
                        format!("service unavailable: {m}")
                    }
                    crate::errors::ApiError::Internal(m) => format!("internal: {m}"),
                    crate::errors::ApiError::Validation(m) => format!("validation: {m}"),
                    crate::errors::ApiError::NotFound(m) => format!("not found: {m}"),
                    crate::errors::ApiError::Unauthorized(m) => format!("unauthorized: {m}"),
                    crate::errors::ApiError::Forbidden(m) => format!("forbidden: {m}"),
                };
                Box::<dyn std::error::Error + Send + Sync>::from(msg)
            })?;

        if events.is_empty() {
            return Ok(());
        }

        // Group events by synthesis so we evaluate each synthesis once per
        // tick even if multiple of its claims received belief updates.
        let mut by_synthesis: HashMap<Uuid, Vec<GraphEvent>> = HashMap::new();
        for ev in &events {
            let claim_id = match Self::claim_id_of(ev) {
                Some(c) => c,
                None => continue,
            };
            let syntheses = SynthesisMembershipRepository::syntheses_citing(
                &self.pool, claim_id, /* only_complete_non_stale */ true,
            )
            .await
            .unwrap_or_default();
            for s in syntheses {
                by_synthesis.entry(s).or_default().push(ev.clone());
            }
        }
        for (synthesis_id, evs) in by_synthesis {
            self.evaluate_synthesis(synthesis_id, &evs).await;
        }

        // Advance watermark: events come back in ascending graph_version (so
        // ascending created_at), so the last one is the newest. Add 1µs so
        // the next poll's `since=` filter (which is `>=`) doesn't re-emit
        // the boundary event.
        let last_ts = events.last().map(|e| e.created_at).unwrap();
        let last_id = events.last().map(|e| e.id.to_string());
        let next_watermark = last_ts + chrono::Duration::microseconds(1);
        *watermark = Some(next_watermark);
        if let Err(e) = WorkerStateRepository::upsert(
            &self.pool,
            STALENESS_WORKER_NAME,
            last_id.as_deref(),
            Some(next_watermark),
        )
        .await
        {
            tracing::warn!(error = %e, "failed to persist worker_state; will retry next tick");
        }
        Ok(())
    }

    /// Evaluate one synthesis against the events that may affect it. The
    /// first event that classifies as a trigger wins (we record one event
    /// and mark stale; mark_stale is idempotent under "already stale").
    async fn evaluate_synthesis(&self, synthesis_id: Uuid, events: &[GraphEvent]) {
        let synth = match SynthesisRepository::get_by_id(&self.pool, synthesis_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, %synthesis_id, "could not load synthesis");
                return;
            }
        };
        let snapshot: &SubgraphSnapshot = &synth.subgraph_snapshot;

        for ev in events {
            if let Some(trigger) = self.classify_belief_drift(ev, snapshot) {
                let claim_id = Self::claim_id_of(ev);
                let affected: Vec<Uuid> = claim_id.map(|c| vec![c]).unwrap_or_default();
                if let Err(e) = SynthesisStalenessRepository::record_event(
                    &self.pool,
                    synthesis_id,
                    trigger,
                    &affected,
                    Some(&ev.payload),
                )
                .await
                {
                    tracing::warn!(error = %e, %synthesis_id, "record staleness event failed");
                    continue;
                }
                if let Err(e) =
                    SynthesisRepository::mark_stale(&self.pool, synthesis_id, trigger).await
                {
                    tracing::warn!(error = %e, %synthesis_id, "mark_stale failed");
                }
                break;
            }
        }
    }

    /// Returns `Some("belief_drift")` if `ev` is a `belief.updated` event
    /// whose new `pignistic_prob` for a claim cited by `snapshot` differs
    /// from the snapshot's recorded value by more than `drift_epsilon`.
    ///
    /// Notes on the upstream payload shape (verified in
    /// `epigraph-wt-episcience-p0/crates/epigraph-api/src/routes/belief.rs`):
    ///
    ///   { claim_id, frame_id, old_belief, new_belief, old_plausibility,
    ///     new_plausibility, pignistic_prob, combination_method,
    ///     total_sources, perspective_id }
    ///
    /// — there is no `old_betp` / `new_betp`; only `pignistic_prob` (the new
    /// post-combination BetP). We compare it to the snapshot's recorded
    /// `pignistic_prob` for the same claim_id.
    fn classify_belief_drift(
        &self,
        ev: &GraphEvent,
        snapshot: &SubgraphSnapshot,
    ) -> Option<&'static str> {
        if ev.event_type != "belief.updated" {
            return None;
        }
        let claim_id = Self::claim_id_of(ev)?;
        let new_betp = ev
            .payload
            .get("pignistic_prob")
            .and_then(|v| v.as_f64())?;
        let recorded = snapshot
            .belief_intervals
            .iter()
            .find(|bi| bi.claim_id == claim_id)
            .map(|bi| bi.pignistic_prob)?;
        if (recorded - new_betp).abs() > self.drift_epsilon {
            Some("belief_drift")
        } else {
            None
        }
    }

    fn claim_id_of(ev: &GraphEvent) -> Option<Uuid> {
        ev.payload
            .get("claim_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
    }
}
