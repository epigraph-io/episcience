//! Integration tests for `StalenessWorker` (Phase 4 Task 4.2).
//!
//! Phase 4 v1 implements only the `belief_drift` trigger; other triggers are
//! deferred until upstream dual-writes their events (see
//! `docs/superpowers/plans/p3-status.md`).
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//!     cargo test -p episcience-api --test staleness_worker_test
//!
//! Each test creates and cleans up its own rows so they're independent. The
//! events client is wiremock-backed: each test starts a `MockServer`, points
//! a fresh `EpigraphEventsClient` at it, and feeds canned `belief.updated`
//! events through the worker's `tick()`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use episcience_api::clients::epigraph_events::EpigraphEventsClient;
use episcience_api::jobs::staleness_worker::StalenessWorker;
use episcience_core::synthesis::{BeliefIntervalEntry, SubgraphSnapshot, Visibility};
use episcience_db::{
    SynthesisMembershipRepository, SynthesisRepository, SynthesisStalenessRepository,
    WorkerStateRepository,
};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis";

async fn connect() -> PgPool {
    let dsn = std::env::var("DATABASE_URL").unwrap_or_else(|_| DSN.to_string());
    PgPool::connect(&dsn)
        .await
        .expect("connect to epigraph_dev_synthesis (set DATABASE_URL to override)")
}

/// Seed a `complete`, non-stale synthesis whose snapshot records
/// `pignistic_prob = recorded_betp` for `claim_id`.
///
/// Uses the public repo APIs (create_pending → save_snapshot → save_narrative)
/// to satisfy the table's CHECK constraints (status='complete' iff narrative
/// IS NOT NULL iff completed_at IS NOT NULL).
async fn seed_complete_synthesis(
    pool: &PgPool,
    synthesis_id: Uuid,
    owner: Uuid,
    claim_id: Uuid,
    recorded_betp: f64,
) {
    SynthesisRepository::create_pending(
        pool,
        synthesis_id,
        "staleness-worker test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Private,
    )
    .await
    .expect("create_pending");

    let snap = SubgraphSnapshot {
        claim_ids: vec![claim_id],
        edge_ids: vec![],
        belief_intervals: vec![BeliefIntervalEntry {
            claim_id,
            frame_id: None,
            belief: 0.7,
            plausibility: 0.95,
            pignistic_prob: recorded_betp,
            framed: false,
        }],
        traversal_config: serde_json::json!({}),
        captured_at: chrono::Utc::now(),
    };
    SynthesisRepository::save_snapshot(pool, synthesis_id, &snap)
        .await
        .expect("save_snapshot");

    let zero_hash = [0u8; 32];
    SynthesisRepository::save_narrative(pool, synthesis_id, "test narrative", &zero_hash)
        .await
        .expect("save_narrative — flips status to complete");

    // Link the synthesis to the claim so syntheses_citing finds it.
    let mut tx = pool.begin().await.expect("begin tx");
    SynthesisMembershipRepository::replace_for_synthesis(&mut tx, synthesis_id, &[claim_id])
        .await
        .expect("replace membership");
    tx.commit().await.expect("commit membership");
}

async fn cleanup_synthesis(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM synthesis_staleness_events WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM synthesis_claim_membership WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM synthesis_jobs WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
}

/// Build a `belief.updated` event payload matching the upstream wire format
/// (see `epigraph-wt-episcience-p0/crates/epigraph-api/src/routes/belief.rs`).
fn belief_updated_event(
    claim_id: Uuid,
    pignistic_prob: f64,
    created_at: DateTime<Utc>,
) -> serde_json::Value {
    serde_json::json!({
        "id": Uuid::now_v7().to_string(),
        "event_type": "belief.updated",
        "actor_id": null,
        "created_at": created_at.to_rfc3339(),
        "payload": {
            "claim_id": claim_id.to_string(),
            "frame_id": null,
            "old_belief": 0.5,
            "new_belief": 0.6,
            "old_plausibility": 0.9,
            "new_plausibility": 0.95,
            "pignistic_prob": pignistic_prob,
            "combination_method": "dempster",
            "total_sources": 2,
            "perspective_id": null
        },
        "graph_version": 100
    })
}

async fn mount_belief_event(server: &MockServer, event: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/api/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "events": [event],
            "total": 1
        })))
        .mount(server)
        .await;
}

/// Mount a `/api/v1/events` mock that returns the supplied batch verbatim
/// in graph_version order. Used by the burst-idempotence test (Task 5.4).
async fn mount_belief_events_batch(server: &MockServer, events: Vec<serde_json::Value>) {
    let total = events.len();
    Mock::given(method("GET"))
        .and(path("/api/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "events": events,
            "total": total
        })))
        .mount(server)
        .await;
}

