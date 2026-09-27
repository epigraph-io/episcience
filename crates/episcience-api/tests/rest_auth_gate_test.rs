//! REST token acceptance and principal requirement (batch E1a: T-A1, T-A2).
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.
//!
//! Every negative case is paired with a positive control built from the SAME
//! spec with one field changed, so a refusal cannot pass for an unrelated
//! reason (bad route, bad body, DB error).

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint, TokenSpec};

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

fn build_test_server(pool: PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        pool,
        blob_dir: std::env::temp_dir().join("episcience-rest-auth-gate-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    };
    let _ = std::fs::create_dir_all(&state.blob_dir);
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
    .bind(format!("rest-auth-gate-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    id
}

async fn get_samples_status(server: &TestServer, token: &str) -> StatusCode {
    let (name, value) = bearer(token);
    server
        .get("/api/v1/eln/samples")
        .add_header(name, value)
        .await
        .status_code()
}

// T-A1 (REST). Kills: dropping the iss pin, the aud pin, the exp check, the
// zero leeway, or the requirement that iss / aud be present at all.
#[tokio::test]
async fn rest_refuses_wrong_or_missing_iss_aud_and_expired_tokens() {
    let pool = connect().await;
    let server = build_test_server(pool);
    let agent = Uuid::now_v7();

    // Positive control: the unmodified spec is accepted on this route.
    let good = TokenSpec::valid(agent);
    assert_eq!(
        get_samples_status(&server, &mint(&good)).await,
        StatusCode::OK,
        "control: a valid kernel token must be accepted"
    );

    let cases: Vec<(&str, TokenSpec)> = vec![
        (
            "wrong aud",
            TokenSpec {
                aud: Some("episcience".into()),
                ..good.clone()
            },
        ),
        (
            "missing aud",
            TokenSpec {
                aud: None,
                ..good.clone()
            },
        ),
        (
            "wrong iss",
            TokenSpec {
                iss: Some("not-epigraph".into()),
                ..good.clone()
            },
        ),
        (
            "missing iss",
            TokenSpec {
                iss: None,
                ..good.clone()
            },
        ),
        // 5 s in the past: inside jsonwebtoken's default 60 s leeway, so this
        // case also kills a mutant that restores the default leeway.
        (
            "expired 5s ago",
            TokenSpec {
                exp_offset_secs: -5,
                ..good.clone()
            },
        ),
        (
            "wrong secret",
            TokenSpec {
                secret: b"some-other-secret-entirely-0123456789".to_vec(),
                ..good.clone()
            },
        ),
    ];
    for (label, spec) in cases {
        assert_eq!(
            get_samples_status(&server, &mint(&spec)).await,
            StatusCode::UNAUTHORIZED,
            "{label}: must be refused"
        );
    }
}

// T-A2. A valid token with no `agent_id` is refused with `principal_required`
// and writes nothing. `sub` is set to a REAL agent so that a `sub` fallback
// (the removed `unwrap_or(claims.sub)`) would succeed and write a row as that
// agent: the mutant turns this test red.
#[tokio::test]
async fn rest_refuses_principal_less_token_and_writes_nothing() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());
    let real_agent = seed_agent(&pool).await;
    let marker = format!("e1a-principal-less-{}", Uuid::now_v7());

    let principal_less = mint(&TokenSpec {
        sub: Some(real_agent),
        agent_id: None,
        ..TokenSpec::valid(real_agent)
    });

    // A protocol body that is valid for `real_agent`.
    let (name, value) = bearer(&principal_less);
    let resp = server
        .post("/api/v1/eln/protocols")
        .add_header(name, value)
        .json(&serde_json::json!({
            "title": marker,
            "authored_by": real_agent,
            "steps": [{"order": 1, "instruction": "x"}],
        }))
        .await;
    assert_eq!(resp.status_code(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("principal_required"),
        "body must name principal_required, got {body}"
    );

    // A synthesis request with the same marker.
    let (name, value) = bearer(&principal_less);
    let resp = server
        .post("/api/v1/eln/syntheses")
        .add_header(name, value)
        .json(&serde_json::json!({ "query": marker }))
        .await;
    assert_eq!(resp.status_code(), StatusCode::UNAUTHORIZED);

    let protocols: i64 = sqlx::query_scalar("SELECT count(*) FROM protocols WHERE title = $1")
        .bind(&marker)
        .fetch_one(&pool)
        .await
        .expect("count protocols");
    let syntheses: i64 = sqlx::query_scalar("SELECT count(*) FROM syntheses WHERE query = $1")
        .bind(&marker)
        .fetch_one(&pool)
        .await
        .expect("count syntheses");
    assert_eq!((protocols, syntheses), (0, 0), "no row may be written");

    // Positive control: the same agent WITH agent_id writes the protocol.
    let (name, value) = bearer(&token::mint_test_jwt(real_agent));
    let resp = server
        .post("/api/v1/eln/protocols")
        .add_header(name, value)
        .json(&serde_json::json!({
            "title": marker,
            "authored_by": real_agent,
            "steps": [{"order": 1, "instruction": "x"}],
        }))
        .await;
    assert_eq!(resp.status_code(), StatusCode::OK, "control must write");
    sqlx::query("DELETE FROM protocols WHERE title = $1")
        .bind(&marker)
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM agents WHERE id = $1")
        .bind(real_agent)
        .execute(&pool)
        .await
        .ok();
}
