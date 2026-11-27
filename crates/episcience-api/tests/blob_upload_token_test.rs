//! Out-of-band blob upload (backlog efc5549c) through the REAL REST router: a
//! short-lived token bound to (agent, sample, SHA-256, size, blob id); `PUT
//! /api/v1/eln/blobs/upload` with that token and the caller's bearer stores the
//! bytes only when every binding holds, and a replay returns the same blob.
//!
//! Cast: H1 and H2 are two agents, each with its own personal group. Every
//! refusal is paired with a success of the same request shape, so a refusal
//! cannot come from a bad body or route.
//!
//! Run under `scripts/e1-test-db.sh` (a fresh template clone per test).
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

#[path = "support/token.rs"]
mod token;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use epigraph_crypto::ContentHasher;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::auth::upload_token::{UploadGrant, UploadTokenKey, MAX_TOKEN_LIFETIME_SECS};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::path::Path;
use std::sync::Arc;
use testdb::{principal, team_group, Principal, TestDb};
use token::{jwt_secret_bytes, mint_test_jwt};
use uuid::Uuid;

const KEY: &[u8] = b"test-upload-token-key-0123456789abcdef";
const UPLOAD: &str = "/api/v1/eln/blobs/upload";

fn key() -> UploadTokenKey {
    UploadTokenKey::new(KEY).expect("a 38-byte key")
}

fn bearer(agent: Uuid) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", mint_test_jwt(agent))).expect("bearer"),
    )
}

fn eln_state(pool_db: episcience_db::tenancy::EpiscienceDb, blob_dir: &Path) -> ElnState {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    ElnState {
        db: pool_db,
        blob_dir: blob_dir.to_path_buf(),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    }
}

/// The REST router with upload tokens on (`key()`).
async fn rest(pool: &PgPool, blob_dir: &Path) -> TestServer {
    let state = eln_state(testdb::app_db_for(pool).await, blob_dir);
    TestServer::new(episcience_api::create_router_with_uploads(
        state,
        Some(key()),
    ))
    .expect("build TestServer")
}

/// A sample prepared by `by` in `group` with `visibility`.
async fn seed_sample(pool: &PgPool, by: Uuid, group: Uuid, visibility: &str) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("upload-token-sample-{id}");
    let hash = ContentHasher::hash(name.as_bytes());
    sqlx::query(
        r#"INSERT INTO samples (id, name, sample_type, prepared_by, content_hash,
                                owner_group_id, visibility)
           VALUES ($1, $2, 'biological', $3, $4, $5, $6)"#,
    )
    .bind(id)
    .bind(&name)
    .bind(by)
    .bind(&hash[..])
    .bind(group)
    .bind(visibility)
    .execute(pool)
    .await
    .expect("seed sample");
    id
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A grant for `bytes` by `agent` on `sample`, expiring `ttl` seconds from now.
fn grant(agent: Uuid, sample: Option<Uuid>, bytes: &[u8], ttl: i64) -> UploadGrant {
    UploadGrant {
        blob_id: Uuid::now_v7(),
        agent_id: agent,
        sample_id: sample,
        sha256: sha256_hex(bytes),
        size_bytes: bytes.len() as u64,
        exp: now() + ttl,
        filename: "trace.bin".into(),
        mime_type: "application/octet-stream".into(),
        labels: vec!["oob".into()],
        properties: json!({"run": 7}),
    }
}

async fn put(server: &TestServer, who: Uuid, token: &str, bytes: &[u8]) -> TestResponse {
    let (n, v) = bearer(who);
    server
        .put(UPLOAD)
        .add_header(n, v)
        .add_header(
            HeaderName::from_static("x-upload-token"),
            HeaderValue::from_str(token).expect("token header"),
        )
        .bytes(bytes.to_vec().into())
        .await
}

async fn rows_with_id(pool: &PgPool, id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM blobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count blobs")
}

async fn rows_by(pool: &PgPool, uploader: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM blobs WHERE uploader_id = $1")
        .bind(uploader)
        .fetch_one(pool)
        .await
        .expect("count blobs")
}

