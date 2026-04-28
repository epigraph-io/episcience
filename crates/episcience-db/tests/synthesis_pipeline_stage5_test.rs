//! Stage 5 (`stage5_compose`) integration tests for `SynthesisPipeline`.
//!
//! # DB strategy
//!
//! Stage 5 doesn't touch the DB: it takes a `synthesis_id` for symmetry with
//! the other stages but only validates LLM output and returns the stripped
//! narrative. We still construct a `SynthesisPipeline` (which requires a
//! `PgPool`), so we connect to `epigraph_dev_synthesis` to match the Stage 1-4
//! test pattern, and we do NOT insert a `syntheses` row.
//!
//! # Tests
//!
//! 1. `stage5_compose_validates_cluster_byte_equality` — happy path.
//!    LLM returns a Markdown narrative that wraps each cluster's summary
//!    verbatim inside its `<<<CLUSTER:{id}:BEGIN>>>...<<<CLUSTER:{id}:END>>>`
//!    sentinels. Stage 5 strips the sentinels and returns the cleaned text.
//!
//! 2. `stage5_compose_anchor_violation_after_two_attempts_fails` — failure.
//!    Both responses mutate the cluster summary inside the anchors. Stage 5
//!    should retry once, then return `ComposeAnchorViolation { cluster_id }`.
//!
//! 3. `stage5_compose_anchor_missing_returns_violation` — missing-anchor path.
//!    Both responses omit the END sentinel. Same terminal-failure semantics.

use std::sync::Arc;

use async_trait::async_trait;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
use episcience_core::synthesis::Cluster;
use episcience_db::SynthesisPipeline;
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::MockLlmClient;
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};

// ──────────────────────────────────────────────────────────────────────────────
// Test doubles. Stage 5 does not invoke embedder or edge provider — these only
// satisfy `SynthesisPipeline`'s generic parameters.
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

fn make_cluster(synthesis_id: Uuid, summary: &str) -> Cluster {
    Cluster {
        id: Uuid::now_v7(),
        synthesis_id,
        cluster_index: 0,
        title: "Topic title".to_string(),
        summary: summary.to_string(),
        member_claim_ids: vec![],
        support_count: 0,
        contradict_count: 0,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Happy path: LLM embeds the cluster summary verbatim between sentinels;
/// Stage 5 strips the sentinels and returns the surrounding narrative + body.
#[tokio::test]
async fn stage5_compose_validates_cluster_byte_equality() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();

    let cluster_summary = "[claim-1] Evidence shows X.";
    let cluster = make_cluster(synthesis_id, cluster_summary);

    let narrative_in = format!(
        "# Topic\n\nFraming.\n\n<<<CLUSTER:{id}:BEGIN>>>{summary}<<<CLUSTER:{id}:END>>>\n\n## Open questions\n\n- ...",
        id = cluster.id,
        summary = cluster_summary,
    );
    let llm = MockLlmClient::with_responses(vec![serde_json::json!({
        "narrative": narrative_in,
    })]);
    let mut pipeline = build_pipeline(pool.clone(), llm);

    let narrative = pipeline
        .stage5_compose(
            synthesis_id,
            "what do we know about X?",
            std::slice::from_ref(&cluster),
        )
        .await
        .expect("stage5_compose should succeed when sentinels match verbatim");

    assert!(
        !narrative.contains("<<<CLUSTER"),
        "anchors must be stripped from the returned narrative, got {:?}",
        narrative
    );
    assert!(
        narrative.contains(cluster_summary),
        "returned narrative should retain the cluster summary text, got {:?}",
        narrative
    );
    assert!(
        narrative.contains("# Topic"),
        "returned narrative should retain the framing markdown, got {:?}",
        narrative
    );
    assert_eq!(
        pipeline.llm_call_count, 1,
        "happy path should make exactly 1 LLM call"
    );
}

/// Failure path: both LLM responses mutate the cluster summary inside the
/// anchors. Stage 5 should retry once and then return `ComposeAnchorViolation`.
#[tokio::test]
async fn stage5_compose_anchor_violation_after_two_attempts_fails() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();

    let cluster_summary = "[claim-1] Evidence shows X.";
    let cluster = make_cluster(synthesis_id, cluster_summary);

    // Both responses replace the verbatim summary with "MUTATED" between the
    // sentinels — anchors are present and well-formed but the body diverges.
    let bad_a = format!(
        "# Topic\n\n<<<CLUSTER:{id}:BEGIN>>>MUTATED A<<<CLUSTER:{id}:END>>>",
        id = cluster.id,
    );
    let bad_b = format!(
        "# Topic\n\n<<<CLUSTER:{id}:BEGIN>>>MUTATED B<<<CLUSTER:{id}:END>>>",
        id = cluster.id,
    );
    let llm = MockLlmClient::with_responses(vec![
        serde_json::json!({"narrative": bad_a}),
        serde_json::json!({"narrative": bad_b}),
    ]);
    let mut pipeline = build_pipeline(pool.clone(), llm);

    let r = pipeline
        .stage5_compose(synthesis_id, "query", std::slice::from_ref(&cluster))
        .await;

    match r {
        Err(SynthesisError::ComposeAnchorViolation { cluster_id }) => {
            assert_eq!(
                cluster_id, cluster.id,
                "violation should report the offending cluster id"
            );
        }
        other => panic!(
            "expected Err(ComposeAnchorViolation {{ .. }}) after exhausting retries, got {:?}",
            other
        ),
    }
    assert_eq!(
        pipeline.llm_call_count, 2,
        "terminal failure should consume exactly 2 LLM calls (initial + 1 retry)"
    );
}

/// Missing-anchor path: LLM omits the END sentinel. Should retry once and then
/// return `ComposeAnchorViolation`.
#[tokio::test]
async fn stage5_compose_anchor_missing_returns_violation() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();

    let cluster_summary = "[claim-1] Evidence shows X.";
    let cluster = make_cluster(synthesis_id, cluster_summary);

    // BEGIN present, END missing on both attempts.
    let bad = format!(
        "# Topic\n\n<<<CLUSTER:{id}:BEGIN>>>{summary} (END is missing)",
        id = cluster.id,
        summary = cluster_summary,
    );
    let llm = MockLlmClient::with_responses(vec![
        serde_json::json!({"narrative": bad.clone()}),
        serde_json::json!({"narrative": bad}),
    ]);
    let mut pipeline = build_pipeline(pool.clone(), llm);

    let r = pipeline
        .stage5_compose(synthesis_id, "query", std::slice::from_ref(&cluster))
        .await;

    match r {
        Err(SynthesisError::ComposeAnchorViolation { cluster_id }) => {
            assert_eq!(cluster_id, cluster.id);
        }
        other => panic!(
            "expected Err(ComposeAnchorViolation {{ .. }}) when END sentinel is missing, got {:?}",
            other
        ),
    }
    assert_eq!(pipeline.llm_call_count, 2);
}
