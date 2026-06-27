//! Integration test for [`SynthesisJobHandler`].
//!
//! Drives a single job through all 6 pipeline stages against the live
//! `epigraph_dev_synthesis` database. The DB is pre-seeded with two `origami`
//! claims (`aaaa…` / `bbbb…`) by Phase 0; Stage 1 uses them as seeds.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//!     cargo test -p episcience-api --test synthesis_job_handler_test
//!
//! # What this test exercises
//!
//! - Stage 1 — `recall::recall("origami")` → 2 claim ids (text-search fallback).
//! - Stage 2 — Empty edge provider → snapshot has the 2 seed ids only.
//!   NOTE: this means BFS / relevance-prune are NOT exercised here —
//!   `EmptyEdgeProvider` returns no neighbours so the traversal loop
//!   immediately drains. Phase 4 will add a real edge provider and a
//!   companion test that exercises the BFS path.
//! - Stage 3 — `cluster_signed` with no edges → 2 singleton clusters.
//! - Stage 4 — Narrates each cluster via the mock LLM.
//! - Stage 5 — Composes the final narrative via the mock LLM.
//! - Stage 6 — Plans 4 provo edges (2 WAS_DERIVED_FROM + 1 ATTRIBUTED_TO + 0
//!   REFINES + 0 COMPOSED_OF), embeds narrative head, writes edges via
//!   `FakeEdgeWriter`, marks synthesis complete.
//!
//! Asserts:
//! - `handle` returns `Ok(JobResult)` with the synthesis id in the output.
//! - `syntheses.status = 'complete'` and narrative non-empty.
//! - `synthesis_provo_edges` rows are all written (`written_at IS NOT NULL`).
//! - `FakeEdgeWriter` saw the expected number of edges.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use epigraph_cli::enrichment::llm_client::MockLlmClient;
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use epigraph_jobs::{Job, JobHandler, JobId, JobState};
use episcience_api::jobs::{
    resolve_skill_for_row, EmptyEdgeProvider, SynthesisJobHandler, SynthesisJobPayload,
};
use episcience_db::{EdgeRequest, EdgeWriter, EdgeWriterError};
use sqlx::PgPool;
use uuid::Uuid;

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis";

// ─── Test doubles ───────────────────────────────────────────────────────────

/// Embedder that:
/// - errors on `generate_query` to force `recall::recall` onto the text-search
///   fallback (deterministic against the pre-seeded `origami` claims).
/// - returns a fixed 1536-dim embedding from `generate` (used by Stage 6
///   narrative-head embedding; the column is `vector(1536)`).
/// - errors on `get` (used by Stage 2 relevance closure — but with
///   `EmptyEdgeProvider` no neighbours are visited so it's never called).
#[derive(Debug)]
struct TestEmbedder {
    embedding: Vec<f32>,
}

impl Default for TestEmbedder {
    fn default() -> Self {
        Self {
            // 1536 = primary embedding dim per epigraph migration 5013.
            embedding: (0..1536).map(|i| (i as f32) * 1e-4).collect(),
        }
    }
}

#[async_trait]
impl EmbeddingService for TestEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.embedding.clone())
    }
    async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(vec![self.embedding.clone()])
    }
    async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
        Ok(())
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
        // Force text-search fallback in recall::recall — same trick as
        // synthesis_pipeline_stage1_test::ErroringEmbedder.
        Err(EmbeddingError::ApiError {
            message: "test stub: generate_query disabled".to_string(),
            status_code: None,
        })
    }
}

/// In-process [`EdgeWriter`] that records every request and never fails.
/// Mirrors `synthesis_pipeline_stage6_test::FakeEdgeWriter` so behaviour is
/// consistent across pipeline tests.
struct FakeEdgeWriter {
    seen: Mutex<Vec<EdgeRequest>>,
}

impl FakeEdgeWriter {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
        }
    }
    fn call_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl EdgeWriter for FakeEdgeWriter {
    async fn create_edge(&self, req: EdgeRequest) -> Result<Uuid, EdgeWriterError> {
        self.seen.lock().unwrap().push(req);
        Ok(Uuid::now_v7())
    }
}

// ─── DB helpers ─────────────────────────────────────────────────────────────

async fn connect() -> PgPool {
    let dsn = std::env::var("DATABASE_URL").unwrap_or_else(|_| DSN.to_string());
    PgPool::connect(&dsn)
        .await
        .expect("connect to epigraph_dev_synthesis (set DATABASE_URL to override)")
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
                 'signed_louvain', 'mock', 'mock-model',
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

/// Insert a minimum-viable `syntheses` row with an explicit `skill_name`.
/// Used by `resolve_skill_for_row_*` tests. The DB CHECK constraint added
/// in migration 5020 only permits `skill_name = 'baseline'`; passing any
/// other value here will fail the insert (which is the intended behaviour
/// until Task 5.1 expands the constraint).
async fn insert_test_synthesis_with_skill(pool: &PgPool, skill_name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, skill_name)
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock-model',
                 $4, 'private', $5)",
    )
    .bind(id)
    .bind("resolve-skill-test")
    .bind(test_agent_id())
    .bind(&[0u8; 32][..])
    .bind(skill_name)
    .execute(pool)
    .await
    .expect("insert synthesis row with skill_name");
    id
}

