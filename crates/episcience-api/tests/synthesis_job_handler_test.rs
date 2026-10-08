//! Integration test for [`SynthesisJobHandler`].
//!
//! Drives single jobs through every pipeline stage on the run's shared clone
//! of the E1 template (`scripts/e1-test-db.sh` exports `DATABASE_URL`; the
//! template is seeded with two public `origami` claims, `aaaa…` / `bbbb…`,
//! which stage 1 recalls). Every job runs the way `episcience-worker` runs it
//! ([`SynthesisJobHandler::run`] on an owner session of the
//! `episcience_worker` application login, acting as the job row's
//! principal): the only runtime the handler has since the in-process runner
//! was deleted. Fixtures are written on the clone's superuser pool.
//!
//! Run through `scripts/e1-test-db.sh <batch> -- cargo test --test synthesis_job_handler_test`.
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
//!   kernel edges in process, marks synthesis complete.
//!
//! Asserts:
//! - `run` returns `Ok(JobResult)` with the synthesis id in the output.
//! - `syntheses.status = 'complete'` and narrative non-empty.
//! - `synthesis_provo_edges` rows are all written (`written_at IS NOT NULL`).
//! - every outbox row names the kernel edge written for it.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use epigraph_cli::enrichment::llm_client::MockLlmClient;
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use epigraph_jobs::JobResult;
use episcience_api::jobs::synthesis_job::RunError;
use episcience_api::jobs::{
    resolve_skill_for_row, EmptyEdgeProvider, OwnerSession, StageSession, SynthesisJobHandler,
    SynthesisJobPayload,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

// ─── Test doubles ───────────────────────────────────────────────────────────

/// Embedder that:
/// - errors on `generate_query` to force `recall::recall` onto the text-search
///   fallback (deterministic against the pre-seeded `origami` claims).
/// - returns a fixed 1536-dim embedding from `generate` (used by Stage 6
///   narrative-head embedding; the column is `vector(1536)`).
/// - errors on `get` (used by Stage 2 relevance closure — but with
///   `EmptyEdgeProvider` no neighbours are visited so it's never called).
///   The production embedder (`OpenAiProvider`) errors on `get` too, so a
///   stage that ranks claims by `get` would find nothing in production.
/// - `query`: `None` (the default) makes `generate_query` error; `Some(v)`
///   makes it return `v` (the theme-seed tests, which need a query vector).
#[derive(Debug)]
struct TestEmbedder {
    embedding: Vec<f32>,
    query: Option<Vec<f32>>,
}

impl Default for TestEmbedder {
    fn default() -> Self {
        Self {
            // 1536 = primary embedding dim per epigraph migration 5013.
            embedding: (0..1536).map(|i| (i as f32) * 1e-4).collect(),
            query: None,
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
        if let Some(q) = &self.query {
            return Ok(q.clone());
        }
        // Force text-search fallback in recall::recall — same trick as
        // synthesis_pipeline_stage1_test::ErroringEmbedder.
        Err(EmbeddingError::ApiError {
            message: "test stub: generate_query disabled".to_string(),
            status_code: None,
        })
    }
}

// ─── DB helpers ─────────────────────────────────────────────────────────────

/// The run's shared clone of the E1 template (scripts/e1-test-db.sh). Refuses
/// port 5432 and any database name not ending in `_test`; no default DSN.
async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
}

/// `ENGINE_POOL` on the database `pool` is connected to: an unstamped pool
/// of the `episcience_worker` application login, exactly what
/// `episcience-worker` hands the handler for the engine's reads
/// (`V1-engine-takes-pool`: public claims only until KE-1).
async fn engine_pool(pool: &PgPool) -> PgPool {
    let opts = testdb::check_test_url(&testdb::login_url_for(pool, testdb::WORKER_LOGIN))
        .unwrap_or_else(|e| panic!("{e}"));
    let engine = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .expect("ENGINE_POOL on the worker login");
    // Not vacuous: every run below is an UNPRIVILEGED session's.
    episcience_db::tenancy_contract::refuse_privileged_session(&engine)
        .await
        .expect("the worker login is unprivileged");
    engine
}

/// Run `handler` for the job whose payload is `payload` the way
/// `episcience-worker` runs a claimed job: acting as the job ROW's
/// `principal_id` (read here as the queue definer returns it), on an owner
/// session of the `episcience_worker` login (every stage in its own
/// transaction stamped as that principal, re-resolved and checked against
/// the synthesis' owner group first).
async fn run_owned(
    pool: &PgPool,
    handler: &SynthesisJobHandler,
    payload: &serde_json::Value,
) -> Result<JobResult, RunError> {
    let payload: SynthesisJobPayload =
        serde_json::from_value(payload.clone()).expect("a synthesis payload");
    let principal: Uuid =
        sqlx::query_scalar("SELECT principal_id FROM synthesis_jobs WHERE id = $1")
            .bind(payload.synthesis_id)
            .fetch_one(pool)
            .await
            .expect("the job row's principal");
    let url = testdb::login_url_for(pool, testdb::WORKER_LOGIN);
    let scoped = ScopedPool::connect_with_options(
        &url,
        SessionGucMode::Session,
        ScopedPoolOptions {
            max_connections: 2,
            ..ScopedPoolOptions::default()
        },
    )
    .await
    .expect("stamped pool on the worker login");
    let resolve_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(testdb::check_test_url(&url).unwrap_or_else(|e| panic!("{e}")))
        .await
        .expect("RESOLVE_POOL on the worker login");
    let session = StageSession::Owner(OwnerSession {
        scoped: Arc::new(scoped),
        resolve_pool,
        principal,
        synthesis_id: payload.synthesis_id,
    });
    handler.run(&session, payload, principal).await
}

fn test_agent_id() -> Uuid {
    "f3951e28-9356-42b6-9c80-27dd9f01b19d".parse().unwrap()
}

async fn insert_synthesis_row(pool: &PgPool, synthesis_id: Uuid, query: &str) {
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, owner_group_id)
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock-model',
                 $4, 'public', public.epigraph_ensure_personal_group($3))",
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
          content_hash, visibility, skill_name, owner_group_id)
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock-model',
                 $4, 'group', $5, public.epigraph_ensure_personal_group($3))",
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
        "INSERT INTO synthesis_jobs (id, job_type, payload, state, principal_id)
         VALUES ($1, 'synthesis', $2, 'queued', ($2->>'agent_id')::uuid)",
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
        seed_theme_id: None,
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
    let handler = SynthesisJobHandler::new(
        engine_pool(&pool).await,
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        Arc::new(EmptyEdgeProvider),
        20, // cost_budget — generous; handler should consume ≤ 3 calls.
        "test-embedding-model",
        false,
    );

    let result = run_owned(&pool, &handler, &payload_value).await;

    // Diagnostics if the handler errors — print the row state so the failure
    // message points at the right stage.
    if let Err(ref e) = result {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT status, narrative FROM syntheses WHERE id = $1")
                .bind(synthesis_id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        eprintln!("handler.run errored: {e:?}; row state = {row:?}");
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

    // E1f: stage 6 writes the kernel PROV edges IN PROCESS on the stage
    // transaction; every written outbox row names a real kernel edge whose
    // source is this synthesis. Kills: marking rows written without an edge.
    let kernel_edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM edges e
           JOIN synthesis_provo_edges p ON p.epigraph_edge_id = e.id
          WHERE p.synthesis_id = $1 AND e.source_id = $1 AND e.source_type = 'synthesis'
            AND e.relationship = p.predicate AND e.target_id = p.target_id",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count kernel edges");
    assert_eq!(
        kernel_edges, total,
        "every outbox row names the kernel edge written for it"
    );

    cleanup(&pool, synthesis_id).await;
}

