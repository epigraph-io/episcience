//! Integration tests for the Phase 3 synthesis REST routes.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//!     cargo test -p episcience-api --test syntheses_routes_test
//!
//! These tests build an `axum::Router` via `episcience_api::create_router`,
//! wrap it in `axum_test::TestServer` (no real listener / no port management),
//! and exercise the routes end-to-end against the live
//! `epigraph_dev_synthesis` database. Each test creates and cleans up its own
//! rows so they're independent.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_core::synthesis::{Cluster, Visibility};
use episcience_db::{
    SynthesisClustersRepository, SynthesisRepository, SynthesisStalenessRepository,
};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

// Shared kernel-shaped token minting (iss/aud/exp/scopes), see support/token.rs.
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

/// Build a `TestServer` wrapping the full episcience-api router.
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

/// Hard-delete a synthesis (cascades to synthesis_jobs / shares / etc.).
async fn cleanup_synthesis(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM synthesis_shares WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    // `synthesis_jobs.id REFERENCES syntheses(id) ON DELETE CASCADE`, so the
    // job row goes with the synthesis row — but be explicit for safety.
    sqlx::query("DELETE FROM synthesis_jobs WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: POST /syntheses returns 202 with id + status="queued"
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_syntheses_returns_202_with_id() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let agent_id_p = testdb::principal(&pool, "agent_id").await;

    let agent_id = agent_id_p.agent;
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses")
        .add_header(hn, hv)
        .json(&serde_json::json!({"query": "DNA origami"}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "expected 202 ACCEPTED, body: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    let id_str = body
        .get("id")
        .and_then(|v| v.as_str())
        .expect("body has id field");
    let id: Uuid = id_str.parse().expect("id parses as UUID");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("queued"),
        "status should be 'queued'"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: POST writes synthesis row + job row in one tx
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_syntheses_writes_synthesis_and_job_row_in_one_tx() {
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
            "query": "atomic insert test",
            "visibility": "private",
        }))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::ACCEPTED);

    let body: serde_json::Value = resp.json();
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    let synth_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("count syntheses");
    assert_eq!(synth_count, 1, "exactly 1 row in syntheses");

    // synthesis_jobs is keyed by the same id (FK).
    let job_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM synthesis_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("count synthesis_jobs");
    assert_eq!(job_count, 1, "exactly 1 row in synthesis_jobs");

    // Verify job state is 'queued' (atomic insert post-condition).
    let state: String = sqlx::query_scalar("SELECT state FROM synthesis_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch state");
    assert_eq!(state, "queued");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: empty query → 422
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_syntheses_empty_query_returns_422() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let agent_id_p = testdb::principal(&pool, "agent_id").await;

    let agent_id = agent_id_p.agent;
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses")
        .add_header(hn, hv)
        .json(&serde_json::json!({"query": ""}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422 for empty query, body: {}",
        resp.text()
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 4: no auth header → 401
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_syntheses_no_auth_returns_401() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses")
        .json(&serde_json::json!({"query": "anything"}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::UNAUTHORIZED,
        "expected 401 with no auth, body: {}",
        resp.text()
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 5: GET /syntheses/:id — owner reads their own synthesis
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_synthesis_owner_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "owner reads test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "expected 200, body: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    assert_eq!(
        body["id"]
            .as_str()
            .and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(id),
        "body.id matches"
    );
    assert_eq!(body["query"].as_str(), Some("owner reads test"));
    assert_eq!(
        body["agent_id"]
            .as_str()
            .and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(owner),
    );
    assert_eq!(body["visibility"].as_str(), Some("group"));
    assert_eq!(
        body["owner_group_id"]
            .as_str()
            .and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(owner_p.personal_group),
        "the owner group is reported"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 6: GET — stranger gets 404 (NOT 403, to avoid existence leak)
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_synthesis_stranger_gets_404() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "stranger 404 test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::NOT_FOUND,
        "stranger should see 404 (existence-hide), got {}; body: {}",
        resp.status_code(),
        resp.text()
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 7: GET — recipient with explicit share row reads
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_synthesis_recipient_with_share_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let recipient_p = testdb::principal(&pool, "recipient").await;
    let recipient = recipient_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "share recipient test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    testdb::reown_to_team_with_reader(&pool, id, &owner_p, recipient).await;

    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "recipient with share row should read; body: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    assert_eq!(
        body["id"]
            .as_str()
            .and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(id),
    );

    cleanup_synthesis(&pool, id).await;
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ Task 3.3 — list / refine / soft-delete / clusters / snapshot / staleness ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// ──────────────────────────────────────────────────────────────────────────────
// Test 8: GET /syntheses — owner sees their own private syntheses
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_owned_syntheses() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id_a = Uuid::now_v7();
    let id_b = Uuid::now_v7();

    for (id, q) in [(id_a, "list owner test A"), (id_b, "list owner test B")] {
        SynthesisRepository::create_pending(
            &pool,
            id,
            q,
            owner,
            None,
            &[],
            "anthropic",
            "claude-sonnet-4-6",
            episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
        )
        .await
        .expect("seed synthesis");
    }

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server.get("/api/v1/eln/syntheses").add_header(hn, hv).await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned_ids: Vec<Uuid> = body
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    assert!(returned_ids.contains(&id_a), "owner sees synthesis A");
    assert!(returned_ids.contains(&id_b), "owner sees synthesis B");

    cleanup_synthesis(&pool, id_a).await;
    cleanup_synthesis(&pool, id_b).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 8b (Task 4.6): GET /syntheses excludes stale rows by default
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_excludes_stale_by_default() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id_fresh = Uuid::now_v7();
    let id_stale = Uuid::now_v7();

    for (id, q) in [(id_fresh, "fresh row"), (id_stale, "stale row")] {
        SynthesisRepository::create_pending(
            &pool,
            id,
            q,
            owner,
            None,
            &[],
            "anthropic",
            "claude-sonnet-4-6",
            episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
        )
        .await
        .expect("seed synthesis");
    }
    SynthesisRepository::mark_stale(&pool, id_stale, "belief_drift")
        .await
        .expect("mark_stale");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server.get("/api/v1/eln/syntheses").add_header(hn, hv).await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned_ids: Vec<Uuid> = body
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    assert!(
        returned_ids.contains(&id_fresh),
        "fresh row must appear in default list (returned: {returned_ids:?})"
    );
    assert!(
        !returned_ids.contains(&id_stale),
        "stale row must NOT appear in default list (returned: {returned_ids:?})"
    );

    cleanup_synthesis(&pool, id_fresh).await;
    cleanup_synthesis(&pool, id_stale).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 8c (Task 4.6): GET /syntheses?include_stale=true returns both
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_includes_stale_when_requested() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id_fresh = Uuid::now_v7();
    let id_stale = Uuid::now_v7();

    for (id, q) in [(id_fresh, "fresh row 2"), (id_stale, "stale row 2")] {
        SynthesisRepository::create_pending(
            &pool,
            id,
            q,
            owner,
            None,
            &[],
            "anthropic",
            "claude-sonnet-4-6",
            episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
        )
        .await
        .expect("seed synthesis");
    }
    SynthesisRepository::mark_stale(&pool, id_stale, "belief_drift")
        .await
        .expect("mark_stale");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    // axum-test 14 percent-encodes a `?` embedded in the path string (because
    // `Url::set_path` treats the whole argument as the path component). Use
    // `add_query_param` to attach the query string properly.
    let resp: TestResponse = server
        .get("/api/v1/eln/syntheses")
        .add_query_param("include_stale", "true")
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned_ids: Vec<Uuid> = body
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    assert!(
        returned_ids.contains(&id_fresh),
        "fresh row should appear with include_stale=true"
    );
    assert!(
        returned_ids.contains(&id_stale),
        "stale row should appear with include_stale=true (returned: {returned_ids:?})"
    );

    cleanup_synthesis(&pool, id_fresh).await;
    cleanup_synthesis(&pool, id_stale).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 8d (Phase 8 review-bot): GET /syntheses?skill_name=code_review filters
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_syntheses_filters_by_skill_name() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id_cr_a = Uuid::now_v7();
    let id_cr_b = Uuid::now_v7();
    let id_baseline = Uuid::now_v7();

    // Seed three syntheses with create_pending (which defaults skill_name to
    // 'baseline' at the DB level), then patch the skill_name on the two
    // code_review rows. The CHECK constraint
    // (`syntheses_skill_name_known`) allows every registered skill
    // (`skills::registered_names()`), `code_review` among them.
    for (id, q) in [
        (id_cr_a, "review-bot test A"),
        (id_cr_b, "review-bot test B"),
        (id_baseline, "review-bot test baseline"),
    ] {
        SynthesisRepository::create_pending(
            &pool,
            id,
            q,
            owner,
            None,
            &[],
            "anthropic",
            "claude-sonnet-4-6",
            episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
        )
        .await
        .expect("seed synthesis");
    }
    for id in [id_cr_a, id_cr_b] {
        sqlx::query("UPDATE syntheses SET skill_name = 'code_review' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("patch skill_name to code_review");
    }

    let token = mint_test_jwt(owner);

    // 1. Filter to skill_name=code_review → only the two CR rows visible to
    //    this owner.
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get("/api/v1/eln/syntheses")
        .add_query_param("skill_name", "code_review")
        .add_header(hn, hv)
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned_ids: Vec<Uuid> = body
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    assert!(
        returned_ids.contains(&id_cr_a),
        "code_review A missing under skill_name filter: {returned_ids:?}"
    );
    assert!(
        returned_ids.contains(&id_cr_b),
        "code_review B missing under skill_name filter: {returned_ids:?}"
    );
    assert!(
        !returned_ids.contains(&id_baseline),
        "baseline row must NOT appear under skill_name=code_review filter: {returned_ids:?}"
    );

    // 2. No filter → all three rows visible to this owner.
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server.get("/api/v1/eln/syntheses").add_header(hn, hv).await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned_ids: Vec<Uuid> = body
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    assert!(returned_ids.contains(&id_cr_a), "owner sees code_review A");
    assert!(returned_ids.contains(&id_cr_b), "owner sees code_review B");
    assert!(
        returned_ids.contains(&id_baseline),
        "owner sees baseline row when no filter is set"
    );

    cleanup_synthesis(&pool, id_cr_a).await;
    cleanup_synthesis(&pool, id_cr_b).await;
    cleanup_synthesis(&pool, id_baseline).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 9: GET /syntheses — strangers do not see private syntheses
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_excludes_others_private_syntheses() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "list stranger exclusion test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server.get("/api/v1/eln/syntheses").add_header(hn, hv).await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned_ids: Vec<Uuid> = body
        .iter()
        .filter_map(|v| v["id"].as_str().and_then(|s| s.parse().ok()))
        .collect();
    assert!(
        !returned_ids.contains(&id),
        "stranger must not see owner's private synthesis"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 10: POST /syntheses/{id}/refine — creates new row with parent link
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn refine_creates_new_synthesis_with_parent_link() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let parent_id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        parent_id,
        "refine parent test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed parent");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .post(&format!("/api/v1/eln/syntheses/{parent_id}/refine"))
        .add_header(hn, hv)
        .json(&serde_json::json!({}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "expected 202, body: {}",
        resp.text()
    );

    let body: serde_json::Value = resp.json();
    let new_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new_id, parent_id, "refined id must differ from parent");
    assert_eq!(
        body["parent_synthesis_id"]
            .as_str()
            .and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(parent_id),
    );
    assert_eq!(body["status"].as_str(), Some("queued"));

    let row_parent: Option<Uuid> =
        sqlx::query_scalar("SELECT parent_synthesis_id FROM syntheses WHERE id = $1")
            .bind(new_id)
            .fetch_one(&pool)
            .await
            .expect("fetch parent_synthesis_id");
    assert_eq!(row_parent, Some(parent_id), "DB row links to parent");

    let row_query: String = sqlx::query_scalar("SELECT query FROM syntheses WHERE id = $1")
        .bind(new_id)
        .fetch_one(&pool)
        .await
        .expect("fetch query");
    assert_eq!(
        row_query, "refine parent test",
        "default query inherited from parent"
    );

    cleanup_synthesis(&pool, new_id).await;
    cleanup_synthesis(&pool, parent_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 11: refine — unreadable parent → 404
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn refine_404_on_unreadable_parent() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let parent_id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        parent_id,
        "refine unreadable parent test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed parent");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .post(&format!("/api/v1/eln/syntheses/{parent_id}/refine"))
        .add_header(hn, hv)
        .json(&serde_json::json!({}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::NOT_FOUND,
        "stranger refining private parent must see 404"
    );

    cleanup_synthesis(&pool, parent_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 12: DELETE /syntheses/{id} — owner soft-deletes
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_owner_succeeds() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "delete owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .delete(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NO_CONTENT);

    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch status");
    assert_eq!(status, "deleted");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 13: DELETE — share recipient is NOT permitted
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_non_owner_403() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let recipient_p = testdb::principal(&pool, "recipient").await;
    let recipient = recipient_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "delete non-owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    testdb::reown_to_team_with_reader(&pool, id, &owner_p, recipient).await;

    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .delete(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::FORBIDDEN,
        "share recipient must not delete; got {}",
        resp.status_code()
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 14: GET /syntheses/{id}/clusters — owner reads two seeded clusters
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn clusters_owner_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "clusters owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    for i in 0..2 {
        let cluster = Cluster {
            id: Uuid::now_v7(),
            synthesis_id: id,
            cluster_index: i,
            title: format!("cluster {i}"),
            summary: format!("summary {i}"),
            member_claim_ids: vec![Uuid::now_v7()],
            support_count: 1,
            contradict_count: 0,
        };
        SynthesisClustersRepository::insert(&pool, &cluster)
            .await
            .expect("insert cluster");
    }

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/clusters"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    assert_eq!(body.len(), 2, "two clusters returned");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 15: GET clusters — stranger sees 404
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn clusters_stranger_404() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "clusters stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/clusters"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NOT_FOUND);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 16: GET /syntheses/{id}/snapshot — owner reads snapshot JSON
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_owner_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "snapshot owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/snapshot"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: serde_json::Value = resp.json();
    assert!(body.is_object(), "snapshot is a JSON object");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 17: GET snapshot — stranger 404
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_stranger_404() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "snapshot stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/snapshot"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NOT_FOUND);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 18: GET /syntheses/{id}/staleness — owner reads (seeded event)
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn staleness_owner_reads_seeded_event() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "staleness owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    SynthesisStalenessRepository::record_event(
        &pool,
        id,
        "belief_drift",
        &[Uuid::now_v7()],
        Some(&serde_json::json!({"score": 0.42})),
    )
    .await
    .expect("seed staleness event");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/staleness"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    assert_eq!(body.len(), 1, "one staleness event returned");
    assert_eq!(body[0]["trigger"].as_str(), Some("belief_drift"));

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 19: GET staleness — stranger 404
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn staleness_stranger_404() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "staleness stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/staleness"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NOT_FOUND);

    cleanup_synthesis(&pool, id).await;
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ Task 3.4 — sharing endpoints (grant / revoke / list / visibility patch)  ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// ──────────────────────────────────────────────────────────────────────────────
// Test 20: POST /syntheses/{id}/shares — owner grants → 201
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 21: POST shares — non-owner 403
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 22: DELETE share — owner revokes → 204
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 23: DELETE share — recipient revokes their own → 204
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 24: DELETE share — stranger forbidden
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 25: GET /syntheses/{id}/shares — owner lists
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 26: GET shares — non-owner 403
// ──────────────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────────────
// Test 27: PATCH visibility — owner switches private → public
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn patch_visibility_owner_succeeds_to_public() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "patch visibility owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .patch(&format!("/api/v1/eln/syntheses/{id}/visibility"))
        .add_header(hn, hv)
        .json(&serde_json::json!({"visibility": "public"}))
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NO_CONTENT);

    let vis: String = sqlx::query_scalar("SELECT visibility FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch visibility");
    assert_eq!(vis, "public");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 28: PATCH visibility — non-owner 403
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn patch_visibility_non_owner_403() {
    // A stranger cannot see the group synthesis at all (404, like a missing
    // one); a READER of its team can see it but not edit it (403); neither
    // changes it. Kills: the edit authorized on the read set, or a 403 that
    // leaks existence to a stranger.
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;
    let owner = owner_p.agent;
    let attacker_p = testdb::principal(&pool, "attacker").await;
    let attacker = attacker_p.agent;
    let reader = testdb::principal(&pool, "reader").await.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "patch visibility attacker test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");
    testdb::reown_to_team_with_reader(&pool, id, &owner_p, reader).await;

    for (who, want) in [
        (attacker, axum::http::StatusCode::NOT_FOUND),
        (reader, axum::http::StatusCode::FORBIDDEN),
    ] {
        let token = mint_test_jwt(who);
        let (hn, hv) = bearer(&token);
        let resp: TestResponse = server
            .patch(&format!("/api/v1/eln/syntheses/{id}/visibility"))
            .add_header(hn, hv)
            .json(&serde_json::json!({"visibility": "public"}))
            .await;
        assert_eq!(resp.status_code(), want, "{}", resp.text());
    }
    let vis: String = sqlx::query_scalar("SELECT visibility FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(vis, "group", "neither refused edit changed the row");

    cleanup_synthesis(&pool, id).await;
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ Phase 5 Task 5.2 — sharing matrix gap-fillers                            ║
// ║                                                                          ║
// ║ The 3 (visibilities) × 4 (agent kinds) × 6 (endpoint groups) sharing     ║
// ║ matrix from the validation plan is mostly covered organically by Tests   ║
// ║ 5–28 above plus the search tests in `synthesis_search_test.rs` and the   ║
// ║ readable_by truth-table test in `phase01_e2e_test.rs`. Rather than       ║
// ║ mechanically re-emit all 72 cells, this section fills the specific gaps  ║
// ║ that the plan flagged as easy-to-miss. Cells already covered:            ║
// ║                                                                          ║
// ║   - private/owner, private/stranger, private/recipient-with-share        ║
// ║     ×  get / clusters / snapshot / staleness / refine / delete           ║
// ║       — Tests 5, 6, 7, 11, 12, 13, 14, 15, 16, 17, 18, 19, 23            ║
// ║   - shared/recipient × get  — Test 7                                     ║
// ║   - public/* × search       — synthesis_search_test::*                   ║
// ║   - readable_by truth table — phase01_e2e_test::test_readable_by_*       ║
// ║                                                                          ║
// ║ Gaps filled below:                                                       ║
// ║                                                                          ║
// ║   Test 29: refine × shared-recipient               → 202 (Test 10 only   ║
// ║                                                       owners; Test 11    ║
// ║                                                       only strangers)    ║
// ║   Test 30: clusters × public-stranger              → 200                 ║
// ║   Test 31: clusters × shared-recipient             → 200                 ║
// ║   Test 32: snapshot × public-stranger              → 200                 ║
// ║   Test 33: snapshot × shared-recipient             → 200                 ║
// ║   Test 34: staleness × public-stranger             → 200                 ║
// ║   Test 35: staleness × shared-recipient            → 200                 ║
// ║   Test 36: get × shared-stranger-without-share-row → 404                 ║
// ║   Test 37: delete × public-stranger                → 403                 ║
// ║   Test 38: list-shares × public-stranger           → 403                 ║
// ╚══════════════════════════════════════════════════════════════════════════╝

// ──────────────────────────────────────────────────────────────────────────────
// Test 29: refine of a TEAM synthesis: a reader of the team is refused (a
// child of a non-public synthesis stays in its group, which the refiner must
// be able to write); a writer of the team succeeds and the child is owned by
// the team.
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn refine_shared_recipient_succeeds() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;
    let owner = owner_p.agent;
    let recipient_p = testdb::principal(&pool, "recipient").await;
    let recipient = recipient_p.agent;
    let writer = testdb::principal(&pool, "writer").await.agent;
    let parent_id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        parent_id,
        "refine recipient test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed parent");
    let team = testdb::reown_to_team_with_reader(&pool, parent_id, &owner_p, recipient).await;
    sqlx::query(
        "INSERT INTO group_memberships (group_id, agent_id, wrapped_key_share, epoch, role) \
         VALUES ($1, $2, '\\x00'::bytea, 0, 'writer')",
    )
    .bind(team)
    .bind(writer)
    .execute(&pool)
    .await
    .expect("writer membership");

    let refine = |who: Uuid| {
        let token = mint_test_jwt(who);
        let (hn, hv) = bearer(&token);
        server
            .post(&format!("/api/v1/eln/syntheses/{parent_id}/refine"))
            .add_header(hn, hv)
            .json(&serde_json::json!({}))
    };
    let refused = refine(recipient).await;
    assert_eq!(
        refused.status_code(),
        axum::http::StatusCode::FORBIDDEN,
        "a team READER may not refine a group synthesis; body: {}",
        refused.text()
    );

    let resp = refine(writer).await;
    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "a team WRITER refines; body: {}",
        resp.text()
    );
    let body: serde_json::Value = resp.json();
    let new_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        body["parent_synthesis_id"]
            .as_str()
            .and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(parent_id),
        "refined row links back to parent"
    );
    let (child_owner, child_vis, child_author): (Uuid, String, Uuid) =
        sqlx::query_as("SELECT owner_group_id, visibility, agent_id FROM syntheses WHERE id = $1")
            .bind(new_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(child_owner, team, "the child stays in the parent's group");
    assert_eq!(child_vis, "group");
    assert_eq!(child_author, writer, "the child is authored by the refiner");

    cleanup_synthesis(&pool, new_id).await;
    cleanup_synthesis(&pool, parent_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 30: GET clusters × public-stranger → 200
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn clusters_public_stranger_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "clusters public stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Public),
    )
    .await
    .expect("seed synthesis");

    let cluster = Cluster {
        id: Uuid::now_v7(),
        synthesis_id: id,
        cluster_index: 0,
        title: "public cluster".into(),
        summary: "public summary".into(),
        member_claim_ids: vec![Uuid::now_v7()],
        support_count: 1,
        contradict_count: 0,
    };
    SynthesisClustersRepository::insert(&pool, &cluster)
        .await
        .expect("insert cluster");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/clusters"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "stranger must read clusters of a public synthesis"
    );
    let body: Vec<serde_json::Value> = resp.json();
    assert_eq!(body.len(), 1, "one public cluster returned");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 31: GET clusters × shared-recipient → 200
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn clusters_shared_recipient_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let recipient_p = testdb::principal(&pool, "recipient").await;
    let recipient = recipient_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "clusters shared recipient test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");
    testdb::reown_to_team_with_reader(&pool, id, &owner_p, recipient).await;

    let cluster = Cluster {
        id: Uuid::now_v7(),
        synthesis_id: id,
        cluster_index: 0,
        title: "shared cluster".into(),
        summary: "shared summary".into(),
        member_claim_ids: vec![Uuid::now_v7()],
        support_count: 1,
        contradict_count: 0,
    };
    SynthesisClustersRepository::insert(&pool, &cluster)
        .await
        .expect("insert cluster");

    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/clusters"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "share recipient must read clusters"
    );
    let body: Vec<serde_json::Value> = resp.json();
    assert_eq!(body.len(), 1);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 32: GET snapshot × public-stranger → 200
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_public_stranger_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "snapshot public stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Public),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/snapshot"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "stranger must read public synthesis snapshot"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 33: GET snapshot × shared-recipient → 200
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_shared_recipient_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let recipient_p = testdb::principal(&pool, "recipient").await;
    let recipient = recipient_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "snapshot shared recipient test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");
    testdb::reown_to_team_with_reader(&pool, id, &owner_p, recipient).await;

    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/snapshot"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "share recipient must read snapshot"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 34: GET staleness × public-stranger → 200
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn staleness_public_stranger_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "staleness public stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Public),
    )
    .await
    .expect("seed synthesis");
    SynthesisStalenessRepository::record_event(
        &pool,
        id,
        "belief_drift",
        &[Uuid::now_v7()],
        Some(&serde_json::json!({"score": 0.31})),
    )
    .await
    .expect("seed staleness event");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/staleness"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "stranger must read public synthesis staleness"
    );
    let body: Vec<serde_json::Value> = resp.json();
    assert_eq!(body.len(), 1, "one staleness event returned");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 35: GET staleness × shared-recipient → 200
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn staleness_shared_recipient_reads() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let recipient_p = testdb::principal(&pool, "recipient").await;
    let recipient = recipient_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "staleness shared recipient test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");
    testdb::reown_to_team_with_reader(&pool, id, &owner_p, recipient).await;
    SynthesisStalenessRepository::record_event(
        &pool,
        id,
        "belief_drift",
        &[Uuid::now_v7()],
        Some(&serde_json::json!({"score": 0.27})),
    )
    .await
    .expect("seed staleness event");

    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/staleness"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "share recipient must read staleness"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 36: GET × shared-stranger-without-share-row → 404