async fn insert_synthesis_job_row(pool: &PgPool, synthesis_id: Uuid, payload: &serde_json::Value) {
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, job_type, payload, state)
         VALUES ($1, 'synthesis', $2, 'queued')",
    )
    .bind(synthesis_id)
    .bind(payload)
    .execute(pool)
    .await
    .expect("insert synthesis_jobs row");
}

async fn cleanup(pool: &PgPool, synthesis_id: Uuid) {
    // Phase 7: descendants first. A rejected synthesis may have spawned a
    // refinement child (parent_synthesis_id FK has no ON DELETE CASCADE),
    // and that child carries its own provo edges + synthesis_jobs row.
    // Recursively drop them, then the row itself.
    let descendants: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM syntheses WHERE parent_synthesis_id = $1")
            .bind(synthesis_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    for child in descendants {
        Box::pin(cleanup(pool, child)).await;
    }

    let _ = sqlx::query("DELETE FROM synthesis_provo_edges WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM synthesis_embeddings WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM synthesis_clusters WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM synthesis_claim_membership WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM synthesis_jobs WHERE id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
}

// ─── Tests ───────────────────────────────────────────────────────────────────
//
// Note on seeds: `epigraph_dev_synthesis` is pre-seeded with two `origami`
// claims (`aaaa…` and `bbbb…`) by Phase 0. Stage 1's text-search fallback
// (forced by `TestEmbedder::generate_query` erroring) returns both for query
// `origami`. Both ids are lowercase hex, satisfying the Stage 4 citation
// regex `[0-9a-f-]{36}` if any cluster summary cites them.

/// End-to-end: handler runs all 6 stages, returns Success, leaves the
/// synthesis `status='complete'` with provo edges all written.
#[tokio::test]
async fn synthesis_handler_runs_all_stages_to_completion() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let query = "origami";

    // Pre-create the synthesis row + the synthesis_jobs row.
    insert_synthesis_row(&pool, synthesis_id, query).await;
    let payload_value = serde_json::to_value(SynthesisJobPayload {
        synthesis_id,
        query: query.into(),
        traversal_config: None,
        agent_id: test_agent_id(),
        parent_synthesis_id: None,
        prereq_synthesis_ids: vec![],
        workflow_run_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;

    // The pipeline produces N singleton clusters, one per seed (Stage 3 with
    // an empty edge list). Stage 4 narrates each cluster and Stage 5 composes
    // a final narrative. Stage 5 requires each cluster's summary to appear
    // VERBATIM between `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels — but the
    // cluster ids are `Uuid::now_v7()`-minted inside Stage 3 at run time, so
    // a static `MockLlmClient::with_responses` cannot know them ahead of
    // time. `LiveStage5Llm` (defined below) sidesteps this by reading the
    // freshly-inserted cluster rows from the DB on each call.
    let llm = Arc::new(LiveStage5Llm::new(pool.clone(), synthesis_id));
    let edges = Arc::new(FakeEdgeWriter::new());
    let handler = SynthesisJobHandler::new(
        pool.clone(),
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        edges.clone(),
        Arc::new(EmptyEdgeProvider),
        20, // cost_budget — generous; handler should consume ≤ 3 calls.
        "test-embedding-model",
        None,
    );

    let job = Job {
        id: JobId::from_uuid(synthesis_id),
        job_type: "synthesis".into(),
        payload: payload_value.clone(),
        state: JobState::Running,
        retry_count: 0,
        max_retries: 3,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        started_at: Some(Utc::now()),
        completed_at: None,
        error_message: None,
    };

    let result = handler.handle(&job).await;

    // Diagnostics if the handler errors — print the row state so the failure
    // message points at the right stage.
    if let Err(ref e) = result {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT status, narrative FROM syntheses WHERE id = $1")
                .bind(synthesis_id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        eprintln!("handler.handle errored: {e:?}; row state = {row:?}");
    }

    let job_result = result.expect("handler should run to completion");
    assert_eq!(
        job_result
            .output
            .get("synthesis_id")
            .and_then(|v| v.as_str()),
        Some(synthesis_id.to_string()).as_deref(),
    );

    // Synthesis row should be `complete` with non-null narrative AND the
    // Stage 6 verifier outcome persisted (Accept) with attempts = 1.
    let (status, narrative, verifier_outcome, verifier_attempts): (
        String,
        Option<String>,
        Option<serde_json::Value>,
        i16,
    ) = sqlx::query_as(
        "SELECT status, narrative, verifier_outcome, verifier_attempts \
         FROM syntheses WHERE id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("fetch synthesis row");
    assert_eq!(status, "complete");
    assert!(
        narrative.as_deref().is_some_and(|n| !n.is_empty()),
        "narrative should be non-empty after Stage 5/6, got {narrative:?}"
    );
    let outcome_json = verifier_outcome.expect("verifier_outcome should be persisted");
    assert_eq!(
        outcome_json["kind"].as_str(),
        Some("accept"),
        "expected Accept outcome on successful run, got {outcome_json}"
    );
    assert_eq!(
        verifier_attempts, 1,
        "verifier should have run exactly once"
    );

    // synthesis_provo_edges: Stage 6 plans (cited × WAS_DERIVED_FROM) + 1
    // ATTRIBUTED_TO. With 2 singleton clusters and 1 member each = 2 cited
    // claims = 2 WAS_DERIVED_FROM + 1 ATTRIBUTED_TO = 3 edges total. All
    // should be written (`written_at IS NOT NULL`).
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_provo_edges
             WHERE synthesis_id = $1 AND written_at IS NULL",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count pending");
    assert_eq!(pending, 0, "all provo edges should be written");

    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM synthesis_provo_edges WHERE synthesis_id = $1")
            .bind(synthesis_id)
            .fetch_one(&pool)
            .await
            .expect("count total");
    assert!(
        total >= 3,
        "expected ≥ 3 provo edges (2 cited claims + 1 agent), got {total}"
    );

    assert!(
        edges.call_count() >= 3,
        "edge writer should have been called ≥ 3 times, was {}",
        edges.call_count()
    );

    cleanup(&pool, synthesis_id).await;
}

/// Stage 6 reject path: an LLM that returns Stage 4 summaries with NO
/// citations produces a narrative the verifier rejects (`UncitedMember`).
/// The handler should persist `verifier_outcome`, bump `verifier_attempts`,
/// flip `status = 'rejected'`, and SKIP the publish bundle (no provo edges,
/// no narrative on the row, no edge writer calls).
#[tokio::test]
async fn synthesis_with_uncited_member_lands_status_rejected() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let query = "origami";

    insert_synthesis_row(&pool, synthesis_id, query).await;
    let payload_value = serde_json::to_value(SynthesisJobPayload {
        synthesis_id,
        query: query.into(),
        traversal_config: None,
        agent_id: test_agent_id(),
        parent_synthesis_id: None,
        prereq_synthesis_ids: vec![],
        workflow_run_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;

    // UncitedStage5Llm is structurally identical to LiveStage5Llm but
    // deliberately omits any [<uuid>] citations from the per-cluster
    // summary, so the verifier rejects on UncitedMember.
    let llm = Arc::new(UncitedStage5Llm::new(pool.clone(), synthesis_id));
    let edges = Arc::new(FakeEdgeWriter::new());
    let handler = SynthesisJobHandler::new(
        pool.clone(),
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        edges.clone(),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        None,
    );

    let job = Job {
        id: JobId::from_uuid(synthesis_id),
        job_type: "synthesis".into(),
        payload: payload_value.clone(),
        state: JobState::Running,
        retry_count: 0,
        max_retries: 3,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        started_at: Some(Utc::now()),
        completed_at: None,
        error_message: None,
    };

    let result = handler.handle(&job).await;
    let job_result =
        result.expect("handler should return Ok on Reject (rejection is not an error)");

    // Output payload carries the rejected status + rubric name so the
    // dispatcher can surface it without re-querying the row.
    assert_eq!(
        job_result.output.get("status").and_then(|v| v.as_str()),
        Some("rejected"),
        "output should advertise status=rejected, got {:?}",
        job_result.output
    );
    assert_eq!(
        job_result.output.get("rubric").and_then(|v| v.as_str()),
        Some("default_citation"),
        "output should name the default_citation rubric"
    );

    // Row state: rejected, verifier_outcome populated, verifier_attempts=1,
    // narrative is still null (publish was skipped).
    let (status, narrative, verifier_outcome, verifier_attempts): (
        String,
        Option<String>,
        Option<serde_json::Value>,
        i16,
    ) = sqlx::query_as(
        "SELECT status, narrative, verifier_outcome, verifier_attempts \
         FROM syntheses WHERE id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("fetch synthesis row");
    assert_eq!(status, "rejected");
    assert!(
        narrative.is_none(),
        "publish bundle should be SKIPPED on Reject; narrative should remain NULL, got {narrative:?}"
    );
    let outcome_json = verifier_outcome.expect("verifier_outcome should be persisted on Reject");
    assert_eq!(outcome_json["kind"].as_str(), Some("reject"));
    assert_eq!(outcome_json["rubric"].as_str(), Some("default_citation"));
    assert_eq!(verifier_attempts, 1);

    // No provo edges OWNED BY the parent (Stage 6 publish was skipped) and
    // no edge-writer calls. Phase 7 may have inserted a REFINES edge with
    // synthesis_id = refinement_child, but that row's source is the child,
    // so a `WHERE synthesis_id = parent` count still returns 0.
    let edge_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM synthesis_provo_edges WHERE synthesis_id = $1")
            .bind(synthesis_id)
            .fetch_one(&pool)
            .await
            .expect("count edges");
    assert_eq!(
        edge_rows, 0,
        "Reject path must not plan any provo edges from the parent",
    );
    assert_eq!(
        edges.call_count(),
        0,
        "Reject path must not call the edge writer"
    );

    cleanup(&pool, synthesis_id).await;
}

/// Phase 7 (refinement): a Reject from Stage 6 spawns a refinement child
/// via PROV-O REFINES, with the child's `refinement_temperature.depth_delta`
/// = parent's + 1 (= 1 when the parent started cold). The parent row is
/// marked `rejected` (terminal), and the child is enqueued in
/// `synthesis_jobs.state = 'queued'`. The REFINES edge is written with
/// `synthesis_id = child` (source) and `target_id = parent`.
#[tokio::test]
async fn rejected_synthesis_spawns_refinement_child() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    let query = "origami";

    insert_synthesis_row(&pool, synthesis_id, query).await;
    let payload_value = serde_json::to_value(SynthesisJobPayload {
        synthesis_id,
        query: query.into(),
        traversal_config: None,
        agent_id: test_agent_id(),
        parent_synthesis_id: None,
        prereq_synthesis_ids: vec![],
        workflow_run_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;

    // UncitedStage5Llm forces a Stage 6 reject (UncitedMember rubric).
    let llm = Arc::new(UncitedStage5Llm::new(pool.clone(), synthesis_id));
    let edges = Arc::new(FakeEdgeWriter::new());
    let handler = SynthesisJobHandler::new(
        pool.clone(),
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        edges.clone(),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        None,
    );

    let job = Job {
        id: JobId::from_uuid(synthesis_id),
        job_type: "synthesis".into(),
        payload: payload_value.clone(),
        state: JobState::Running,
        retry_count: 0,
        max_retries: 3,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        started_at: Some(Utc::now()),
        completed_at: None,
        error_message: None,
    };

    let result = handler.handle(&job).await;
    let job_result = result.expect("Reject path returns Ok");

    // Output names the refinement child + depth_delta=1.
    let child_id_str = job_result
        .output
        .get("refinement_child_id")
        .and_then(|v| v.as_str())
        .expect("output should include refinement_child_id on Reject");
    let child_id: Uuid = child_id_str.parse().expect("child_id parses as Uuid");
    assert_eq!(
        job_result
            .output
            .get("depth_delta")
            .and_then(|v| v.as_u64()),
        Some(1),
        "first refinement should anneal depth_delta 0 → 1, got {:?}",
        job_result.output,
    );

    // Parent row: status = 'rejected'.
    let parent_status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .fetch_one(&pool)
        .await
        .expect("fetch parent status");
    assert_eq!(parent_status, "rejected");

    // Child row exists, parent_synthesis_id = parent, status = 'pending',
    // refinement_temperature.depth_delta = 1, allow_soft_verifier = true.
    let (child_status, child_parent, child_temp): (
        String,
        Option<Uuid>,
        Option<serde_json::Value>,
    ) = sqlx::query_as(
        "SELECT status, parent_synthesis_id, refinement_temperature
             FROM syntheses WHERE id = $1",
    )
    .bind(child_id)
    .fetch_one(&pool)
    .await
    .expect("fetch child row");
    assert_eq!(child_status, "pending", "child should start pending");
    assert_eq!(
        child_parent,
        Some(synthesis_id),
        "child.parent must equal original"
    );
    let temp_json = child_temp.expect("child should carry refinement_temperature");
    assert_eq!(
        temp_json["depth_delta"].as_u64(),
        Some(1),
        "child temp.depth_delta should be 1, got {temp_json}",
    );
    assert_eq!(
        temp_json["allow_soft_verifier"].as_bool(),
        Some(true),
        "child temp.allow_soft_verifier should be true, got {temp_json}",
    );

    // REFINES edge: source=child, target=parent.
    let refines_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_provo_edges
         WHERE synthesis_id = $1 AND predicate = 'REFINES'
           AND target_kind = 'synthesis' AND target_id = $2",
    )
    .bind(child_id)
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count REFINES edge");
    assert_eq!(
        refines_count, 1,
        "exactly one REFINES edge child→parent should exist",
    );

    // Child is enqueued in synthesis_jobs as 'queued'.
    let child_job_state: String =
        sqlx::query_scalar("SELECT state FROM synthesis_jobs WHERE id = $1")
            .bind(child_id)
            .fetch_one(&pool)
            .await
            .expect("fetch child job state");
    assert_eq!(child_job_state, "queued", "child job must be enqueued");

    cleanup(&pool, synthesis_id).await;
}

// `UncitedStage5Llm` mirrors LiveStage5Llm's structure but returns empty
// summaries (no `[<uuid>]` tokens), driving the verifier into Reject.
struct UncitedStage5Llm {
    pool: PgPool,
    synthesis_id: Uuid,
    call_count: Mutex<u32>,
}

impl UncitedStage5Llm {
    fn new(pool: PgPool, synthesis_id: Uuid) -> Self {
        Self {
            pool,
            synthesis_id,
            call_count: Mutex::new(0),
        }
    }
}

impl std::fmt::Debug for UncitedStage5Llm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UncitedStage5Llm").finish()
    }
}

#[async_trait]
impl epigraph_cli::enrichment::llm_client::LlmClient for UncitedStage5Llm {
    async fn complete_json(
        &self,
        _prompt: &str,
    ) -> Result<serde_json::Value, epigraph_cli::enrichment::llm_client::LlmError> {
        let n = {
            let mut c = self.call_count.lock().unwrap();
            *c += 1;
            *c
        };
        let rows: Vec<(Uuid, i32, String, String)> = sqlx::query_as(
            "SELECT id, cluster_index, title, summary
             FROM synthesis_clusters
             WHERE synthesis_id = $1
             ORDER BY cluster_index ASC",
        )
        .bind(self.synthesis_id)
        .fetch_all(&self.pool)
        .await
        .map_err(
            |e| epigraph_cli::enrichment::llm_client::LlmError::RequestFailed {
                message: format!("UncitedStage5Llm db query: {e}"),
            },
        )?;

        let n_clusters = rows.len() as u32;
        if n_clusters == 0 {
            return Ok(serde_json::json!({"title": "", "summary": ""}));
        }
        if n <= n_clusters {
            // Empty summary -> no citations -> verifier rejects.
            return Ok(serde_json::json!({"title": "T", "summary": ""}));
        }
        // Stage 5 compose with empty cluster summaries — passes Stage 5's
        // verbatim-anchor validator (empty body == empty body) but Stage 6
        // verifier rejects because the cited set is empty while members
        // are non-empty.
        let mut narrative = String::new();
        for (id, _idx, _title, summary) in &rows {
            narrative.push_str(&format!(
                "<<<CLUSTER:{id}:BEGIN>>>{summary}<<<CLUSTER:{id}:END>>>\n",
            ));
        }
        Ok(serde_json::json!({"narrative": narrative}))
    }

    fn model_name(&self) -> &str {
        "uncited-stage5-mock"
    }
}

// ─── LiveStage5Llm: a mock LLM that knows the synthesis's runtime cluster IDs
//
// Stage 5's anchor protocol requires the LLM's narrative to wrap each cluster
// summary in `<<<CLUSTER:{id}:BEGIN>>>{summary}<<<CLUSTER:{id}:END>>>` *byte
// for byte*. The cluster ids are minted with `Uuid::now_v7()` inside Stage 3
// at run time, so a static `MockLlmClient::with_responses` cannot know them.
//
// `LiveStage5Llm` solves this by querying `synthesis_clusters` from the live
// DB on each call:
//
// - Calls 1-2 (Stage 4 narrate): respond with `{title, summary: ""}` for each
//   cluster. Empty summary means the citation regex finds nothing to validate.
// - Call 3+ (Stage 5 compose): responds with a narrative that lists every
//   cluster's BEGIN/END sentinel pair with the empty summary between them.

struct LiveStage5Llm {
    pool: PgPool,
    synthesis_id: Uuid,
    call_count: Mutex<u32>,
}

impl LiveStage5Llm {
    fn new(pool: PgPool, synthesis_id: Uuid) -> Self {
        Self {
            pool,
            synthesis_id,
            call_count: Mutex::new(0),
        }
    }
}

impl std::fmt::Debug for LiveStage5Llm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveStage5Llm").finish()
    }
}

#[async_trait]
impl epigraph_cli::enrichment::llm_client::LlmClient for LiveStage5Llm {
    async fn complete_json(
        &self,
        _prompt: &str,
    ) -> Result<serde_json::Value, epigraph_cli::enrichment::llm_client::LlmError> {
        // Bump call count first so we know which stage we're servicing.
        let n = {
            let mut c = self.call_count.lock().unwrap();
            *c += 1;
            *c
        };

        // Read clusters for this synthesis ordered by `cluster_index`. We
        // include `member_claim_ids` (UUID[]) so Stage 4 summaries can cite
        // each member — Phase 4's verifier rejects narratives that omit any
        // member citation, so producing a citing summary is now required for
        // the Accept path.
        let rows: Vec<(Uuid, i32, String, String, Vec<Uuid>)> = sqlx::query_as(
            "SELECT id, cluster_index, title, summary, member_claim_ids
             FROM synthesis_clusters
             WHERE synthesis_id = $1
             ORDER BY cluster_index ASC",
        )
        .bind(self.synthesis_id)
        .fetch_all(&self.pool)
        .await
        .map_err(
            |e| epigraph_cli::enrichment::llm_client::LlmError::RequestFailed {
                message: format!("LiveStage5Llm db query: {e}"),
            },
        )?;

        // Heuristic: Stage 4 calls happen 1..=N where N == row count.
        // Stage 5 happens at call N+1. Stage 4 responses are per-cluster
        // `{title, summary}` where the summary cites every member id;
        // Stage 5 is a compose narrative.
        let n_clusters = rows.len() as u32;
        if n_clusters == 0 {
            // No clusters yet — must be a pre-cluster call (shouldn't happen
            // in Phase 2 v1 since Stages 1-3 don't call the LLM). Return an
            // empty title/summary so any downstream parsing succeeds.
            return Ok(serde_json::json!({"title": "", "summary": ""}));
        }

        if n <= n_clusters {
            // Stage 4 narrate — return {title, summary} for the n-th cluster.
            // The summary cites every member id as `[<uuid>]` so the Stage 4
            // citation validator AND the Phase 4 verifier both accept it.
            let row_idx = (n - 1) as usize;
            let (_id, _idx, _title, _summary, members) = &rows[row_idx];
            let title = format!("Cluster {n} title");
            let citations = members
                .iter()
                .map(|m| format!("[{m}]"))
                .collect::<Vec<_>>()
                .join(" ");
            let summary = format!("Summary citing {citations}");
            return Ok(serde_json::json!({
                "title": title,
                "summary": summary,
            }));
        }

        // Stage 5 compose — build the verbatim narrative from the clusters.
        // Each cluster's summary now cites its member ids (per Stage 4
        // above), so the body between BEGIN/END contains the same citations
        // and the verifier accepts.
        let mut narrative = String::from("# Synthesis\n\n");
        for (id, _idx, _title, summary, _members) in &rows {
            narrative.push_str(&format!(
                "<<<CLUSTER:{id}:BEGIN>>>{summary}<<<CLUSTER:{id}:END>>>\n",
            ));
        }
        Ok(serde_json::json!({"narrative": narrative}))
    }

    fn model_name(&self) -> &str {
        "live-stage5-mock"
    }
}

// ─── Cost-budget cap test (per plan, Test #1) ───────────────────────────────
//
// "Test that cost_budget=2 → third llm call errors with CostBudgetExceeded.
//  Use SynthesisPipeline directly (not the handler) — call_llm_with_retry 3
//  times."
//
// Per the plan note, this test could live in `episcience-db/tests/` since it
// exercises the pipeline directly. Keeping it here too (alongside the
// handler test) for proximity to the cost-budget contract the handler relies
// on, and to keep all Phase-2 integration tests buildable from one entry
// point.

#[tokio::test]
async fn pipeline_respects_cost_budget_cap() {
    use episcience_core::synthesis::errors::SynthesisError;
    use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
    use episcience_db::SynthesisPipeline;

    struct UnusedEdgeProvider;
    #[async_trait]
    impl EdgeProvider for UnusedEdgeProvider {
        async fn neighbors(&self, _claim: Uuid, _types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
            vec![]
        }
    }

    let pool = connect().await;
    // Empty responses → MockLlmClient returns `[]` for each call. Validator
    // below always accepts, so each call counts as one budget tick.
    let llm = MockLlmClient::with_responses(vec![
        serde_json::json!([]),
        serde_json::json!([]),
        serde_json::json!([]),
    ]);
    let mut pipeline: SynthesisPipeline<MockLlmClient, UnusedEdgeProvider> = SynthesisPipeline::new(
        pool,
        Arc::new(TestEmbedder::default()),
        llm,
        UnusedEdgeProvider,
        vec![],
        // cost_budget = 2: first call (count=1) and second call (count=2)
        // succeed; the third call's pre-check (count=2 >= budget=2) trips
        // CostBudgetExceeded *before* the third LLM call is made.
        2,
    );

    let validator = |_: &serde_json::Value| Ok::<(), SynthesisError>(());

    pipeline
        .call_llm_with_retry("first", 0, validator)
        .await
        .expect("first call within budget");
    pipeline
        .call_llm_with_retry("second", 0, validator)
        .await
        .expect("second call within budget");

    let third = pipeline.call_llm_with_retry("third", 0, validator).await;
    match third {
        Err(SynthesisError::CostBudgetExceeded { limit }) => {
            assert_eq!(limit, 2);
        }
        other => panic!("expected CostBudgetExceeded, got {other:?}"),
    }
    assert_eq!(
        pipeline.llm_call_count, 2,
        "third call must not increment count"
    );
}

// ─── resolve_skill_for_row tests (Task 2.3) ─────────────────────────────────
//
// Proves the job-handler's row-to-skill resolver returns the named skill when
// it exists, and falls back to baseline when the row is missing. The third
// case (known row, unknown skill name) is exercised in Task 5.1 once the
// CHECK constraint admits a second value; the current constraint only allows
// `'baseline'`, so we cannot insert any other name into the column from a
// test today.

/// Happy path: `skill_name = 'baseline'` round-trips to a baseline skill.
#[tokio::test]
async fn resolve_skill_for_row_returns_baseline_for_known_name() {
    let pool = connect().await;
    let id = insert_test_synthesis_with_skill(&pool, "baseline").await;

    let skill = resolve_skill_for_row(&pool, id)
        .await
        .expect("resolve baseline skill");
    assert_eq!(skill.name(), "baseline");

    cleanup(&pool, id).await;
}

/// Fallback: a non-existent synthesis id returns baseline (and the resolver
/// emits a `warn!` — not asserted here, but visible in test output).
#[tokio::test]
async fn resolve_skill_for_row_falls_back_on_missing_row() {
    let pool = connect().await;
    let unknown_id = Uuid::new_v4();

    let skill = resolve_skill_for_row(&pool, unknown_id)
        .await
        .expect("resolve should not error on missing row");
    assert_eq!(skill.name(), "baseline");
}

/// Phase 5: a row with `skill_name = 'lab_notebook'` resolves to the
/// `LabNotebookSkill`. Proves migration 5022's CHECK extension is wired
/// through and the registry's new arm is reachable from the job handler.
#[tokio::test]
async fn resolve_skill_for_row_returns_lab_notebook_when_named() {
    let pool = connect().await;
    let id = insert_test_synthesis_with_skill(&pool, "lab_notebook").await;

    let skill = resolve_skill_for_row(&pool, id)
        .await
        .expect("resolve lab_notebook skill");
    assert_eq!(skill.name(), "lab_notebook");

    cleanup(&pool, id).await;
}

// ─── POST /api/v1/eln/syntheses skill_name plumbing (Task 2.4) ──────────────
//
// Prove the HTTP route accepts an optional `skill_name` in the body and
// writes it through to the `syntheses` row. The route hits the same
// `enqueue_synthesis` → `create_pending_tx` chain used in production, so
// both tests exercise the full deserialization + threading path end to end.
//
// Until Task 5.1 expands the `syntheses_skill_name_known` CHECK constraint, the
// only value allowed in the column is `'baseline'` — so the two tests below
// both end up asserting the row contains `'baseline'`. That's still load-
// bearing: it proves (a) the request deserializer accepts the optional
// field, (b) the value (or its default) reaches the INSERT.

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;

fn jwt_secret_bytes() -> Vec<u8> {
    std::env::var("EPIGRAPH_JWT_SECRET")
        .map(|s| s.into_bytes())
        .unwrap_or_else(|_| b"epigraph-dev-secret-change-in-production!!".to_vec())
}

fn mint_test_jwt(agent_id: Uuid) -> String {
    #[derive(Serialize)]
    struct Claims {
        sub: Uuid,
        agent_id: Uuid,
        exp: i64,
        iat: i64,
        nbf: i64,
        jti: Uuid,
        scopes: Vec<String>,
        client_type: String,
    }

    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: agent_id,
        agent_id,
        exp: now + 3600,
        iat: now,
        nbf: now,
        jti: Uuid::now_v7(),
        scopes: vec!["edges:write".to_string(), "claims:read".to_string()],
        client_type: "service".to_string(),
    };

    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(&jwt_secret_bytes()),
    )
    .expect("mint JWT")
}

