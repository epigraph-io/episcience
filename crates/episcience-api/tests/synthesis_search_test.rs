//! Integration tests for Phase 3 Task 3.5: `POST /api/v1/eln/syntheses/search`.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//!     cargo test -p episcience-api --test synthesis_search_test
//!
//! Strategy: build the full router via `episcience_api::create_router`,
//! injecting a `MockProvider` embedder. Seeds embeddings by calling
//! `MockProvider::generate(text)` directly — so when the route asks the
//! same provider to embed the same query string, the resulting vector is
//! bit-identical and cosine = 1.0. The visibility predicate is exercised
//! end-to-end (owner / public / explicit share / stranger).

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::{TestResponse, TestServer};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_core::synthesis::Visibility;
use episcience_db::{
    SynthesisEmbeddingsRepository, SynthesisRepository, SynthesisSharesRepository,
};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis";

/// JWT secret matching the bin's `DEV_JWT_SECRET` fallback.
fn jwt_secret_bytes() -> Vec<u8> {
    std::env::var("EPIGRAPH_JWT_SECRET")
        .map(|s| s.into_bytes())
        .unwrap_or_else(|_| b"epigraph-dev-secret-change-in-production!!".to_vec())
}

/// Mint an HS256 JWT for `agent_id` matching the [`EpiGraphClaims`] schema.
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
    PgPool::connect(DSN)
        .await
        .expect("connect to epigraph_dev_synthesis")
}

