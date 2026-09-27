//! Interim own-authored read filter on the two routes that read kernel
//! `claims` directly (batch E1a: T-R0).
//!
//! H1 and H2 each author one claim carrying the same unique search word and
//! the same unique label. Each caller must get exactly its own claim back from
//! full-text search, and the notebook export must cover exactly its own claim.
//! The export's `x-content-hash` header is the BLAKE3 over the exported
//! entries, so the test recomputes it from the expected rows instead of
//! parsing the PDF (whose text streams may be compressed).
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::TestServer;
use chrono::{DateTime, Utc};
use epigraph_crypto::ContentHasher;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint_test_jwt};

fn bearer(token: &str) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
    )
}

async fn connect() -> PgPool {
    // No default DSN: a stray run without the gate env must fail, not reach
    // whatever database listens on a default port.
    let dsn = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must name a migrated throwaway *_test database (no default)");
    PgPool::connect(&dsn)
        .await
        .expect("connect (set DATABASE_URL to a migrated *_test database)")
}

fn rest_server(pool: PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        pool,
        blob_dir: std::env::temp_dir().join("episcience-interim-read-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024,
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
    .bind(format!("interim-read-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    id
}

/// A letters-only word no English stemmer rewrites and no other test uses.
fn unique_word() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .map(|c| match c.to_digit(16) {
            Some(d) => (b'k' + d as u8) as char,
            None => c,
        })
        .collect()
}

struct SeededClaim {
    id: Uuid,
    content: String,
    truth_value: f64,
    created_at: DateTime<Utc>,
}

async fn seed_claim(pool: &PgPool, author: Uuid, word: &str, label: &str) -> SeededClaim {
    let id = Uuid::now_v7();
    let content = format!("interim filter observation {word} by {author}");
    let hash = ContentHasher::hash(content.as_bytes());
    let (truth_value, created_at): (f64, DateTime<Utc>) = sqlx::query_as(
        r#"INSERT INTO claims (id, content, content_hash, truth_value, agent_id, labels)
           VALUES ($1, $2, $3, 0.7, $4, ARRAY[$5])
           RETURNING truth_value, created_at"#,
    )
    .bind(id)
    .bind(&content)
    .bind(&hash[..])
    .bind(author)
    .bind(label)
    .fetch_one(pool)
    .await
    .expect("seed claim");
    SeededClaim {
        id,
        content,
        truth_value,
        created_at,
    }
}

/// The export route's integrity hash for exactly `entries`, in order.
fn export_hash(entries: &[&SeededClaim]) -> String {
    let mut input = String::new();
    for e in entries {
        input.push_str(&e.id.to_string());
        input.push_str(&e.content);
        input.push_str(&e.truth_value.to_string());
        input.push_str(&e.created_at.to_rfc3339());
    }
    hex::encode(ContentHasher::hash(input.as_bytes()))
}

// T-R0. Kills: removing the interim `agent_id = principal` filter from
// full-text search or from the notebook export.
#[tokio::test]
async fn fulltext_and_export_return_only_the_callers_claims() {
    let pool = connect().await;
    let server = rest_server(pool.clone());
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let word = unique_word();
    let label = format!("t-r0-{word}");
    let c1 = seed_claim(&pool, h1, &word, &label).await;
    let c2 = seed_claim(&pool, h2, &word, &label).await;
    let today = Utc::now().date_naive();

    for (me, mine, other) in [(h1, &c1, &c2), (h2, &c2, &c1)] {
        let token = mint_test_jwt(me);

        let (n, v) = bearer(&token);
        let resp = server
            .get("/api/v1/eln/search/fulltext")
            .add_query_param("q", &word)
            .add_header(n, v)
            .await;
        assert_eq!(resp.status_code(), StatusCode::OK);
        let hits: Vec<Uuid> = resp
            .json::<serde_json::Value>()
            .as_array()
            .expect("array")
            .iter()
            .map(|h| h["claim_id"].as_str().unwrap().parse().unwrap())
            .collect();
        assert_eq!(
            hits,
            vec![mine.id],
            "search must return only the caller's claim"
        );
        assert!(!hits.contains(&other.id));

        let (n, v) = bearer(&token);
        let resp = server
            .get("/api/v1/eln/export/notebook.pdf")
            .add_query_param("from", today.pred_opt().unwrap().to_string())
            .add_query_param("to", today.succ_opt().unwrap().to_string())
            .add_query_param("label", &label)
            .add_header(n, v)
            .await;
        assert_eq!(resp.status_code(), StatusCode::OK);
        let got = resp
            .headers()
            .get("x-content-hash")
            .expect("x-content-hash header")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            got,
            export_hash(&[mine]),
            "export must cover exactly the caller's claim"
        );
        assert_ne!(got, export_hash(&[&c1, &c2]));
    }
}