/// An acting principal that cannot be resolved stops the job before any
/// stage runs: `run` returns an AUTHORITY refusal (terminal: the worker
/// finishes the job `failed: authority` and never retries it), the synthesis
/// row is never touched (the principal's authority to write it is unknown),
/// no stage writes and the model is never called.
///
/// Kills: running the stages with a fallback viewer when resolution fails,
/// and mapping the failure to a retryable error.
#[tokio::test]
async fn handler_fails_closed_when_the_owner_cannot_be_resolved() {
    let db = testdb::TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "origami").await;
    let payload_value = serde_json::to_value(SynthesisJobPayload {
        synthesis_id,
        query: "origami".into(),
        traversal_config: None,
        agent_id: test_agent_id(),
        parent_synthesis_id: None,
        prereq_synthesis_ids: vec![],
        workflow_run_id: None,
        seed_theme_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;
    let llm = Arc::new(LiveStage5Llm::new(pool.clone(), synthesis_id));
    let handler = SynthesisJobHandler::new(
        engine_pool(&pool).await,
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        false,
    );
    // An unknown principal resolves to an EMPTY viewer (public only), which
    // is not a failure. A resolution failure needs the membership read itself
    // to fail, so this throwaway clone loses the table it reads (a
    // deterministic error, not a transient one).
    sqlx::query("ALTER TABLE public.group_memberships RENAME TO group_memberships_gone")
        .execute(&pool)
        .await
        .expect("break membership reads on this clone");
    let result = run_owned(&pool, &handler, &payload_value).await;
    match result {
        Err(RunError::Authority(m)) => assert!(m.contains("cannot be resolved"), "{m}"),
        other => panic!("expected an authority refusal, got {other:?}"),
    }
    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .fetch_one(&pool)
        .await
        .expect("row");
    assert_eq!(status, "pending", "the row is never touched");
    let members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(members, 0, "no stage may run");
    assert_eq!(
        *llm.call_count.lock().unwrap(),
        0,
        "the model is never called"
    );
}

/// T-J8 end to end (brief E1f requirement 3): `run` APPLIES the seed filter
/// to what the engine recalls. A public synthesis keeps public seeds only; a
/// group synthesis keeps public seeds plus claims its own owner group owns;
/// a claim of any other group never seeds it, whoever can read it.
///
/// The engine's reads run on the handler's `pool`. Here that is the fresh
/// clone's superuser pool, standing in for the engine read the KE-1
/// follow-up puts on the owner's stamped connection: it lets recall return
/// what the acting principal can read (H1's `group(H1pg)` and `group(T2)`
/// claims), which the worker's unstamped `ENGINE_POOL` cannot (public rows
/// only, V1), so the filter is observable today. Only the engine read is
/// substituted: every stage transaction, the filter's included, runs on the
/// `episcience_worker` login stamped as H1 ([`run_owned`]). At the KE-1
/// follow-up this pool becomes the stamped connection.
///
/// Positive controls (not vacuous): each non-public claim joins the group
/// synthesis its own group owns, so recall did return it.
///
/// Kills: `run` computing the filter and seeding stage 2 with the unfiltered
/// recall (the public synthesis would take H1's group claim, which the claim
/// guard admits on a row of H1's own group; a group(T) or group(T2) synthesis
/// would fail on the claim guard instead of completing), and a filter that
/// admits every group the owner can READ rather than the synthesis' own.
#[tokio::test]
async fn t_j8_run_seeds_only_from_public_claims_and_the_synthesis_own_group() {
    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    // The stage session's login is an unprivileged application login.
    let _ = engine_pool(&admin).await;
    let h1 = testdb::principal(&admin, "h1").await;
    let team_t = testdb::team_group(&admin, &h1, &[]).await;
    let team_t2 = testdb::team_group(&admin, &h1, &[]).await;
    let in_h1pg = testdb::claim(
        &admin,
        h1.agent,
        "origami seed-filter note owned by H1 personal group",
        0.9,
        epigraph_core::TenancyDecl::group(h1.personal_group),
    )
    .await;
    let in_t2 = testdb::claim(
        &admin,
        h1.agent,
        "origami seed-filter note owned by team T2",
        0.9,
        epigraph_core::TenancyDecl::group(team_t2),
    )
    .await;
    assert_eq!(
        testdb::claim_pair(&admin, in_h1pg).await,
        ("group".to_string(), h1.personal_group)
    );
    assert_eq!(
        testdb::claim_pair(&admin, in_t2).await,
        ("group".to_string(), team_t2)
    );
    let public_seed = Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa);

    // (visibility, owner group, takes the H1pg claim, takes the T2 claim)
    let cases = [
        ("public", h1.personal_group, false, false),
        ("group", h1.personal_group, true, false),
        ("group", team_t, false, false),
        ("group", team_t2, false, true),
    ];
    for (visibility, owner, takes_h1pg, takes_t2) in cases {
        let synthesis_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO syntheses
             (id, query, agent_id, status, subgraph_snapshot,
              clustering_method, llm_provider, llm_model,
              content_hash, visibility, owner_group_id)
             VALUES ($1, 'origami', $2, 'pending', '{}'::jsonb,
                     'signed_louvain', 'mock', 'mock-model', $3, $4, $5)",
        )
        .bind(synthesis_id)
        .bind(h1.agent)
        .bind(&[0u8; 32][..])
        .bind(visibility)
        .bind(owner)
        .execute(&admin)
        .await
        .expect("insert synthesis row");
        let payload_value = serde_json::to_value(SynthesisJobPayload {
            synthesis_id,
            query: "origami".into(),
            traversal_config: None,
            agent_id: h1.agent,
            parent_synthesis_id: None,
            prereq_synthesis_ids: vec![],
            workflow_run_id: None,
            seed_theme_id: None,
        })
        .expect("serialize payload");
        insert_synthesis_job_row(&admin, synthesis_id, &payload_value).await;
        let handler = SynthesisJobHandler::new(
            admin.clone(),
            Arc::new(TestEmbedder::default()),
            Arc::new(LiveStage5Llm::new(admin.clone(), synthesis_id)),
            Arc::new(EmptyEdgeProvider),
            20,
            "test-embedding-model",
            false,
        );
        let case = format!("{visibility} synthesis owned by {owner}");
        run_owned(&admin, &handler, &payload_value)
            .await
            .unwrap_or_else(|e| panic!("{case}: completes: {e:?}"));
        let members: Vec<Uuid> = sqlx::query_scalar(
            "SELECT claim_id FROM synthesis_claim_membership WHERE synthesis_id = $1",
        )
        .bind(synthesis_id)
        .fetch_all(&admin)
        .await
        .expect("membership");
        assert!(members.contains(&public_seed), "{case}: {members:?}");
        assert_eq!(
            members.contains(&in_h1pg),
            takes_h1pg,
            "{case}: {members:?}"
        );
        assert_eq!(members.contains(&in_t2), takes_t2, "{case}: {members:?}");
        let (status, stored): (String, String) =
            sqlx::query_as("SELECT status, visibility FROM syntheses WHERE id = $1")
                .bind(synthesis_id)
                .fetch_one(&admin)
                .await
                .expect("row");
        assert_eq!(status, "complete", "{case}");
        assert_eq!(stored, visibility, "{case}: never narrowed");
    }
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
        seed_theme_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;

    // UncitedStage5Llm is structurally identical to LiveStage5Llm but
    // deliberately omits any [<uuid>] citations from the per-cluster
    // summary, so the verifier rejects on UncitedMember.
    let llm = Arc::new(UncitedStage5Llm::new(pool.clone(), synthesis_id));
    let handler = SynthesisJobHandler::new(
        engine_pool(&pool).await,
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        false,
    );

    let result = run_owned(&pool, &handler, &payload_value).await;
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
    let kernel_edges: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM edges WHERE source_id = $1 AND source_type = 'synthesis'",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("count kernel edges");
    assert_eq!(
        kernel_edges, 0,
        "Reject path must write no kernel edge from the parent"
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
        seed_theme_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;
    // The parent JOB acts as a principal other than the parent row's author
    // (and the payload's agent), and the parent row names a prerequisite.
    let acting = testdb::principal(&pool, "refiner").await;
    let prereq = Uuid::now_v7();
    insert_synthesis_row(&pool, prereq, "prerequisite").await;
    sqlx::query("UPDATE syntheses SET prereq_synthesis_ids = ARRAY[$2]::uuid[] WHERE id = $1")
        .bind(synthesis_id)
        .bind(prereq)
        .execute(&pool)
        .await
        .expect("parent prerequisites");
    sqlx::query("UPDATE synthesis_jobs SET principal_id = $2 WHERE id = $1")
        .bind(synthesis_id)
        .bind(acting.agent)
        .execute(&pool)
        .await
        .expect("parent job principal");
    // The acting principal must be able to write the synthesis (the worker's
    // owner session checks it before every stage): the parent lives in the
    // refiner's own group while its row stays authored by another agent.
    sqlx::query("UPDATE syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(synthesis_id)
        .bind(acting.personal_group)
        .execute(&pool)
        .await
        .expect("the parent in the acting principal's group");

    // UncitedStage5Llm forces a Stage 6 reject (UncitedMember rubric).
    let llm = Arc::new(UncitedStage5Llm::new(pool.clone(), synthesis_id));
    let handler = SynthesisJobHandler::new(
        engine_pool(&pool).await,
        Arc::new(TestEmbedder::default()),
        llm.clone(),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        false,
    );

    let result = run_owned(&pool, &handler, &payload_value).await;
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

    // T-W17 at the worker's refinement site: the child row is authored by,
    // and its job acts as, the parent JOB's principal (never the parent
    // row's author or the payload's agent). E1d review R10: the child row
    // carries the parent's prerequisites (publishability reads the row).
    let (child_author, child_prereqs): (Uuid, Option<Vec<Uuid>>) =
        sqlx::query_as("SELECT agent_id, prereq_synthesis_ids FROM syntheses WHERE id = $1")
            .bind(child_id)
            .fetch_one(&pool)
            .await
            .expect("child author and prerequisites");
    assert_eq!(child_author, acting.agent);
    assert_eq!(child_prereqs, Some(vec![prereq]));
    let (child_principal, child_payload_agent): (Uuid, String) = sqlx::query_as(
        "SELECT principal_id, payload->>'agent_id' FROM synthesis_jobs WHERE id = $1",
    )
    .bind(child_id)
    .fetch_one(&pool)
    .await
    .expect("child job principal");
    assert_eq!(child_principal, acting.agent);
    assert_eq!(child_payload_agent, acting.agent.to_string());

    cleanup(&pool, synthesis_id).await;
    cleanup(&pool, prereq).await;
}

/// T-J4a, the EVENT half (brief E1d requirement 10; in process since E1f):
/// with events on, a PUBLIC synthesis that completes publishes `synthesis.complete`,
/// and a GROUP synthesis that completes publishes no `synthesis.*` event at
/// all (the kernel `events` table has no row security; the payload would
/// leak the query). Kills: `emit_event_if_configured` ignoring
/// publishability.
#[tokio::test]
async fn synthesis_events_are_published_for_public_syntheses_only() {
    let pool = connect().await;
    let mut ids = Vec::new();
    for visibility in ["public", "group"] {
        let synthesis_id = Uuid::now_v7();
        insert_synthesis_row(&pool, synthesis_id, "origami").await;
        sqlx::query("UPDATE syntheses SET visibility = $2 WHERE id = $1")
            .bind(synthesis_id)
            .bind(visibility)
            .execute(&pool)
            .await
            .expect("set visibility");
        let payload_value = serde_json::to_value(SynthesisJobPayload {
            synthesis_id,
            query: "origami".into(),
            traversal_config: None,
            agent_id: test_agent_id(),
            parent_synthesis_id: None,
            prereq_synthesis_ids: vec![],
            workflow_run_id: None,
            seed_theme_id: None,
        })
        .expect("serialize payload");
        insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;
        let handler = SynthesisJobHandler::new(
            engine_pool(&pool).await,
            Arc::new(TestEmbedder::default()),
            Arc::new(LiveStage5Llm::new(pool.clone(), synthesis_id)),
            Arc::new(EmptyEdgeProvider),
            20,
            "test-embedding-model",
            true,
        );
        run_owned(&pool, &handler, &payload_value)
            .await
            .unwrap_or_else(|e| panic!("{visibility} synthesis completes: {e:?}"));
        ids.push(synthesis_id);
    }
    // E1f: the events are written IN PROCESS into the kernel `events` table
    // on the completing transaction.
    let seen = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT event_type::text FROM events
                  WHERE event_type::text LIKE 'synthesis.%' AND payload->>'synthesis_id' = $1
                  ORDER BY graph_version",
            )
            .bind(id.to_string())
            .fetch_all(&pool)
            .await
            .expect("read events")
        }
    };
    assert_eq!(
        seen(ids[0]).await,
        vec!["synthesis.complete".to_string()],
        "a public synthesis publishes exactly its completion"
    );
    assert!(
        seen(ids[1]).await.is_empty(),
        "a group synthesis publishes no synthesis.* event"
    );
    for id in ids {
        cleanup(&pool, id).await;
    }
}