/// Build a `(TestServer, MockProvider Arc)` pair so tests can use the same
/// embedder the router does to pre-seed deterministic synthesis embeddings.
fn build_test_server(pool: PgPool) -> (TestServer, Arc<MockProvider>) {
    let mock = Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let embedder: Arc<dyn EmbeddingService> = mock.clone();
    let state = ElnState {
        pool,
        blob_dir: std::path::PathBuf::from("/tmp/episcience-search-test-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    };
    let _ = std::fs::create_dir_all(&state.blob_dir);
    let app = episcience_api::create_router(state);
    let server = TestServer::new(app).expect("build TestServer");
    (server, mock)
}

/// Hard-delete a synthesis and its dependents.
async fn cleanup_synthesis(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM synthesis_shares WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM synthesis_embeddings WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
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

/// Seed a synthesis row + an embedding row keyed off the given text. The
/// embedding is produced by the provided `MockProvider` so it matches whatever
/// the route's embedder will produce for the same `text`.
async fn seed_synthesis_with_embedding(
    pool: &PgPool,
    mock: &MockProvider,
    id: Uuid,
    owner: Uuid,
    visibility: Visibility,
    text: &str,
) {
    SynthesisRepository::create_pending(
        pool,
        id,
        text,
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        visibility,
    )
    .await
    .expect("seed synthesis");

    let embedding = mock.generate(text).await.expect("mock embed text");
    SynthesisEmbeddingsRepository::upsert(
        pool,
        id,
        &embedding,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .expect("upsert embedding");
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: search returns hits across owner/public/shared visibility
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_returns_hits_for_visible_syntheses() {
    let pool = connect().await;
    let (server, mock) = build_test_server(pool.clone());

    let agent_x = Uuid::now_v7();
    let agent_y = Uuid::now_v7();
    let agent_z = Uuid::now_v7();

    let id_a = Uuid::now_v7();
    let id_b = Uuid::now_v7();
    let id_c = Uuid::now_v7();
    // Seeded but NOT visible to agent_x — must be excluded from hits.
    let id_unrelated = Uuid::now_v7();

    // Use a single query string so all four embeddings collapse to the same
    // deterministic vector; cosine = 1.0 across all rows. The visibility
    // predicate is what differentiates which appear in results.
    let query = "synthesis search visibility test query";

    // A: owned by agent_x (private)
    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id_a,
        agent_x,
        Visibility::Private,
        query,
    )
    .await;
    // B: public, owned by agent_y
    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id_b,
        agent_y,
        Visibility::Public,
        query,
    )
    .await;
    // C: private, owned by agent_z, but shared to agent_x
    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id_c,
        agent_z,
        Visibility::Shared,
        query,
    )
    .await;
    SynthesisSharesRepository::grant(&pool, id_c, agent_x, agent_z)
        .await
        .expect("grant share to agent_x");

    // Unrelated: private, owned by agent_y, NOT shared with agent_x
    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id_unrelated,
        agent_y,
        Visibility::Private,
        query,
    )
    .await;

    let token = mint_test_jwt(agent_x);
    let (hn, hv) = bearer(&token);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses/search")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "query": query,
            "limit": 50,
        }))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::OK,
        "expected 200, body: {}",
        resp.text()
    );

    let body: Vec<serde_json::Value> = resp.json();
    let returned: Vec<Uuid> = body
        .iter()
        .filter_map(|v| {
            v.get("synthesis_id")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok())
        })
        .collect();

    assert!(
        returned.contains(&id_a),
        "owner sees their own private synthesis A (returned: {returned:?})"
    );
    assert!(
        returned.contains(&id_b),
        "everyone sees public synthesis B (returned: {returned:?})"
    );
    assert!(
        returned.contains(&id_c),
        "explicit share recipient sees C (returned: {returned:?})"
    );
    assert!(
        !returned.contains(&id_unrelated),
        "must NOT see strangers' private synthesis (returned: {returned:?})"
    );

    // Each visible hit's score should be ~1.0 (same embedding for query and
    // seed). Use a generous floor to absorb any pgvector float drift.
    for hit in &body {
        let score = hit["score"].as_f64().expect("score is f64");
        let id: Uuid = hit["synthesis_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        if [id_a, id_b, id_c].contains(&id) {
            assert!(
                score > 0.99,
                "exact-match score should be ~1.0, got {score} for {id}"
            );
        }
    }

    cleanup_synthesis(&pool, id_a).await;
    cleanup_synthesis(&pool, id_b).await;
    cleanup_synthesis(&pool, id_c).await;
    cleanup_synthesis(&pool, id_unrelated).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: stranger's private synthesis is excluded from search
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_excludes_strangers_private_syntheses() {
    let pool = connect().await;
    let (server, mock) = build_test_server(pool.clone());

    let agent_x = Uuid::now_v7();
    let agent_y = Uuid::now_v7();
    let id = Uuid::now_v7();
    let query = "stranger private synthesis test";

    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id,
        agent_y,
        Visibility::Private,
        query,
    )
    .await;

    let token = mint_test_jwt(agent_x);
    let (hn, hv) = bearer(&token);
    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses/search")
        .add_header(hn, hv)
        .json(&serde_json::json!({
            "query": query,
            "limit": 50,
        }))
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned: Vec<Uuid> = body
        .iter()
        .filter_map(|v| {
            v.get("synthesis_id")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok())
        })
        .collect();

    assert!(
        !returned.contains(&id),
        "stranger's private synthesis must not appear (returned: {returned:?})"
    );

    cleanup_synthesis(&pool, id).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: empty query → 422
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_empty_query_422() {
    let pool = connect().await;
    let (server, _mock) = build_test_server(pool);

    let agent_x = Uuid::now_v7();
    let token = mint_test_jwt(agent_x);
    let (hn, hv) = bearer(&token);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses/search")
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
// Test 3b (Task 4.6): search excludes stale syntheses by default
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_excludes_stale_by_default() {
    let pool = connect().await;
    let (server, mock) = build_test_server(pool.clone());

    let agent_x = Uuid::now_v7();
    let id_fresh = Uuid::now_v7();
    let id_stale = Uuid::now_v7();
    let query = "search stale exclusion test";

    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id_fresh,
        agent_x,
        Visibility::Private,
        query,
    )
    .await;
    seed_synthesis_with_embedding(
        &pool,
        &mock,
        id_stale,
        agent_x,
        Visibility::Private,
        query,
    )
    .await;
    SynthesisRepository::mark_stale(&pool, id_stale, "belief_drift")
        .await
        .expect("mark_stale");

    let token = mint_test_jwt(agent_x);
    let (hn, hv) = bearer(&token);
    // Default request body: no include_stale → stale rows hidden.
    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses/search")
        .add_header(hn, hv)
        .json(&serde_json::json!({"query": query, "limit": 50}))
        .await;

    assert_eq!(resp.status_code(), axum::http::StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json();
    let returned: Vec<Uuid> = body
        .iter()
        .filter_map(|v| {
            v.get("synthesis_id")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse().ok())
        })
        .collect();

    assert!(
        returned.contains(&id_fresh),
        "fresh synthesis must appear in default search (returned: {returned:?})"
    );
    assert!(
        !returned.contains(&id_stale),
        "stale synthesis must NOT appear in default search (returned: {returned:?})"
    );

    cleanup_synthesis(&pool, id_fresh).await;
    cleanup_synthesis(&pool, id_stale).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 4: missing Authorization → 401
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn search_no_auth_401() {
    let pool = connect().await;
    let (server, _mock) = build_test_server(pool);

    let resp: TestResponse = server
        .post("/api/v1/eln/syntheses/search")
        .json(&serde_json::json!({"query": "anything"}))
        .await;

    assert_eq!(
        resp.status_code(),
        axum::http::StatusCode::UNAUTHORIZED,
        "expected 401, body: {}",
        resp.text()
    );
}
