//! Integration tests for the Phase 1 EpiClaw workflow-run route.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
//!     cargo test -p episcience-api --test workflow_runs_route_test
//!
//! These tests build an `axum::Router` via `episcience_api::create_router`,
//! wrap it in `axum_test::TestServer` (no real listener / no port management),
//! and exercise `POST /api/v1/eln/workflow_runs` end-to-end. Each test seeds
//! its own agent row (32-byte random public_key) so `samples.prepared_by` (FK
//! to `agents.id`) is satisfied, and cleans up after itself.
//!
//! The boilerplate (`jwt_secret_bytes`, `mint_test_jwt`, `bearer`, `connect`,
//! `build_test_server`, `seed_agent`) is duplicated from
//! `protocols_routes_test.rs` / `syntheses_routes_test.rs`. Consolidating into
//! a shared `tests/common/mod.rs` is known tech debt — out of scope for
//! Phase 1.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint_test_jwt};

fn bearer(token: &str) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
    )
}

/// The run's shared clone of the E1 template (scripts/e1-test-db.sh). Refuses
/// port 5432 and any database name not ending in `_test`; no default DSN.
async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
}

async fn build_test_server(pool: PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
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

/// Seed an agent row with a random 32-byte public_key so it satisfies the
/// `samples.prepared_by` FK. Returns the new agent's UUID.
async fn seed_agent(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let pk: Vec<u8> = (0..32)
        .map(|i| ((id.as_u128() >> (i % 16)) as u8) ^ i)
        .collect();
    sqlx::query(
        r#"INSERT INTO agents (id, public_key, display_name, agent_type, role, state)
           VALUES ($1, $2, $3, 'service', 'custom', 'active')"#,
    )
    .bind(id)
    .bind(&pk)
    .bind(format!("workflow-runs-test-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    // As the kernel's OAuth mint path does for every token's agent: a live
    // personal group (admin). Without one the caller may write no group, and
    // the request path refuses every write before it begins (E1g).
    sqlx::query("SELECT public.epigraph_ensure_personal_group($1)")
        .bind(id)
        .execute(pool)
        .await
        .expect("provision the agent's personal group");
    id
}

async fn cleanup_sample(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM sample_claims WHERE sample_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM samples WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
}

async fn cleanup_agent(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM agents WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: happy path — POST creates a workflow_run sample carrying workflow_id
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_workflow_run_creates_sample_with_workflow_id_property() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let agent_id = seed_agent(&pool).await;
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let workflow_id = Uuid::now_v7();
    let canonical_name = format!("test/wf-{workflow_id}");
    let started_at = chrono::Utc::now();

    let resp: TestResponse = server
        .post("/api/v1/eln/workflow_runs")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "workflow_id": workflow_id,
            "canonical_name": canonical_name,
            "prepared_by": agent_id,
            "started_at": started_at.to_rfc3339(),
            "labels": ["phase1", "epiclaw"],
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::CREATED,
        "expected 201 CREATED, body: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    let sample_id: Uuid = body
        .get("sample_id")
        .and_then(|v| v.as_str())
        .expect("body has sample_id")
        .parse()
        .expect("sample_id parses as UUID");
    assert_eq!(
        body.get("sample_type").and_then(|v| v.as_str()),
        Some("workflow_run"),
        "response sample_type field should be 'workflow_run'",
    );
    assert_eq!(
        body.get("workflow_id").and_then(|v| v.as_str()),
        Some(workflow_id.to_string().as_str()),
        "response workflow_id should round-trip",
    );

    // Read back the row and assert key fields.
    let row: (String, String, serde_json::Value, Vec<String>, Uuid) = sqlx::query_as(
        r#"SELECT sample_type, name, properties, labels, prepared_by
           FROM samples WHERE id = $1"#,
    )
    .bind(sample_id)
    .fetch_one(&pool)
    .await
    .expect("fetch sample row");

    let (db_sample_type, db_name, db_props, db_labels, db_prepared_by) = row;
    assert_eq!(db_sample_type, "workflow_run");
    assert_eq!(db_name, canonical_name);
    assert_eq!(db_prepared_by, agent_id);
    assert_eq!(
        db_props.get("workflow_id").and_then(|v| v.as_str()),
        Some(workflow_id.to_string().as_str()),
        "properties.workflow_id matches the posted workflow_id",
    );
    assert_eq!(
        db_props.get("canonical_name").and_then(|v| v.as_str()),
        Some(canonical_name.as_str()),
    );
    assert!(
        db_props.get("started_at").is_some(),
        "properties.started_at present",
    );
    // labels = caller-supplied + "workflow_run"
    assert!(
        db_labels.iter().any(|l| l == "workflow_run"),
        "labels include 'workflow_run' marker; got {db_labels:?}",
    );
    assert!(
        db_labels.iter().any(|l| l == "phase1"),
        "labels include caller-supplied 'phase1'; got {db_labels:?}",
    );

    cleanup_sample(&pool, sample_id).await;
    cleanup_agent(&pool, agent_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: prepared_by != auth.agent_id is rejected with 403
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_workflow_run_rejects_mismatched_prepared_by() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let auth_agent = seed_agent(&pool).await;
    let token = mint_test_jwt(auth_agent);
    let (hn, hv) = bearer(&token);

    // Different UUID — does not need an agents row since the handler
    // short-circuits before the INSERT.
    let other_agent = Uuid::now_v7();
    assert_ne!(auth_agent, other_agent);

    let workflow_id = Uuid::now_v7();
    let resp: TestResponse = server
        .post("/api/v1/eln/workflow_runs")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "workflow_id": workflow_id,
            "canonical_name": format!("test/wf-mismatch-{workflow_id}"),
            "prepared_by": other_agent,
            "started_at": chrono::Utc::now().to_rfc3339(),
            "labels": [],
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::FORBIDDEN,
        "expected 403 FORBIDDEN, body: {}",
        resp.text(),
    );

    // Negative-side check: no sample row was created for this workflow_id.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM samples WHERE properties->>'workflow_id' = $1")
            .bind(workflow_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("count samples");
    assert_eq!(count, 0, "no samples row should be inserted on 403");

    cleanup_agent(&pool, auth_agent).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Idempotency (backlog 015db9a5): the route is keyed on its content_hash
// (canonical_name, workflow_id, started_at) for the calling agent
// ──────────────────────────────────────────────────────────────────────────────

/// POST one run as `agent`; returns the status and the `sample_id`.
async fn post_run(
    server: &TestServer,
    agent: Uuid,
    workflow_id: Uuid,
    canonical_name: &str,
    started_at: &str,
) -> (axum::http::StatusCode, Uuid) {
    let (hn, hv) = bearer(&mint_test_jwt(agent));
    let resp: TestResponse = server
        .post("/api/v1/eln/workflow_runs")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "workflow_id": workflow_id,
            "canonical_name": canonical_name,
            "started_at": started_at,
            "labels": ["replay"],
        }))
        .await;
    let status = resp.status_code();
    let body: serde_json::Value = resp.json();
    let id = body
        .get("sample_id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("status {status}, body has no sample_id: {body}"))
        .parse()
        .expect("sample_id parses as UUID");
    (status, id)
}

/// The workflow_run rows recorded for `workflow_id` (read on the admin pool,
/// so a row the caller cannot see is counted too).
async fn run_rows(pool: &PgPool, workflow_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM samples WHERE sample_type = 'workflow_run' \
           AND properties->>'workflow_id' = $1 ORDER BY id",
    )
    .bind(workflow_id.to_string())
    .fetch_all(pool)
    .await
    .expect("list workflow_run rows")
}

/// Replay: the same run POSTed twice is ONE samples row; the first answer is
/// 201, the replay 200 with the same id. The same instant written with
/// another UTC offset is the same run. A different `started_at` is a new run
/// (201, a second row). Kills: no lookup before the INSERT (the replay
/// inserts a duplicate), or a lookup that ignores `started_at`.
#[tokio::test]
async fn replaying_a_workflow_run_returns_the_existing_sample() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let agent = seed_agent(&pool).await;
    let workflow_id = Uuid::now_v7();
    let name = format!("test/wf-replay-{workflow_id}");
    // Nanosecond precision: the key is the RFC 3339 text, which Postgres'
    // microsecond timestamptz would truncate.
    let started = "2026-11-27T10:00:00.123456789Z";

    let (first, id) = post_run(&server, agent, workflow_id, &name, started).await;
    assert_eq!(first, axum::http::StatusCode::CREATED);
    let (replay, again) = post_run(&server, agent, workflow_id, &name, started).await;
    assert_eq!(replay, axum::http::StatusCode::OK, "a replay is 200");
    assert_eq!(again, id, "a replay returns the existing sample");
    let (offset, same) = post_run(
        &server,
        agent,
        workflow_id,
        &name,
        "2026-11-27T12:00:00.123456789+02:00",
    )
    .await;
    assert_eq!(offset, axum::http::StatusCode::OK);
    assert_eq!(
        same, id,
        "the same instant in another offset is the same run"
    );
    assert_eq!(run_rows(&pool, workflow_id).await, vec![id], "one row");

    let (later, other) = post_run(
        &server,
        agent,
        workflow_id,
        &name,
        "2026-11-27T10:00:00.123457Z",
    )
    .await;
    assert_eq!(
        later,
        axum::http::StatusCode::CREATED,
        "a new start is a new run"
    );
    assert_ne!(other, id);
    let mut both = vec![id, other];
    both.sort();
    assert_eq!(run_rows(&pool, workflow_id).await, both, "two rows");

    for id in both {
        cleanup_sample(&pool, id).await;
    }
    cleanup_agent(&pool, agent).await;
}

/// The key is per calling agent: another agent POSTing the exact same run
/// gets its own sample (201), never the first agent's row (which would file
/// its record under someone else's `prepared_by`). Kills: a lookup that does
/// not filter on `prepared_by`.
#[tokio::test]
async fn another_agent_posting_the_same_run_gets_its_own_sample() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let a = seed_agent(&pool).await;
    let b = seed_agent(&pool).await;
    let workflow_id = Uuid::now_v7();
    let name = format!("test/wf-two-agents-{workflow_id}");
    let started = "2026-11-27T10:00:00Z";

    let (sa, ida) = post_run(&server, a, workflow_id, &name, started).await;
    let (sb, idb) = post_run(&server, b, workflow_id, &name, started).await;
    assert_eq!(sa, axum::http::StatusCode::CREATED);
    assert_eq!(
        sb,
        axum::http::StatusCode::CREATED,
        "B's first POST creates"
    );
    assert_ne!(ida, idb, "B never receives A's sample");
    let preparer: Uuid = sqlx::query_scalar("SELECT prepared_by FROM samples WHERE id = $1")
        .bind(idb)
        .fetch_one(&pool)
        .await
        .expect("read B's row");
    assert_eq!(preparer, b);

    cleanup_sample(&pool, ida).await;
    cleanup_sample(&pool, idb).await;
    cleanup_agent(&pool, a).await;
    cleanup_agent(&pool, b).await;
}

