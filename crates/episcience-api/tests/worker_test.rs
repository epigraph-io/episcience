//! `episcience-worker` on its own application login (batch E1f).
//!
//! Every test runs the REAL worker loop (`jobs::worker::Worker`) on the CI
//! `episcience_worker` login against a fresh clone of the E1 template, with
//! row security, the grant matrix and the queue definers of 5036/5037 in
//! force. Fixtures are written on the clone's superuser pool; everything the
//! worker does goes through its own login. Each test first asserts that the
//! login is unprivileged, so none can pass vacuously on a bypassing session.
//!
//! Run through `scripts/e1-test-db.sh <batch> -- cargo test --test worker_test`.

#[path = "../../episcience-db/tests/support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use epigraph_embeddings::errors::EmbeddingError;
use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
use episcience_api::jobs::worker::{JobOutcome, Worker, REASON_EXPIRED, REASON_OPERATED};
use episcience_api::jobs::{EmptyEdgeProvider, SynthesisJobHandler};
use episcience_core::Visibility;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use support::{Principal, TestDb, WORKER_LOGIN};
use uuid::Uuid;

// ─── Test doubles ───────────────────────────────────────────────────────────

/// Fixed 1536-dim embedding for the narrative head; `generate_query` fails so
/// stage 1 recalls through the kernel's text-search fallback (deterministic
/// against the template's `origami` claims).
#[derive(Debug)]
struct TestEmbedder;

#[async_trait]
impl EmbeddingService for TestEmbedder {
    async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok((0..1536).map(|i| (i as f32) * 1e-4).collect())
    }
    async fn batch_generate(&self, _t: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(vec![])
    }
    async fn store(&self, _c: Uuid, _e: &[f32]) -> Result<(), EmbeddingError> {
        Ok(())
    }
    async fn get(&self, claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
        Err(EmbeddingError::NotFound { claim_id })
    }
    async fn similar(
        &self,
        _e: &[f32],
        _k: usize,
        _m: f32,
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
        Err(EmbeddingError::ApiError {
            message: "test stub: generate_query disabled".into(),
            status_code: None,
        })
    }
}

/// An LLM that narrates and composes validly for whatever clusters the
/// synthesis has, reading them on the fixture (superuser) pool: stage 4 gets
/// a summary citing every member, stage 5 the summaries verbatim inside their
/// sentinels, so the verifier accepts.
struct ValidLlm {
    admin: PgPool,
    synthesis_id: Uuid,
    calls: Mutex<u32>,
}

impl std::fmt::Debug for ValidLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValidLlm")
    }
}

#[async_trait]
impl LlmProvider for ValidLlm {
    fn name(&self) -> &str {
        "valid-mock"
    }
    fn is_active(&self) -> bool {
        true
    }
    fn model_name(&self) -> &str {
        "valid-mock"
    }
    async fn complete_json(&self, _prompt: &str) -> Result<serde_json::Value, LlmError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        let rows: Vec<(Uuid, String, Vec<Uuid>)> = sqlx::query_as(
            "SELECT id, summary, member_claim_ids FROM synthesis_clusters
              WHERE synthesis_id = $1 ORDER BY cluster_index, id",
        )
        .bind(self.synthesis_id)
        .fetch_all(&self.admin)
        .await
        .map_err(|e| LlmError::RequestFailed {
            message: e.to_string(),
        })?;
        let k = rows.len() as u32;
        if n <= k {
            let (_, _, members) = &rows[(n - 1) as usize];
            let cites: Vec<String> = members.iter().map(|m| format!("[{m}]")).collect();
            return Ok(serde_json::json!({
                "title": format!("Cluster {n}"),
                "summary": format!("Summary citing {}", cites.join(" ")),
            }));
        }
        let body: String = rows
            .iter()
            .map(|(id, summary, _)| {
                format!("<<<CLUSTER:{id}:BEGIN>>>{summary}<<<CLUSTER:{id}:END>>>\n\n")
            })
            .collect();
        Ok(serde_json::json!({ "narrative": format!("Narrative.\n\n{body}") }))
    }
}

/// An LLM whose cluster summaries cite nothing: stage 4's citation check
/// passes (no citation is not a wrong one), stage 5 embeds the summaries
/// verbatim, and the verifier REJECTS the narrative (an uncited member).
struct RejectLlm {
    admin: PgPool,
    calls: Mutex<u32>,
}

impl std::fmt::Debug for RejectLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RejectLlm")
    }
}