/// Every regular file under `dir`.
fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("read blob dir") {
            let p = e.expect("dir entry").path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn blob_path(dir: &Path, blake3_hex: &str) -> std::path::PathBuf {
    dir.join(&blake3_hex[0..2])
        .join(&blake3_hex[2..4])
        .join(format!("{blake3_hex}.blob"))
}

async fn setup() -> (TestDb, tempfile::TempDir, TestServer, Principal, Principal) {
    let db = TestDb::fresh().await;
    let blobs = tempfile::TempDir::new().expect("blob dir");
    let server = rest(&db.admin, blobs.path()).await;
    let h1 = principal(&db.admin, "h1").await;
    let h2 = principal(&db.admin, "h2").await;
    (db, blobs, server, h1, h2)
}

// A valid token and its bytes store the blob under the token's blob id, with
// the token's metadata, the BLAKE3 content hash and the bytes on disk; the
// same PUT again returns the same blob and adds no row.
// Kills: the route storing under a fresh id (replay makes a second row), the
// store failing on the replay's id conflict, metadata not taken from the grant.
#[tokio::test]
async fn valid_upload_stores_the_blob_and_a_replay_returns_the_same_blob() {
    let (db, blobs, server, h1, _h2) = setup().await;
    let sample = seed_sample(&db.admin, h1.agent, h1.personal_group, "public").await;
    let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 241) as u8).collect();
    let g = grant(h1.agent, Some(sample), &bytes, 300);
    let token = key().sign(&g).unwrap();

    let first = put(&server, h1.agent, &token, &bytes).await;
    assert_eq!(first.status_code(), StatusCode::OK, "{}", first.text());
    let b: Value = first.json();
    assert_eq!(b["id"], json!(g.blob_id));
    assert_eq!(b["uploader_id"], json!(h1.agent));
    assert_eq!(b["sample_id"], json!(sample));
    assert_eq!(b["filename"], json!("trace.bin"));
    assert_eq!(b["labels"], json!(["oob"]));
    assert_eq!(b["properties"], json!({"run": 7}));
    assert_eq!(b["size_bytes"], json!(bytes.len()));
    let blake3_hex = hex::encode(ContentHasher::hash(&bytes));
    assert_eq!(
        std::fs::read(blob_path(blobs.path(), &blake3_hex)).expect("blob file"),
        bytes
    );
    assert_eq!(rows_with_id(&db.admin, g.blob_id).await, 1);

    let replay = put(&server, h1.agent, &token, &bytes).await;
    assert_eq!(replay.status_code(), StatusCode::OK, "{}", replay.text());
    assert_eq!(replay.json::<Value>()["id"], json!(g.blob_id));
    assert_eq!(
        rows_by(&db.admin, h1.agent).await,
        1,
        "a replay adds no row"
    );
    assert_eq!(
        files_under(blobs.path()).len(),
        1,
        "one content-addressed file, no staging leftovers"
    );
}

// H1's token under H2's bearer is refused and stores nothing; the same token
// under H1's bearer then succeeds.
// Kills: the route not comparing the bearer's agent with the token's.
#[tokio::test]
async fn another_agents_bearer_is_refused() {
    let (db, _blobs, server, h1, h2) = setup().await;
    let bytes = b"bound to h1".to_vec();
    let g = grant(h1.agent, None, &bytes, 300);
    let token = key().sign(&g).unwrap();

    let r = put(&server, h2.agent, &token, &bytes).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(rows_with_id(&db.admin, g.blob_id).await, 0);
    assert_eq!(rows_by(&db.admin, h2.agent).await, 0);

    let ok = put(&server, h1.agent, &token, &bytes).await;
    assert_eq!(ok.status_code(), StatusCode::OK, "{}", ok.text());
}

// An expired token (and one living past the 10-minute cap) is refused.
// Kills: the expiry or the lifetime check removed from the route's verify.
#[tokio::test]
async fn expired_and_overlong_tokens_are_refused() {
    let (db, _blobs, server, h1, _h2) = setup().await;
    let bytes = b"late".to_vec();

    let expired = grant(h1.agent, None, &bytes, -1);
    let r = put(&server, h1.agent, &key().sign(&expired).unwrap(), &bytes).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(rows_with_id(&db.admin, expired.blob_id).await, 0);

    let overlong = grant(h1.agent, None, &bytes, MAX_TOKEN_LIFETIME_SECS + 120);
    let r = put(&server, h1.agent, &key().sign(&overlong).unwrap(), &bytes).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(rows_with_id(&db.admin, overlong.blob_id).await, 0);

    let fresh = grant(h1.agent, None, &bytes, 300);
    let r = put(&server, h1.agent, &key().sign(&fresh).unwrap(), &bytes).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
}

