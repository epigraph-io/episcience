//! Stage 1 (`stage1_seed`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! Each test runs on its own clone of the E1 template (`TestDb::fresh`): the
//! kernel schema at the pinned rev (built by the kernel's `epigraph-migrate`)
//! plus EpiScience's, seeded by scripts/ci-seed.sql with two PUBLIC
//! `origami melts at ...` claims (truth 0.8 / 0.85).
//!
//! # Embedding strategy
//!
//! The tests use an `ErroringEmbedder` whose `generate_query` always returns
//! `Err`, forcing `recall::recall` onto its text-search fallback (`ILIKE` on
//! `claims.content`, filtered by the viewer), so a unique sentinel string
//! returns exactly zero rows.
//!
//! # Viewer
//!
//! Stage 1 recalls AS the synthesis owner. `stage1_seed_excludes_claims_the_owner_cannot_read`
//! (T-J5s) pins that another principal's group-owned claim never seeds this
//! owner's synthesis.
mod support;
use support::TestDb;

use std::sync::Arc;

use async_trait::async_trait;
use epigraph_core::TenancyDecl;
use epigraph_db::Viewer;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
use episcience_db::SynthesisPipeline;
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
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
impl LlmProvider for MockLlmClient {
    fn name(&self) -> &str {
        "mock"
    }

    fn is_active(&self) -> bool {
        true
    }

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

/// The seed agent (scripts/ci-seed.sql) that authored the two public claims.
const SEED_AGENT: Uuid = Uuid::from_u128(0xf3951e28_9356_42b6_9c80_27dd9f01b19d);

fn build_pipeline(pool: PgPool) -> SynthesisPipeline<MockLlmClient, MockEdgeProvider> {
    SynthesisPipeline::new(
        pool,
        Arc::new(ErroringEmbedder),
        MockLlmClient,
        MockEdgeProvider,
        // Stage 1 doesn't read query_embedding; pass empty vec.
        vec![],
        // cost_budget — irrelevant to Stage 1 (no LLM calls); spec default.
        20,
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
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let viewer = Viewer::resolve(&pool, SEED_AGENT).await.expect("resolve");
    let pipeline = build_pipeline(pool);

    let seeds = pipeline
        .stage1_seed(&viewer, "origami", 50, 0.5)
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
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let viewer = Viewer::resolve(&pool, SEED_AGENT).await.expect("resolve");
    let pipeline = build_pipeline(pool);

    let r = pipeline
        .stage1_seed(&viewer, "never-occurring-string-xyz123", 50, 0.5)
        .await;

    assert!(
        matches!(r, Err(SynthesisError::EmptyResult)),
        "expected EmptyResult for sentinel query, got {:?}",
        r
    );
}

/// T-J5s: Stage 1 seeds for H2's synthesis never include H1's GROUP-owned
/// claim, while H1's own synthesis does seed from it (so the fixture is
/// findable and the exclusion is the viewer's doing, not a miss).
///
/// Kills: passing an unrestricted / wrong viewer to `recall` (for example the
/// seed agent or a bypass viewer instead of the synthesis owner), or dropping
/// the viewer from the stage entirely.
#[tokio::test]
async fn stage1_seed_excludes_claims_the_owner_cannot_read() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let h1 = support::principal(&pool, "h1").await;
    let h2 = support::principal(&pool, "h2").await;
    let private = support::claim(
        &pool,
        h1.agent,
        "origami folding notebook entry kept inside H1 personal group",
        0.9,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(
        support::claim_pair(&pool, private).await,
        ("group".to_string(), h1.personal_group),
        "fixture must really be group-owned, or the exclusion below is vacuous"
    );

    let h1_viewer = Viewer::resolve(&pool, h1.agent).await.expect("resolve h1");
    let h2_viewer = Viewer::resolve(&pool, h2.agent).await.expect("resolve h2");
    let pipeline = build_pipeline(pool);

    let h2_seeds = pipeline
        .stage1_seed(&h2_viewer, "origami", 50, 0.5)
        .await
        .expect("public origami claims still seed H2's synthesis");
    assert!(
        !h2_seeds.contains(&private),
        "H2's seeds must not include H1's group-owned claim"
    );
    assert!(
        h2_seeds.contains(&Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa)),
        "H2 still sees the public seed claim"
    );

    let h1_seeds = pipeline
        .stage1_seed(&h1_viewer, "origami", 50, 0.5)
        .await
        .expect("H1 seeds");
    assert!(
        h1_seeds.contains(&private),
        "H1's own synthesis seeds from H1's group-owned claim"
    );
}