fn bearer(token: &str) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
    )
}

fn build_test_server(pool: PgPool) -> TestServer {
    use epigraph_embeddings::EmbeddingService as EmbeddingServiceTrait;
    let embedder: Arc<dyn EmbeddingServiceTrait> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        pool,
        blob_dir: std::path::PathBuf::from("/tmp/episcience-test-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    };
    let _ = std::fs::create_dir_all(&state.blob_dir);
    let app = episcience_api::create_router(state);
    TestServer::new(app).expect("build TestServer")
}

/// Explicit `skill_name = "baseline"` in the POST body lands in the row.
#[tokio::test]
async fn post_syntheses_accepts_skill_name() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let agent_id = Uuid::now_v7();
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "query": "skill_name explicit baseline",
            "skill_name": "baseline",
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "expected 202 ACCEPTED, body: {}",
        resp.text()
    );
    let body: serde_json::Value = resp.json();
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let stored: String = sqlx::query_scalar("SELECT skill_name FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch skill_name");
    assert_eq!(stored, "baseline");

    cleanup(&pool, id).await;
}

/// Omitting `skill_name` in the POST body defaults the row to `"baseline"`.
#[tokio::test]
async fn post_syntheses_omitted_skill_defaults_to_baseline() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let agent_id = Uuid::now_v7();
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "query": "skill_name omitted",
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "expected 202 ACCEPTED, body: {}",
        resp.text()
    );
    let body: serde_json::Value = resp.json();
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let stored: String = sqlx::query_scalar("SELECT skill_name FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch skill_name");
    assert_eq!(stored, "baseline");

    cleanup(&pool, id).await;
}

