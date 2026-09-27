//! REST scope gate (batch E1a: T-A3, REST half).
//!
//! A `claims:read`-only token is refused with 403 `insufficient_scope` on every
//! write route; the SAME request with a read+write token is not refused by the
//! gate (positive control, so a 403 cannot come from a handler's own check or
//! a bad body). A `claims:write`-only token is refused on reads.
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.

use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_crypto::{AgentSigner, ContentHasher};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
#[path = "support/write_routes.rs"]
mod write_routes;
use token::{jwt_secret_bytes, mint, mint_test_jwt, read_only_jwt, TokenSpec, CLAIMS_WRITE};
use write_routes::{bearer, send, write_routes};

async fn connect() -> PgPool {
    // No default DSN: a stray run without the gate env must fail, not reach
    // whatever database listens on a default port.
    let dsn = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must name a migrated throwaway *_test database (no default)");
    PgPool::connect(&dsn)
        .await
        .expect("connect (set DATABASE_URL to a migrated *_test database)")
}

fn build_test_server(pool: PgPool, blob_dir: &std::path::Path) -> TestServer {
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

async fn seed_agent(pool: &PgPool, public_key: &[u8]) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agents (id, public_key, display_name, agent_type, role, state)
           VALUES ($1, $2, $3, 'service', 'custom', 'active')"#,
    )
    .bind(id)
    .bind(public_key)
    .bind(format!("rest-scope-gate-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    id
}

async fn seed_sample(pool: &PgPool, prepared_by: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("rest-scope-gate-sample-{id}");
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

// T-A3 (REST). Kills: removing the scope check from `bearer_auth_middleware`,
// or mapping any write method to `claims:read`.
#[tokio::test]
async fn read_only_token_is_refused_on_every_write_route() {
    let pool = connect().await;
    let blob_dir = tempfile::TempDir::new().expect("blob dir");
    let server = build_test_server(pool.clone(), blob_dir.path());
    let signer = AgentSigner::generate();
    let agent = seed_agent(&pool, &signer.public_key()).await;
    let rw = mint_test_jwt(agent);
    let ro = read_only_jwt(agent);

    // Fixtures owned by `agent`, created with the read+write token.
    let sample = seed_sample(&pool, agent).await;
    let (n, v) = bearer(&rw);
    let created = server
        .post("/api/v1/eln/syntheses")
        .add_header(n, v)
        .json(&serde_json::json!({"query": format!("scope gate {agent}")}))
        .await;
    assert_eq!(created.status_code(), StatusCode::ACCEPTED);
    let synthesis: Uuid = created.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let writes = write_routes(agent, sample, synthesis, &hex::encode(signer.public_key()));

    for (label, w) in &writes {
        let resp = send(&server, w, &ro).await;
        assert_eq!(
            resp.status_code(),
            StatusCode::FORBIDDEN,
            "{label}: a claims:read-only token must be refused"
        );
        let body: serde_json::Value = resp.json();
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("insufficient_scope"),
            "{label}: refusal must come from the scope gate, got {body}"
        );
    }

    // Positive control: the same requests with the read+write token are not
    // refused by the gate (401/403). Handler-level outcomes (2xx, 404 for the
    // unknown claim, 422 for the zero signature) are fine here.
    for (label, w) in &writes {
        let status = send(&server, w, &rw).await.status_code();
        assert!(
            status != StatusCode::FORBIDDEN && status != StatusCode::UNAUTHORIZED,
            "{label}: control with claims:write must pass the gate, got {status}"
        );
    }
}

// Reads need `claims:read`; `claims:write` alone does not imply it.
// Kills: skipping the gate for GET, or treating write as a superset of read.
#[tokio::test]
async fn write_only_token_is_refused_on_reads_and_read_token_is_accepted() {
    let pool = connect().await;
    let blob_dir = tempfile::TempDir::new().expect("blob dir");
    let server = build_test_server(pool.clone(), blob_dir.path());
    let agent = Uuid::now_v7();

    let write_only = mint(&TokenSpec {
        scopes: vec![CLAIMS_WRITE.to_string()],
        ..TokenSpec::valid(agent)
    });
    let (n, v) = bearer(&write_only);
    let resp = server.get("/api/v1/eln/samples").add_header(n, v).await;
    assert_eq!(resp.status_code(), StatusCode::FORBIDDEN);

    let (n, v) = bearer(&read_only_jwt(agent));
    let resp = server.get("/api/v1/eln/samples").add_header(n, v).await;
    assert_eq!(resp.status_code(), StatusCode::OK);
}
