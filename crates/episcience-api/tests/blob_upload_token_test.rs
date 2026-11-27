//! Out-of-band blob upload (backlog efc5549c, 4a1abad4) through the REAL REST
//! router and the MCP tool: `attach_blob` in upload mode issues a short-lived
//! token bound to (agent, sample, SHA-256, size, blob id); `PUT
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
use episcience_api::mcp::blobs::AttachBlobArgs;
use episcience_api::mcp::EpiscienceServer;
use episcience_api::middleware::{AuthContext, JwtConfig};
use episcience_api::state::ElnState;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Extensions};
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

/// The MCP server, sharing the blob directory and the key.
async fn mcp(pool: &PgPool, blob_dir: &Path, with_key: bool) -> EpiscienceServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    EpiscienceServer::new(
        testdb::app_db_for(pool).await,
        embedder,
        blob_dir.to_path_buf(),
        1024 * 1024,
    )
    .with_upload_tokens(
        with_key.then(key),
        Some("https://eln.example.test".to_string()),
    )
}

async fn as_caller(server: &EpiscienceServer, agent: Uuid) -> Extensions {
    let mut ext = Extensions::new();
    server
        .attach_caller(
            &mut ext,
            AuthContext {
                agent_id: agent,
                client_id: Uuid::new_v4(),
                owner_id: None,
                client_type: "human".to_string(),
                scopes: vec!["claims:read".to_string(), "claims:write".to_string()],
            },
        )
        .await
        .expect("the caller resolves");
    ext
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

fn body_json(result: &CallToolResult) -> Value {
    match result.content.first().expect("tool result content") {
        ContentBlock::Text(t) => serde_json::from_str(&t.text).expect("body is JSON"),
        other => panic!("unexpected content variant: {other:?}"),
    }
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

// The body is capped by `max_upload_bytes`, not by axum's 2 MB default body
// limit: under a 4 MiB cap a 3 MiB upload is stored whole under the token's
// id, and a token declaring one byte past the cap is refused (422) before
// the body is read, leaving no row and no staging file.
// Kills: a request body limit at axum's default on the route (the 3 MiB
// upload would be refused), the route not holding the declared size to
// `max_upload_bytes` (the over-cap upload would be stored).
#[tokio::test]
async fn the_size_cap_is_max_upload_bytes_not_axums_default() {
    let db = TestDb::fresh().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let mut state = eln_state(testdb::app_db_for(&db.admin).await, blobs.path());
    state.max_upload_bytes = 4 * 1024 * 1024;
    let server = TestServer::new(episcience_api::create_router_with_uploads(
        state,
        Some(key()),
    ))
    .unwrap();
    let h1 = principal(&db.admin, "h1").await;

    let big: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    let g = grant(h1.agent, None, &big, 300);
    let r = put(&server, h1.agent, &key().sign(&g).unwrap(), &big).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Value>()["id"], json!(g.blob_id));
    let blake3_hex = hex::encode(ContentHasher::hash(&big));
    assert_eq!(
        std::fs::read(blob_path(blobs.path(), &blake3_hex)).unwrap(),
        big
    );

    let over = vec![9u8; 4 * 1024 * 1024 + 1];
    let g = grant(h1.agent, None, &over, 300);
    let r = put(&server, h1.agent, &key().sign(&g).unwrap(), &over).await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    assert_eq!(rows_with_id(&db.admin, g.blob_id).await, 0);
    assert_eq!(
        files_under(blobs.path()),
        vec![blob_path(blobs.path(), &blake3_hex)],
        "only the stored blob, no staging leftovers"
    );
}

// ─── MCP attach_blob, upload mode ───────────────────────────────────────────

fn upload_args(bytes: &[u8], sample: Option<Uuid>) -> AttachBlobArgs {
    AttachBlobArgs {
        content_hash: Some(sha256_hex(bytes).to_uppercase()),
        size_bytes: Some(bytes.len() as u64),
        filename: Some("spectrum.csv".into()),
        mime_type: Some("text/csv".into()),
        sample_id: sample,
        labels: vec!["spectra".into(), "raw".into()],
        properties: json!({"instrument": "x1"}),
        ..Default::default()
    }
}

// The two-step flow (attach_blob without bytes, then the PUT with its token)
// yields the same blob as the inline path for identical bytes: same BLAKE3
// content hash, size, filename, MIME type, sample, labels, properties,
// uploader, owner group and visibility, and the same stored bytes; the
// PUT's blob has the id the tool announced.
// Kills: upload mode dropping or altering any metadata the inline path
// keeps, or the tool announcing an id the PUT does not use.
#[tokio::test]
async fn two_step_upload_yields_the_inline_blob() {
    let (db, blobs, server, h1, _h2) = setup().await;
    let sample = seed_sample(&db.admin, h1.agent, h1.personal_group, "public").await;
    let tool = mcp(&db.admin, blobs.path(), true).await;
    let caller = as_caller(&tool, h1.agent).await;
    let bytes: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 251) as u8).collect();

    let inline = tool
        .attach_blob(
            Parameters(AttachBlobArgs {
                file_bytes_base64: Some({
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                }),
                content_hash: None,
                size_bytes: None,
                ..upload_args(&bytes, Some(sample))
            }),
            caller.clone(),
        )
        .await
        .expect("inline attach_blob");
    let inline_id: Uuid = body_json(&inline)["id"].as_str().unwrap().parse().unwrap();

    let issued = tool
        .attach_blob(
            Parameters(upload_args(&bytes, Some(sample))),
            caller.clone(),
        )
        .await
        .expect("upload-mode attach_blob");
    let issued = body_json(&issued);
    assert_eq!(issued["method"], json!("PUT"));
    assert_eq!(
        issued["upload_url"],
        json!("https://eln.example.test/api/v1/eln/blobs/upload")
    );
    assert_eq!(issued["token_header"], json!("X-Upload-Token"));
    assert_eq!(issued["sha256"], json!(sha256_hex(&bytes)));
    let blob_id: Uuid = issued["blob_id"].as_str().unwrap().parse().unwrap();
    let expires: chrono::DateTime<chrono::Utc> =
        issued["expires_at"].as_str().unwrap().parse().unwrap();
    let left = (expires - chrono::Utc::now()).num_seconds();
    assert!(
        left > 0 && left <= MAX_TOKEN_LIFETIME_SECS,
        "expires in {left}s"
    );
    assert_eq!(
        rows_with_id(&db.admin, blob_id).await,
        0,
        "nothing stored yet"
    );

    let r = put(&server, h1.agent, issued["token"].as_str().unwrap(), &bytes).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Value>()["id"], json!(blob_id));

    type Row = (
        Vec<u8>,
        i64,
        String,
        String,
        Option<Uuid>,
        Vec<String>,
        Value,
        Uuid,
        Option<Uuid>,
        Option<String>,
    );
    let row = |id: Uuid| {
        let pool = db.admin.clone();
        async move {
            sqlx::query_as::<_, Row>(
                "SELECT content_hash, size_bytes, filename, mime_type, sample_id, labels, \
                        properties, uploader_id, owner_group_id, visibility::text \
                   FROM blobs WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("blob row")
        }
    };
    let a = row(inline_id).await;
    let b = row(blob_id).await;
    assert_eq!(
        a, b,
        "the two-step blob equals the inline blob but for its id"
    );
    assert_eq!(a.0, ContentHasher::hash(&bytes).to_vec());
    assert_eq!(
        std::fs::read(blob_path(blobs.path(), &hex::encode(&b.0))).unwrap(),
        bytes
    );
}

