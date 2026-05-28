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

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test";

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

async fn connect() -> PgPool {
    let dsn = std::env::var("DATABASE_URL").unwrap_or_else(|_| DSN.to_string());
    PgPool::connect(&dsn)
        .await
        .expect("connect to epigraph_db_repo_test (set DATABASE_URL to override)")
}

fn build_test_server(pool: PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
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
    let server = build_test_server(pool.clone());

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
    let server = build_test_server(pool.clone());

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