//
// `Visibility::Group` is NOT readable by everyone — it only signals that
// the synthesis CAN be shared. A stranger without an explicit share row
// must still see 404 (the existence-hide semantics matter for `Shared`
// just as they do for `Private`).
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_shared_stranger_without_share_row_404() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "shared stranger no share row test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Group),
    )
    .await
    .expect("seed synthesis");
    // No SynthesisSharesRepository::grant call — stranger has no share row.

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::NOT_FOUND,
        "stranger w/o share row on Shared synthesis must see 404 (existence-hide)"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 37: DELETE × public-stranger → 403
//
// Public visibility lets a stranger READ but does NOT grant write/delete
// privilege. The existence-hide invariant doesn't apply once a stranger
// can already prove existence via GET, so 403 is the correct shape (403
// is also what owner-gated PATCH/DELETE returns elsewhere).
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_public_stranger_403() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;

    let owner_p = testdb::principal(&pool, "owner").await;

    let owner = owner_p.agent;
    let stranger_p = testdb::principal(&pool, "stranger").await;
    let stranger = stranger_p.agent;
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "delete public stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        episcience_core::Ownership::new(owner_p.personal_group, Visibility::Public),
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .delete(&format!("/api/v1/eln/syntheses/{id}"))
        .add_header(hn, hv)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::FORBIDDEN,
        "stranger must NOT be able to delete a public synthesis"
    );

    // Confirm row is still alive (status != deleted).
    let status: String = sqlx::query_scalar("SELECT status FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("fetch status");
    assert_ne!(status, "deleted", "row must not be soft-deleted");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 38: GET shares × public-stranger → 403
//
// Listing share rows is owner-only — even when a synthesis is public, a
// stranger who can READ the synthesis must NOT enumerate its share rows
// (they reveal recipient agent IDs, which are not public-by-default).
// ──────────────────────────────────────────────────────────────────────────────

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║ E1d — group ownership over REST                                          ║
// ╚══════════════════════════════════════════════════════════════════════════╝

/// Synthesis shares are retired: every share route answers 410 and writes
/// nothing. Kills: a share route left wired to the frozen table.
#[tokio::test]
async fn share_routes_are_gone() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let owner_p = testdb::principal(&pool, "owner").await;
    let other = testdb::principal(&pool, "other").await.agent;
    let id = testdb::pending_synthesis(&pool, &owner_p, Visibility::Group).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM synthesis_shares")
        .fetch_one(&pool)
        .await
        .unwrap();
    let token = mint_test_jwt(owner_p.agent);
    for resp in [
        server
            .post(&format!("/api/v1/eln/syntheses/{id}/shares"))
            .add_header(bearer(&token).0, bearer(&token).1)
            .json(&serde_json::json!({"shared_with_agent_id": other}))
            .await,
        server
            .get(&format!("/api/v1/eln/syntheses/{id}/shares"))
            .add_header(bearer(&token).0, bearer(&token).1)
            .await,
        server
            .delete(&format!("/api/v1/eln/syntheses/{id}/shares/{other}"))
            .add_header(bearer(&token).0, bearer(&token).1)
            .await,
    ] {
        assert_eq!(
            resp.status_code(),
            axum::http::StatusCode::GONE,
            "{}",
            resp.text()
        );
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM synthesis_shares")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    // `shared` as a visibility is retired too.
    let resp = server
        .post("/api/v1/eln/syntheses")
        .add_header(bearer(&token).0, bearer(&token).1)
        .json(&serde_json::json!({"query": "shared visibility", "visibility": "shared"}))
        .await;
    assert_eq!(resp.status_code(), axum::http::StatusCode::GONE);
    cleanup_synthesis(&pool, id).await;
}

/// T-W1a: a create naming an owner group the caller cannot WRITE (another
/// principal's personal group; a team where the caller is only a reader) is
/// refused 403 before anything is written; naming a writable team group
/// stores it. Kills: the requested group taken unchecked, or checked against
/// the read set.
#[tokio::test]
async fn create_naming_an_unwritable_owner_group_is_403_before_any_write() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let h1 = testdb::principal(&pool, "h1").await;
    let h2 = testdb::principal(&pool, "h2").await;
    let reader_team = testdb::team_group(&pool, &h1, &[(h2.agent, "reader")]).await;
    let writer_team = testdb::team_group(&pool, &h1, &[(h2.agent, "writer")]).await;
    let token = mint_test_jwt(h2.agent);
    for group in [h1.personal_group, reader_team] {
        let marker = format!("t-w1a-{}", Uuid::now_v7());
        let resp = server
            .post("/api/v1/eln/syntheses")
            .add_header(bearer(&token).0, bearer(&token).1)
            .json(&serde_json::json!({"query": marker, "owner_group_id": group}))
            .await;
        assert_eq!(
            resp.status_code(),
            axum::http::StatusCode::FORBIDDEN,
            "{}",
            resp.text()
        );
        let written: i64 = sqlx::query_scalar("SELECT count(*) FROM syntheses WHERE query = $1")
            .bind(&marker)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(written, 0, "nothing may be written before the refusal");
    }
    let resp = server
        .post("/api/v1/eln/syntheses")
        .add_header(bearer(&token).0, bearer(&token).1)
        .json(&serde_json::json!({"query": "t-w1a writable", "owner_group_id": writer_team}))
        .await;
    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "{}",
        resp.text()
    );
    let id: Uuid = resp.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (owner, author, principal): (Uuid, Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT s.owner_group_id, s.agent_id, j.principal_id \
           FROM syntheses s JOIN synthesis_jobs j ON j.id = s.id WHERE s.id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owner, writer_team);
    assert_eq!(author, h2.agent);
    assert_eq!(principal, Some(h2.agent), "the job acts as the caller");
    cleanup_synthesis(&pool, id).await;
}

