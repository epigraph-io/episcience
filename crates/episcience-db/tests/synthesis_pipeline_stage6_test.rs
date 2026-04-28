//! Stage 6 (`publish::*`) integration tests.
//!
//! # DB strategy
//!
//! Targets the live `epigraph_dev_synthesis` database (same as Stages 2-5).
//! Each test creates its own `syntheses` row, exercises a single substep,
//! verifies expected DB state, and cleans up.
//!
//! # Tests
//!
//! Substep coverage matches the Task 2.7 spec. Tests are added incrementally
//! as each substep lands; the file grows commit-by-commit.
//!
//! 1. `stage6_plan_inserts_edges_for_each_cited_claim` — 2.7a happy path
//! 2. `stage6_plan_idempotent_on_retry` — 2.7a re-entry
//! 3. `stage6_embed_creates_synthesis_embeddings_row` — 2.7b
//! 4. `compute_content_hash_is_deterministic` — 2.7c determinism
//! 5. `compute_content_hash_changes_on_input_change` — 2.7c sensitivity
//! 6. `stage6_write_edges_marks_all_pending_written` — 2.7d
//! 7. `stage6_mark_complete_only_when_no_pending` — 2.7e
//! 8. `startup_reconciliation_replays_pending_edges_for_complete_synthesis` — 2.7f
//! 9. `stage6_happy_path_plan_embed_hash_write_complete` — integration walkthrough

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use episcience_core::synthesis::SubgraphSnapshot;
use episcience_db::publish;
use episcience_db::{EdgeRequest, EdgeWriter, EdgeWriterError, SynthesisProvoEdgesRepository};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use sqlx::PgPool;
use uuid::Uuid;

fn empty_snapshot() -> SubgraphSnapshot {
    SubgraphSnapshot {
        claim_ids: vec![],
        edge_ids: vec![],
        belief_intervals: vec![],
        traversal_config: serde_json::json!({}),
        captured_at: Utc::now(),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Test doubles
// ──────────────────────────────────────────────────────────────────────────────

/// Stub embedder that always returns a 1536-dim vector matching the
/// `synthesis_embeddings.embedding` column. We don't care what's in it —
/// just that it's the right size and deterministic.
#[derive(Debug)]
struct FixedEmbedder {
    embedding: Vec<f32>,
}

impl Default for FixedEmbedder {
    fn default() -> Self {
        // 1536 = epigraph's primary embedding dim (see migration 5013).
        Self {
            embedding: (0..1536).map(|i| (i as f32) * 1e-4).collect(),
        }
    }
}

#[async_trait]
impl EmbeddingService for FixedEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
    async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(vec![self.embedding.clone()])
    }
    async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
        Ok(())
    }
    async fn get(&self, _claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
    async fn similar(
        &self,
        _embedding: &[f32],
        _k: usize,
        _min_similarity: f32,
    ) -> Result<Vec<SimilarClaim>, EmbeddingError> {
        Ok(vec![])
    }
    fn dimension(&self) -> usize {
        self.embedding.len()
    }
    fn token_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }
    fn reset_token_usage(&self) {}
    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }
    async fn generate_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
}

/// In-process [`EdgeWriter`] stub. Returns a fresh UUID for every call,
/// records the request, and (optionally) fails on demand.
struct FakeEdgeWriter {
    /// All requests this writer has seen, in call order.
    seen: Mutex<Vec<EdgeRequest>>,
    /// If true, every call returns `ServiceUnavailable("forced failure")`.
    fail: Mutex<bool>,
}

impl FakeEdgeWriter {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            fail: Mutex::new(false),
        }
    }

    #[allow(dead_code)]
    fn set_fail(&self, fail: bool) {
        *self.fail.lock().unwrap() = fail;
    }

    fn call_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl EdgeWriter for FakeEdgeWriter {
    async fn create_edge(&self, req: EdgeRequest) -> Result<Uuid, EdgeWriterError> {
        let fail = *self.fail.lock().unwrap();
        self.seen.lock().unwrap().push(req);
        if fail {
            Err(EdgeWriterError::ServiceUnavailable("forced failure".into()))
        } else {
            Ok(Uuid::now_v7())
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

async fn connect_epigraph() -> PgPool {
    PgPool::connect("postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis")
        .await
        .expect("connect to epigraph_dev_synthesis")
}

fn test_agent_id() -> Uuid {
    "f3951e28-9356-42b6-9c80-27dd9f01b19d".parse().unwrap()
}

async fn insert_synthesis_row(pool: &PgPool, synthesis_id: Uuid, query: &str) {
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility)
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock',
                 $4, 'private')",
    )
    .bind(synthesis_id)
    .bind(query)
    .bind(test_agent_id())
    .bind(&[0u8; 32][..])
    .execute(pool)
    .await
    .expect("insert synthesis row");
}

