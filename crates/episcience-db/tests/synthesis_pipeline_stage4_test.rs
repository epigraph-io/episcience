//! Stage 4 (`stage4_narrate`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! Same as Stage 2/3: targets the live `epigraph_dev_synthesis` database. We
//! pre-insert a `syntheses` row plus one `synthesis_clusters` row with empty
//! title/summary, then run `stage4_narrate` against an LLM-mock that returns
//! a queue of canned responses. The mock is the upstream
//! `epigraph_cli::enrichment::llm_client::MockLlmClient::with_responses(...)`
//! which we use directly — no need to reimplement.
//!
//! # Tests
//!
//! 1. `stage4_narrate_validates_claim_ids_in_response` — happy path.
//!    Queue returns one valid `{title, summary}` JSON whose summary cites
//!    the cluster's known member id. Assert the returned cluster has
//!    non-empty title/summary, the persisted row in `synthesis_clusters`
//!    matches, and exactly one LLM call was made (`llm_call_count == 1`).
//!
//! 2. `stage4_narrate_retries_on_hallucinated_claim_id` — retry path.
//!    First response cites `[FAKE-ID...]` (a valid-format UUID not in the
//!    cluster); second cites the real member id. Assert success on the
//!    second call, `llm_call_count == 2`, and that the persisted summary
//!    matches the second response.
//!
//! 3. `stage4_narrate_fails_after_two_retries` — terminal failure.
//!    Both responses cite hallucinated ids. Assert
//!    `Err(SynthesisError::HallucinatedClaimId(_))` and `llm_call_count == 2`
//!    (one initial + one retry).

use std::sync::Arc;

use async_trait::async_trait;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
use episcience_core::synthesis::Cluster;
use episcience_db::{SynthesisClustersRepository, SynthesisPipeline};
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::MockLlmClient;
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};

// ──────────────────────────────────────────────────────────────────────────────
// Test doubles for L (we use upstream MockLlmClient directly), Embedder, and P.
// Stage 4 doesn't actually invoke embedder or edge-provider — they exist only
// to satisfy SynthesisPipeline's generic parameters.
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct ConstantEmbedder {
    embedding: Vec<f32>,
}

impl Default for ConstantEmbedder {
    fn default() -> Self {
        Self {
            embedding: vec![1.0; 8],
        }
    }
}

#[async_trait]
impl EmbeddingService for ConstantEmbedder {
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

struct UnusedEdgeProvider;

#[async_trait]
impl EdgeProvider for UnusedEdgeProvider {
    async fn neighbors(&self, _claim: Uuid, _types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
        vec![]
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
    let _ = sqlx::query("DELETE FROM synthesis_clusters WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
}

fn build_pipeline(
    pool: PgPool,
    llm: MockLlmClient,
) -> SynthesisPipeline<MockLlmClient, UnusedEdgeProvider> {
    SynthesisPipeline::new(
        pool,
        Arc::new(ConstantEmbedder::default()),
        llm,
        UnusedEdgeProvider,
        vec![1.0; 8],
        // cost_budget = 10 — generous enough for retries; spec default 20.
        10,
    )
}

/// Insert one cluster row directly (skipping Stage 3) so Stage 4 has something
/// to narrate. Returns the cluster with empty title/summary that the caller
/// passes to `stage4_narrate`. Member ids must be lowercase-hex UUIDs because
/// the validator regex `[0-9a-f-]{36}` matches lowercase only.
async fn insert_cluster(
    pool: &PgPool,
    synthesis_id: Uuid,
    cluster_index: i32,
    member_claim_ids: Vec<Uuid>,
) -> Cluster {
    let c = Cluster {
        id: Uuid::now_v7(),
        synthesis_id,
        cluster_index,
        title: String::new(),
        summary: String::new(),
        member_claim_ids,
        support_count: 0,
        contradict_count: 0,
    };
    SynthesisClustersRepository::insert(pool, &c)
        .await
        .expect("insert cluster row");
    c
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Happy path: LLM returns valid JSON whose summary cites a real member id.
/// Stage 4 should populate title/summary and persist via `update_text`.
#[tokio::test]
async fn stage4_narrate_validates_claim_ids_in_response() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage4 happy-path test").await;

    // The cluster's member id — uses a lowercase-hex Uuid so the validator
    // regex `[0-9a-f-]{36}` matches it. Uuid::now_v7().to_string() is already
    // lowercase by default, but be explicit to document the constraint.
    let member_id = Uuid::now_v7();
    let cluster =
        insert_cluster(&pool, synthesis_id, 0, vec![member_id]).await;

    let summary_text = format!(
        "Strong evidence supports the result, see [{}] for details.",
        member_id
    );
    let llm = MockLlmClient::with_responses(vec![serde_json::json!({
        "title": "Origami melts at low temperature",
        "summary": summary_text,
    })]);
    let mut pipeline = build_pipeline(pool.clone(), llm);

    let updated = pipeline
        .stage4_narrate(synthesis_id, &[cluster.clone()])
        .await
        .expect("stage4_narrate should succeed on valid response");

    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].title, "Origami melts at low temperature");
    assert!(
        updated[0].summary.contains(&member_id.to_string()),
        "summary should retain the cited member id, got {:?}",
        updated[0].summary
    );
    assert_eq!(
        pipeline.llm_call_count, 1,
        "happy path should make exactly 1 LLM call"
    );