#[async_trait]
impl LlmProvider for RejectLlm {
    fn name(&self) -> &str {
        "reject-mock"
    }
    fn is_active(&self) -> bool {
        true
    }
    fn model_name(&self) -> &str {
        "reject-mock"
    }
    async fn complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError> {
        *self.calls.lock().unwrap() += 1;
        if !prompt.contains("<<<CLUSTER:") {
            return Ok(serde_json::json!({ "title": "t", "summary": "Summary citing nothing." }));
        }
        // Compose: every cluster id named in the prompt, its summary verbatim.
        let ids: Vec<Uuid> = prompt
            .split("<<<CLUSTER:")
            .skip(1)
            .filter_map(|rest| rest.split(':').next())
            .filter_map(|id| id.parse().ok())
            .collect();
        let mut body = String::from("Narrative.\n\n");
        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            if seen.insert(id) {
                let summary: String =
                    sqlx::query_scalar("SELECT summary FROM synthesis_clusters WHERE id = $1")
                        .bind(id)
                        .fetch_one(&self.admin)
                        .await
                        .map_err(|e| LlmError::RequestFailed {
                            message: e.to_string(),
                        })?;
                body.push_str(&format!(
                    "<<<CLUSTER:{id}:BEGIN>>>{summary}<<<CLUSTER:{id}:END>>>\n\n"
                ));
            }
        }
        Ok(serde_json::json!({ "narrative": body }))
    }
}

/// An LLM whose transport always fails (a transient error).
#[derive(Debug)]
struct DownLlm;

#[async_trait]
impl LlmProvider for DownLlm {
    fn name(&self) -> &str {
        "down"
    }
    fn is_active(&self) -> bool {
        true
    }
    fn model_name(&self) -> &str {
        "down"
    }
    async fn complete_json(&self, _prompt: &str) -> Result<serde_json::Value, LlmError> {
        Err(LlmError::RequestFailed {
            message: "test: the model is down".into(),
        })
    }
}

// ─── Fixtures ───────────────────────────────────────────────────────────────

async fn worker(db: &TestDb, llm: Arc<dyn LlmProvider>) -> Worker {
    let url = db.login_url(WORKER_LOGIN);
    let scoped = ScopedPool::connect_with_options(
        &url,
        SessionGucMode::Session,
        ScopedPoolOptions {
            max_connections: 3,
            ..ScopedPoolOptions::default()
        },
    )
    .await
    .expect("stamped pool on the worker login");
    let plain = || {
        PgPoolOptions::new()
            .max_connections(2)
            .connect_with(db.login_options(WORKER_LOGIN))
    };
    let resolve_pool = plain().await.expect("RESOLVE_POOL");
    let engine_pool = plain().await.expect("ENGINE_POOL");
    // Not vacuous: every assertion below is about what an UNPRIVILEGED
    // session may do.
    episcience_db::tenancy_contract::refuse_privileged_session(&resolve_pool)
        .await
        .expect("the worker login is unprivileged");
    Worker {
        name: "worker-test".into(),
        scoped: Arc::new(scoped),
        resolve_pool,
        handler: SynthesisJobHandler::new(
            engine_pool,
            Arc::new(TestEmbedder),
            llm,
            Arc::new(EmptyEdgeProvider),
            20,
            "test-embedding-model",
            true,
        ),
        retry_delay: Duration::ZERO,
    }
}

fn valid_llm(db: &TestDb, synthesis_id: Uuid) -> Arc<dyn LlmProvider> {
    Arc::new(ValidLlm {
        admin: db.admin.clone(),
        synthesis_id,
        calls: Mutex::new(0),
    })
}

/// A pending synthesis owned `(group, visibility)`, authored by `author`, and
/// its queued job acting as `principal` (both written on the fixture pool, a
/// privileged session, so the principal is supplied explicitly).
async fn enqueue(
    a: &PgPool,
    author: Uuid,
    principal: Uuid,
    group: Uuid,
    visibility: Visibility,
) -> Uuid {
    let id = Uuid::now_v7();
    episcience_db::SynthesisRepository::create_pending(
        a,
        id,
        "origami",
        author,
        None,
        &[],
        "mock",
        "mock-model",
        episcience_core::Ownership::new(group, visibility),
    )
    .await
    .expect("create_pending");
    let payload = serde_json::json!({
        "synthesis_id": id,
        "query": "origami",
        "traversal_config": null,
        "agent_id": principal,
        "parent_synthesis_id": null,
        "prereq_synthesis_ids": [],
        "workflow_run_id": null,
    });
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, job_type, payload, state, principal_id)
         VALUES ($1, 'synthesis', $2, 'queued', $3)",
    )
    .bind(id)
    .bind(payload)
    .bind(principal)
    .execute(a)
    .await
    .expect("enqueue");
    id
}

/// `(clusters, membership, embeddings, outbox)` rows of a synthesis.
async fn derived(a: &PgPool, id: Uuid) -> (i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM synthesis_clusters WHERE synthesis_id = $1),
                (SELECT count(*) FROM synthesis_claim_membership WHERE synthesis_id = $1),
                (SELECT count(*) FROM synthesis_embeddings WHERE synthesis_id = $1),
                (SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1)",
    )
    .bind(id)
    .fetch_one(a)
    .await
    .expect("derived counts")
}

