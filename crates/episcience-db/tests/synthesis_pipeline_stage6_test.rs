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

use async_trait::async_trait;
use episcience_db::publish;
use episcience_db::SynthesisProvoEdgesRepository;
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use sqlx::PgPool;
use uuid::Uuid;

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
