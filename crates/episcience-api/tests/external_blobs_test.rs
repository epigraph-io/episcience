//! External blobs (backlog a0bb3631, migration 5043) through the REAL REST
//! router: register by URI and hash with no bytes, download returns the
//! location, a worker's in-place verification is recorded (`ok`, `mismatch`
//! with the observed hash kept, `unreachable`), and tenancy holds on every
//! write.
//!
//! Cast: H1 and H2 are two agents, each with its own personal group and a
//! read+write token; neither is a member of the other's group. Every refusal
//! is paired with H1 doing the same thing successfully, so a refusal cannot
//! come from a bad body or route.
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database
//! (`scripts/e1-test-db.sh <batch> -- cargo test --test external_blobs_test`).
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_DISPOSITION};
use axum::http::StatusCode;
use axum_test::multipart::{MultipartForm, Part};
use axum_test::{TestResponse, TestServer};
use epigraph_crypto::ContentHasher;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint_test_jwt};

fn bearer(agent: Uuid) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", mint_test_jwt(agent))).expect("bearer"),
    )
}

async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
}

async fn rest_server(pool: PgPool, blob_dir: &std::path::Path) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        db: testdb::app_db_for(&pool).await,
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
    .bind(format!("ext-blob-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    sqlx::query("SELECT public.epigraph_ensure_personal_group($1)")
        .bind(id)
        .execute(pool)
        .await
        .expect("provision the agent's personal group");
    id
}

/// A sample prepared by `by`, in its personal group, with `visibility`.
async fn seed_sample(pool: &PgPool, by: Uuid, visibility: &str) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("ext-blob-sample-{id}");
    let hash = ContentHasher::hash(name.as_bytes());
    sqlx::query(
        r#"INSERT INTO samples (id, name, sample_type, prepared_by, content_hash,
                                owner_group_id, visibility)
           VALUES ($1, $2, 'biological', $3, $4,
                   public.epigraph_ensure_personal_group($3), $5)"#,
    )
    .bind(id)
    .bind(&name)
    .bind(by)
    .bind(&hash[..])
    .bind(visibility)
    .execute(pool)
    .await
    .expect("seed sample");
    id
}

/// A 10 GB sha256 trajectory on a shared volume, as a filer would register it.
fn registration(sample: Option<Uuid>, hash: &[u8]) -> Value {
    json!({
        "filename": "traj.dcd",
        "mime_type": "application/octet-stream",
        "size_bytes": 10_737_418_240_i64,
        "content_hash": hex::encode(hash),
        "hash_algorithm": "sha256",
        "location_uri": format!("pvc://shared-volume/run-{}/traj.dcd", Uuid::now_v7()),
        "is_manifest": false,
        "sample_id": sample,
        "labels": ["md-trajectory"],
    })
}

async fn register(server: &TestServer, who: Uuid, body: &Value) -> TestResponse {
    let (n, v) = bearer(who);
    server
        .post("/api/v1/eln/blobs/external")
        .add_header(n, v)
        .json(body)
        .await
}

async fn post_verification(
    server: &TestServer,
    who: Uuid,
    blob: Uuid,
    body: Value,
) -> TestResponse {
    let (n, v) = bearer(who);
    server
        .post(&format!("/api/v1/eln/blobs/{blob}/verification"))
        .add_header(n, v)
        .json(&body)
        .await
}