// ─── resolve_traversal_config precedence tests (Task 3.3) ───────────────────
//
// Helper precedence: payload (if parseable) > skill.traversal_config() > default.
// A malformed payload falls through to the skill, NOT to the default — letting
// the skill have a say even when the request was buggy. Tests exercise the
// helper directly; no DB or pipeline involved.

/// `OpinionatedSkill` returns a non-default `TraversalConfig` with `max_hops =
/// 99`, used to distinguish skill-supplied from default-supplied configs. Kept
/// as a module-scope stub so all relevant tests share one definition.
#[derive(Debug)]
struct OpinionatedSkill;

#[async_trait::async_trait]
impl episcience_core::synthesis::skill::SynthesisSkill for OpinionatedSkill {
    fn name(&self) -> &'static str {
        "opinionated"
    }
    fn section(&self, _: episcience_core::synthesis::skill::SynthesisStage) -> Option<&str> {
        None
    }
    fn traversal_config(&self) -> Option<episcience_core::synthesis::traversal::TraversalConfig> {
        Some(episcience_core::synthesis::traversal::TraversalConfig {
            max_hops: 99,
            ..Default::default()
        })
    }
}

#[test]
fn resolve_traversal_config_payload_wins_over_skill() {
    // Payload supplied with parseable JSON -> wins, even when the skill has
    // an opinion. Field names match `TraversalConfig`'s real shape (max_hops,
    // edge_types as PascalCase EdgeType variants, relevance_prune,
    // follow_via_paper, max_subgraph_size).
    let payload_cfg = serde_json::json!({
        "max_hops": 5,
        "edge_types": ["Supports"],
        "follow_via_paper": false,
        "relevance_prune": 0.7,
        "max_subgraph_size": 100,
    });

    let resolved =
        episcience_api::jobs::resolve_traversal_config(Some(&payload_cfg), &OpinionatedSkill);
    assert_eq!(resolved.max_hops, 5, "payload should win over skill");
    assert_eq!(
        resolved.relevance_prune, 0.7,
        "payload's relevance_prune should be used"
    );
}