// A token whose grant was rewritten to declare other bytes (the MAC kept) is
// refused, even when the body matches the rewritten hash; so is a token
// signed with another key.
// Kills: the route accepting a grant without verifying its MAC.
#[tokio::test]
async fn a_tampered_or_foreign_token_is_refused() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let (db, _blobs, server, h1, _h2) = setup().await;
    let declared = b"declared bytes".to_vec();
    let other = b"other bytes!!!".to_vec();
    let g = grant(h1.agent, None, &declared, 300);
    let token = key().sign(&g).unwrap();
    let mut forged = g.clone();
    forged.sha256 = sha256_hex(&other);
    let parts: Vec<&str> = token.split('.').collect();
    let tampered = format!(
        "{}.{}.{}",
        parts[0],
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap()),
        parts[2]
    );
    let r = put(&server, h1.agent, &tampered, &other).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());

    let foreign = UploadTokenKey::new(b"another-key-entirely-0123456789abcdef")
        .unwrap()
        .sign(&forged)
        .unwrap();
    let r = put(&server, h1.agent, &foreign, &other).await;
    assert_eq!(r.status_code(), StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(rows_by(&db.admin, h1.agent).await, 0);
}

// A body whose SHA-256 is not the declared one is refused (409) and leaves
// no row and no file; a body of another size is refused (422). The declared
// bytes then succeed.
// Kills: the route skipping the SHA-256 comparison or the size comparison.
#[tokio::test]
async fn a_body_that_does_not_match_the_token_is_refused() {
    let (db, blobs, server, h1, _h2) = setup().await;
    let declared = b"the declared payload".to_vec();
    let swapped = b"the swapped payload!".to_vec();
    assert_eq!(declared.len(), swapped.len());
    let g = grant(h1.agent, None, &declared, 300);
    let token = key().sign(&g).unwrap();

    let r = put(&server, h1.agent, &token, &swapped).await;
    assert_eq!(r.status_code(), StatusCode::CONFLICT, "{}", r.text());
    assert_eq!(rows_with_id(&db.admin, g.blob_id).await, 0);
    assert!(
        files_under(blobs.path()).is_empty(),
        "{:?}",
        files_under(blobs.path())
    );

    let r = put(&server, h1.agent, &token, &declared[..declared.len() - 1]).await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    let mut longer = declared.clone();
    longer.push(b'!');
    let r = put(&server, h1.agent, &token, &longer).await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    assert_eq!(rows_with_id(&db.admin, g.blob_id).await, 0);

    let r = put(&server, h1.agent, &token, &declared).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
}

// Write access to the sample is checked again at upload: H1 writes team T
// when the token is issued, is demoted to reader before the PUT, and the
// PUT is refused (404, as for a sample it cannot write) with nothing stored.
// Kills: the route trusting the sample binding of the token without the
// recheck.
#[tokio::test]
async fn a_revoked_sample_write_is_refused_at_upload() {
    let (db, _blobs, server, h1, h2) = setup().await;
    let team = team_group(&db.admin, &h2, &[(h1.agent, "writer")]).await;
    let sample = seed_sample(&db.admin, h2.agent, team, "group").await;
    let bytes = b"team data".to_vec();

    // Control: while H1 writes T, a token for T's sample uploads.
    let ok = grant(h1.agent, Some(sample), &bytes, 300);
    let r = put(&server, h1.agent, &key().sign(&ok).unwrap(), &bytes).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());

    let g = grant(h1.agent, Some(sample), &bytes, 300);
    let token = key().sign(&g).unwrap();
    sqlx::query(
        "UPDATE group_memberships SET role = 'reader' WHERE group_id = $1 AND agent_id = $2",
    )
    .bind(team)
    .bind(h1.agent)
    .execute(&db.admin)
    .await
    .expect("demote H1 to reader of T");
    let r = put(&server, h1.agent, &token, &bytes).await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{}", r.text());
    assert_eq!(rows_with_id(&db.admin, g.blob_id).await, 0);
}

// Without a key the route refuses every upload (503), whatever the token.
// Kills: an unconfigured server accepting tokens.
#[tokio::test]
async fn an_unconfigured_server_refuses_uploads() {
    let db = TestDb::fresh().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let state = eln_state(testdb::app_db_for(&db.admin).await, blobs.path());
    let server = TestServer::new(episcience_api::create_router(state)).unwrap();
    let h1 = principal(&db.admin, "h1").await;
    let bytes = b"no key".to_vec();
    let g = grant(h1.agent, None, &bytes, 300);
    let r = put(&server, h1.agent, &key().sign(&g).unwrap(), &bytes).await;
    assert_eq!(
        r.status_code(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        r.text()
    );
    assert_eq!(rows_by(&db.admin, h1.agent).await, 0);
}
