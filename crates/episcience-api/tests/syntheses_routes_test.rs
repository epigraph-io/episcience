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