#[test]
fn resolve_traversal_config_skill_wins_when_no_payload() {
    let resolved = episcience_api::jobs::resolve_traversal_config(None, &OpinionatedSkill);
    assert_eq!(
        resolved.max_hops, 99,
        "skill's traversal_config should win when no payload supplied"
    );
}

#[test]
fn resolve_traversal_config_default_when_neither() {
    use episcience_core::synthesis::skills::baseline::BaselineSkill;
    let resolved = episcience_api::jobs::resolve_traversal_config(None, &BaselineSkill);
    let default = episcience_core::synthesis::traversal::TraversalConfig::default();
    assert_eq!(resolved.max_hops, default.max_hops);
    assert_eq!(resolved.relevance_prune, default.relevance_prune);
    assert_eq!(resolved.max_subgraph_size, default.max_subgraph_size);
    assert_eq!(resolved.follow_via_paper, default.follow_via_paper);
    assert_eq!(resolved.edge_types.len(), default.edge_types.len());
}

#[test]
fn resolve_traversal_config_malformed_payload_falls_through_to_skill() {
    // A payload that doesn't deserialize to TraversalConfig (missing required
    // fields, wrong types) should fall through to the skill's opinion, not
    // silently land on the schema default. This protects users with bad
    // requests from accidentally bypassing the skill's expertise.
    let bad_payload = serde_json::json!({
        "not_a_real_field": "garbage",
    });

    let resolved =
        episcience_api::jobs::resolve_traversal_config(Some(&bad_payload), &OpinionatedSkill);
    assert_eq!(
        resolved.max_hops, 99,
        "malformed payload should fall through to the skill (99), not to default"
    );
}