    // Persistence check: the DB row should have the new title/summary.
    let (db_title, db_summary): (String, String) =
        sqlx::query_as("SELECT title, summary FROM synthesis_clusters WHERE id = $1")
            .bind(cluster.id)
            .fetch_one(&pool)
            .await
            .expect("fetch persisted cluster");
    assert_eq!(db_title, "Origami melts at low temperature");
    assert!(db_summary.contains(&member_id.to_string()));

    cleanup(&pool, synthesis_id).await;
}

/// Retry path: first response cites a valid-format UUID NOT in the cluster
/// (treated as hallucination). Second response cites a real member. Stage 4
/// should retry once and succeed; `llm_call_count == 2`.
#[tokio::test]
async fn stage4_narrate_retries_on_hallucinated_claim_id() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage4 retry test").await;

    let member_id = Uuid::now_v7();
    let cluster =
        insert_cluster(&pool, synthesis_id, 0, vec![member_id]).await;

    // First response: a syntactically valid UUID (lowercase, 36 chars with
    // dashes) that is NOT a cluster member → validator rejects, retry fires.
    let bogus_id = Uuid::now_v7();
    assert_ne!(bogus_id, member_id, "test setup: ids must differ");
    let bad_summary = format!("Citing fabricated source [{}].", bogus_id);
    let good_summary = format!("Real citation [{}] is well-supported.", member_id);

    let llm = MockLlmClient::with_responses(vec![
        serde_json::json!({"title": "draft", "summary": bad_summary}),
        serde_json::json!({"title": "Final summary", "summary": good_summary.clone()}),
    ]);
    let mut pipeline = build_pipeline(pool.clone(), llm);

    let updated = pipeline
        .stage4_narrate(synthesis_id, &[cluster.clone()])
        .await
        .expect("stage4_narrate should succeed after one retry");

    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].title, "Final summary");
    assert!(
        updated[0].summary.contains(&member_id.to_string()),
        "summary should be the second (good) response, got {:?}",
        updated[0].summary
    );
    assert!(
        !updated[0].summary.contains(&bogus_id.to_string()),
        "summary should NOT contain the hallucinated id from the first response"
    );
    assert_eq!(
        pipeline.llm_call_count, 2,
        "retry path should make exactly 2 LLM calls (initial + 1 retry)"
    );

    // Persistence check.
    let (db_title, db_summary): (String, String) =
        sqlx::query_as("SELECT title, summary FROM synthesis_clusters WHERE id = $1")
            .bind(cluster.id)
            .fetch_one(&pool)
            .await
            .expect("fetch persisted cluster");
    assert_eq!(db_title, "Final summary");
    assert_eq!(db_summary, good_summary);

    cleanup(&pool, synthesis_id).await;
}

/// Terminal failure: both responses hallucinate. Stage 4 should bail with
/// `HallucinatedClaimId` after 1 initial + 1 retry, with no DB write.
#[tokio::test]
async fn stage4_narrate_fails_after_two_retries() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage4 terminal-failure test").await;

    let member_id = Uuid::now_v7();
    let cluster =
        insert_cluster(&pool, synthesis_id, 0, vec![member_id]).await;

    // Two distinct hallucinated UUIDs (both syntactically valid, neither in
    // member_claim_ids). The implementation does max_retries=1 → 2 attempts
    // total, so both will be exhausted and the second's error surfaces.
    let bogus_a = Uuid::now_v7();
    let bogus_b = Uuid::now_v7();
    let llm = MockLlmClient::with_responses(vec![
        serde_json::json!({"title": "x", "summary": format!("[{}]", bogus_a)}),
        serde_json::json!({"title": "x", "summary": format!("[{}]", bogus_b)}),
    ]);
    let mut pipeline = build_pipeline(pool.clone(), llm);

    let r = pipeline
        .stage4_narrate(synthesis_id, &[cluster.clone()])
        .await;

    match r {
        Err(SynthesisError::HallucinatedClaimId(id)) => {
            assert_eq!(
                id, bogus_b,
                "the surfaced hallucination should be the LAST attempt's id"
            );
        }
        other => panic!(
            "expected Err(HallucinatedClaimId(_)) after exhausting retries, got {:?}",
            other
        ),
    }
    assert_eq!(
        pipeline.llm_call_count, 2,
        "terminal failure should still consume exactly 2 LLM calls"
    );

    // The cluster's title/summary should remain empty (no DB write happened).
    let (db_title, db_summary): (String, String) =
        sqlx::query_as("SELECT title, summary FROM synthesis_clusters WHERE id = $1")
            .bind(cluster.id)
            .fetch_one(&pool)
            .await
            .expect("fetch persisted cluster");
    assert!(
        db_title.is_empty() && db_summary.is_empty(),
        "DB row should be untouched on terminal failure, got title={:?} summary={:?}",
        db_title,
        db_summary
    );

    cleanup(&pool, synthesis_id).await;
}
