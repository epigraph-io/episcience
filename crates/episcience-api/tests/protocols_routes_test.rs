//! Integration tests for the protocols REST routes (Phase 9: structured
//! section vocabulary).
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
//!     cargo test -p episcience-api --test protocols_routes_test
//!
//! These tests build the full router via `episcience_api::create_router`,
//! wrap it in `axum_test::TestServer`, and exercise `POST /protocols`
//! against the repo test DB. Each test seeds its own agent row (32-byte
//! random public_key) and cleans up after itself.

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

const PROTOCOL_WARNINGS_HEADER: &str = "x-episcience-protocol-warnings";

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
/// `protocols.authored_by` FK. Returns the new agent's UUID.
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
    .bind(format!("protocols-test-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    id
}

async fn cleanup_protocol(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM protocols WHERE id = $1")
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

fn base_protocol_body(authored_by: Uuid) -> serde_json::Value {
    serde_json::json!({
        "title": "Phase 9 sections protocol",
        "authored_by": authored_by,
        "steps": [
            {
                "order": 1,
                "instruction": "Step 1",
            }
        ],
        "equipment": ["pipette"],
        "labels": ["phase9"],
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: valid sections persist + serialize on response
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn protocol_with_valid_sections_persists_and_serializes() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let agent_id = seed_agent(&pool).await;
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let mut body = base_protocol_body(agent_id);
    body["sections"] = serde_json::json!({
        "overview": "Why we do this.",
        "planning": "Reagent prep + risk register.",
        "implementation": "Run the wet steps.",
        "interpretation": "Read the gel.",
        "validation": "Replicate at scale.",
    });

    let resp: TestResponse = server
        .post("/api/v1/eln/protocols")
        .add_header(hn, hv)
        .json(&body)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "expected 200 OK, body: {}",
        resp.text()
    );
    // No warning header expected on all-valid input.
    assert!(
        resp.headers().get(PROTOCOL_WARNINGS_HEADER).is_none(),
        "no warning header expected when all sections are vocabulary keys"
    );

    let resp_json: serde_json::Value = resp.json();
    let id: Uuid = resp_json["id"]
        .as_str()
        .expect("id in body")
        .parse()
        .unwrap();
    let sections = &resp_json["sections"];
    assert_eq!(sections["overview"], "Why we do this.");
    assert_eq!(sections["planning"], "Reagent prep + risk register.");
    assert_eq!(sections["implementation"], "Run the wet steps.");
    assert_eq!(sections["interpretation"], "Read the gel.");
    assert_eq!(sections["validation"], "Replicate at scale.");
    // `extras` skipped when empty.
    assert!(
        sections.get("extras").is_none(),
        "extras should be omitted when empty"
    );

    // Verify the DB row stores all five named sections.
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT sections FROM protocols WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch sections row");
    assert_eq!(stored["overview"], "Why we do this.");
    assert_eq!(stored["validation"], "Replicate at scale.");

    cleanup_protocol(&pool, id).await;
    cleanup_agent(&pool, agent_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: off-vocab keys produce warning header + land in extras
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn protocol_with_off_vocab_sections_emits_warning_header() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let agent_id = seed_agent(&pool).await;
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let mut body = base_protocol_body(agent_id);
    body["sections"] = serde_json::json!({
        "overview": "ok",
        "weird": "leftover",
    });

    let resp: TestResponse = server
        .post("/api/v1/eln/protocols")
        .add_header(hn, hv)
        .json(&body)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "expected 200 OK (off-vocab keys are non-fatal), body: {}",
        resp.text()
    );

    let warning = resp
        .headers()
        .get(PROTOCOL_WARNINGS_HEADER)
        .expect("warning header present on off-vocab keys")
        .to_str()
        .expect("warning header value is ascii")
        .to_string();
    assert!(
        warning.contains("weird"),
        "warning header should name `weird`, got: {warning}"
    );
    assert!(
        warning.starts_with("extras_dropped="),
        "warning header has expected prefix, got: {warning}"
    );

    let resp_json: serde_json::Value = resp.json();
    let id: Uuid = resp_json["id"].as_str().unwrap().parse().unwrap();
    let sections = &resp_json["sections"];
    assert_eq!(sections["overview"], "ok");
    assert_eq!(
        sections["extras"]["weird"], "leftover",
        "off-vocab key landed in extras"
    );

    // DB-side check: extras key is persisted in the JSONB column.
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT sections FROM protocols WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch sections row");
    assert_eq!(stored["extras"]["weird"], "leftover");

    cleanup_protocol(&pool, id).await;
    cleanup_agent(&pool, agent_id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: omitted sections → empty struct, no warning header
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn protocol_without_sections_defaults_to_empty() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());

    let agent_id = seed_agent(&pool).await;
    let token = mint_test_jwt(agent_id);
    let (hn, hv) = bearer(&token);

    let body = base_protocol_body(agent_id);
    // No `sections` key.

    let resp: TestResponse = server
        .post("/api/v1/eln/protocols")
        .add_header(hn, hv)
        .json(&body)
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "expected 200 OK with no sections, body: {}",
        resp.text()
    );
    assert!(
        resp.headers().get(PROTOCOL_WARNINGS_HEADER).is_none(),
        "no warning header expected when sections is omitted"
    );

    let resp_json: serde_json::Value = resp.json();
    let id: Uuid = resp_json["id"].as_str().unwrap().parse().unwrap();
    // All named fields are skipped in serialization when None; extras
    // skipped when empty. So `sections` on the wire is `{}`.
    assert_eq!(
        resp_json["sections"],
        serde_json::json!({}),
        "sections should serialize as empty object"
    );

    // DB row: empty JSONB object.
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT sections FROM protocols WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch sections row");
    assert_eq!(stored, serde_json::json!({}));

    cleanup_protocol(&pool, id).await;
    cleanup_agent(&pool, agent_id).await;
}