/// A matching row the caller cannot read is treated as absent: the caller's
/// own earlier run, moved (on the admin pool) into a team group the caller is
/// not a member of with `group` visibility, is not returned. The POST
/// creates a new sample (201) and the hidden id appears nowhere in the
/// answer. Kills: the lookup run outside the caller's stamped transaction
/// (on a pool that bypasses row security).
#[tokio::test]
async fn a_matching_row_the_caller_cannot_read_is_not_returned() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let caller = seed_agent(&pool).await;
    let owner = testdb::principal(&pool, "wf-hidden-owner").await;
    let team = testdb::team_group(&pool, &owner, &[]).await;
    let workflow_id = Uuid::now_v7();
    let name = format!("test/wf-hidden-{workflow_id}");
    let started = "2026-11-27T10:00:00Z";

    let (created, hidden) = post_run(&server, caller, workflow_id, &name, started).await;
    assert_eq!(created, axum::http::StatusCode::CREATED);
    sqlx::query("UPDATE samples SET owner_group_id = $2, visibility = 'group' WHERE id = $1")
        .bind(hidden)
        .bind(team)
        .execute(&pool)
        .await
        .expect("move the row out of the caller's reach");

    let (hn, hv) = bearer(&mint_test_jwt(caller));
    let resp: TestResponse = server
        .post("/api/v1/eln/workflow_runs")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "workflow_id": workflow_id,
            "canonical_name": name,
            "started_at": started,
        }))
        .await;
    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::CREATED,
        "{}",
        resp.text()
    );
    assert!(
        !resp.text().contains(&hidden.to_string()),
        "the hidden row's id must not leak: {}",
        resp.text()
    );
    let rows = run_rows(&pool, workflow_id).await;
    assert_eq!(rows.len(), 2, "a new row beside the hidden one");

    for id in rows {
        cleanup_sample(&pool, id).await;
    }
    cleanup_agent(&pool, caller).await;
}

