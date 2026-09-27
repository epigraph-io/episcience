//! REST scope gate (batch E1a: T-A3, REST half).
//!
//! A `claims:read`-only token is refused with 403 `insufficient_scope` on every
//! write route; the SAME request with a read+write token is not refused by the
//! gate (positive control, so a 403 cannot come from a handler's own check or
//! a bad body). A `claims:write`-only token is refused on reads.
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::multipart::{MultipartForm, Part};
use axum_test::{TestResponse, TestServer};
use epigraph_crypto::{AgentSigner, ContentHasher};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint, mint_test_jwt, read_only_jwt, TokenSpec, CLAIMS_WRITE};

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

/// One write request, re-buildable so it can be sent with two tokens.
#[derive(Clone)]
enum Write {
    Json(&'static str, String, serde_json::Value),
    Blob(Uuid),
}

async fn send(server: &TestServer, w: &Write, token: &str) -> TestResponse {
    let (name, value) = bearer(token);
    match w {
        Write::Json(method, path, body) => {
            let req = match *method {
                "POST" => server.post(path),
                "PATCH" => server.patch(path),
                "DELETE" => server.delete(path),
                other => panic!("unexpected method {other}"),
            };
            req.add_header(name, value).json(body).await
        }
        Write::Blob(uploader) => {
            let form = MultipartForm::new()
                .add_part(
                    "file",
                    Part::bytes(b"scope gate payload".to_vec())
                        .file_name("gate.txt")
                        .mime_type("text/plain"),
                )
                .add_text("uploader_id", uploader.to_string());
            server
                .post("/api/v1/eln/blobs")
                .add_header(name, value)
                .multipart(form)
                .await
        }
    }
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

    let writes: Vec<(&str, Write)> = vec![
        (
            "create sample",
            Write::Json(
                "POST",
                "/api/v1/eln/samples".into(),
                serde_json::json!({"name": format!("gate-{agent}"), "sample_type": "biological", "prepared_by": agent}),
            ),
        ),
        (
            "sample status",
            Write::Json(
                "PATCH",
                format!("/api/v1/eln/samples/{sample}/status"),
                serde_json::json!({"status": "in_use"}),
            ),
        ),
        (
            "add observation",
            Write::Json(
                "POST",
                format!("/api/v1/eln/samples/{sample}/observations"),
                serde_json::json!({"content": "gate observation", "agent_id": agent}),
            ),
        ),
        (
            "create protocol",
            Write::Json(
                "POST",
                "/api/v1/eln/protocols".into(),
                serde_json::json!({"title": "gate", "authored_by": agent, "steps": [{"order": 1, "instruction": "x"}]}),
            ),
        ),
        ("upload blob", Write::Blob(agent)),
        (
            "create workflow run",
            Write::Json(
                "POST",
                "/api/v1/eln/workflow_runs".into(),
                serde_json::json!({"workflow_id": Uuid::now_v7(), "canonical_name": "gate", "prepared_by": agent, "started_at": chrono::Utc::now()}),
            ),
        ),
        (
            "create synthesis",
            Write::Json(
                "POST",
                "/api/v1/eln/syntheses".into(),
                serde_json::json!({"query": "gate"}),
            ),
        ),
        (
            "refine synthesis",
            Write::Json(
                "POST",
                format!("/api/v1/eln/syntheses/{synthesis}/refine"),
                serde_json::json!({}),
            ),
        ),
        (
            "grant share",
            Write::Json(
                "POST",
                format!("/api/v1/eln/syntheses/{synthesis}/shares"),
                serde_json::json!({"shared_with_agent_id": agent}),
            ),
        ),
        (
            "revoke share",
            Write::Json(
                "DELETE",
                format!("/api/v1/eln/syntheses/{synthesis}/shares/{agent}"),
                serde_json::json!({}),
            ),
        ),
        (
            "update visibility",
            Write::Json(
                "PATCH",
                format!("/api/v1/eln/syntheses/{synthesis}/visibility"),
                serde_json::json!({"visibility": "public"}),
            ),
        ),
        (
            "synthesis search (POST)",
            Write::Json(
                "POST",
                "/api/v1/eln/syntheses/search".into(),
                serde_json::json!({"query": "gate"}),
            ),
        ),
        (
            "countersign",
            Write::Json(
                "POST",
                "/api/v1/eln/countersign".into(),
                serde_json::json!({"claim_id": Uuid::now_v7(), "signer_id": agent, "signature_meaning": "approved", "signature_hex": "00".repeat(64), "public_key_hex": hex::encode(signer.public_key())}),
            ),
        ),
        (
            "delete synthesis",
            Write::Json(
                "DELETE",
                format!("/api/v1/eln/syntheses/{synthesis}"),
                serde_json::json!({}),
            ),
        ),
    ];

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