fn id_of(resp: &TestResponse) -> Uuid {
    resp.json::<Value>()["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("uuid")
}

/// The verification record, read on the admin pool (not through the API
/// under test): `(verify_status, last_observed_hash, last_verified_by,
/// last_verified_at is set)`.
async fn verification_row(
    pool: &PgPool,
    blob: Uuid,
) -> (Option<String>, Option<Vec<u8>>, Option<Uuid>, bool) {
    sqlx::query_as(
        "SELECT verify_status, last_observed_hash, last_verified_by, last_verified_at IS NOT NULL \
           FROM blobs WHERE id = $1",
    )
    .bind(blob)
    .fetch_one(pool)
    .await
    .expect("verification row")
}

/// Every regular file under `dir` (the blob directory).
fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
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

// Registration writes one metadata row and no bytes; the download is the
// location (JSON, no attachment header); metadata shows storage external.
// No upload path is involved: the request is JSON.
// Kills: register_external writing `storage` as local, the download route
// returning bytes or an attachment for an external blob.
#[tokio::test]
async fn register_by_uri_writes_no_bytes_and_download_is_the_location() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path()).await;
    let h1 = seed_agent(&pool).await;
    let sample = seed_sample(&pool, h1, "public").await;
    let hash = [0x5au8; 32];
    let body = registration(Some(sample), &hash);

    let resp = register(&server, h1, &body).await;
    assert_eq!(resp.status_code(), StatusCode::OK, "{}", resp.text());
    let blob = id_of(&resp);
    let got: Value = resp.json();
    assert_eq!(got["storage"], "external");
    assert_eq!(got["location_uri"], body["location_uri"]);
    assert_eq!(got["hash_algorithm"], "sha256");
    assert_eq!(got["size_bytes"], 10_737_418_240_i64);
    assert_eq!(got["uploader_id"], h1.to_string());
    assert!(got["verify_status"].is_null());
    assert!(
        files_under(blobs.path()).is_empty(),
        "registration writes no bytes"
    );
    let (storage, uri, alg, stored_hash, sample_of): (String, String, String, Vec<u8>, Uuid) =
        sqlx::query_as(
            "SELECT storage, location_uri, hash_algorithm, content_hash, sample_id FROM blobs WHERE id = $1",
        )
        .bind(blob)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (
            storage.as_str(),
            uri.as_str(),
            alg.as_str(),
            stored_hash,
            sample_of
        ),
        (
            "external",
            body["location_uri"].as_str().unwrap(),
            "sha256",
            hash.to_vec(),
            sample
        )
    );

    let (n, v) = bearer(h1);
    let dl = server
        .get(&format!("/api/v1/eln/blobs/{blob}/download"))
        .add_header(n, v)
        .await;
    assert_eq!(dl.status_code(), StatusCode::OK);
    assert!(dl.headers().get(CONTENT_DISPOSITION).is_none());
    assert_eq!(dl.headers()["X-Blob-Storage"], "external");
    let loc: Value = dl.json();
    assert_eq!(loc["location_uri"], body["location_uri"]);
    assert_eq!(loc["content_hash"], hex::encode(hash));
    assert_eq!(loc["hash_algorithm"], "sha256");

    // (The by-sample listing is not exercised: on main,
    // GET /api/v1/eln/blobs/by-sample is routed to /blobs/:id and answers 400.)
    let (n, v) = bearer(h1);
    let meta = server
        .get(&format!("/api/v1/eln/blobs/{blob}"))
        .add_header(n, v)
        .await;
    assert_eq!(meta.status_code(), StatusCode::OK, "{}", meta.text());
    assert_eq!(meta.json::<Value>()["storage"], "external");
}

// A declared hash never reads the blob directory. H2 uploads a local blob on
// its GROUP sample (H1 cannot read it). H1 registers an external blob that
// declares the same BLAKE3 hash, then downloads and verifies its own blob:
// it must get its location and its recorded status, never H2's bytes nor a
// re-hash of H2's file. Kills: the storage guard removed from download (H2's
// bytes come back) or from GET verify (`integrity_ok: true` from H2's file).
#[tokio::test]
async fn a_declared_hash_never_reads_another_tenants_local_bytes() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path()).await;
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let private = seed_sample(&pool, h2, "group").await;
    let secret = format!("H2 private bytes {}", Uuid::now_v7()).into_bytes();

    let (n, v) = bearer(h2);
    let up = server
        .post("/api/v1/eln/blobs")
        .add_header(n, v)
        .multipart(
            MultipartForm::new()
                .add_part(
                    "file",
                    Part::bytes(secret.clone())
                        .file_name("secret.txt")
                        .mime_type("text/plain"),
                )
                .add_text("sample_id", private.to_string()),
        )
        .await;
    assert_eq!(up.status_code(), StatusCode::OK, "{}", up.text());
    let h2_blob = id_of(&up);
    let h2_hash = ContentHasher::hash(&secret);
    // Control: H1 cannot read H2's blob directly.
    let (n, v) = bearer(h1);
    let direct = server
        .get(&format!("/api/v1/eln/blobs/{h2_blob}/download"))
        .add_header(n, v)
        .await;
    assert_eq!(direct.status_code(), StatusCode::NOT_FOUND);

    let mut body = registration(None, &h2_hash);
    body["hash_algorithm"] = json!("blake3");
    let resp = register(&server, h1, &body).await;
    assert_eq!(resp.status_code(), StatusCode::OK, "{}", resp.text());
    let mine = id_of(&resp);

    let (n, v) = bearer(h1);
    let dl = server
        .get(&format!("/api/v1/eln/blobs/{mine}/download"))
        .add_header(n, v)
        .await;
    assert_eq!(dl.status_code(), StatusCode::OK);
    let bytes = dl.as_bytes().to_vec();
    assert!(
        !bytes.windows(secret.len()).any(|w| w == secret.as_slice()),
        "an external blob's download returned another tenant's local bytes"
    );
    assert_eq!(dl.json::<Value>()["location_uri"], body["location_uri"]);

    let (n, v) = bearer(h1);
    let ver: Value = server
        .get(&format!("/api/v1/eln/blobs/{mine}/verify"))
        .add_header(n, v)
        .await
        .json();
    assert!(
        ver["integrity_ok"].is_null(),
        "never verified: no answer from the local directory, got {ver}"
    );
    assert_eq!(ver["storage"], "external");
}

