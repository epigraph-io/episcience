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
//!
//! 4. `stage5_compose_accepts_a_wiki_article_with_blocks_reordered_across_clusters`
//!    — the article shape `WikiArticleSkill` asks for: title, lede, a heading
//!    before each block, the blocks in the composer's own order (cluster 1's
//!    before cluster 0's), then `## Contested` and `## Gaps`. Accepted on the
//!    first attempt; the skill's composition section reaches the prompt.
mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
use episcience_core::synthesis::Cluster;
use episcience_db::SynthesisPipeline;
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider, MockLlmClient};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use episcience_core::synthesis::skill::{SynthesisSkill, SynthesisStage};
use episcience_core::synthesis::skills::wiki_article::WikiArticleSkill;

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

/// Scripted LLM that also records every prompt it is sent, so a test can see
/// what the active skill contributed. Responses are consumed in order.
#[derive(Debug)]
struct RecordingLlm {
    responses: Mutex<Vec<serde_json::Value>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmProvider for RecordingLlm {
    fn name(&self) -> &str {
        "recording"
    }

    fn model_name(&self) -> &str {
        "recording"
    }

    fn is_active(&self) -> bool {
        true
    }

    async fn complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        let mut r = self.responses.lock().unwrap();
        if r.is_empty() {
            Ok(serde_json::json!({}))
        } else {
            Ok(r.remove(0))
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

async fn connect_epigraph() -> PgPool {
    // The run's shared clone of the E1 template; refuses port 5432 and any
    // database name not ending in `_test` (support::check_test_url).
    support::shared_pool("DATABASE_URL").await
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

/// The article shape `WikiArticleSkill`'s composition section asks for is one
/// `stage5_compose` accepts: a `# Title`, a lede, a `##` heading before each
/// block, the blocks in the composer's own teaching order (cluster 1's block
/// BEFORE cluster 0's), then `## Contested` citing members and `## Gaps`. The
/// validator checks each cluster on its own (BEGIN before END, verbatim
/// body), never the order of clusters relative to each other. Kills: an
/// anchor validator that also enforces cluster order (every wiki composition
/// that reorders would fail twice and the job with it), a validator that
/// rejects text between blocks, and the skill's composition section not
/// reaching the compose prompt.
#[tokio::test]
async fn stage5_compose_accepts_a_wiki_article_with_blocks_reordered_across_clusters() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    let (m1, m2, m3) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    let summary_0 = format!("Origami began as ceremonial paper folding [{m1}].");
    let summary_1 = format!("Crease patterns fix every fold before folding starts [{m2}] [{m3}].");
    let mut c0 = make_cluster(synthesis_id, &summary_0);
    c0.title = "History".to_string();
    c0.member_claim_ids = vec![m1];
    let mut c1 = make_cluster(synthesis_id, &summary_1);
    c1.cluster_index = 1;
    c1.title = "Crease patterns".to_string();
    c1.member_claim_ids = vec![m2, m3];

    let article = format!(
        "# Origami\n\nOrigami is the art of folding a single sheet of paper into a form.\n\n\
         ## Crease patterns\n\n<<<CLUSTER:{id1}:BEGIN>>>{summary_1}<<<CLUSTER:{id1}:END>>>\n\n\
         ## History\n\n<<<CLUSTER:{id0}:BEGIN>>>{summary_0}<<<CLUSTER:{id0}:END>>>\n\n\
         ## Contested\n\n- Whether patterns predate ceremony is disputed [{m1}] [{m2}].\n\n\
         ## Gaps\n\n- The cited claims say nothing about wet folding.\n",
        id0 = c0.id,
        id1 = c1.id,
    );
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let llm = RecordingLlm {
        responses: Mutex::new(vec![serde_json::json!({ "narrative": article })]),
        prompts: prompts.clone(),
    };
    let mut pipeline = SynthesisPipeline::new(
        pool.clone(),
        Arc::new(ConstantEmbedder::default()),
        llm,
        UnusedEdgeProvider,
        vec![1.0; 8],
        10,
    )
    .with_skill(Arc::new(WikiArticleSkill));

    let narrative = pipeline
        .stage5_compose(synthesis_id, "Origami", &[c0.clone(), c1.clone()])
        .await
        .expect("a wiki article with the blocks reordered across clusters is accepted");

    assert_eq!(
        pipeline.llm_call_count, 1,
        "accepted on the first attempt, not after a retry"
    );
    assert!(!narrative.contains("<<<CLUSTER"), "{narrative:?}");
    let (p0, p1) = (
        narrative.find(&summary_0).expect("cluster 0 verbatim"),
        narrative.find(&summary_1).expect("cluster 1 verbatim"),
    );
    assert!(p1 < p0, "the composer's order is kept: {narrative:?}");
    for kept in [
        "# Origami\n",
        "## Crease patterns",
        "## History",
        "## Contested",
        "## Gaps",
    ] {
        assert!(
            narrative.contains(kept),
            "{kept:?} missing from {narrative:?}"
        );
    }

    let comp = WikiArticleSkill
        .section(SynthesisStage::Composition)
        .expect("wiki composition section");
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains(&format!("Skill guidance: {comp}")),
        "the wiki composition section must follow \"Skill guidance:\" in {:?}",
        prompts[0]
    );
}