/// `(state, last_error, attempts, job rows for this synthesis)`.
async fn job(a: &PgPool, id: Uuid) -> (String, Option<String>, i32, i64) {
    sqlx::query_as(
        "SELECT state, last_error, attempts,
                (SELECT count(*) FROM synthesis_jobs WHERE id = $1)
           FROM synthesis_jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(a)
    .await
    .expect("job row")
}

async fn status(a: &PgPool, id: Uuid) -> (String, String, Uuid) {
    sqlx::query_as("SELECT status, visibility, owner_group_id FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(a)
        .await
        .expect("synthesis row")
}

async fn kernel_edges(a: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM edges WHERE source_id = $1 AND source_type = 'synthesis'",
    )
    .bind(id)
    .fetch_one(a)
    .await
    .expect("kernel edges")
}

async fn events(a: &PgPool, id: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT event_type::text FROM events
          WHERE payload->>'synthesis_id' = $1 OR payload->>'source_id' = $1
          ORDER BY graph_version",
    )
    .bind(id.to_string())
    .fetch_all(a)
    .await
    .expect("events")
}

async fn revoke(a: &PgPool, group: Uuid, agent: Uuid) {
    let n = sqlx::query(
        "UPDATE group_memberships SET revoked_at = now()
          WHERE group_id = $1 AND agent_id = $2 AND revoked_at IS NULL",
    )
    .bind(group)
    .bind(agent)
    .execute(a)
    .await
    .expect("revoke membership")
    .rows_affected();
    assert_eq!(n, 1, "the fixture membership existed");
}

// ─── Tests ──────────────────────────────────────────────────────────────────

/// T-J1. A job enqueued by H1 runs to `complete` on the worker login; every
/// derived row it wrote carries the synthesis' pair; exactly one job row. And
/// a forged stage insert, on the same stamped connection, into H2's public
/// synthesis is refused 42501 (row security's WITH CHECK on H1's writable
/// set). Kills: stages writing on an unstamped or privileged connection, the
/// derived pair not inherited, and the worker acting as anyone but the queue
/// principal.
#[tokio::test]
async fn t_j1_a_job_runs_stamped_as_its_principal_and_writes_only_its_group() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let h2 = support::principal(a, "h2").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    let foreign = support::pending_synthesis(a, &h2, Visibility::Public).await;

    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(w.run_once().await.expect("run"), JobOutcome::Completed(s));

    let (st, vis, owner) = status(a, s).await;
    assert_eq!(
        (st.as_str(), vis.as_str(), owner),
        ("complete", "public", h1.personal_group)
    );
    let (clusters, members, embeddings, outbox) = derived(a, s).await;
    assert!(clusters > 0 && members > 0 && embeddings == 1 && outbox > 0);
    let off_pair: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM synthesis_clusters WHERE synthesis_id = $1 AND (owner_group_id, visibility) <> ($2, 'public'))
              + (SELECT count(*) FROM synthesis_claim_membership WHERE synthesis_id = $1 AND (owner_group_id, visibility) <> ($2, 'public'))
              + (SELECT count(*) FROM synthesis_embeddings WHERE synthesis_id = $1 AND (owner_group_id, visibility) <> ($2, 'public'))
              + (SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND (owner_group_id, visibility) <> ($2, 'public'))",
    )
    .bind(s)
    .bind(h1.personal_group)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(off_pair, 0, "every derived row carries the synthesis' pair");
    let (state, err, attempts, rows) = job(a, s).await;
    assert_eq!(
        (state.as_str(), err, attempts, rows),
        ("complete", None, 1, 1)
    );

    // The forged insert: H1's stamped transaction, a cluster under H2's
    // (visible, public) synthesis.
    let v1 = support::viewer_of(a, h1.agent).await;
    let mut tx = w.scoped.begin_as(&v1).await.expect("begin_as H1");
    let e = sqlx::query(
        "INSERT INTO synthesis_clusters
         (id, synthesis_id, cluster_index, title, summary, member_claim_ids, support_count, contradict_count)
         VALUES ($1, $2, 0, 'forged', 'forged', '{}', 0, 0)",
    )
    .bind(Uuid::now_v7())
    .bind(foreign)
    .execute(&mut *tx)
    .await
    .expect_err("a stage insert into another group is refused");
    let code = e
        .as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.to_string());
    assert_eq!(code.as_deref(), Some("42501"), "{e}");
}

/// T-J2. Full revocation between enqueue and run: the principal holds no
/// live membership at all. The job ends `failed: authority: …` on its first
/// attempt, the synthesis row is untouched and no derived row exists.
/// Kills: running before authorizing, and retrying an authority refusal.
#[tokio::test]
async fn t_j2_a_fully_revoked_principal_fails_closed_with_nothing_written() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Group).await;
    revoke(a, h1.personal_group, h1.agent).await;

    let w = worker(&db, valid_llm(&db, s)).await;
    match w.run_once().await.expect("run") {
        JobOutcome::Failed { job, reason } => {
            assert_eq!(job, s);
            assert!(reason.starts_with("authority: "), "{reason}");
        }
        other => panic!("expected an authority failure, got {other:?}"),
    }
    let (state, err, attempts, _) = job(a, s).await;
    assert_eq!(state, "failed");
    assert!(err.unwrap_or_default().starts_with("authority: "));
    assert_eq!(attempts, 1, "never retried");
    assert_eq!(status(a, s).await.0, "pending", "the row is untouched");
    assert_eq!(derived(a, s).await, (0, 0, 0, 0));
}