// The worker reports what it observed; the server decides against the row's
// own hash and records it: the correct hash gives `ok`, a different one gives
// `mismatch` with the observed hash kept (and `integrity_ok: false`), an
// unreadable store gives `unreachable`. Each step is read back on the admin
// pool. An observation computed with another algorithm is refused and
// changes nothing. Kills: the ok/mismatch comparison inverted or dropped, the
// observed hash not kept, the verifier not recorded, the algorithm check
// removed.
#[tokio::test]
async fn verification_records_ok_mismatch_and_unreachable() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path()).await;
    let h1 = seed_agent(&pool).await;
    let hash = [0x11u8; 32];
    let other = [0x22u8; 32];
    let blob = id_of(&register(&server, h1, &registration(None, &hash)).await);
    assert_eq!(
        verification_row(&pool, blob).await,
        (None, None, None, false)
    );

    let r = post_verification(
        &server,
        h1,
        blob,
        json!({"observed_hash": hex::encode(hash), "hash_algorithm": "sha256"}),
    )
    .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Value>()["verify_status"], "ok");
    assert_eq!(
        verification_row(&pool, blob).await,
        (Some("ok".into()), Some(hash.to_vec()), Some(h1), true)
    );

    let r = post_verification(
        &server,
        h1,
        blob,
        json!({"observed_hash": hex::encode(other), "hash_algorithm": "sha256"}),
    )
    .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Value>()["verify_status"], "mismatch");
    assert_eq!(
        verification_row(&pool, blob).await,
        (
            Some("mismatch".into()),
            Some(other.to_vec()),
            Some(h1),
            true
        ),
        "a mismatch is recorded with the hash the worker observed"
    );
    let (n, v) = bearer(h1);
    let ver: Value = server
        .get(&format!("/api/v1/eln/blobs/{blob}/verify"))
        .add_header(n, v)
        .await
        .json();
    assert_eq!(ver["integrity_ok"], false);
    assert_eq!(ver["verify_status"], "mismatch");

    // Another algorithm: refused, the mismatch record stays.
    let r = post_verification(
        &server,
        h1,
        blob,
        json!({"observed_hash": hex::encode(hash), "hash_algorithm": "blake3"}),
    )
    .await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    assert_eq!(
        verification_row(&pool, blob).await.0.as_deref(),
        Some("mismatch")
    );

    let r = post_verification(&server, h1, blob, json!({"unreachable": true})).await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    assert_eq!(
        verification_row(&pool, blob).await,
        (Some("unreachable".into()), None, Some(h1), true)
    );

    // Malformed observations: refused, nothing recorded over the last one.
    for bad in [
        json!({}),
        json!({"observed_hash": hex::encode(hash), "hash_algorithm": "sha256", "unreachable": true}),
        json!({"observed_hash": hex::encode(hash)}),
        json!({"observed_hash": "ab".repeat(16), "hash_algorithm": "sha256"}),
        json!({"observed_hash": hex::encode(hash), "hash_algorithm": "md5"}),
    ] {
        let r = post_verification(&server, h1, blob, bad.clone()).await;
        assert_eq!(r.status_code(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    assert_eq!(
        verification_row(&pool, blob).await.0.as_deref(),
        Some("unreachable")
    );
}

// Tenancy. H1's external blob is PUBLIC: H2 can read it, but recording a
// verification needs write access to its owner group, so H2 gets 404 and the
// record is unchanged (H1's own post, before and after, succeeds). H2 cannot
// register onto H1's sample (404, no row), and a GROUP external blob of H1
// is invisible to H2 (404 on metadata and download). Kills: the writable
// check removed from verification (H2's post would land) or from the
// sample lookup on registration.
#[tokio::test]
async fn verification_and_registration_need_write_access() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path()).await;
    let h1 = seed_agent(&pool).await;
    let h2 = seed_agent(&pool).await;
    let hash = [0x33u8; 32];
    let public_sample = seed_sample(&pool, h1, "public").await;
    let blob = id_of(&register(&server, h1, &registration(Some(public_sample), &hash)).await);

    let (n, v) = bearer(h2);
    let meta = server
        .get(&format!("/api/v1/eln/blobs/{blob}"))
        .add_header(n, v)
        .await;
    assert_eq!(
        meta.status_code(),
        StatusCode::OK,
        "a public blob is readable"
    );

    let r = post_verification(&server, h2, blob, json!({"unreachable": true})).await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{}", r.text());
    assert_eq!(
        verification_row(&pool, blob).await,
        (None, None, None, false)
    );
    let r = post_verification(
        &server,
        h1,
        blob,
        json!({"observed_hash": hex::encode(hash), "hash_algorithm": "sha256"}),
    )
    .await;
    assert_eq!(r.status_code(), StatusCode::OK, "{}", r.text());
    let r = post_verification(&server, h2, blob, json!({"unreachable": true})).await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(
        verification_row(&pool, blob).await.0.as_deref(),
        Some("ok"),
        "a non-writer's report never lands"
    );

    let body = registration(Some(public_sample), &hash);
    let r = register(&server, h2, &body).await;
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{}", r.text());
    let n_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs WHERE location_uri = $1")
        .bind(body["location_uri"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n_rows, 0, "a refused registration writes nothing");

    let mut as_h2 = registration(None, &hash);
    as_h2["uploader_id"] = json!(h2);
    let r = register(&server, h1, &as_h2).await;
    assert_eq!(
        r.status_code(),
        StatusCode::FORBIDDEN,
        "uploader_id must be the caller"
    );

    let private = seed_sample(&pool, h1, "group").await;
    let hidden = id_of(&register(&server, h1, &registration(Some(private), &hash)).await);
    for path in [
        format!("/api/v1/eln/blobs/{hidden}"),
        format!("/api/v1/eln/blobs/{hidden}/download"),
    ] {
        let (n, v) = bearer(h2);
        assert_eq!(
            server.get(&path).add_header(n, v).await.status_code(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

// A local blob is verified by the server from its own bytes: posting an
// observation for one is refused and records nothing. Invalid registrations
// are 422 and write no row. Kills: the storage check removed from the
// verification route (the UPDATE's own `storage = 'external'` filter would
// turn it into a 404, not a 422), and any one validation removed (the CHECKs
// would answer 500).
#[tokio::test]
async fn local_blobs_and_malformed_registrations_are_refused() {
    let pool = connect().await;
    let blobs = tempfile::TempDir::new().unwrap();
    let server = rest_server(pool.clone(), blobs.path()).await;
    let h1 = seed_agent(&pool).await;

    let (n, v) = bearer(h1);
    let up = server
        .post("/api/v1/eln/blobs")
        .add_header(n, v)
        .multipart(
            MultipartForm::new().add_part(
                "file",
                Part::bytes(format!("local {}", Uuid::now_v7()).into_bytes())
                    .file_name("l.txt")
                    .mime_type("text/plain"),
            ),
        )
        .await;
    assert_eq!(up.status_code(), StatusCode::OK, "{}", up.text());
    assert_eq!(up.json::<Value>()["storage"], "local");
    let local = id_of(&up);
    let r = post_verification(&server, h1, local, json!({"unreachable": true})).await;
    assert_eq!(
        r.status_code(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        r.text()
    );
    assert_eq!(
        verification_row(&pool, local).await,
        (None, None, None, false)
    );

    let good = registration(None, &[0x44u8; 32]);
    let cases: Vec<(&str, Value)> = vec![
        ("no scheme", json!("/data/traj.dcd")),
        ("whitespace", json!("pvc://shared/a b")),
        ("upper-case scheme", json!("PVC://shared/a")),
    ]
    .into_iter()
    .map(|(w, uri)| {
        let mut b = good.clone();
        b["location_uri"] = uri;
        (w, b)
    })
    .chain(
        [
            ("short hash", "content_hash", json!("ab".repeat(16))),
            ("non-hex hash", "content_hash", json!("zz".repeat(32))),
            ("md5", "hash_algorithm", json!("md5")),
            ("zero size", "size_bytes", json!(0)),
            ("negative size", "size_bytes", json!(-1)),
            ("blank filename", "filename", json!("  ")),
            ("long mime", "mime_type", json!("a".repeat(256))),
        ]
        .into_iter()
        .map(|(w, k, val)| {
            let mut b = good.clone();
            b[k] = val;
            (w, b)
        }),
    )
    .collect();
    for (what, body) in &cases {
        let r = register(&server, h1, body).await;
        assert_eq!(
            r.status_code(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{what}: {}",
            r.text()
        );
    }
    let n_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM blobs WHERE uploader_id = $1 AND storage = 'external'",
    )
    .bind(h1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n_rows, 0, "no refused registration wrote a row");
    // Control: the unmodified body registers.
    assert_eq!(
        register(&server, h1, &good).await.status_code(),
        StatusCode::OK
    );
}
