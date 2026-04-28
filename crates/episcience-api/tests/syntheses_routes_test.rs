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

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_core::synthesis::{Cluster, Visibility};
use episcience_db::{
    SynthesisClustersRepository, SynthesisRepository, SynthesisSharesRepository,
    SynthesisStalenessRepository,
};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis";

/// JWT secret used by the bin (server.rs `DEV_JWT_SECRET`). The tests build
/// the router directly rather than spawning the bin, so we duplicate the
/// secret bytes here. If `EPIGRAPH_JWT_SECRET` is set in the test env, we
/// honour it (so the tests work in CI with a non-default secret too).
fn jwt_secret_bytes() -> Vec<u8> {
    std::env::var("EPIGRAPH_JWT_SECRET")
        .map(|s| s.into_bytes())
        .unwrap_or_else(|_| b"dev-only-insecure-secret-change-in-production".to_vec())
}

/// Mint an HS256 JWT for `agent_id`. Includes the fields the
/// [`episcience_api::middleware::EpiGraphClaims`] struct expects.
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

async fn connect() -> PgPool {
    PgPool::connect(DSN).await.expect("connect to epigraph_dev_synthesis")
}

/// Build a `TestServer` wrapping the full episcience-api router.
fn build_test_server(pool: PgPool) -> TestServer {
    let state = ElnState {
        pool,
        blob_dir: std::path::PathBuf::from("/tmp/episcience-test-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
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
    let server = build_test_server(pool.clone());

    let agent_id = Uuid::now_v7();
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
    let server = build_test_server(pool.clone());

    let agent_id = Uuid::now_v7();
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

    let synth_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM syntheses WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("count syntheses");
    assert_eq!(synth_count, 1, "exactly 1 row in syntheses");

    // synthesis_jobs is keyed by the same id (FK).
    let job_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM synthesis_jobs WHERE id = $1")
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
    let server = build_test_server(pool);

    let agent_id = Uuid::now_v7();
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
    let server = build_test_server(pool);

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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
        body["id"].as_str().and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(id),
        "body.id matches"
    );
    assert_eq!(body["query"].as_str(), Some("owner reads test"));
    assert_eq!(
        body["agent_id"].as_str().and_then(|s: &str| s.parse::<Uuid>().ok()),
        Some(owner),
    );
    assert_eq!(body["visibility"].as_str(), Some("private"));

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 6: GET — stranger gets 404 (NOT 403, to avoid existence leak)
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_synthesis_stranger_gets_404() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
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
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient, owner)
        .await
        .expect("grant share");

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
        body["id"].as_str().and_then(|s: &str| s.parse::<Uuid>().ok()),
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
            Visibility::Private,
        )
        .await
        .expect("seed synthesis");
    }

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get("/api/v1/eln/syntheses")
        .add_header(hn, hv)
        .await;

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
// Test 9: GET /syntheses — strangers do not see private syntheses
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_excludes_others_private_syntheses() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
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
        Visibility::Private,
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get("/api/v1/eln/syntheses")
        .add_header(hn, hv)
        .await;

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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
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
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient, owner)
        .await
        .expect("grant share");

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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
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
        Visibility::Private,
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

#[tokio::test]
async fn grant_owner_succeeds_201() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "grant owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .post(&format!("/api/v1/eln/syntheses/{id}/shares"))
        .add_header(hn, hv)
        .json(&serde_json::json!({"shared_with_agent_id": recipient}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::CREATED,
        "expected 201, body: {}",
        resp.text()
    );

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_shares WHERE synthesis_id = $1 AND shared_with_agent_id = $2",
    )
    .bind(id)
    .bind(recipient)
    .fetch_one(&pool)
    .await
    .expect("count shares");
    assert_eq!(count, 1);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 21: POST shares — non-owner 403
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn grant_non_owner_403() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let attacker = Uuid::now_v7();
    let target = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "grant non-owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Private,
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(attacker);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .post(&format!("/api/v1/eln/syntheses/{id}/shares"))
        .add_header(hn, hv)
        .json(&serde_json::json!({"shared_with_agent_id": target}))
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::FORBIDDEN);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 22: DELETE share — owner revokes → 204
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn revoke_owner_succeeds() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "revoke owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient, owner)
        .await
        .expect("grant share");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .delete(&format!(
            "/api/v1/eln/syntheses/{id}/shares/{recipient}"
        ))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NO_CONTENT);

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_shares WHERE synthesis_id = $1 AND shared_with_agent_id = $2",
    )
    .bind(id)
    .bind(recipient)
    .fetch_one(&pool)
    .await
    .expect("count shares");
    assert_eq!(count, 0, "share row removed");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 23: DELETE share — recipient revokes their own → 204
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn revoke_recipient_self_succeeds() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "revoke recipient self test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient, owner)
        .await
        .expect("grant share");

    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .delete(&format!(
            "/api/v1/eln/syntheses/{id}/shares/{recipient}"
        ))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::NO_CONTENT);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 24: DELETE share — stranger forbidden
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn revoke_stranger_403() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "revoke stranger test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient, owner)
        .await
        .expect("grant share");

    let token = mint_test_jwt(stranger);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .delete(&format!(
            "/api/v1/eln/syntheses/{id}/shares/{recipient}"
        ))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::FORBIDDEN);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 25: GET /syntheses/{id}/shares — owner lists
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_shares_owner_succeeds() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient_a = Uuid::now_v7();
    let recipient_b = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "list shares owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient_a, owner)
        .await
        .expect("grant a");
    SynthesisSharesRepository::grant(&pool, id, recipient_b, owner)
        .await
        .expect("grant b");

    let token = mint_test_jwt(owner);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/shares"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    assert_eq!(body.len(), 2, "two share rows returned");

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 26: GET shares — non-owner 403
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_shares_non_owner_403() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let recipient = Uuid::now_v7();
    let id = Uuid::now_v7();

    SynthesisRepository::create_pending(
        &pool,
        id,
        "list shares non-owner test",
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        Visibility::Shared,
    )
    .await
    .expect("seed synthesis");

    SynthesisSharesRepository::grant(&pool, id, recipient, owner)
        .await
        .expect("grant share");

    // Recipient can read the synthesis but cannot enumerate shares.
    let token = mint_test_jwt(recipient);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .get(&format!("/api/v1/eln/syntheses/{id}/shares"))
        .add_header(hn, hv)
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::FORBIDDEN);

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 27: PATCH visibility — owner switches private → public
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn patch_visibility_owner_succeeds_to_public() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
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
        Visibility::Private,
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
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let owner = Uuid::now_v7();
    let attacker = Uuid::now_v7();
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
        Visibility::Private,
    )
    .await
    .expect("seed synthesis");

    let token = mint_test_jwt(attacker);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .patch(&format!("/api/v1/eln/syntheses/{id}/visibility"))
        .add_header(hn, hv)
        .json(&serde_json::json!({"visibility": "public"}))
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::FORBIDDEN);

    cleanup_synthesis(&pool, id).await;
}