async fn mount_no_events(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "events": [],
            "total": 0
        })))
        .mount(server)
        .await;
}

/// Each test passes a unique `worker_name` so concurrent tests don't race
/// over the single `episcience_worker_state` row that
/// `STALENESS_WORKER_NAME` would otherwise share. Without this, test 4's
/// `worker_state IS NULL` assertion can flake under cargo's default
/// parallel execution because tests 1-3 also persist worker state on tick.
fn build_worker(pool: PgPool, server: &MockServer, worker_name: &str) -> StalenessWorker {
    let client = Arc::new(EpigraphEventsClient::new(
        server.uri(),
        "test-token".to_string(),
    ));
    StalenessWorker::new(pool, client).with_worker_name(worker_name)
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: belief_drift > epsilon → mark stale + record event
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn belief_drift_triggers_stale() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let claim_id = Uuid::now_v7();

    seed_complete_synthesis(
        &pool,
        synthesis_id,
        owner,
        claim_id,
        /* recorded */ 0.8,
    )
    .await;

    let server = MockServer::start().await;
    // recorded 0.8, new 0.4 → drift 0.4 > default 0.10
    mount_belief_event(&server, belief_updated_event(claim_id, 0.4, Utc::now())).await;

    let worker = build_worker(
        pool.clone(),
        &server,
        "staleness_worker_test_drift_triggers",
    );
    let mut wm: Option<DateTime<Utc>> = None;
    worker.tick(&mut wm).await.expect("tick succeeds");

    let s = SynthesisRepository::get_by_id(&pool, synthesis_id)
        .await
        .expect("get synthesis");
    assert!(s.stale_since.is_some(), "stale_since should be set");
    assert_eq!(
        s.stale_reason.as_deref(),
        Some("belief_drift"),
        "stale_reason should be belief_drift"
    );

    let events = SynthesisStalenessRepository::list_for_synthesis(&pool, synthesis_id)
        .await
        .expect("list staleness events");
    assert_eq!(events.len(), 1, "exactly one staleness event recorded");
    assert_eq!(events[0].trigger, "belief_drift");
    assert!(events[0].affected_claim_ids.contains(&claim_id));

    cleanup_synthesis(&pool, synthesis_id).await;
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind("staleness_worker_test_drift_triggers")
        .execute(&pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: belief_drift < epsilon → no change
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn belief_drift_below_epsilon_does_not_trigger() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let claim_id = Uuid::now_v7();

    seed_complete_synthesis(
        &pool,
        synthesis_id,
        owner,
        claim_id,
        /* recorded */ 0.80,
    )
    .await;

    let server = MockServer::start().await;
    // recorded 0.80, new 0.85 → drift 0.05 < default 0.10
    mount_belief_event(&server, belief_updated_event(claim_id, 0.85, Utc::now())).await;

    let worker = build_worker(pool.clone(), &server, "staleness_worker_test_below_epsilon");
    let mut wm: Option<DateTime<Utc>> = None;
    worker.tick(&mut wm).await.expect("tick succeeds");

    let s = SynthesisRepository::get_by_id(&pool, synthesis_id)
        .await
        .expect("get synthesis");
    assert!(
        s.stale_since.is_none(),
        "stale_since should NOT be set for sub-epsilon drift"
    );

    let events = SynthesisStalenessRepository::list_for_synthesis(&pool, synthesis_id)
        .await
        .expect("list staleness events");
    assert!(
        events.is_empty(),
        "no staleness event should be recorded for sub-epsilon drift"
    );

    cleanup_synthesis(&pool, synthesis_id).await;
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind("staleness_worker_test_below_epsilon")
        .execute(&pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: event for unrelated claim → no change
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn belief_update_for_unrelated_claim_does_not_trigger() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let claim_x = Uuid::now_v7();
    let claim_y = Uuid::now_v7();

    seed_complete_synthesis(&pool, synthesis_id, owner, claim_x, 0.80).await;

    let server = MockServer::start().await;
    // Event references claim_y, which the synthesis does NOT cite.
    mount_belief_event(&server, belief_updated_event(claim_y, 0.10, Utc::now())).await;

    let worker = build_worker(
        pool.clone(),
        &server,
        "staleness_worker_test_unrelated_claim",
    );
    let mut wm: Option<DateTime<Utc>> = None;
    worker.tick(&mut wm).await.expect("tick succeeds");

    let s = SynthesisRepository::get_by_id(&pool, synthesis_id)
        .await
        .expect("get synthesis");
    assert!(
        s.stale_since.is_none(),
        "synthesis must not be marked stale for an unrelated claim's belief update"
    );

    cleanup_synthesis(&pool, synthesis_id).await;
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind("staleness_worker_test_unrelated_claim")
        .execute(&pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3b (Task 4.5): startup reconciliation drains pre-existing events
//
// Regression for Task 4.5 ("watermark catchup"). Pre-stages a
// `belief.updated` event whose timestamp is 30 minutes in the past — i.e.
// representative of an event that accumulated upstream while the worker was
// offline — and asserts that a single `tick(&mut None)` call still picks
// it up and marks the synthesis stale. This pins the property that the
// drain loop's first iteration runs BEFORE its first sleep; if a future
// refactor inserts a leading sleep the synthesis would remain non-stale
// after one tick, and this test would fail. (Distinct from
// `belief_drift_triggers_stale`, which uses `Utc::now()` as the event ts —
// that test doesn't differentiate "first tick processes pre-existing
// events" from "first tick processes only events generated after worker
// start".)
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn watermark_catchup_processes_pre_existing_events() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let claim_id = Uuid::now_v7();

    seed_complete_synthesis(
        &pool,
        synthesis_id,
        owner,
        claim_id,
        /* recorded */ 0.8,
    )
    .await;

    // Event ts is 30 minutes in the past — older than "now" but newer than
    // a fresh worker's None watermark. With no leading sleep, the first
    // tick must still drain it.
    let event_ts = Utc::now() - chrono::Duration::minutes(30);
    let server = MockServer::start().await;
    mount_belief_event(&server, belief_updated_event(claim_id, 0.4, event_ts)).await;

    let worker = build_worker(pool.clone(), &server, "staleness_worker_test_catchup");
    let mut wm: Option<DateTime<Utc>> = None;
    worker.tick(&mut wm).await.expect("tick succeeds");

    // The drain happened on the first iteration → synthesis is stale.
    let s = SynthesisRepository::get_by_id(&pool, synthesis_id)
        .await
        .expect("get synthesis");
    assert!(
        s.stale_since.is_some(),
        "first tick should drain pre-existing events and mark stale"
    );
    assert_eq!(s.stale_reason.as_deref(), Some("belief_drift"));

    // Watermark should be the event's ts (+1µs), NOT Utc::now(). This pins
    // the "advance to last event ts, not wall clock" semantic.
    let new_wm = wm.expect("watermark advances");
    assert!(
        new_wm > event_ts && new_wm < Utc::now(),
        "watermark should track the event ts (event_ts={event_ts}, wm={new_wm}, now={})",
        Utc::now()
    );

    cleanup_synthesis(&pool, synthesis_id).await;
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind("staleness_worker_test_catchup")
        .execute(&pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 4: watermark advances (no events → unchanged; events → advances)
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn watermark_advances_after_tick() {
    let pool = connect().await;
    let worker_name = "staleness_worker_test_watermark";

    // Reset worker_state for this worker name to a known empty state in case
    // a prior run left a row behind.
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind(worker_name)
        .execute(&pool)
        .await
        .ok();

    // Empty-events tick: watermark stays None.
    {
        let server = MockServer::start().await;
        mount_no_events(&server).await;
        let worker = build_worker(pool.clone(), &server, worker_name);
        let mut wm: Option<DateTime<Utc>> = None;
        worker.tick(&mut wm).await.expect("tick (empty) succeeds");
        assert!(
            wm.is_none(),
            "watermark should remain None when no events are returned"
        );
        let stored = WorkerStateRepository::get(&pool, worker_name)
            .await
            .expect("worker_state get");
        assert!(
            stored.is_none(),
            "no worker_state row should be persisted on an empty tick"
        );
    }

    // Single-event tick: watermark advances to event_ts + 1µs.
    let synthesis_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let claim_id = Uuid::now_v7();
    seed_complete_synthesis(&pool, synthesis_id, owner, claim_id, 0.80).await;

    let event_ts = Utc::now();
    {
        let server = MockServer::start().await;
        // sub-epsilon so we don't conflate with the drift assertions; we
        // only care about watermark + worker_state here.
        mount_belief_event(&server, belief_updated_event(claim_id, 0.82, event_ts)).await;
        let worker = build_worker(pool.clone(), &server, worker_name);
        let mut wm: Option<DateTime<Utc>> = None;
        worker.tick(&mut wm).await.expect("tick (event) succeeds");
        let new_wm = wm.expect("watermark should advance");
        assert!(
            new_wm > event_ts,
            "watermark must advance past the event ts (got {new_wm} <= {event_ts})"
        );
        let stored = WorkerStateRepository::get(&pool, worker_name)
            .await
            .expect("worker_state get")
            .expect("worker_state row should exist after a non-empty tick");
        assert!(
            stored.last_event_ts.is_some(),
            "worker_state.last_event_ts should be populated"
        );
        assert!(
            stored.last_event_id.is_some(),
            "worker_state.last_event_id should be populated"
        );
    }

    cleanup_synthesis(&pool, synthesis_id).await;
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind(worker_name)
        .execute(&pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 5 (Task 5.4 — burst idempotence): 50 belief.updated events for the
// same claim drained in one tick → exactly 1 staleness row, exactly 1
// stale_since transition.
//
// Pins two layered idempotence guards in the worker:
//
//   1. `evaluate_synthesis` (jobs/staleness_worker.rs:207-241) `break`s on
//      the FIRST event that classifies as a trigger, so repeated events
//      for the same synthesis in a single tick produce at most one
//      `record_event` call.
//   2. `SynthesisRepository::mark_stale` (synthesis.rs:264-274) is
//      `WHERE stale_since IS NULL`, so a second mark_stale would be a
//      no-op even if guard #1 ever regressed.
//
// Without both, a noisy claim — e.g. an upstream BetP that wobbles
// rapidly during DST combination — would produce dozens of staleness
// rows for a single semantic transition, breaking the
// "one stale event per drift" UI contract.
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn belief_drift_burst_creates_one_staleness_row() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let claim_id = Uuid::now_v7();

    seed_complete_synthesis(
        &pool,
        synthesis_id,
        owner,
        claim_id,
        /* recorded */ 0.8,
    )
    .await;

    // 50 events for the same claim, all crossing the drift threshold
    // (recorded 0.8, new 0.4 → drift 0.4 > default 0.10). Timestamps
    // increase monotonically by 10 ms so the upstream `since` filter
    // and watermark advance cleanly.
    let base = Utc::now() - chrono::Duration::seconds(5);
    let events: Vec<serde_json::Value> = (0..50)
        .map(|i| {
            let ts = base + chrono::Duration::milliseconds(i * 10);
            belief_updated_event(claim_id, 0.4, ts)
        })
        .collect();

    let server = MockServer::start().await;
    mount_belief_events_batch(&server, events).await;

    let worker = build_worker(pool.clone(), &server, "staleness_worker_test_burst");
    let mut wm: Option<DateTime<Utc>> = None;
    worker.tick(&mut wm).await.expect("tick succeeds");

    // Synthesis is stale exactly once.
    let s = SynthesisRepository::get_by_id(&pool, synthesis_id)
        .await
        .expect("get synthesis");
    assert!(
        s.stale_since.is_some(),
        "synthesis should be marked stale after the burst"
    );
    assert_eq!(s.stale_reason.as_deref(), Some("belief_drift"));

    // ONE staleness row, not 50. This is the load-bearing assertion.
    let events = SynthesisStalenessRepository::list_for_synthesis(&pool, synthesis_id)
        .await
        .expect("list staleness events");
    assert_eq!(
        events.len(),
        1,
        "burst of 50 events for the same claim must produce exactly 1 staleness row, got {}: {events:?}",
        events.len()
    );
    assert_eq!(events[0].trigger, "belief_drift");
    assert!(events[0].affected_claim_ids.contains(&claim_id));

    // Sanity: a second tick (same mock, same events) must not add rows
    // either — even if guard #1 regressed, mark_stale's
    // WHERE stale_since IS NULL would still hold. (We can't easily
    // retest record_event's lack of dedup without flipping the row
    // back to non-stale, so this second-tick check primarily exercises
    // the synthesis-already-stale fast path: `syntheses_citing` filters
    // on `only_complete_non_stale = true`, so the second tick should
    // see zero target syntheses and not call evaluate_synthesis at all.)
    worker.tick(&mut wm).await.expect("second tick succeeds");
    let events = SynthesisStalenessRepository::list_for_synthesis(&pool, synthesis_id)
        .await
        .expect("list staleness events after second tick");
    assert_eq!(
        events.len(),
        1,
        "second tick must not add staleness rows (already-stale fast path)"
    );

    cleanup_synthesis(&pool, synthesis_id).await;
    sqlx::query("DELETE FROM episcience_worker_state WHERE worker_id = $1")
        .bind("staleness_worker_test_burst")
        .execute(&pool)
        .await
        .ok();
}