// ─── Phase 6: Stage 7 novelty ────────────────────────────────────────────────

/// Empty-priors path: a candidate that shares no cluster members with any
/// prior `complete` synthesis must score exactly 1.0 (fully novel) with no
/// neighbours. The backend short-circuits before embedding, so any embedder
/// is acceptable here; `TestEmbedder` is reused for consistency.
///
/// The candidate is synthetic (Uuid::now_v7() plus two synthetic member
/// ids); no DB rows are pre-seeded for it. The query MUST find zero
/// overlap so the early-return path runs — using freshly-minted member
/// ids guarantees this.
#[tokio::test]
async fn novelty_is_one_when_no_priors() {
    use episcience_core::synthesis::novelty::NoveltyBackend;
    use episcience_db::synthesis::novelty_backend_internal::InternalNoveltyBackend;

    let pool = connect().await;
    let embedder: Arc<dyn EmbeddingService> = Arc::new(TestEmbedder::default());
    let backend = InternalNoveltyBackend {
        pool: pool.clone(),
        embedder,
    };
    let cand_id = Uuid::now_v7();
    let members = vec![Uuid::now_v7(), Uuid::now_v7()];

    let score = backend
        .score(cand_id, "a novel summary", &members)
        .await
        .expect("score should succeed");

    assert_eq!(score.score, 1.0, "no priors → fully novel");
    assert!(
        score.neighbours.is_empty(),
        "no priors → no neighbours, got {:?}",
        score.neighbours
    );
    assert_eq!(score.backend, "internal_prior_syntheses");
    assert!(
        score.rationale.contains("no prior synthesis"),
        "rationale should mention no priors, got {:?}",
        score.rationale
    );
}

