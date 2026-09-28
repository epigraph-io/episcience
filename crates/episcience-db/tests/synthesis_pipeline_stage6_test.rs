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
mod support;

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use episcience_core::synthesis::SubgraphSnapshot;
use episcience_db::publish;
use episcience_db::{EdgeRequest, EdgeWriter, EdgeWriterError, SynthesisProvoEdgesRepository};
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
    // The run's shared clone of the E1 template; refuses port 5432 and any
    // database name not ending in `_test` (support::check_test_url).
    support::shared_pool("DATABASE_URL").await
}

fn test_agent_id() -> Uuid {
    "f3951e28-9356-42b6-9c80-27dd9f01b19d".parse().unwrap()
}

async fn insert_synthesis_row(pool: &PgPool, synthesis_id: Uuid, query: &str) {
    insert_synthesis_row_with(pool, synthesis_id, query, "public", &[]).await;
}

/// A pending synthesis by the seed agent, owned by its personal group with
/// `visibility`, naming `prereqs`.
async fn insert_synthesis_row_with(
    pool: &PgPool,
    synthesis_id: Uuid,
    query: &str,
    visibility: &str,
    prereqs: &[Uuid],
) {
    let prereqs: Option<Vec<Uuid>> = (!prereqs.is_empty()).then(|| prereqs.to_vec());
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, owner_group_id, prereq_synthesis_ids)
         SELECT $1, $2, $3, 'pending', '{}'::jsonb,
                'signed_louvain', 'mock', 'mock',
                $4, $5, g.id, $6
           FROM groups g
          WHERE g.kind = 'personal' AND g.did_key = 'did:epigraph:personal:' || $3::text",
    )
    .bind(synthesis_id)
    .bind(query)
    .bind(test_agent_id())
    .bind(&[0u8; 32][..])
    .bind(visibility)
    .bind(prereqs)
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

/// One cluster of `synthesis_id` whose members are `claims`: the citation set
/// the in-process writer checks every claim-target outbox row against.
async fn cite(pool: &PgPool, synthesis_id: Uuid, claims: &[Uuid]) {
    sqlx::query(
        "INSERT INTO synthesis_clusters
         (id, synthesis_id, cluster_index, title, summary, member_claim_ids,
          support_count, contradict_count)
         VALUES ($1, $2, 0, 'cluster', 'summary', $3, 0, 0)",
    )
    .bind(Uuid::now_v7())
    .bind(synthesis_id)
    .bind(claims)
    .execute(pool)
    .await
    .expect("cluster citing the claims");
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
        None,
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
        predicates
            .iter()
            .filter(|p| **p == "WAS_DERIVED_FROM")
            .count(),
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
        None,
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
        None,
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

    assert_ne!(base, changed_query, "different query must change the hash");
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
        None,
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
        None,
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
        None,
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

/// The legacy runner's IN-PROCESS reconcile (`reconcile_stage6_inprocess`)
/// acts as each synthesis' JOB principal (D-S9): for a complete public
/// synthesis with pending outbox rows and a job row, it writes the kernel
/// edges and every `edge.added` event carries that principal as its actor. A
/// synthesis with pending rows but NO job row is skipped: no kernel edge, no
/// event, its rows stay pending (it never writes an event with no actor).
/// Kills: writing a principal-less synthesis' edges.
#[tokio::test]
async fn inprocess_reconcile_acts_as_the_job_principal_and_skips_one_without() {
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let owner = support::principal(&pool, "owner").await;
    let claim = support::any_public_claim(&pool).await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO syntheses
             (id, query, agent_id, status, subgraph_snapshot,
              clustering_method, llm_provider, llm_model,
              content_hash, visibility, owner_group_id)
             VALUES ($1, 'reconcile', $2, 'pending', '{}'::jsonb,
                     'signed_louvain', 'mock', 'mock', $3, 'public', $4)",
        )
        .bind(id)
        .bind(owner.agent)
        .bind(&[0u8; 32][..])
        .bind(owner.personal_group)
        .execute(&pool)
        .await
        .expect("insert synthesis row");
        publish::stage6_plan_edges(&pool, id, &[claim], None, &[], owner.agent, None)
            .await
            .expect("plan edges");
        cite(&pool, id, &[claim]).await;
        force_complete(&pool, id).await;
        ids.push(id);
    }
    let (with_job, without_job) = (ids[0], ids[1]);
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, job_type, payload, state, principal_id)
         VALUES ($1, 'synthesis', '{}'::jsonb, 'complete', $2)",
    )
    .bind(with_job)
    .bind(owner.agent)
    .execute(&pool)
    .await
    .expect("job row");

    publish::reconcile_stage6_inprocess(&pool)
        .await
        .expect("reconcile");

    let edges = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM edges WHERE source_id = $1 AND source_type = 'synthesis'",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let actors = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT actor_id FROM events WHERE event_type = 'edge.added'
                   AND payload->>'source_id' = $1",
            )
            .bind(id.to_string())
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(edges(with_job).await, 2, "claim + ATTRIBUTED_TO written");
    let a = actors(with_job).await;
    assert_eq!(a.len(), 2, "one edge.added per edge");
    assert!(a.iter().all(|x| *x == Some(owner.agent)), "{a:?}");
    assert_eq!(
        SynthesisProvoEdgesRepository::count_pending(&pool, with_job)
            .await
            .unwrap(),
        0
    );

    assert_eq!(edges(without_job).await, 0, "no job principal: skipped");
    assert!(
        actors(without_job).await.is_empty(),
        "no principal-less event"
    );
    assert_eq!(
        SynthesisProvoEdgesRepository::count_pending(&pool, without_job)
            .await
            .unwrap(),
        2,
        "its rows stay pending"
    );
}