/// T-J2b (B-M1). H1 loses team T but keeps its personal group, so it still
/// resolves with a non-empty writable set: the refusal must come from the
/// PER-STAGE check (the T-owned synthesis is neither visible nor writable
/// any more). The job ends `failed: authority`, no stage row changes, and it
/// is never `complete` nor retried. Kills: authorizing only at job start,
/// and treating a stage refusal as transient.
#[tokio::test]
async fn t_j2b_losing_the_owner_group_mid_life_stops_every_stage() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let owner = support::principal(a, "t-admin").await;
    let h1 = support::principal(a, "h1").await;
    let team = support::team_group(a, &owner, &[(h1.agent, "writer")]).await;
    let s = enqueue(a, h1.agent, h1.agent, team, Visibility::Group).await;
    revoke(a, team, h1.agent).await;

    let w = worker(&db, valid_llm(&db, s)).await;
    // Start authorization passes (the personal group is writable) ...
    w.authorize(h1.agent)
        .await
        .expect("H1 still resolves and may write its personal group");
    // ... and the stage check refuses.
    match w.run_once().await.expect("run") {
        JobOutcome::Failed { reason, .. } => {
            assert!(reason.starts_with("authority: "), "{reason}")
        }
        other => panic!("expected an authority failure, got {other:?}"),
    }
    let (state, _, attempts, rows) = job(a, s).await;
    assert_eq!((state.as_str(), attempts, rows), ("failed", 1, 1));
    assert_eq!(status(a, s).await.0, "pending");
    assert_eq!(derived(a, s).await, (0, 0, 0, 0));
}

/// B-M1, the writable half: a principal that can READ the owner group but not
/// write it (a `reader` of team T, the synthesis `group(T)`) passes the start
/// authorization (its personal group is writable) and the synthesis is
/// visible to it, so only the per-stage writable check can refuse. The job
/// ends `failed: authority` with nothing written. Kills: the owner-group
/// writable check removed from `StageSession::begin` (visibility alone would
/// let the stages run, and row security would then refuse mid-stage as a
/// transient error that is retried).
#[tokio::test]
async fn t_j2c_a_reader_of_the_owner_group_cannot_run_its_job() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let owner = support::principal(a, "t-admin").await;
    let r = support::principal(a, "reader").await;
    let team = support::team_group(a, &owner, &[(r.agent, "reader")]).await;
    let s = enqueue(a, owner.agent, r.agent, team, Visibility::Group).await;

    let w = worker(&db, valid_llm(&db, s)).await;
    let v = w
        .authorize(r.agent)
        .await
        .expect("the reader resolves and may write its own group");
    assert!(
        v.group_bind().is_some_and(|g| g.contains(&team)),
        "the reader can read T"
    );
    match w.run_once().await.expect("run") {
        JobOutcome::Failed { reason, .. } => {
            assert!(reason.contains("may not write"), "{reason}")
        }
        other => panic!("expected an authority failure, got {other:?}"),
    }
    let (state, _, attempts, _) = job(a, s).await;
    assert_eq!((state.as_str(), attempts), ("failed", 1));
    assert_eq!(status(a, s).await.0, "pending");
    assert_eq!(derived(a, s).await, (0, 0, 0, 0));
}