// ─── Phase 9: PaperNoveltyBackend dispatch ───────────────────────────────────
//
// The handler dispatches on `pipeline.skill.name()` to choose a novelty
// backend (see `select_novelty_backend` in `synthesis_job.rs`). The DB-
// level integration test for PaperNoveltyBackend's behaviour lives in
// `episcience-db` (helper-level tests) and a follow-on full E2E would
// require seeding a `'doi'`-labeled claim with an embedding plus a
// synthesis with `skill_name='literature'` — heavy for what the dispatch
// itself proves. These tests instead exercise the dispatch function
// directly, asserting that the right backend's `name()` comes back per
// skill. Together with the `novelty_is_one_when_no_priors` test above
// (which proves InternalNoveltyBackend's no-priors path) and the
// `backend_name_is_paper_novelty` unit test in episcience-db (which
// proves PaperNoveltyBackend's identifier is stable), they form the
// Phase 9 dispatch coverage triangle.

/// Build a `PgPool` that never actually connects. The dispatch tests
/// only call `select_novelty_backend(...).name()`, which neither reads
/// from nor writes to the DB — a lazy pool is sufficient and lets
/// these tests run without the full `epigraph_dev_synthesis` fixture
/// the heavier `connect()` helper requires.
fn lazy_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://test:test@127.0.0.1:5432/test")
        .expect("lazy pool must construct without a DB roundtrip")
}

