//! Stage 6 (`publish::*`) integration tests.
//!
//! # DB strategy
//!
//! The run's shared clone of the E1 template (tests that must not see other
//! tests' rows take their own clone). Each test creates its own `syntheses`
//! row and drives the stage-6 steps the way the synthesis handler does: each
//! on one transaction of the fixture pool (a privileged session; the
//! application-login behaviour is pinned by the worker and handler suites),
//! then verifies the database state.
//!
//! # Tests
//!
//! 1. `stage6_plan_inserts_edges_for_each_cited_claim` — plan, happy path
//! 2. `stage6_plan_idempotent_on_retry` — plan, re-entry
//! 3. `stage6_embed_creates_synthesis_embeddings_row` — the narrative head
//! 4. `compute_content_hash_is_deterministic` — hash determinism
//! 5. `compute_content_hash_changes_on_input_change` — hash sensitivity
//! 6. `stage6_write_edges_marks_all_pending_written` — kernel edges + events
//! 7. `stage6_mark_complete_only_when_no_pending` — completion gate
//! 8. `the_writer_discards_an_uncited_row_and_never_names_its_claim`
//! 9. `stage6_happy_path_plan_embed_hash_write_complete` — walkthrough
//! 10. `a_group_synthesis_posts_no_edge_and_defers_its_outbox` — public-only
//! 11. `a_public_synthesis_with_a_group_prerequisite_posts_no_edge`
mod support;

use async_trait::async_trait;
use chrono::Utc;
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::SubgraphSnapshot;
use episcience_db::publish::{self, EdgeWriteOutcome};
use episcience_db::{SynthesisEmbeddingsRepository, SynthesisProvoEdgesRepository};
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

/// Stage 6a on one transaction of `pool` (`publish::stage6_plan_edges_conn`).
async fn plan(
    pool: &PgPool,
    synthesis_id: Uuid,
    cited: &[Uuid],
    parent: Option<Uuid>,
    prereqs: &[Uuid],
    owner: Uuid,
) -> Result<(), SynthesisError> {
    let mut tx = pool.begin().await.expect("begin");
    publish::stage6_plan_edges_conn(&mut tx, synthesis_id, cited, parent, prereqs, owner, None)
        .await?;
    tx.commit().await.expect("commit");
    Ok(())
}

/// Stage 6b as the handler runs it: embed the narrative head, then store it.
async fn embed(
    pool: &PgPool,
    embedder: &dyn EmbeddingService,
    synthesis_id: Uuid,
    narrative: &str,
    model: &str,
) {
    let embedding = embedder
        .generate(publish::narrative_head(narrative))
        .await
        .expect("embed");
    SynthesisEmbeddingsRepository::upsert(pool, synthesis_id, &embedding, model, "narrative_head")
        .await
        .expect("store the embedding");
}

/// Stage 6d on one transaction of `pool`, acting as `actor`
/// (`publish::stage6_write_edges_conn`: kernel edges and their events in
/// process). The transaction commits whatever the writer did.
async fn write(pool: &PgPool, synthesis_id: Uuid, actor: Uuid) -> EdgeWriteOutcome {
    let mut tx = pool.begin().await.expect("begin");
    let outcome = publish::stage6_write_edges_conn(&mut tx, synthesis_id, Some(actor))
        .await
        .expect("the outbox is readable");
    tx.commit().await.expect("commit");
    outcome
}

/// Stage 6e on one transaction of `pool` (`publish::stage6_mark_complete_conn`).
async fn mark_complete(
    pool: &PgPool,
    synthesis_id: Uuid,
    narrative: &str,
    hash: &[u8; 32],
) -> Result<(), SynthesisError> {
    let mut tx = pool.begin().await.expect("begin");
    publish::stage6_mark_complete_conn(&mut tx, synthesis_id, narrative, hash).await?;
    tx.commit().await.expect("commit");
    Ok(())
}