/// T-J4 + T-J7 (D-M2, D-M3). On the worker login: a PUBLIC synthesis gets its
/// kernel PROV edges in stage 6 (the kernel admits the synthesis endpoint for
/// its owner), one `edge.added` per edge and `synthesis.complete`; a GROUP
/// synthesis gets no kernel edge, no event, and its outbox rows are deferred
/// `private`. After the group one is widened, the `stage6_pending` worklist
/// writes its edges AS its principal. Exactly one job row per synthesis
/// throughout, and the complete job's type, payload and state never change.
/// Kills: public-only gating removed, the worklist not writing, and any path
/// that re-enqueues a job for an existing synthesis.
#[tokio::test]
async fn t_j4_public_edges_in_stage_6_group_deferred_then_written_by_the_worklist() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let public = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    let group = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Group).await;

    for s in [public, group] {
        let w = worker(&db, valid_llm(&db, s)).await;
        assert_eq!(w.run_once().await.expect("run"), JobOutcome::Completed(s));
    }

    let outbox_pub = derived(a, public).await.3;
    let written_pub = kernel_edges(a, public).await;
    assert!(written_pub >= 2, "claims + ATTRIBUTED_TO");
    assert_eq!(
        written_pub, outbox_pub,
        "every outbox row became a kernel edge"
    );
    let ev = events(a, public).await;
    assert_eq!(
        ev.iter().filter(|e| *e == "edge.added").count() as i64,
        written_pub
    );
    assert_eq!(ev.iter().filter(|e| *e == "synthesis.complete").count(), 1);

    assert_eq!(kernel_edges(a, group).await, 0);
    assert!(
        events(a, group).await.is_empty(),
        "T-J7: no event for a group synthesis"
    );
    let deferred: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND deferred_reason = 'private'",
    )
    .bind(group)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(deferred, derived(a, group).await.3, "every row deferred");
    assert!(deferred > 0);

    let before: (String, String, serde_json::Value) =
        sqlx::query_as("SELECT job_type, state, payload FROM synthesis_jobs WHERE id = $1")
            .bind(group)
            .fetch_one(a)
            .await
            .unwrap();

    // Widen (the interlock, all inputs public) and release the outbox, as the
    // visibility route does.
    let mut t = a.begin().await.unwrap();
    sqlx::query("SET LOCAL episcience.allow_widen = 'yes'")
        .execute(&mut *t)
        .await
        .unwrap();
    sqlx::query("UPDATE syntheses SET visibility = 'public' WHERE id = $1")
        .bind(group)
        .execute(&mut *t)
        .await
        .expect("widen");
    sqlx::query("UPDATE synthesis_provo_edges SET deferred_reason = NULL WHERE synthesis_id = $1")
        .bind(group)
        .execute(&mut *t)
        .await
        .unwrap();
    t.commit().await.unwrap();

    let w = worker(&db, valid_llm(&db, group)).await;
    let r = w.run_worklist(50).await.expect("worklist");
    assert_eq!(r.edges_written, 1, "{r:?}");
    assert_eq!(kernel_edges(a, group).await, deferred);
    let after: (String, String, serde_json::Value) =
        sqlx::query_as("SELECT job_type, state, payload FROM synthesis_jobs WHERE id = $1")
            .bind(group)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(before, after, "the complete job row is unchanged");
    for s in [public, group] {
        assert_eq!(job(a, s).await.3, 1, "exactly one job row");
    }
    // Idempotent: nothing is pending any more.
    assert_eq!(w.run_worklist(50).await.unwrap().edges_written, 0);
}

/// T-J5 (V1 PIN; EXPECTED TO FLIP at KE-1, when stage 1 moves onto the
/// stamped transaction). On the worker, the kernel engine reads on the
/// UNSTAMPED application pool, so H1's own `group(H1pg)` claim is NOT among
/// the stage-1 seeds of H1's group synthesis, while the public claims are.
/// Kills: running the engine on a privileged pool (the flip would come early
/// and silently).
#[tokio::test]
async fn t_j5_v1_the_worker_seeds_public_claims_only_until_ke_1() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let private = support::claim(
        a,
        h1.agent,
        "origami private folding result",
        0.9,
        epigraph_core::TenancyDecl::group(h1.personal_group),
    )
    .await;
    assert_eq!(support::claim_pair(a, private).await.0, "group");
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Group).await;

    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(w.run_once().await.expect("run"), JobOutcome::Completed(s));
    let members: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT m.claim_id, c.visibility::text FROM synthesis_claim_membership m
           JOIN claims c ON c.id = m.claim_id WHERE m.synthesis_id = $1",
    )
    .bind(s)
    .fetch_all(a)
    .await
    .unwrap();
    assert!(!members.is_empty(), "public origami claims seed it");
    assert!(members.iter().all(|(_, v)| v == "public"), "{members:?}");
    assert!(!members.iter().any(|(c, _)| *c == private));
}

