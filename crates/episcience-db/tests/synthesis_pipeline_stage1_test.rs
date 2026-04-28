//! Stage 1 (`stage1_seed`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! These tests target the live `epigraph_dev_synthesis` database (the same DB
//! used by `crates/episcience-api/tests/phase01_e2e_test.rs::test_phase0_library_recall_callable`).
//! That DB is migrated with the **upstream** epigraph schema (claims, evidence,
//! agents, frames, ...), which is what `epigraph_engine::recall::recall`
//! requires.
//!
//! Why not `#[sqlx::test(migrations = ...)]` like Phase 1's repo tests?
//! - The upstream `claims` and `evidence` tables (and their `embedding`
//!   `vector(1536)` columns plus pgvector extension) are not part of this
//!   repo's `migrations/` tree. The local migrations only contain additive
//!   ALTERs (5001-5010) and the synthesis subdir (5011+). Neither defines
//!   `claims` or `evidence`. Duplicating the upstream `001_initial_schema.sql`
//!   (~1900 lines) into this repo would couple us to upstream churn.
//! - Phase 0 already pre-seeds two `origami melts at ...` claims with truth
//!   values 0.8 / 0.85 in `epigraph_dev_synthesis`. We rely on those.
//!
//! # Embedding strategy
//!
//! Both tests use an `ErroringEmbedder` whose `generate_query` always returns
//! `Err`. This forces `recall::recall` onto the `text_search_fallback` path
//! (`ClaimRepository::list` with `ILIKE`). Why force the fallback?
//! - `MockProvider::generate_query` returns `Ok` deterministically, which
//!   takes recall onto `EvidenceRepository::search_by_embedding`. That returns
//!   the K-nearest evidence rows regardless of how unrelated the query is —
//!   so a "never-occurring-string-xyz123" query would still find non-empty
//!   results, breaking Test 2.
//! - The text-search fallback is ILIKE on `claims.content`, so a unique
//!   sentinel string returns exactly zero rows.

use std::sync::Arc;

use async_trait::async_trait;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
use episcience_db::SynthesisPipeline;
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::{LlmClient, LlmError};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};

// ──────────────────────────────────────────────────────────────────────────────
// Test doubles
// ──────────────────────────────────────────────────────────────────────────────

/// An embedder whose `generate_query` always errors. Forces `recall::recall`
/// onto the text-search fallback path so test outcomes are predictable.
#[derive(Debug, Default)]
struct ErroringEmbedder;

#[async_trait]
impl EmbeddingService for ErroringEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: generate disabled".to_string(),
            status_code: None,
        })
    }

    async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: batch_generate disabled".to_string(),
            status_code: None,
        })
    }

    async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
        Err(EmbeddingError::ApiError {
            message: "test stub: store disabled".to_string(),
            status_code: None,
        })
    }

    async fn get(&self, claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::NotFound { claim_id })
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
        1536
    }

    fn token_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }

    fn reset_token_usage(&self) {}

    async fn health_check(&self) -> Result<(), EmbeddingError> {
        Ok(())
    }

    async fn generate_query(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        // Force recall::recall onto its text-search fallback path.
        Err(EmbeddingError::ApiError {
            message: "test stub: generate_query disabled — use text fallback".to_string(),
            status_code: None,
        })
    }
}

#[derive(Debug, Default)]
struct MockLlmClient;

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete_json(&self, _prompt: &str) -> Result<serde_json::Value, LlmError> {
        Ok(serde_json::json!({}))
    }

    fn model_name(&self) -> &str {
        "mock"
    }
}

struct MockEdgeProvider;

#[async_trait]
impl EdgeProvider for MockEdgeProvider {
    async fn neighbors(&self, _claim: Uuid, _types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
        vec![]
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Connect to `epigraph_dev_synthesis` (upstream schema + Phase 0 seed data).
async fn connect_epigraph() -> PgPool {
    PgPool::connect("postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis")
        .await
        .expect("connect to epigraph_dev_synthesis")
}

fn build_pipeline(pool: PgPool) -> SynthesisPipeline<MockLlmClient, MockEdgeProvider> {
    SynthesisPipeline::new(
        pool,
        Arc::new(ErroringEmbedder),
        MockLlmClient,
        MockEdgeProvider,
        // Stage 1 doesn't read query_embedding; pass empty vec.
        vec![],
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 1 should return seed UUIDs for a query that matches pre-seeded claims.
///
/// The `epigraph_dev_synthesis` DB is pre-seeded with two claims containing
/// "origami" in their content. With `ErroringEmbedder` forcing the text-search
/// fallback, `query="origami"` runs `ILIKE '%origami%'` and matches both rows.
#[tokio::test]
async fn stage1_seed_returns_recall_results() {
    let pool = connect_epigraph().await;
    let pipeline = build_pipeline(pool);

    let seeds = pipeline
        .stage1_seed("origami", 50, 0.5)
        .await
        .expect("stage1_seed should succeed against pre-seeded DB");

    assert!(
        !seeds.is_empty(),
        "expected at least one seed for query 'origami', got {} results",
        seeds.len()
    );
    // Sanity: every returned id parses as a Uuid (the pipeline already does this,
    // but assert it here so the test fails loudly if the contract changes).
    for id in &seeds {
        assert_ne!(*id, Uuid::nil(), "seed id should be non-nil");
    }
}

/// Stage 1 should return `EmptyResult` when recall has no matches.
///
/// Forces text-search fallback (via `ErroringEmbedder`) and queries for a
/// sentinel string that cannot occur in claim content. The ILIKE query
/// returns zero rows, so `recall::recall` returns `Ok(vec![])`, and
/// `stage1_seed` maps that to `SynthesisError::EmptyResult`.
#[tokio::test]
async fn stage1_seed_empty_returns_error() {
    let pool = connect_epigraph().await;
    let pipeline = build_pipeline(pool);

    let r = pipeline
        .stage1_seed("never-occurring-string-xyz123", 50, 0.5)
        .await;

    assert!(
        matches!(r, Err(SynthesisError::EmptyResult)),
        "expected EmptyResult for sentinel query, got {:?}",
        r
    );
}