/// Kernel edges sourced at synthesis `id`.
async fn kernel_edges(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_id = $1 AND source_type = 'synthesis'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("kernel edges")
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
// 6a — plan
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_plan_inserts_edges_for_each_cited_claim() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6a happy path").await;

    let claim_a = Uuid::now_v7();
    let claim_b = Uuid::now_v7();

    plan(
        &pool,
        synthesis_id,
        &[claim_a, claim_b],
        None,
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan happy path");

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
    plan(
        &pool,
        synthesis_id,
        &[claim_a],
        Some(parent),
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan first call");

    let n1 = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count first");
    assert_eq!(n1, 3, "first call should plan 3 edges");

    // Second invocation with identical args — must succeed and not duplicate.
    plan(
        &pool,
        synthesis_id,
        &[claim_a],
        Some(parent),
        &[],
        test_agent_id(),
    )
    .await
    .expect("plan second call (idempotent)");

    let n2 = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count second");
    assert_eq!(n2, n1, "second call must not insert duplicates");

    cleanup(&pool, synthesis_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// 6b — embed the narrative head
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_embed_creates_synthesis_embeddings_row() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6b embed").await;

    let embedder = FixedEmbedder::default();
    let narrative = "First paragraph thesis sentence.\n\nSecond paragraph detail.";
    assert_eq!(
        publish::narrative_head(narrative),
        "First paragraph thesis sentence.",
        "the head is the first paragraph"
    );

    embed(
        &pool,
        &embedder,
        synthesis_id,
        narrative,
        "test-embedding-model-v1",
    )
    .await;

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
// 6c — compute_content_hash
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
// 6d — write the kernel edges in process
// ──────────────────────────────────────────────────────────────────────────────

/// A public synthesis' planned outbox is written as kernel edges on the
/// caller's transaction: every row written and naming its kernel edge, one
/// `edge.added` event per edge carrying the acting principal. Kills: rows
/// marked written without an edge, and events written with no actor.
#[tokio::test]
async fn stage6_write_edges_marks_all_pending_written() {
    // Its own database: it writes kernel edges and events.
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6d write edges").await;
    let claims = [
        support::any_public_claim(&pool).await,
        support::any_public_claim(&pool).await,
    ];
    cite(&pool, synthesis_id, &claims).await;

    // 2 cited claims + ATTRIBUTED_TO = 3.
    plan(&pool, synthesis_id, &claims, None, &[], test_agent_id())
        .await
        .expect("plan edges");

    let outcome = write(&pool, synthesis_id, test_agent_id()).await;
    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.written.len(), 3, "one kernel edge per planned row");

    let remaining = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .expect("count_pending after write");
    assert_eq!(remaining, 0, "all edges should be written");
    let named: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM synthesis_provo_edges p JOIN edges e ON e.id = p.epigraph_edge_id
          WHERE p.synthesis_id = $1 AND e.source_id = $1 AND e.relationship = p.predicate
            AND e.target_id = p.target_id",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("rows naming their edge");
    assert_eq!(named, 3, "every row names the kernel edge written for it");
    let actors: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT actor_id FROM events WHERE event_type = 'edge.added' AND payload->>'source_id' = $1",
    )
    .bind(synthesis_id.to_string())
    .fetch_all(&pool)
    .await
    .expect("events");
    assert_eq!(
        actors,
        vec![Some(test_agent_id()); 3],
        "one edge.added per edge, as the actor"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// 6e — mark complete
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stage6_mark_complete_only_when_no_pending() {
    // Its own database: it writes kernel edges and events.
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage6e mark complete").await;
    let claim = support::any_public_claim(&pool).await;
    cite(&pool, synthesis_id, &[claim]).await;

    // Plan some edges but DON'T write them.
    plan(&pool, synthesis_id, &[claim], None, &[], test_agent_id())
        .await
        .expect("plan edges");

    // First attempt: must refuse because edges are still pending.
    let hash = [42u8; 32];
    let r = mark_complete(&pool, synthesis_id, "narrative", &hash).await;
    match r {
        Err(SynthesisError::EdgeWrite(msg)) => {
            assert!(
                msg.contains("pending"),
                "error should mention pending edges, got {msg:?}"
            );
        }
        other => panic!("expected EdgeWrite error, got {other:?}"),
    }

    // Now write them.
    let outcome = write(&pool, synthesis_id, test_agent_id()).await;
    assert_eq!(outcome.failure, None);

    // Second attempt: must succeed and set status='complete'.
    mark_complete(&pool, synthesis_id, "narrative", &hash)
        .await
        .expect("mark complete after writing edges");

    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .fetch_one(&pool)
        .await
        .expect("fetch status");
    assert_eq!(status, "complete");
}

/// The in-process writer never names a claim the synthesis does not cite
/// (E1f review D1). A public synthesis carries an unwritten outbox row for a
/// claim no cluster cites: what an earlier attempt leaves behind when its
/// retry no longer cites the claim and the replan could not discard the row,
/// or a row accumulated before the replan rule existed. The writer writes the
/// cited claim's edge and the attribution, DISCARDS the uncited row, and
/// writes no kernel edge and no `edge.added` naming that claim. Kills: the
/// write-time citation guard removed (the uncited row becomes a third kernel
/// edge).
#[tokio::test]
async fn the_writer_discards_an_uncited_row_and_never_names_its_claim() {
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
    plan(&pool, id, &[cited, uncited], None, &[], owner.agent)
        .await
        .expect("plan edges");
    cite(&pool, id, &[cited]).await;

    let outcome = write(&pool, id, owner.agent).await;
    assert_eq!((outcome.discarded, outcome.failure), (1, None));

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
    // Its own database: it writes kernel edges and events.
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = Uuid::now_v7();
    let query = "what is well-supported about X?";
    insert_synthesis_row(&pool, synthesis_id, query).await;

    let claim_a = support::any_public_claim(&pool).await;
    cite(&pool, synthesis_id, &[claim_a]).await;
    let narrative = "Lead paragraph stating thesis.\n\nDetail paragraph.";
    let snap = empty_snapshot();

    // 1. Plan
    plan(&pool, synthesis_id, &[claim_a], None, &[], test_agent_id())
        .await
        .expect("plan");

    // 2. Embed
    let embedder = FixedEmbedder::default();
    embed(&pool, &embedder, synthesis_id, narrative, "stub-model-1").await;

    // 3. Hash
    let hash = publish::compute_content_hash(query, &snap, narrative);

    // 4. Write
    let outcome = write(&pool, synthesis_id, test_agent_id()).await;
    assert_eq!((outcome.written.len(), outcome.failure), (2, None));

    // 5. Mark complete
    mark_complete(&pool, synthesis_id, narrative, &hash)
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

/// T-J4a: a GROUP synthesis writes no kernel edge; its outbox rows are
/// deferred as `private`, which does not block completion; after it is
/// widened (the deferral cleared, as the visibility PATCH does) the next
/// write (the worker's `stage6_pending` worklist) writes them. Kills: the
/// public-only gate removed (edges would be written), deferred rows counted
/// as pending (completion would be refused), or the writer skipping released
/// rows.
#[tokio::test]
async fn a_group_synthesis_posts_no_edge_and_defers_its_outbox() {
    let db = support::TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row_with(&pool, synthesis_id, "stage6 group synthesis", "group", &[]).await;
    let claim = support::any_public_claim(&pool).await;
    cite(&pool, synthesis_id, &[claim]).await;
    plan(&pool, synthesis_id, &[claim], None, &[], test_agent_id())
        .await
        .expect("plan edges");

    let outcome = write(&pool, synthesis_id, test_agent_id()).await;
    assert_eq!(
        (outcome.written.len(), outcome.deferred, outcome.failure),
        (0, 2, None)
    );
    assert_eq!(
        kernel_edges(&pool, synthesis_id).await,
        0,
        "no kernel edge may name a group synthesis"
    );
    let rows = deferred_rows(&pool, synthesis_id).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|(_, r)| r.as_deref() == Some("private")),
        "{rows:?}"
    );
    mark_complete(&pool, synthesis_id, "narrative", &[1u8; 32])
        .await
        .expect("deferred rows do not block completion");

    // Widened out of band (the PATCH's statements: the interlock, the
    // visibility, the release), then written again.
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
    let outcome = write(&pool, synthesis_id, test_agent_id()).await;
    assert_eq!((outcome.written.len(), outcome.failure), (2, None));
    assert_eq!(
        kernel_edges(&pool, synthesis_id).await,
        2,
        "the released rows are written once the synthesis is public"
    );
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
    plan(&pool, synthesis_id, &[], None, &[prereq], test_agent_id())
        .await
        .expect("plan edges");
    let outcome = write(&pool, synthesis_id, test_agent_id()).await;
    assert_eq!(
        (outcome.written.len(), outcome.deferred, outcome.failure),
        (0, 2, None)
    );
    assert_eq!(kernel_edges(&pool, synthesis_id).await, 0);
    assert!(!publish::is_publishable(&pool, synthesis_id).await.unwrap());
}