/// T-J6. The `staleness_check` worklist, as the owner: a completed synthesis
/// whose recorded BetP no longer matches the kernel's is marked stale
/// (`belief_drift`, one staleness event naming the drifted claims) and its
/// `staleness_checked_at` advances; one whose only recorded claim the engine
/// cannot see is NOT stale (unknown is never drift) but is checked; the
/// complete job row is unchanged. Kills: advancing without checking, marking
/// stale on an unknown claim, and a recheck that writes a job row.
#[tokio::test]
async fn t_j6_the_staleness_recheck_marks_drift_as_the_owner() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let h2 = support::principal(a, "h2").await;
    let drift = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    let hidden = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    for s in [drift, hidden] {
        let w = worker(&db, valid_llm(&db, s)).await;
        assert_eq!(w.run_once().await.expect("run"), JobOutcome::Completed(s));
    }
    // Recorded BetP far from any real value (drift), and, for the second, a
    // snapshot naming only a claim of ANOTHER principal's group (unknown).
    let other = support::claim(
        a,
        h2.agent,
        "a claim the owner cannot see",
        0.7,
        epigraph_core::TenancyDecl::group(h2.personal_group),
    )
    .await;
    sqlx::query(
        "UPDATE syntheses SET staleness_checked_at = NULL,
                subgraph_snapshot = jsonb_set(subgraph_snapshot, '{belief_intervals}',
                  (SELECT jsonb_agg(jsonb_set(e, '{pignistic_prob}', '-1'::jsonb))
                     FROM jsonb_array_elements(subgraph_snapshot->'belief_intervals') e))
          WHERE id = $1",
    )
    .bind(drift)
    .execute(a)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE syntheses SET staleness_checked_at = NULL,
                subgraph_snapshot = jsonb_set(subgraph_snapshot, '{belief_intervals}',
                  jsonb_build_array(jsonb_build_object('claim_id', $2::text, 'frame_id', null,
                    'belief', 0.0, 'plausibility', 1.0, 'pignistic_prob', -1.0, 'framed', false)))
          WHERE id = $1",
    )
    .bind(hidden)
    .bind(other.to_string())
    .execute(a)
    .await
    .unwrap();
    let jobs_before: Vec<(
        Uuid,
        String,
        serde_json::Value,
        i32,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT id, state, payload, attempts, updated_at FROM synthesis_jobs
              WHERE id = ANY($1) ORDER BY id",
    )
    .bind(vec![drift, hidden])
    .fetch_all(a)
    .await
    .unwrap();

    let w = worker(&db, valid_llm(&db, drift)).await;
    let r = w.run_worklist(50).await.expect("worklist");
    assert_eq!((r.rechecked, r.marked_stale), (2, 1), "{r:?}");

    let row = |id: Uuid| async move {
        sqlx::query_as::<_, (Option<String>, bool)>(
            "SELECT stale_reason, staleness_checked_at IS NOT NULL FROM syntheses WHERE id = $1",
        )
        .bind(id)
        .fetch_one(a)
        .await
        .unwrap()
    };
    assert_eq!(row(drift).await, (Some("belief_drift".into()), true));
    assert_eq!(row(hidden).await, (None, true), "unknown is not drift");
    let events: Vec<(String, Vec<Uuid>)> = sqlx::query_as(
        "SELECT trigger, affected_claim_ids FROM synthesis_staleness_events WHERE synthesis_id = $1",
    )
    .bind(drift)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "belief_drift");
    assert!(!events[0].1.is_empty());
    let jobs_after: Vec<(
        Uuid,
        String,
        serde_json::Value,
        i32,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT id, state, payload, attempts, updated_at FROM synthesis_jobs
              WHERE id = ANY($1) ORDER BY id",
    )
    .bind(vec![drift, hidden])
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(
        jobs_before, jobs_after,
        "a recheck never touches the job row"
    );
    // Checked within the period: not due again.
    assert_eq!(w.run_worklist(50).await.unwrap().rechecked, 0);
}

/// T-J8. The seed filter on the stamped session: for a GROUP(T) synthesis it
/// keeps public claims and T's claims and drops a claim of H1's personal
/// group that H1 CAN read; for a PUBLIC synthesis it keeps public claims
/// only. Order is kept. Kills: filtering on readability instead of the
/// synthesis' own pair.
#[tokio::test]
async fn t_j8_the_seed_filter_keeps_public_and_own_group_claims_only() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let team = support::team_group(a, &h1, &[]).await;
    let public = support::any_public_claim(a).await;
    let team_claim = support::claim(
        a,
        h1.agent,
        "team claim",
        0.8,
        epigraph_core::TenancyDecl::group(team),
    )
    .await;
    let personal = support::claim(
        a,
        h1.agent,
        "personal claim",
        0.8,
        epigraph_core::TenancyDecl::group(h1.personal_group),
    )
    .await;
    let group_syn = enqueue(a, h1.agent, h1.agent, team, Visibility::Group).await;
    let public_syn = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;

    let w = worker(&db, valid_llm(&db, group_syn)).await;
    let v1 = support::viewer_of(a, h1.agent).await;
    let mut tx = w.scoped.begin_as(&v1).await.unwrap();
    let seeds = vec![personal, team_claim, public];
    let kept = episcience_api::jobs::synthesis_job::seed_filter(&mut tx, group_syn, &seeds)
        .await
        .unwrap();
    assert_eq!(kept, vec![team_claim, public]);
    let kept = episcience_api::jobs::synthesis_job::seed_filter(&mut tx, public_syn, &seeds)
        .await
        .unwrap();
    assert_eq!(kept, vec![public]);
}

/// T-J9. A job older than 24 h is failed `expired` without running: the
/// synthesis stays pending, nothing derived exists. Kills: the age cap
/// removed or checked after the stages.
#[tokio::test]
async fn t_j9_a_job_older_than_a_day_is_failed_unrun() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    sqlx::query("UPDATE synthesis_jobs SET created_at = now() - interval '25 hours' WHERE id = $1")
        .bind(s)
        .execute(a)
        .await
        .unwrap();
    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(
        w.run_once().await.unwrap(),
        JobOutcome::Failed {
            job: s,
            reason: REASON_EXPIRED.into()
        }
    );
    assert_eq!(status(a, s).await.0, "pending");
    assert_eq!(derived(a, s).await, (0, 0, 0, 0));
}