/// Force a synthesis row to `status='complete'`. The table CHECK constraint
/// requires `narrative IS NOT NULL` and `completed_at IS NOT NULL` whenever
/// status='complete', so we set those alongside in a single UPDATE.
async fn force_complete(pool: &PgPool, synthesis_id: Uuid) {
    sqlx::query(
        "UPDATE syntheses
         SET status = 'complete',
             narrative = COALESCE(narrative, 'placeholder narrative'),
             narrative_format = 'markdown',
             completed_at = COALESCE(completed_at, now())
         WHERE id = $1",
    )
    .bind(synthesis_id)
    .execute(pool)
    .await
    .expect("force complete");
}

async fn cleanup(pool: &PgPool, synthesis_id: Uuid) {
    let _ = sqlx::query("DELETE FROM synthesis_provo_edges WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM synthesis_embeddings WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7a — stage6_plan_edges
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_plan_inserts_edges_for_each_cited_claim() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6a happy path").await;

    let claim_a = Uuid::now_v7();
    let claim_b = Uuid::now_v7();

    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[claim_a, claim_b],
        None,
        &[],
        test_agent_id(),
    )
    .await
    .expect("stage6_plan_edges happy path");

    let pending = SynthesisProvoEdgesRepository::list_pending(&pool, synthesis_id)
        .await
        .expect("list_pending");
    // 2 WAS_DERIVED_FROM (one per cited claim) + 1 ATTRIBUTED_TO = 3.
    assert_eq!(
        pending.len(),
        3,
        "expected 2 cited-claim edges + 1 ATTRIBUTED_TO, got {pending:?}"
    );

    let predicates: Vec<&str> = pending.iter().map(|e| e.predicate.as_str()).collect();
    assert_eq!(
        predicates.iter().filter(|p| **p == "WAS_DERIVED_FROM").count(),
        2,
        "expected 2 WAS_DERIVED_FROM rows, got {predicates:?}"
    );
    assert_eq!(
        predicates.iter().filter(|p| **p == "ATTRIBUTED_TO").count(),
        1,
        "expected 1 ATTRIBUTED_TO row, got {predicates:?}"
    );

    cleanup(&pool, synthesis_id).await;
}