/// T-U1 over REST (the seamless UX): H2, a WRITER in team T, lists, gets and
/// edits the `group(T)` synthesis H1 created; R, a READER in T, lists and
/// gets it but its edit is 403; H2 never sees H1's personal synthesis.
/// Kills: reads or edits keyed on authorship, or the edit check using the
/// read set.
#[tokio::test]
async fn a_team_writer_lists_gets_and_edits_a_team_synthesis() {
    let pool = connect().await;
    let server = build_test_server(pool.clone()).await;
    let h1 = testdb::principal(&pool, "h1").await;
    let h2 = testdb::principal(&pool, "h2").await;
    let r = testdb::principal(&pool, "reader").await;
    let t = testdb::team_group(&pool, &h1, &[(h2.agent, "writer"), (r.agent, "reader")]).await;
    let t1 = mint_test_jwt(h1.agent);
    let resp = server
        .post("/api/v1/eln/syntheses")
        .add_header(bearer(&t1).0, bearer(&t1).1)
        .json(&serde_json::json!({"query": "team synthesis", "owner_group_id": t}))
        .await;
    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::ACCEPTED,
        "{}",
        resp.text()
    );
    let team_id: Uuid = resp.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let personal = testdb::pending_synthesis(&pool, &h1, Visibility::Group).await;

    for who in [h2.agent, r.agent] {
        let tok = mint_test_jwt(who);
        let resp = server
            .get("/api/v1/eln/syntheses")
            .add_query_param("limit", "1000")
            .add_header(bearer(&tok).0, bearer(&tok).1)
            .await;
        assert_eq!(
            resp.status_code(),
            axum::http::StatusCode::OK,
            "{}",
            resp.text()
        );
        let listed: Vec<serde_json::Value> = resp.json();
        let ids: Vec<String> = listed
            .iter()
            .filter_map(|v| v["id"].as_str().map(str::to_string))
            .collect();
        assert!(ids.contains(&team_id.to_string()), "a team member lists it");
        assert!(
            !ids.contains(&personal.to_string()),
            "never H1's personal row"
        );
        let got = server
            .get(&format!("/api/v1/eln/syntheses/{team_id}"))
            .add_header(bearer(&tok).0, bearer(&tok).1)
            .await;
        assert_eq!(got.status_code(), axum::http::StatusCode::OK);
        let hidden = server
            .get(&format!("/api/v1/eln/syntheses/{personal}"))
            .add_header(bearer(&tok).0, bearer(&tok).1)
            .await;
        assert_eq!(hidden.status_code(), axum::http::StatusCode::NOT_FOUND);
    }

    let tr = mint_test_jwt(r.agent);
    let refused = server
        .delete(&format!("/api/v1/eln/syntheses/{team_id}"))
        .add_header(bearer(&tr).0, bearer(&tr).1)
        .await;
    assert_eq!(refused.status_code(), axum::http::StatusCode::FORBIDDEN);

    let t2 = mint_test_jwt(h2.agent);
    let edited = server
        .patch(&format!("/api/v1/eln/syntheses/{team_id}/visibility"))
        .add_header(bearer(&t2).0, bearer(&t2).1)
        .json(&serde_json::json!({"visibility": "public"}))
        .await;
    assert_eq!(
        edited.status_code(),
        axum::http::StatusCode::NO_CONTENT,
        "{}",
        edited.text()
    );
    let deleted = server
        .delete(&format!("/api/v1/eln/syntheses/{team_id}"))
        .add_header(bearer(&t2).0, bearer(&t2).1)
        .await;
    assert_eq!(deleted.status_code(), axum::http::StatusCode::NO_CONTENT);
    let (vis, status): (String, String) =
        sqlx::query_as("SELECT visibility, status FROM syntheses WHERE id = $1")
            .bind(team_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((vis.as_str(), status.as_str()), ("public", "deleted"));
    cleanup_synthesis(&pool, team_id).await;
    cleanup_synthesis(&pool, personal).await;
}