/// The in-process writer never names a claim the synthesis does not cite
/// (E1f review D1). A complete public synthesis carries an unwritten outbox
/// row for a claim no cluster cites: what an earlier attempt leaves behind
/// when its retry no longer cites the claim and the replan could not discard
/// the row, or a row accumulated before the replan rule existed. The legacy
/// runner's reconcile writes the cited claim's edge and the attribution,
/// DISCARDS the uncited row, and writes no kernel edge and no `edge.added`
/// naming that claim. Kills: the write-time citation guard removed (the
/// uncited row becomes a third kernel edge).
#[tokio::test]
async fn the_inprocess_writer_discards_an_uncited_row_and_never_names_its_claim() {
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let owner = support::principal(&pool, "owner").await;
    let cited = support::any_public_claim(&pool).await;
    let uncited = support::any_public_claim(&pool).await;
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, owner_group_id)
         VALUES ($1, 'uncited', $2, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock', $3, 'public', $4)",
    )
    .bind(id)
    .bind(owner.agent)
    .bind(&[0u8; 32][..])
    .bind(owner.personal_group)
    .execute(&pool)
    .await
    .expect("insert synthesis row");
    publish::stage6_plan_edges(&pool, id, &[cited, uncited], None, &[], owner.agent, None)
        .await
        .expect("plan edges");
    cite(&pool, id, &[cited]).await;
    force_complete(&pool, id).await;
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, job_type, payload, state, principal_id)
         VALUES ($1, 'synthesis', '{}'::jsonb, 'complete', $2)",
    )
    .bind(id)
    .bind(owner.agent)
    .execute(&pool)
    .await
    .expect("job row");

    publish::reconcile_stage6_inprocess(&pool)
        .await
        .expect("reconcile");

    let mut targets: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT target_type, target_id FROM edges WHERE source_id = $1 AND source_type = 'synthesis'",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    targets.sort();
    let mut want = vec![
        ("agent".to_string(), owner.agent),
        ("claim".to_string(), cited),
    ];
    want.sort();
    assert_eq!(targets, want, "the cited claim and the attribution only");
    let (rows, named): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND target_id = $2),
                (SELECT count(*) FROM events WHERE event_type = 'edge.added' AND payload->>'target_id' = $3)",
    )
    .bind(id)
    .bind(uncited)
    .bind(uncited.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (rows, named),
        (0, 0),
        "the uncited row is discarded; no event names it"
    );
    assert_eq!(
        SynthesisProvoEdgesRepository::count_pending(&pool, id)
            .await
            .unwrap(),
        0
    );
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
        None,
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