/// `"literature"` → `PaperNoveltyBackend`. Mirror of the production
/// dispatch path so the rule "literature skill → paper_novelty backend"
/// is regression-protected without standing up the full pipeline.
#[tokio::test]
async fn select_novelty_backend_literature_picks_paper_novelty() {
    use episcience_api::jobs::select_novelty_backend;

    let pool = lazy_pool();
    let embedder: Arc<dyn EmbeddingService> = Arc::new(TestEmbedder::default());
    let backend = select_novelty_backend("literature", pool, embedder);
    assert_eq!(
        backend.name(),
        "paper_novelty",
        "literature skill must select PaperNoveltyBackend"
    );
}

/// Non-literature skills (`baseline`, `lab_notebook`, `code_review`,
/// `registry_diff`, and unknown names) MUST continue to use
/// `InternalNoveltyBackend`. This guards the spec's hard rule "zero
/// behaviour change for those skills." The five skill names below
/// cover every named skill in `episcience-core` plus an unknown name
/// to exercise the default arm of the dispatch.
#[tokio::test]
async fn select_novelty_backend_other_skills_pick_internal() {
    use episcience_api::jobs::select_novelty_backend;

    let pool = lazy_pool();
    let embedder: Arc<dyn EmbeddingService> = Arc::new(TestEmbedder::default());
    for skill in [
        "baseline",
        "lab_notebook",
        "code_review",
        "registry_diff",
        "unknown_skill_xyz",
    ] {
        let backend = select_novelty_backend(skill, pool.clone(), embedder.clone());
        assert_eq!(
            backend.name(),
            "internal_prior_syntheses",
            "skill {skill:?} must select InternalNoveltyBackend (default arm)"
        );
    }
}
