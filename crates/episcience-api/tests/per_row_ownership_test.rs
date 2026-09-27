//! Per-row ownership on writes that name an existing row or an author
//! (batch E1a: T-A8, T-W14a, T-W14b; REST and MCP).
//!
//! Cast: H1 and H2 are two agents, each with its own valid read+write token.
//! H1 prepared the sample. Every refusal is paired with H1 doing the same
//! thing successfully, so a refusal cannot come from a bad body or route.
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::multipart::{MultipartForm, Part};
use axum_test::{TestResponse, TestServer};
use epigraph_crypto::ContentHasher;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint_test_jwt};

#[path = "support/mcp_http.rs"]
mod mcp_http;
use mcp_http::{bearer_auth, start_mcp, McpClient};

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test";

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
        .expect("connect (set DATABASE_URL to a migrated *_test database)")
}

fn rest_server(pool: PgPool, blob_dir: &std::path::Path) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        pool,
        blob_dir: blob_dir.to_path_buf(),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    };
    TestServer::new(episcience_api::create_router(state)).expect("build TestServer")
}

async fn seed_agent(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let pk: Vec<u8> = (0..32u8)
        .map(|i| (id.as_u128() >> (i % 16)) as u8 ^ i)
        .collect();
    sqlx::query(
        r#"INSERT INTO agents (id, public_key, display_name, agent_type, role, state)
           VALUES ($1, $2, $3, 'service', 'custom', 'active')"#,
    )
    .bind(id)
    .bind(&pk)
    .bind(format!("per-row-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    id
}

async fn seed_sample(pool: &PgPool, prepared_by: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("per-row-sample-{id}");
    let hash = ContentHasher::hash(name.as_bytes());
    sqlx::query(
        r#"INSERT INTO samples (id, name, sample_type, prepared_by, content_hash)
           VALUES ($1, $2, 'biological', $3, $4)"#,
    )
    .bind(id)
    .bind(&name)
    .bind(prepared_by)
    .bind(&hash[..])
    .execute(pool)
    .await
    .expect("seed sample");
    id
}

async fn sample_status(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM samples WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("sample status")
}

async fn observation_count(pool: &PgPool, sample: Uuid, content: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM sample_claims sc JOIN claims c ON c.id = sc.claim_id
          WHERE sc.sample_id = $1 AND c.content = $2",
    )
    .bind(sample)
    .bind(content)
    .fetch_one(pool)
    .await
    .expect("count observations")
}

async fn blob_count(pool: &PgPool, sample: Uuid, uploader: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM blobs WHERE sample_id = $1 AND uploader_id = $2")
        .bind(sample)
        .bind(uploader)
        .fetch_one(pool)
        .await
        .expect("count blobs")
}

async fn upload(server: &TestServer, token: &str, uploader: Uuid, sample: Uuid) -> TestResponse {
    let (n, v) = bearer(token);
    let form = MultipartForm::new()
        .add_part(
            "file",
            Part::bytes(format!("per-row payload {}", Uuid::now_v7()).into_bytes())
                .file_name("per-row.txt")
                .mime_type("text/plain"),
        )
        .add_text("uploader_id", uploader.to_string())
        .add_text("sample_id", sample.to_string());
    server
        .post("/api/v1/eln/blobs")
        .add_header(n, v)
        .multipart(form)
        .await
}

// T-A8. Kills: trusting the body's `authored_by` (B4).
#[tokio::test]
async fn protocol_author_is_the_caller_never_the_body() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path());
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let marker = format!("t-a8-{}", Uuid::now_v7());

    let (n, v) = bearer(&mint_test_jwt(h1));
    let resp = server
        .post("/api/v1/eln/protocols")
        .add_header(n, v)
        .json(&json!({"title": marker, "authored_by": h2, "steps": [{"order": 1, "instruction": "x"}]}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::FORBIDDEN);
    let n_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM protocols WHERE title = $1")
        .bind(&marker)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n_rows, 0, "a refused protocol must not be written");

    // Absent author: stored as the caller.
    let (n, v) = bearer(&mint_test_jwt(h1));
    let resp = server
        .post("/api/v1/eln/protocols")
        .add_header(n, v)
        .json(&json!({"title": marker, "steps": [{"order": 1, "instruction": "x"}]}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::OK);
    let stored: Uuid = sqlx::query_scalar("SELECT authored_by FROM protocols WHERE title = $1")
        .bind(&marker)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, h1);
}