// ──────────────────────────────────────────────────────────────────────────────
// E1d — stage 6 is public-only
// ──────────────────────────────────────────────────────────────────────────────

async fn deferred_rows(pool: &PgPool, synthesis_id: Uuid) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT predicate, deferred_reason FROM synthesis_provo_edges
          WHERE synthesis_id = $1 ORDER BY predicate, target_id",
    )
    .bind(synthesis_id)
    .fetch_all(pool)
    .await
    .expect("outbox rows")
}

/// T-J4a: a GROUP synthesis POSTs no kernel edge; its outbox rows are
/// deferred as `private`, which does not block completion; after it is
/// widened (the deferral cleared, as the visibility PATCH does) the startup
/// reconcile writes them. Kills: the public-only gate removed (the writer
/// would be called), deferred rows counted as pending (completion would be
/// refused), or the reconcile skipping released rows.
#[tokio::test]
async fn a_group_synthesis_posts_no_edge_and_defers_its_outbox() {
    // Its own database: the reconcile below scans every complete synthesis.
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row_with(&pool, synthesis_id, "stage6 group synthesis", "group", &[]).await;
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[Uuid::now_v7()],
        None,
        &[],
        test_agent_id(),
        None,
    )
    .await
    .expect("plan edges");

    let writer = FakeEdgeWriter::new();
    publish::stage6_write_edges(&pool, &writer, synthesis_id)
        .await
        .expect("a group synthesis' stage 6 succeeds without writing");
    assert_eq!(
        writer.call_count(),
        0,
        "no kernel edge may name a group synthesis"
    );
    let rows = deferred_rows(&pool, synthesis_id).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|(_, r)| r.as_deref() == Some("private")),
        "{rows:?}"
    );
    publish::stage6_mark_complete(&pool, synthesis_id, "narrative", &[1u8; 32])
        .await
        .expect("deferred rows do not block completion");

    // Widened out of band (the PATCH's statements: the interlock, the
    // visibility, the release), then reconciled.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('episcience.allow_widen', 'yes', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE syntheses SET visibility = 'public' WHERE id = $1")
        .bind(synthesis_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE synthesis_provo_edges SET deferred_reason = NULL WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let writer = FakeEdgeWriter::new();
    publish::reconcile_stage6_on_startup(&pool, &writer)
        .await
        .expect("reconcile");
    assert!(
        writer
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.source_id == synthesis_id)
            .count()
            == 2,
        "the released rows are written once the synthesis is public"
    );
    cleanup(&pool, synthesis_id).await;
}

/// D-M3 (the stage-6 half): a PUBLIC synthesis whose prerequisite is a GROUP
/// synthesis is not publishable, so no kernel edge is written (the COMPOSED_OF
/// edge would name the private prerequisite in public). Kills: a gate on the
/// synthesis' own visibility alone.
#[tokio::test]
async fn a_public_synthesis_with_a_group_prerequisite_posts_no_edge() {
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let prereq = Uuid::now_v7();
    insert_synthesis_row_with(&pool, prereq, "stage6 group prereq", "group", &[]).await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row_with(
        &pool,
        synthesis_id,
        "stage6 public over group",
        "public",
        &[prereq],
    )
    .await;
    publish::stage6_plan_edges(
        &pool,
        synthesis_id,
        &[],
        None,
        &[prereq],
        test_agent_id(),
        None,
    )
    .await
    .expect("plan edges");
    let writer = FakeEdgeWriter::new();
    publish::stage6_write_edges(&pool, &writer, synthesis_id)
        .await
        .expect("stage 6 succeeds without writing");
    assert_eq!(writer.call_count(), 0);
    assert!(!publish::is_publishable(&pool, synthesis_id).await.unwrap());
    cleanup(&pool, synthesis_id).await;
    cleanup(&pool, prereq).await;
}