/// Parity refusal: a job whose principal has an operator link (the kernel
/// refuses that agent's tokens) is failed `principal_operated` unrun.
/// Kills: the `operator_of_author` check removed.
#[tokio::test]
async fn an_operated_principal_is_failed_unrun() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let op = support::principal(a, "operator").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    sqlx::query(
        "INSERT INTO operator_links (agent_id, operator_id, operator_group_id) VALUES ($1, $2, $3)",
    )
    .bind(h1.agent)
    .bind(op.agent)
    .bind(op.personal_group)
    .execute(a)
    .await
    .expect("link");
    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(
        w.run_once().await.unwrap(),
        JobOutcome::Failed {
            job: s,
            reason: REASON_OPERATED.into()
        }
    );
    assert_eq!(derived(a, s).await, (0, 0, 0, 0));
}

/// Parity on the WORKLIST: a principal that gained an operator link after its
/// synthesis completed still holds its live admin membership, so the
/// worklist definer (which filters on membership only) OFFERS both of its
/// items: a public synthesis with pending outbox rows (`stage6_pending`) that
/// is also due a recheck (`staleness_check`). The worker refuses to act as
/// that principal for either: no kernel edge is written, the recheck does not
/// advance `staleness_checked_at`, and both items count as skipped (which
/// proves they were offered, so the test cannot pass vacuously). Kills: the
/// `authorize` call removed from `write_pending_edges`, and `authorize`
/// replaced by a bare `Viewer::resolve` in `recheck_staleness` (the only
/// parity check on these paths: `StageSession::begin` re-resolves and checks
/// the owner group, never the operator link).
#[tokio::test]
async fn an_operated_principal_gets_nothing_done_from_the_worklist() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let op = support::principal(a, "operator").await;
    // A group synthesis completes with its outbox deferred, then is widened
    // and its outbox released (as the visibility route does): pending rows
    // on a complete PUBLIC synthesis, never checked for staleness.
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Group).await;
    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(w.run_once().await.expect("run"), JobOutcome::Completed(s));
    let mut t = a.begin().await.unwrap();
    sqlx::query("SET LOCAL episcience.allow_widen = 'yes'")
        .execute(&mut *t)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE syntheses SET visibility = 'public', staleness_checked_at = NULL WHERE id = $1",
    )
    .bind(s)
    .execute(&mut *t)
    .await
    .expect("widen");
    sqlx::query("UPDATE synthesis_provo_edges SET deferred_reason = NULL WHERE synthesis_id = $1")
        .bind(s)
        .execute(&mut *t)
        .await
        .unwrap();
    t.commit().await.unwrap();
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND written_at IS NULL",
    )
    .bind(s)
    .fetch_one(a)
    .await
    .unwrap();
    assert!(pending > 0, "the fixture has pending outbox rows");

    sqlx::query(
        "INSERT INTO operator_links (agent_id, operator_id, operator_group_id) VALUES ($1, $2, $3)",
    )
    .bind(h1.agent)
    .bind(op.agent)
    .bind(op.personal_group)
    .execute(a)
    .await
    .expect("link");

    let r = w.run_worklist(50).await.expect("worklist");
    assert_eq!(
        (r.edges_written, r.rechecked, r.skipped),
        (0, 0, 2),
        "both items offered, both refused: {r:?}"
    );
    assert_eq!(
        kernel_edges(a, s).await,
        0,
        "no kernel edge as an operated principal"
    );
    let checked: bool =
        sqlx::query_scalar("SELECT staleness_checked_at IS NOT NULL FROM syntheses WHERE id = $1")
            .bind(s)
            .fetch_one(a)
            .await
            .unwrap();
    assert!(!checked, "the recheck did not run as an operated principal");
}

/// A transient failure goes back to the queue through the retry definer
/// (never a new job row) until `max_attempts`, then ends `failed`. Kills:
/// finishing on the first transient failure, and retrying forever.
#[tokio::test]
async fn a_transient_failure_is_retried_until_the_attempts_run_out() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    let w = worker(&db, Arc::new(DownLlm)).await;
    for attempt in 1..=2 {
        match w.run_once().await.unwrap() {
            JobOutcome::Retried { job, .. } => assert_eq!(job, s),
            other => panic!("attempt {attempt}: expected a retry, got {other:?}"),
        }
        let (state, _, attempts, rows) = job(a, s).await;
        assert_eq!((state.as_str(), attempts, rows), ("queued", attempt, 1));
    }
    assert!(matches!(
        w.run_once().await.unwrap(),
        JobOutcome::Failed { .. }
    ));
    let (state, err, attempts, rows) = job(a, s).await;
    assert_eq!((state.as_str(), attempts, rows), ("failed", 3, 1));
    let err = err.unwrap_or_default();
    assert!(err.contains("the model is down"), "{err}");
    assert_eq!(w.run_once().await.unwrap(), JobOutcome::Idle);
}