// T-W14a. Kills: removing the owner check from `update_status` (B4).
#[tokio::test]
async fn only_the_preparer_changes_a_sample_status() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path());
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let sample = seed_sample(&pool, h1).await;
    let path = format!("/api/v1/eln/samples/{sample}/status");

    let (n, v) = bearer(&mint_test_jwt(h2));
    let resp = server
        .patch(&path)
        .add_header(n, v)
        .json(&json!({"status": "in_use"}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(
        sample_status(&pool, sample).await,
        "prepared",
        "row unchanged"
    );

    let (n, v) = bearer(&mint_test_jwt(h1));
    let resp = server
        .patch(&path)
        .add_header(n, v)
        .json(&json!({"status": "in_use"}))
        .await;
    assert_eq!(
        resp.status_code(),
        StatusCode::OK,
        "control: the preparer may"
    );
    assert_eq!(sample_status(&pool, sample).await, "in_use");
}

// T-W14b (REST). Kills: removing the sample-ownership check from the REST
// observation or blob routes (B4).
#[tokio::test]
async fn rest_observation_and_blob_need_an_owned_sample() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path());
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let sample = seed_sample(&pool, h1).await;
    let content = format!("t-w14b-{}", Uuid::now_v7());
    let obs_path = format!("/api/v1/eln/samples/{sample}/observations");

    // H2 writes as H2 onto H1's sample.
    let (n, v) = bearer(&mint_test_jwt(h2));
    let resp = server
        .post(&obs_path)
        .add_header(n, v)
        .json(&json!({"content": content, "agent_id": h2}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(observation_count(&pool, sample, &content).await, 0);

    let resp = upload(&server, &mint_test_jwt(h2), h2, sample).await;
    assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(blob_count(&pool, sample, h2).await, 0);

    // Control: H1 does both.
    let (n, v) = bearer(&mint_test_jwt(h1));
    let resp = server
        .post(&obs_path)
        .add_header(n, v)
        .json(&json!({"content": content, "agent_id": h1}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::OK);
    assert_eq!(observation_count(&pool, sample, &content).await, 1);
    let resp = upload(&server, &mint_test_jwt(h1), h1, sample).await;
    assert_eq!(resp.status_code(), StatusCode::OK);
    assert_eq!(blob_count(&pool, sample, h1).await, 1);
}

// T-W14b (MCP), over real HTTP. Kills: removing the sample-ownership check
// from the MCP `add_observation` or `attach_blob` tools (B4).
#[tokio::test]
async fn mcp_observation_and_blob_need_an_owned_sample() {
    let pool = connect().await;
    let blob_dir = tempfile::TempDir::new().unwrap();
    let addr = start_mcp(
        pool.clone(),
        blob_dir.path().to_path_buf(),
        bearer_auth(&jwt_secret_bytes()),
    )
    .await;
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let sample = seed_sample(&pool, h1).await;
    let content = format!("t-w14b-mcp-{}", Uuid::now_v7());
    let blob_args = |tag: &str| {
        json!({
            "file_bytes_base64": base64_of(&format!("mcp per-row {tag} {}", Uuid::now_v7())),
            "sample_id": sample,
        })
    };

    let mut h2c = McpClient::new(addr, Some(mint_test_jwt(h2)));
    assert_eq!(h2c.initialize().await, reqwest::StatusCode::OK);
    let reply = h2c
        .call_tool(
            "add_observation",
            json!({"sample_id": sample, "content": content}),
        )
        .await;
    assert!(
        reply.error_message().contains("not found"),
        "{}",
        reply.error_message()
    );
    assert_eq!(observation_count(&pool, sample, &content).await, 0);
    let reply = h2c.call_tool("attach_blob", blob_args("h2")).await;
    assert!(
        reply.error_message().contains("not found"),
        "{}",
        reply.error_message()
    );
    assert_eq!(blob_count(&pool, sample, h2).await, 0);

    // Control: H1 does both through the same server.
    let mut h1c = McpClient::new(addr, Some(mint_test_jwt(h1)));
    assert_eq!(h1c.initialize().await, reqwest::StatusCode::OK);
    h1c.call_tool(
        "add_observation",
        json!({"sample_id": sample, "content": content}),
    )
    .await
    .result();
    assert_eq!(observation_count(&pool, sample, &content).await, 1);
    h1c.call_tool("attach_blob", blob_args("h1")).await.result();
    assert_eq!(blob_count(&pool, sample, h1).await, 1);
}

fn base64_of(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}