/// E1d review R10: stage 6 plans its prerequisite (`COMPOSED_OF`) edges from
/// the synthesis ROW, the source every publishability check reads, not from
/// the job payload: a prerequisite named only in the payload gets no edge,
/// the row's gets one. Kills: planning from `payload.prereq_synthesis_ids`
/// (a payload that disagrees with the row would publish an edge to an input
/// the checks never saw).
#[tokio::test]
async fn stage6_plans_prerequisite_edges_from_the_row_not_the_payload() {
    let pool = connect().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "origami").await;
    let on_row = Uuid::now_v7();
    insert_synthesis_row(&pool, on_row, "row prerequisite").await;
    sqlx::query("UPDATE syntheses SET prereq_synthesis_ids = ARRAY[$2]::uuid[] WHERE id = $1")
        .bind(synthesis_id)
        .bind(on_row)
        .execute(&pool)
        .await
        .expect("row prerequisites");
    let only_in_payload = Uuid::now_v7();
    let payload_value = serde_json::to_value(SynthesisJobPayload {
        synthesis_id,
        query: "origami".into(),
        traversal_config: None,
        agent_id: test_agent_id(),
        parent_synthesis_id: None,
        prereq_synthesis_ids: vec![only_in_payload],
        workflow_run_id: None,
        seed_theme_id: None,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(&pool, synthesis_id, &payload_value).await;
    let handler = SynthesisJobHandler::new(
        engine_pool(&pool).await,
        Arc::new(TestEmbedder::default()),
        Arc::new(LiveStage5Llm::new(pool.clone(), synthesis_id)),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        false,
    );
    run_owned(&pool, &handler, &payload_value)
        .await
        .expect("handler runs to completion");
    let targets: Vec<Uuid> = sqlx::query_scalar(
        "SELECT target_id FROM synthesis_provo_edges \
          WHERE synthesis_id = $1 AND predicate = 'COMPOSED_OF' ORDER BY 1",
    )
    .bind(synthesis_id)
    .fetch_all(&pool)
    .await
    .expect("planned prerequisite edges");
    assert_eq!(targets, vec![on_row]);
    cleanup(&pool, synthesis_id).await;
    cleanup(&pool, on_row).await;
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
impl epigraph_cli::enrichment::llm_client::LlmProvider for UncitedStage5Llm {
    fn name(&self) -> &str {
        "uncited-stage5-mock"
    }

    fn is_active(&self) -> bool {
        true
    }

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
impl epigraph_cli::enrichment::llm_client::LlmProvider for LiveStage5Llm {
    fn name(&self) -> &str {
        "live-stage5-mock"
    }

    fn is_active(&self) -> bool {
        true
    }

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
// Both tests below use `'baseline'` (explicit, then the default). That is
// still load-bearing: it proves (a) the request deserializer accepts the
// optional field, (b) the value (or its default) reaches the INSERT. Which
// names the `syntheses_skill_name_known` CHECK accepts (every registered
// skill, as of 5041) is pinned in
// `episcience-db/tests/synthesis_repo_test.rs`.

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint_test_jwt};

fn bearer(token: &str) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
    )
}

async fn build_test_server(pool: PgPool) -> TestServer {
    use epigraph_embeddings::EmbeddingService as EmbeddingServiceTrait;
    let embedder: Arc<dyn EmbeddingServiceTrait> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        db: testdb::app_db_for(&pool).await,
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
    let server = build_test_server(pool.clone()).await;

    let agent_id_p = testdb::principal(&pool, "agent_id").await;

    let agent_id = agent_id_p.agent;
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
    let server = build_test_server(pool.clone()).await;

    let agent_id_p = testdb::principal(&pool, "agent_id").await;

    let agent_id = agent_id_p.agent;
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

/// Empty-priors path: a candidate with no prior to compare must score exactly
/// 1.0 (fully novel) with no neighbours, whatever its embeddings.
///
/// The candidate is synthetic (Uuid::now_v7() plus two synthetic member
/// ids); no DB rows are pre-seeded for it, so it has no job principal and is
/// compared with nothing — the early-return path runs.
#[tokio::test]
async fn novelty_is_one_when_no_priors() {
    use episcience_db::synthesis::novelty::{NoveltyBackend, NoveltyCandidate};
    use episcience_db::synthesis::novelty_backend_internal::InternalNoveltyBackend;

    let pool = connect().await;
    let reader = epigraph_db::Viewer::resolve(&pool, Uuid::now_v7())
        .await
        .expect("resolve a reader");
    let head = TestEmbedder::default()
        .generate("a novel summary")
        .await
        .expect("embed");
    let members = vec![Uuid::now_v7(), Uuid::now_v7()];
    let mut conn = pool.acquire().await.expect("connection");

    let score = InternalNoveltyBackend
        .score(
            &mut conn,
            &reader,
            &NoveltyCandidate {
                id: Uuid::now_v7(),
                member_ids: &members,
                head_embedding: &head,
                narrative_embedding: None,
            },
        )
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

/// `"literature"` → `PaperNoveltyBackend`. Mirror of the production
/// dispatch path so the rule "literature skill → paper_novelty backend"
/// is regression-protected without standing up the full pipeline.
#[test]
fn select_novelty_backend_literature_picks_paper_novelty() {
    use episcience_api::jobs::select_novelty_backend;

    let backend = select_novelty_backend("literature");
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
#[test]
fn select_novelty_backend_other_skills_pick_internal() {
    use episcience_api::jobs::select_novelty_backend;

    for skill in [
        "baseline",
        "lab_notebook",
        "code_review",
        "registry_diff",
        "unknown_skill_xyz",
    ] {
        let backend = select_novelty_backend(skill);
        assert_eq!(
            backend.name(),
            "internal_prior_syntheses",
            "skill {skill:?} must select InternalNoveltyBackend (default arm)"
        );
    }
}

// ─── Theme-anchored Stage 1 seed (wiki articles) ────────────────────────────
//
// Each test clones its own database: the fixture adds PUBLIC claims, which a
// concurrent text-recall test on the shared clone could otherwise pick up.
// Fixture claim texts never contain "origami" (the template's text-recall
// term), so the text path can be told apart from the theme path.

/// 1536 = `claims.embedding` (kernel migration 001).
const DIM: usize = 1536;

/// Unit vector on axis `k`.
fn e(k: usize) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    v[k] = 1.0;
    v
}

fn pgvec(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

/// A theme T and its claims (see [`seed_theme_fixture`]).
struct ThemeFixture {
    theme: Uuid,
    actor: testdb::Principal,
    other_group: Uuid,
    a: Uuid,
    b: Uuid,
    c: Uuid,
    d: Uuid,
    e: Uuid,
    f: Uuid,
    g: Uuid,
}

/// A claim with a stored embedding, optionally a member of `theme`.
async fn embedded_claim(
    pool: &PgPool,
    author: Uuid,
    content: &str,
    decl: epigraph_core::TenancyDecl,
    embedding: &[f32],
    theme: Option<Uuid>,
) -> Uuid {
    let id = testdb::claim(pool, author, content, 0.9, decl).await;
    sqlx::query("UPDATE public.claims SET embedding = $2::vector, theme_id = $3 WHERE id = $1")
        .bind(id)
        .bind(pgvec(embedding))
        .bind(theme)
        .execute(pool)
        .await
        .expect("store fixture embedding + theme");
    id
}

/// One theme T ("Origami folding", keyed on `properties`) and:
/// - A, B, C: public members of T on axes 1, 2, 3. A also leans 0.05 toward
///   the query axis 0, so it is the most relevant member and is picked first
///   (deterministic: without it A and D tie at relevance 0 and the kernel's
///   ORDER BY has no tiebreak).
/// - D: public member of T, A's near-duplicate (axis 1 plus 0.001 on axis 4;
///   cosine to A ≈ 0.9987 ≥ `DUP_COSINE`).
/// - E: public, NOT in T, embedding exactly the query vector `e(0)` — the
///   first thing a text/vector seed would pick.
/// - F: member of T, `group`-private to a team the actor is not in.
/// - G: member of T, `group`-private to the actor's own personal group (the
///   positive control: a readable group claim does flow through the theme seed).
async fn seed_theme_fixture(pool: &PgPool) -> ThemeFixture {
    let actor = testdb::principal(pool, "wiki-actor").await;
    let author = testdb::principal(pool, "wiki-author").await;
    let outsider = testdb::principal(pool, "wiki-outsider").await;
    let other_group = testdb::team_group(pool, &outsider, &[]).await;
    let theme: Uuid = sqlx::query_scalar(
        "INSERT INTO public.claim_themes (label, description, properties) \
         VALUES ('Origami folding', '', \
                 jsonb_build_object('cluster_run_id', gen_random_uuid(), 'cluster_id', 7)) \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("insert theme");
    let public = epigraph_core::TenancyDecl::public(author.personal_group);
    let mut a_emb = e(1);
    a_emb[0] = 0.05;
    let mut d_emb = e(1);
    d_emb[4] = 0.001;
    let t = Some(theme);
    let a = embedded_claim(pool, author.agent, "crease fact A", public, &a_emb, t).await;
    let b = embedded_claim(pool, author.agent, "crease fact B", public, &e(2), t).await;
    let c = embedded_claim(pool, author.agent, "crease fact C", public, &e(3), t).await;
    let d = embedded_claim(pool, author.agent, "crease fact A again", public, &d_emb, t).await;
    let e_off = embedded_claim(pool, author.agent, "off-theme fact E", public, &e(0), None).await;
    let f = embedded_claim(
        pool,
        outsider.agent,
        "crease fact F, private to another team",
        epigraph_core::TenancyDecl::group(other_group),
        &e(6),
        t,
    )
    .await;
    let g = embedded_claim(
        pool,
        actor.agent,
        "crease fact G, private to the actor's group",
        epigraph_core::TenancyDecl::group(actor.personal_group),
        &e(5),
        t,
    )
    .await;
    assert_eq!(
        testdb::claim_pair(pool, f).await,
        ("group".to_string(), other_group),
        "F must really be private, or every 'never F' assertion is vacuous"
    );
    ThemeFixture {
        theme,
        actor,
        other_group,
        a,
        b,
        c,
        d,
        e: e_off,
        f,
        g,
    }
}

/// A pending synthesis of `agent` (`visibility`, `owner`) plus its queued job
/// row; returns the job payload.
async fn enqueue_owned(
    pool: &PgPool,
    agent: Uuid,
    visibility: &str,
    owner: Uuid,
    query: &str,
    seed_theme_id: Option<Uuid>,
) -> serde_json::Value {
    let synthesis_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, owner_group_id)
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock-model', $4, $5, $6)",
    )
    .bind(synthesis_id)
    .bind(query)
    .bind(agent)
    .bind(&[0u8; 32][..])
    .bind(visibility)
    .bind(owner)
    .execute(pool)
    .await
    .expect("insert synthesis row");
    let payload = serde_json::to_value(SynthesisJobPayload {
        synthesis_id,
        query: query.into(),
        traversal_config: None,
        agent_id: agent,
        parent_synthesis_id: None,
        prereq_synthesis_ids: vec![],
        workflow_run_id: None,
        seed_theme_id,
    })
    .expect("serialize payload");
    insert_synthesis_job_row(pool, synthesis_id, &payload).await;
    payload
}

fn payload_synthesis_id(payload: &serde_json::Value) -> Uuid {
    payload["synthesis_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("synthesis_id in payload")
}

async fn members_of(pool: &PgPool, synthesis_id: Uuid) -> std::collections::BTreeSet<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT claim_id FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(synthesis_id)
    .fetch_all(pool)
    .await
    .expect("membership")
    .into_iter()
    .collect()
}

fn theme_handler(
    engine: PgPool,
    pool: &PgPool,
    synthesis_id: Uuid,
    query: Option<Vec<f32>>,
) -> SynthesisJobHandler {
    SynthesisJobHandler::new(
        engine,
        Arc::new(TestEmbedder {
            query,
            ..TestEmbedder::default()
        }),
        Arc::new(LiveStage5Llm::new(pool.clone(), synthesis_id)),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        false,
    )
}

/// A theme-seeded run seeds from T's members only, ranked against the query
/// embedding and de-duplicated: exactly {A, B, C}. E (the query vector
/// itself, outside T) never enters; D (A restated) is suppressed. Runs on the
/// worker's real `ENGINE_POOL` and an embedder whose `get` errors, as the
/// production `OpenAiProvider::get` does.
///
/// Kills: seeding by text/vector recall when a theme is given (E would be
/// seed one); dropping near-duplicate suppression (D joins); ranking members
/// by `EmbeddingService::get` (every candidate is skipped and the run fails
/// `EmptyResult`, as it would in production).
#[tokio::test]
async fn theme_seed_anchors_to_theme_and_drops_duplicates() {
    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let fx = seed_theme_fixture(&admin).await;
    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "public",
        fx.actor.personal_group,
        "Origami folding",
        Some(fx.theme),
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let handler = theme_handler(engine_pool(&admin).await, &admin, sid, Some(e(0)));
    run_owned(&admin, &handler, &payload)
        .await
        .expect("a theme-seeded run completes");
    let members = members_of(&admin, sid).await;
    let expected: std::collections::BTreeSet<Uuid> = [fx.a, fx.b, fx.c].into_iter().collect();
    assert_eq!(members, expected, "A, B, C only (D, E absent)");
    assert!(!members.contains(&fx.e), "off-theme E never seeds");
    assert!(
        !(members.contains(&fx.a) && members.contains(&fx.d)),
        "A and its restatement D never both seed"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(sid)
        .fetch_one(&admin)
        .await
        .expect("row");
    assert_eq!(status, "complete");
}

/// A claim of a group the acting principal is not in, sitting in the theme,
/// is never seeded and never cited — while the actor's OWN group claim G in
/// the same theme is (positive control: group claims are not dropped
/// wholesale, so F's absence is the viewer filter's doing).
///
/// The engine pool here is the clone's superuser pool, as in T-J8: on the
/// worker's `ENGINE_POOL` row security hides every non-public claim
/// (`V1-engine-takes-pool`, until KE-1), which would make F's absence say
/// nothing about this code. Here F is reachable by SQL, so only the viewer
/// predicate the kernel splices into the theme-member read and the seed
/// filter (public + the synthesis' owner group) stand between F and the
/// article. This pins the end-to-end conjunction only; it is not coverage of
/// stage-2 scoping.
///
/// Kills: reading theme members without the acting viewer (or with a
/// bypass viewer) AND dropping the seed filter after the theme seed. Either
/// layer alone hides F, so this test cannot see a wrong viewer in the theme
/// read while the seed filter stands; that the viewer bounds the theme read
/// itself is pinned at pipeline level by
/// `stage1_theme_seed_reads_only_members_the_viewer_can_read` and
/// `stage1_theme_seed_member_read_spends_the_viewer`
/// (crates/episcience-db/tests/synthesis_pipeline_stage1_test.rs). That the
/// seed filter runs on the theme seed is pinned by
/// `public_theme_seed_drops_the_owner_groups_private_claims` and
/// `theme_seed_filter_drops_readable_claims_of_other_groups`.
#[tokio::test]
async fn theme_seed_excludes_other_groups_claims() {
    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let _ = engine_pool(&admin).await; // the stage session's login is unprivileged
    let fx = seed_theme_fixture(&admin).await;
    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "group",
        fx.actor.personal_group,
        "Origami folding",
        Some(fx.theme),
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let handler = theme_handler(admin.clone(), &admin, sid, Some(e(0)));
    run_owned(&admin, &handler, &payload)
        .await
        .expect("a theme-seeded group run completes");
    let members = members_of(&admin, sid).await;
    assert!(
        members.contains(&fx.g),
        "positive control: G seeds: {members:?}"
    );
    assert!(
        !members.contains(&fx.f),
        "F (group {}) never seeds: {members:?}",
        fx.other_group
    );
    let expected: std::collections::BTreeSet<Uuid> = [fx.a, fx.b, fx.c, fx.g].into_iter().collect();
    assert_eq!(members, expected);
    let narrative: Option<String> =
        sqlx::query_scalar("SELECT narrative FROM syntheses WHERE id = $1")
            .bind(sid)
            .fetch_one(&admin)
            .await
            .expect("row");
    let narrative = narrative.expect("a complete run stores its narrative");
    assert!(
        narrative.contains(&fx.g.to_string()),
        "positive control: G is cited"
    );
    assert!(!narrative.contains(&fx.f.to_string()), "F is never cited");
}

/// The seed filter (public + the synthesis' owner group only) runs on the
/// THEME seed, not just on text recall. H sits in theme T, private to a team
/// the actor is a reader of, so the actor's viewer CAN read it — the
/// per-group curator case (an article owned by group:main whose agent also
/// reads its personal group and any other group it belongs to). The article
/// is owned by the actor's personal group, not by that team, so H must not
/// seed it.
///
/// Row security does not hide H here: the engine pool is the clone's
/// superuser pool (as it will not once KE-1 makes the engine read as the
/// stamped viewer). The theme-member read runs as the actor, who may read H
/// (asserted below by calling `stage1_seed_theme` directly), and
/// `EmptyEdgeProvider` means stage 2 adds nothing. G (private to the owner
/// group) is the positive control: the filter keeps owner-group claims, so
/// H's absence is the owner-group check, not group claims being dropped
/// wholesale.
///
/// Behind the seed filter sits a second, fail-closed layer: the membership
/// tenancy guard refuses to attach a group claim to a synthesis its group
/// does not own. So without the filter H does not leak — the whole article
/// FAILS ("refused by the tenancy guard"), which is what this test's
/// "completes" expectation catches. The filter is what keeps a curator whose
/// viewer spans several groups producing articles at all; the case where it
/// is the only guard against a leak is
/// `public_theme_seed_drops_the_owner_groups_private_claims`.
///
/// Kills: applying `seed_filter` only in the text-recall arm of `run_stages`
/// step 4, or skipping it for theme seeds (the run fails on the tenancy
/// guard instead of completing without H).
#[tokio::test]
async fn theme_seed_filter_drops_readable_claims_of_other_groups() {
    use episcience_db::SynthesisPipeline;

    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let _ = engine_pool(&admin).await; // the stage session's login is unprivileged
    let fx = seed_theme_fixture(&admin).await;
    let team_admin = testdb::principal(&admin, "wiki-team-admin").await;
    let team = testdb::team_group(&admin, &team_admin, &[(fx.actor.agent, "reader")]).await;
    let h = embedded_claim(
        &admin,
        team_admin.agent,
        "crease fact H, private to a team the actor reads",
        epigraph_core::TenancyDecl::group(team),
        &e(7),
        Some(fx.theme),
    )
    .await;
    assert_eq!(
        testdb::claim_pair(&admin, h).await,
        ("group".to_string(), team),
        "H must really be private to the team, or 'never H' is vacuous"
    );
    assert_ne!(team, fx.actor.personal_group);

    // Non-vacuity: the theme read itself hands H (and G) to the actor, so
    // only the seed filter can keep H out of the article below.
    let actor_viewer = testdb::viewer_of(&admin, fx.actor.agent).await;
    let pipeline: SynthesisPipeline<MockLlmClient, EmptyEdgeProvider> = SynthesisPipeline::new(
        admin.clone(),
        Arc::new(TestEmbedder::default()),
        MockLlmClient::new(),
        EmptyEdgeProvider,
        e(0),
        20,
    );
    let raw: std::collections::BTreeSet<Uuid> = pipeline
        .stage1_seed_theme(&actor_viewer, fx.theme)
        .await
        .expect("the actor's theme read seeds")
        .into_iter()
        .collect();
    assert!(
        raw.contains(&h),
        "the actor's theme read reaches H: {raw:?}"
    );
    assert!(
        raw.contains(&fx.g),
        "the actor's theme read reaches G: {raw:?}"
    );

    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "group",
        fx.actor.personal_group,
        "Origami folding",
        Some(fx.theme),
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let handler = theme_handler(admin.clone(), &admin, sid, Some(e(0)));
    run_owned(&admin, &handler, &payload).await.expect(
        "a theme-seeded group run completes (without the seed filter H reaches \
             stage-2 persistence and the membership tenancy guard fails the run)",
    );
    let members = members_of(&admin, sid).await;
    assert!(
        !members.contains(&h),
        "H (team {team}, not the owner group) never seeds: {members:?}"
    );
    let expected: std::collections::BTreeSet<Uuid> = [fx.a, fx.b, fx.c, fx.g].into_iter().collect();
    assert_eq!(members, expected, "A, B, C and the owner group's G only");
    let narrative: Option<String> =
        sqlx::query_scalar("SELECT narrative FROM syntheses WHERE id = $1")
            .bind(sid)
            .fetch_one(&admin)
            .await
            .expect("row");
    let narrative = narrative.expect("a complete run stores its narrative");
    assert!(!narrative.contains(&h.to_string()), "H is never cited");
}

/// A PUBLIC theme-seeded article drops the owner group's own private claim
/// G, which the actor can read and which sits in the theme. This is the case
/// where the seed filter is the ONLY layer: the engine pool is the clone's
/// superuser pool (row security does not hide G), the theme read runs as the
/// actor (who may read G — positive control in
/// `theme_seed_excludes_other_groups_claims` and the direct read below), and
/// the membership tenancy guard admits G because G's group IS the
/// synthesis' owner group. Without the filter, a public article would seed
/// and cite a group-private claim.
///
/// Kills: applying `seed_filter` only in the text-recall arm of `run_stages`
/// step 4, or skipping it for theme seeds (G becomes a member of a public
/// article).
#[tokio::test]
async fn public_theme_seed_drops_the_owner_groups_private_claims() {
    use episcience_db::SynthesisPipeline;

    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let _ = engine_pool(&admin).await; // the stage session's login is unprivileged
    let fx = seed_theme_fixture(&admin).await;
    assert_eq!(
        testdb::claim_pair(&admin, fx.g).await,
        ("group".to_string(), fx.actor.personal_group),
        "G must really be private, or 'never G' is vacuous"
    );

    // Non-vacuity: the actor's theme read reaches G.
    let actor_viewer = testdb::viewer_of(&admin, fx.actor.agent).await;
    let pipeline: SynthesisPipeline<MockLlmClient, EmptyEdgeProvider> = SynthesisPipeline::new(
        admin.clone(),
        Arc::new(TestEmbedder::default()),
        MockLlmClient::new(),
        EmptyEdgeProvider,
        e(0),
        20,
    );
    let raw = pipeline
        .stage1_seed_theme(&actor_viewer, fx.theme)
        .await
        .expect("the actor's theme read seeds");
    assert!(
        raw.contains(&fx.g),
        "the actor's theme read reaches G: {raw:?}"
    );

    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "public",
        fx.actor.personal_group,
        "Origami folding",
        Some(fx.theme),
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let handler = theme_handler(admin.clone(), &admin, sid, Some(e(0)));
    run_owned(&admin, &handler, &payload)
        .await
        .expect("a public theme-seeded run completes");
    let members = members_of(&admin, sid).await;
    assert!(
        !members.contains(&fx.g),
        "G (private to the owner group) never seeds a PUBLIC article: {members:?}"
    );
    let expected: std::collections::BTreeSet<Uuid> = [fx.a, fx.b, fx.c].into_iter().collect();
    assert_eq!(members, expected, "public members only");
    let narrative: Option<String> =
        sqlx::query_scalar("SELECT narrative FROM syntheses WHERE id = $1")
            .bind(sid)
            .fetch_one(&admin)
            .await
            .expect("row");
    let narrative = narrative.expect("a complete run stores its narrative");
    assert!(!narrative.contains(&fx.g.to_string()), "G is never cited");
}

/// With no `seed_theme_id` the run seeds by text recall exactly as before:
/// the template's two `origami` claims, and none of T's members although the
/// theme's label is the query's subject.
#[tokio::test]
async fn text_seed_path_is_unchanged_when_no_theme() {
    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let fx = seed_theme_fixture(&admin).await;
    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "public",
        fx.actor.personal_group,
        "origami",
        None,
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let handler = theme_handler(engine_pool(&admin).await, &admin, sid, None);
    run_owned(&admin, &handler, &payload)
        .await
        .expect("a text-seeded run completes");
    let expected: std::collections::BTreeSet<Uuid> = [
        Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa),
        Uuid::from_u128(0xbbbbbbbb_bbbb_bbbb_bbbb_bbbbbbbbbbbb),
    ]
    .into_iter()
    .collect();
    assert_eq!(members_of(&admin, sid).await, expected);
}

/// A theme seed without a query embedding (embedder down) fails the job with
/// a reason naming the embedding — not a misleading "no claims" — and seeds
/// nothing. Kills: silently falling back to text recall, or to an unranked
/// theme seed, when the embedder fails.
#[tokio::test]
async fn theme_seed_without_query_embedding_fails_with_reason() {
    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let fx = seed_theme_fixture(&admin).await;
    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "public",
        fx.actor.personal_group,
        "Origami folding",
        Some(fx.theme),
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let handler = theme_handler(engine_pool(&admin).await, &admin, sid, None);
    match run_owned(&admin, &handler, &payload).await {
        Err(RunError::Failed(_)) => {}
        other => panic!("expected a failed run, got {other:?}"),
    }
    let (status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_reason FROM syntheses WHERE id = $1")
            .bind(sid)
            .fetch_one(&admin)
            .await
            .expect("row");
    assert_eq!(status, "failed");
    let reason = reason.expect("a failure reason");
    assert!(reason.contains("query embedding"), "{reason}");
    assert!(members_of(&admin, sid).await.is_empty(), "nothing seeded");
}

/// A payload enqueued before `seed_theme_id` existed still decodes (to the
/// text-recall path).
#[test]
fn old_payload_without_seed_theme_id_still_deserialises() {
    let v = serde_json::json!({"synthesis_id": Uuid::nil(), "query": "q", "traversal_config": null,
        "agent_id": Uuid::nil(), "parent_synthesis_id": null});
    let p: SynthesisJobPayload = serde_json::from_value(v).unwrap();
    assert!(p.seed_theme_id.is_none());
}

/// A rejected theme-seeded article's refinement child is enqueued with the
/// parent's `seed_theme_id`, so the retry seeds from the same theme rather
/// than drifting to text recall, and its ROW carries the parent's wiki page
/// key and seed theme (5042 columns), so a refined article that completes
/// lands on the same wiki page. Kills: building the child payload with
/// `seed_theme_id: None`; a child INSERT that does not copy `wiki_key` /
/// `seed_theme_id` (the page registry would never see the refined article).
#[tokio::test]
async fn refinement_child_keeps_the_parents_seed_theme_id() {
    let db = testdb::TestDb::fresh().await;
    let admin = db.admin.clone();
    let fx = seed_theme_fixture(&admin).await;
    let payload = enqueue_owned(
        &admin,
        fx.actor.agent,
        "public",
        fx.actor.personal_group,
        "Origami folding",
        Some(fx.theme),
    )
    .await;
    let sid = payload_synthesis_id(&payload);
    let page_key = episcience_core::wiki::WikiKey {
        run_id: Uuid::now_v7(),
        cluster_id: 7,
        split_part: Some(2),
    }
    .as_slug();
    episcience_db::SynthesisRepository::set_wiki_seed_tx(&admin, sid, fx.theme, &page_key)
        .await
        .expect("the parent is a wiki article");
    // UncitedStage5Llm forces a Stage 6 reject (UncitedMember rubric).
    let handler = SynthesisJobHandler::new(
        engine_pool(&admin).await,
        Arc::new(TestEmbedder {
            query: Some(e(0)),
            ..TestEmbedder::default()
        }),
        Arc::new(UncitedStage5Llm::new(admin.clone(), sid)),
        Arc::new(EmptyEdgeProvider),
        20,
        "test-embedding-model",
        false,
    );
    run_owned(&admin, &handler, &payload)
        .await
        .expect("the reject path returns Ok");
    let child: Uuid = sqlx::query_scalar("SELECT id FROM syntheses WHERE parent_synthesis_id = $1")
        .bind(sid)
        .fetch_one(&admin)
        .await
        .expect("a refinement child row");
    let child_theme: Option<String> =
        sqlx::query_scalar("SELECT payload->>'seed_theme_id' FROM synthesis_jobs WHERE id = $1")
            .bind(child)
            .fetch_one(&admin)
            .await
            .expect("the child's job row");
    assert_eq!(child_theme, Some(fx.theme.to_string()));
    let child_row: (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT seed_theme_id, wiki_key FROM syntheses WHERE id = $1")
            .bind(child)
            .fetch_one(&admin)
            .await
            .expect("the child's row");
    assert_eq!(child_row, (Some(fx.theme), Some(page_key)));
}