/// Concurrent replays: several identical POSTs of one run in flight at once
/// are still ONE samples row, exactly one 201 and the rest 200 with the same
/// id. Each POST goes through its own server (axum-test holds a server's
/// transport lock across a request, so one server would run them one at a
/// time) and so its own pool and transaction: without the advisory lock on
/// the key every transaction can miss the lookup before any commits and each
/// inserts its own row. Repeated over a few runs so an unlucky schedule
/// cannot hide a missing lock. Kills: the lock dropped, or taken after the
/// lookup.
#[tokio::test]
async fn concurrent_replays_of_a_workflow_run_insert_one_row() {
    const IN_FLIGHT: usize = 4;
    const ROUNDS: usize = 4;
    let pool = connect().await;
    let mut servers = Vec::new();
    for _ in 0..IN_FLIGHT {
        servers.push(build_test_server(pool.clone()).await);
    }
    let agent = seed_agent(&pool).await;

    let mut created = Vec::new();
    for round in 0..ROUNDS {
        let workflow_id = Uuid::now_v7();
        let name = format!("test/wf-concurrent-{round}-{workflow_id}");
        let started = "2026-11-27T10:00:00Z";
        let answers = futures_util::future::join_all(
            servers
                .iter()
                .map(|server| post_run(server, agent, workflow_id, &name, started)),
        )
        .await;

        let rows = run_rows(&pool, workflow_id).await;
        created.extend(rows.iter().copied());
        assert_eq!(rows.len(), 1, "round {round}: one row, got {rows:?}");
        let fresh = answers
            .iter()
            .filter(|(s, _)| *s == axum::http::StatusCode::CREATED)
            .count();
        assert_eq!(fresh, 1, "round {round}: exactly one 201: {answers:?}");
        assert!(
            answers.iter().all(|(s, id)| *id == rows[0]
                && (*s == axum::http::StatusCode::CREATED || *s == axum::http::StatusCode::OK)),
            "round {round}: every answer names the one row: {answers:?}"
        );
    }

    for id in created {
        cleanup_sample(&pool, id).await;
    }
    cleanup_agent(&pool, agent).await;
}