#[tokio::test]
async fn stage6_plan_idempotent_on_retry() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6a idempotency").await;

    let claim_a = Uuid::now_v7();
    let parent = Uuid::now_v7();

    // First invocation: 1 cited + 1 parent (REFINES) + 1 ATTRIBUTED_TO = 3.
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[claim_a],
        Some(parent),
        &[],
        test_agent_id(),
    )
    .await
    .expect("stage6_plan_edges first call");

    let n1 = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count first");
    assert_eq!(n1, 3, "first call should plan 3 edges");

    // Second invocation with identical args — must succeed and not duplicate.
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[claim_a],
        Some(parent),
        &[],
        test_agent_id(),
    )
    .await
    .expect("stage6_plan_edges second call (idempotent)");

    let n2 = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count second");
    assert_eq!(n2, n1, "second call must not insert duplicates");

    cleanup(&pool, synthesis_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7b — stage6_embed_narrative
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_embed_creates_synthesis_embeddings_row() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6b embed").await;

    let embedder = FixedEmbedder::default();
    let narrative = "First paragraph thesis sentence.\n\nSecond paragraph detail.";

    publish::stage6_embed_narrative(
        &pool,
        &embedder,
        synthesis_id,
        narrative,
        "test-embedding-model-v1",
    )
    .await
    .expect("stage6_embed_narrative happy path");

    let (model, input): (String, String) = sqlx::query_as(
        "SELECT embedding_model, embedding_input FROM synthesis_embeddings WHERE synthesis_id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("fetch embedding row");

    assert_eq!(model, "test-embedding-model-v1");
    assert_eq!(
        input, "narrative_head",
        "embedding_input should be 'narrative_head' per spec"
    );

    cleanup(&pool, synthesis_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7c — compute_content_hash
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn compute_content_hash_is_deterministic() {
    let snap = empty_snapshot();
    let h1 = publish::compute_content_hash("query", &snap, "narrative");
    let h2 = publish::compute_content_hash("query", &snap, "narrative");
    assert_eq!(h1, h2, "identical inputs must produce identical hashes");
    // Sanity: BLAKE3 zero-input hash is well-known nonzero. We just require
    // SOME nonzero bytes; if every byte is zero something pathological
    // happened (e.g. accidentally zeroing the buffer).
    assert!(
        h1.iter().any(|b| *b != 0),
        "hash should not be all-zero, got {h1:?}"
    );
}

#[tokio::test]
async fn compute_content_hash_changes_on_input_change() {
    let snap = empty_snapshot();
    let base = publish::compute_content_hash("query", &snap, "narrative");
    let changed_query = publish::compute_content_hash("QUERY", &snap, "narrative");
    let changed_narrative = publish::compute_content_hash("query", &snap, "Narrative");
    let mut snap2 = empty_snapshot();
    snap2.claim_ids.push(Uuid::nil());
    let changed_snapshot = publish::compute_content_hash("query", &snap2, "narrative");

    assert_ne!(
        base, changed_query,
        "different query must change the hash"
    );
    assert_ne!(
        base, changed_narrative,
        "different narrative must change the hash"
    );
    assert_ne!(
        base, changed_snapshot,
        "different snapshot must change the hash"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7d — stage6_write_edges
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_write_edges_marks_all_pending_written() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6d write edges").await;

    // Pre-plan a small edge set: 2 cited claims + ATTRIBUTED_TO = 3.
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[Uuid::now_v7(), Uuid::now_v7()],
        None,
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan edges");

    let writer = FakeEdgeWriter::new();
    publish::stage6_write_edges(&pool, &writer, synthesis_id)
        .await
        .expect("stage6_write_edges happy path");

    let remaining = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count_pending after write");
    assert_eq!(remaining, 0, "all edges should be written");
    assert_eq!(
        writer.call_count(),
        3,
        "writer should have received one call per planned edge"
    );

    cleanup(&pool, synthesis_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7e — stage6_mark_complete
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_mark_complete_only_when_no_pending() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6e mark complete").await;

    // Plan some edges but DON'T write them.
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[Uuid::now_v7()],
        None,
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan edges");

    // First attempt: must refuse because edges are still pending.
    let hash = [42u8; 32];
    let r = publish::stage6_mark_complete(&pool, synthesis_id, "narrative", &hash).await;
    match r {
        Err(episcience_core::synthesis::errors::SynthesisError::EdgeWrite(msg)) => {
            assert!(
                msg.contains("pending"),
                "error should mention pending edges, got {msg:?}"
            );
        }
        other => panic!("expected EdgeWrite error, got {other:?}"),
    }

    // Now write them via the fake writer.
    let writer = FakeEdgeWriter::new();
    publish::stage6_write_edges(&pool, &writer, synthesis_id)
        .await
        .expect("write edges");

    // Second attempt: must succeed and set status='complete'.
    publish::stage6_mark_complete(&pool, synthesis_id, "narrative", &hash)
        .await
        .expect("mark complete after writing edges");

    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .fetch_one(&pool)
        .await
        .expect("fetch status");
    assert_eq!(status, "complete");

    cleanup(&pool, synthesis_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7f — reconcile_stage6_on_startup
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn startup_reconciliation_replays_pending_edges_for_complete_synthesis() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6f reconcile").await;

    // Manufacture: synthesis is 'complete' but provo edges are pending.
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[Uuid::now_v7()],
        None,
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan edges");
    force_complete(&pool, synthesis_id).await;

    let n0 = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count_pending before reconcile");
    assert!(n0 > 0, "test setup: should have pending edges");

    // Reconcile drains them.
    let writer = FakeEdgeWriter::new();
    publish::reconcile_stage6_on_startup(&pool, &writer)
        .await
        .expect("reconcile happy path");

    let n1 = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count_pending after reconcile");
    assert_eq!(n1, 0, "reconcile should drain all pending edges");
    assert!(
        writer.call_count() >= n0 as usize,
        "writer should have been called at least once per pending edge"
    );

    cleanup(&pool, synthesis_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Integration: full Stage 6 happy path
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_happy_path_plan_embed_hash_write_complete() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    let query = "what is well-supported about X?";
    insert_synthesis_row(&pool, synthesis_id, query).await;

    let claim_a = Uuid::now_v7();
    let narrative = "Lead paragraph stating thesis.\n\nDetail paragraph.";
    let snap = empty_snapshot();

    // 1. Plan
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[claim_a],
        None,
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan");

    // 2. Embed
    let embedder = FixedEmbedder::default();
    publish::stage6_embed_narrative(&pool, &embedder, synthesis_id, narrative, "stub-model-1")
        .await
        .expect("embed");

    // 3. Hash
    let hash = publish::compute_content_hash(query, &snap, narrative);

    // 4. Write
    let writer = FakeEdgeWriter::new();
    publish::stage6_write_edges(&pool, &writer, synthesis_id)
        .await
        .expect("write");

    // 5. Mark complete
    publish::stage6_mark_complete(&pool, synthesis_id, narrative, &hash)
        .await
        .expect("mark complete");

    // Verify final state.
    let (status, persisted_narrative, db_hash): (String, Option<String>, Vec<u8>) =
        sqlx::query_as("SELECT status, narrative, content_hash FROM syntheses WHERE id = $1")
            .bind(synthesis_id)
            .fetch_one(&pool)
            .await
            .expect("fetch final");
    assert_eq!(status, "complete");
    assert_eq!(persisted_narrative.as_deref(), Some(narrative));
    assert_eq!(db_hash.as_slice(), &hash[..]);

    cleanup(&pool, synthesis_id).await;
}