// Upload mode refuses a sample the caller cannot write (as the inline path
// does), issues nothing without a key, and refuses a call naming both modes
// or neither.
// Kills: upload mode issuing a token without the sample-write check; a
// server without a key issuing tokens.
#[tokio::test]
async fn upload_mode_refusals() {
    let (db, blobs, _server, h1, h2) = setup().await;
    let theirs = seed_sample(&db.admin, h2.agent, h2.personal_group, "public").await;
    let mine = seed_sample(&db.admin, h1.agent, h1.personal_group, "public").await;
    let tool = mcp(&db.admin, blobs.path(), true).await;
    let caller = as_caller(&tool, h1.agent).await;
    let bytes = b"refusals".to_vec();

    let r = tool
        .attach_blob(
            Parameters(upload_args(&bytes, Some(theirs))),
            caller.clone(),
        )
        .await;
    assert!(r.is_err(), "a sample H1 cannot write must be refused");
    let r = tool
        .attach_blob(Parameters(upload_args(&bytes, Some(mine))), caller.clone())
        .await;
    assert!(r.is_ok(), "control: H1's own sample is served");

    let both = AttachBlobArgs {
        file_bytes_base64: Some("aGVsbG8=".into()),
        ..upload_args(&bytes, None)
    };
    assert!(tool
        .attach_blob(Parameters(both), caller.clone())
        .await
        .is_err());
    assert!(tool
        .attach_blob(Parameters(AttachBlobArgs::default()), caller.clone())
        .await
        .is_err());
    let too_big = AttachBlobArgs {
        size_bytes: Some(1024 * 1024 + 1),
        ..upload_args(&bytes, None)
    };
    assert!(tool
        .attach_blob(Parameters(too_big), caller.clone())
        .await
        .is_err());

    let keyless = mcp(&db.admin, blobs.path(), false).await;
    let keyless_caller = as_caller(&keyless, h1.agent).await;
    assert!(keyless
        .attach_blob(Parameters(upload_args(&bytes, Some(mine))), keyless_caller)
        .await
        .is_err());
    assert_eq!(
        rows_by(&db.admin, h1.agent).await,
        0,
        "upload mode stores nothing"
    );
}