/// The worker never acts on the payload's `agent_id`: a job row whose
/// principal is H1 but whose payload names H2 runs as H1 (the database
/// forced the principal at insert for app sessions; here a privileged
/// fixture wrote a disagreeing payload). The synthesis completes as H1
/// and its ATTRIBUTED_TO edge names H1. Kills: `run` receiving
/// `payload.agent_id` as the acting principal.
#[tokio::test]
async fn the_worker_acts_as_the_queue_principal_never_the_payload() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let h2: Principal = support::principal(a, "h2").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    sqlx::query(
        "UPDATE synthesis_jobs SET payload = jsonb_set(payload, '{agent_id}', to_jsonb($2::text)) WHERE id = $1",
    )
    .bind(s)
    .bind(h2.agent.to_string())
    .execute(a)
    .await
    .unwrap();
    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(w.run_once().await.unwrap(), JobOutcome::Completed(s));
    let attributed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT target_id FROM synthesis_provo_edges WHERE synthesis_id = $1 AND predicate = 'ATTRIBUTED_TO'",
    )
    .bind(s)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(attributed, vec![h1.agent]);
}

/// The in-process writer writes only the synthesis PROV shapes stage 6 plans.
/// The outbox's CHECKs pin each column to its vocabulary; the writer pins the
/// PAIRING: `ATTRIBUTED_TO` naming a claim (both words allowed, the pair
/// never planned) is refused, recorded
/// on the row (one attempt, the reason) and never becomes a kernel edge,
/// while the synthesis' real rows are written. Kills: the shape check
/// removed (the kernel's HTTP route validated relationships; the repository
/// the worker now calls does not).
#[tokio::test]
async fn an_outbox_row_outside_the_prov_shapes_is_refused_not_written() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Public).await;
    let w = worker(&db, valid_llm(&db, s)).await;
    assert_eq!(w.run_once().await.unwrap(), JobOutcome::Completed(s));
    let before = kernel_edges(a, s).await;
    let target = support::any_public_claim(a).await;
    sqlx::query(
        "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id)
         VALUES ($1, 'ATTRIBUTED_TO', 'claim', $2)",
    )
    .bind(s)
    .bind(target)
    .execute(a)
    .await
    .expect("plant a foreign outbox row");

    let r = w.run_worklist(50).await.unwrap();
    assert_eq!((r.edges_written, r.skipped), (0, 1), "{r:?}");
    assert_eq!(kernel_edges(a, s).await, before, "no kernel edge for it");
    let (attempts, err): (i32, Option<String>) = sqlx::query_as(
        "SELECT attempt_count, last_error FROM synthesis_provo_edges
          WHERE synthesis_id = $1 AND predicate = 'ATTRIBUTED_TO' AND target_kind = 'claim'",
    )
    .bind(s)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert!(err
        .unwrap_or_default()
        .contains("not a synthesis PROV edge shape"));
}

/// The Reject path on the worker login (the one multi-write path the accept
/// tests never reach): the verifier rejects, and in ONE stamped transaction
/// the parent is marked `rejected`, a refinement child is inserted in the
/// parent's pair and authored by the queue principal, its `REFINES` outbox
/// row names the parent, and its job is queued acting as the same principal.
/// The parent's job is `complete` after one attempt (a rejection is an
/// outcome, not a failure), and the next claim runs the child. Kills: any of
/// those four inserts refused on the application login (the worker would
/// retry every LLM stage to the same refusal and end `failed`), and a child
/// authored or enqueued as anyone but the acting principal.
#[tokio::test]
async fn a_rejected_synthesis_spawns_its_refinement_on_the_worker_login() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let s = enqueue(a, h1.agent, h1.agent, h1.personal_group, Visibility::Group).await;
    let w = worker(
        &db,
        Arc::new(RejectLlm {
            admin: a.clone(),
            calls: Mutex::new(0),
        }),
    )
    .await;

    assert_eq!(w.run_once().await.expect("run"), JobOutcome::Completed(s));
    assert_eq!(status(a, s).await.0, "rejected");
    let (state, err, attempts, _) = job(a, s).await;
    assert_eq!((state.as_str(), err, attempts), ("complete", None, 1));

    let children: Vec<(Uuid, Uuid, String, Uuid, String)> = sqlx::query_as(
        "SELECT id, agent_id, visibility, owner_group_id, status FROM syntheses
          WHERE parent_synthesis_id = $1",
    )
    .bind(s)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(children.len(), 1, "exactly one refinement child");
    let (child, author, vis, owner, st) = children[0].clone();
    assert_eq!(
        (author, vis.as_str(), owner, st.as_str()),
        (h1.agent, "group", h1.personal_group, "pending"),
        "the child keeps the parent's pair and is authored by the acting principal"
    );
    let refines: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM synthesis_provo_edges
          WHERE synthesis_id = $1 AND predicate = 'REFINES' AND target_kind = 'synthesis' AND target_id = $2",
    )
    .bind(child)
    .bind(s)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(refines, 1);
    let (cstate, cprincipal): (String, Uuid) =
        sqlx::query_as("SELECT state, principal_id FROM synthesis_jobs WHERE id = $1")
            .bind(child)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!((cstate.as_str(), cprincipal), ("queued", h1.agent));

    // The next claim runs the child (and, rejected again, refines it once more).
    assert_eq!(
        w.run_once().await.expect("run"),
        JobOutcome::Completed(child)
    );
    assert_eq!(status(a, child).await.0, "rejected");
}
