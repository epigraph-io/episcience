//! REST token acceptance and principal requirement (batch E1a: T-A1, T-A2).
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.
//!
//! Every negative case is paired with a positive control built from the SAME
//! spec with one field changed, so a refusal cannot pass for an unrelated
//! reason (bad route, bad body, DB error).
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_crypto::{AgentSigner, ContentHasher};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint, TokenSpec};
#[path = "support/write_routes.rs"]
mod write_routes;
use write_routes::{bearer, send, write_routes};

/// The run's shared clone of the E1 template (scripts/e1-test-db.sh). Refuses
/// port 5432 and any database name not ending in `_test`; no default DSN.
async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
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

async fn seed_agent_with_key(pool: &PgPool, public_key: &[u8]) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO agents (id, public_key, display_name, agent_type, role, state)
           VALUES ($1, $2, $3, 'service', 'custom', 'active')"#,
    )
    .bind(id)
    .bind(public_key)
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
// zero leeway, the requirement that iss / aud be present at all, or accepting
// an array `aud` (kernel parity: its claims type `aud` as one string).
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
        // An array audience that CONTAINS the expected value: `jsonwebtoken`
        // admits it by membership, the kernel's typed claims refuse it.
        (
            "aud array containing epigraph-api",
            TokenSpec {
                aud_list: Some(vec!["other-api".into(), "epigraph-api".into()]),
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

/// The tables a REST write can touch: EpiScience's 16 tables plus the kernel
/// `claims` table (observations create claims). These are every target of an
/// `INSERT` / `UPDATE` / `DELETE` in `episcience-db/src` and
/// `episcience-api/src`, plus the rest of the 16. Kernel tables outside this
/// set are NOT counted: in CI a kernel sidecar shares the database and runs a
/// scheduled background job, so their counts can move for reasons unrelated
/// to this test.
const WRITE_SURFACE: [&str; 17] = [
    "blobs",
    "countersignatures",
    "episcience_worker_state",
    "experiment_results",
    "experiments",
    "protocols",
    "sample_claims",
    "samples",
    "syntheses",
    "synthesis_claim_membership",
    "synthesis_clusters",
    "synthesis_embeddings",
    "synthesis_jobs",
    "synthesis_provo_edges",
    "synthesis_shares",
    "synthesis_staleness_events",
    "claims",
];

/// Exact row count of every table in [`WRITE_SURFACE`]; panics if one is
/// missing, so a renamed table cannot silently drop out of the snapshot.
async fn row_counts(pool: &PgPool) -> BTreeMap<String, i64> {
    let mut counts = BTreeMap::new();
    for t in WRITE_SURFACE {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM public.\"{t}\""))
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("count {t} (table missing?): {e}"));
        counts.insert(t.to_string(), n);
    }
    counts
}

async fn seed_sample(pool: &PgPool, prepared_by: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("rest-auth-gate-sample-{id}");
    let hash = ContentHasher::hash(name.as_bytes());
    sqlx::query(
        // Declared: public, in the preparer's personal group (provisioned
        // here exactly as the OAuth mint would).
        r#"INSERT INTO samples (id, name, sample_type, prepared_by, content_hash,
                                owner_group_id, visibility)
           VALUES ($1, $2, 'biological', $3, $4,
                   public.epigraph_ensure_personal_group($3), 'public')"#,
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

// T-A2. A valid token with no `agent_id` is refused with `principal_required`
// on EVERY REST write route, and no row changes in any table a REST write can
// touch (exact counts of the 16 EpiScience tables and `claims`, before and
// after). `sub` is set to a REAL agent that
// owns the sample and synthesis the routes target, so a `sub` fallback (the
// removed `unwrap_or(claims.sub)`) would pass ownership checks and write.
// Kills: the `sub` fallback, and a write route mounted outside the gated
// router (it would answer something other than 401 principal_required, or
// write a row).
//
// Row counts are stable here: test binaries run one at a time, the only other
// test in this binary (T-A1) issues GETs, and the snapshot excludes kernel
// tables a CI sidecar may write in the background.
#[tokio::test]
async fn rest_refuses_principal_less_token_and_writes_nothing() {
    let pool = connect().await;
    let server = build_test_server(pool.clone());
    let signer = AgentSigner::generate();
    let real_agent = seed_agent_with_key(&pool, &signer.public_key()).await;
    let rw = token::mint_test_jwt(real_agent);

    // Path-parameter fixtures owned by `real_agent`, created before the
    // snapshot.
    let sample = seed_sample(&pool, real_agent).await;
    let (name, value) = bearer(&rw);
    let created = server
        .post("/api/v1/eln/syntheses")
        .add_header(name, value)
        .json(&serde_json::json!({"query": format!("principal-less fixture {real_agent}")}))
        .await;
    assert_eq!(created.status_code(), StatusCode::ACCEPTED);
    let synthesis: Uuid = created.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let principal_less = mint(&TokenSpec {
        sub: Some(real_agent),
        agent_id: None,
        ..TokenSpec::valid(real_agent)
    });
    let routes = write_routes(
        real_agent,
        sample,
        synthesis,
        &hex::encode(signer.public_key()),
    );

    let before = row_counts(&pool).await;
    for (label, w) in &routes {
        let resp = send(&server, w, &principal_less).await;
        assert_eq!(
            resp.status_code(),
            StatusCode::UNAUTHORIZED,
            "{label}: a principal-less token must be refused"
        );
        let body: serde_json::Value = resp.json();
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("principal_required"),
            "{label}: body must name principal_required, got {body}"
        );
    }
    let after = row_counts(&pool).await;
    let changed: Vec<String> = before
        .iter()
        .filter(|(t, n)| after.get(*t) != Some(n))
        .map(|(t, n)| format!("{t}: {n} -> {:?}", after.get(t)))
        .collect();
    assert!(changed.is_empty(), "no row may change: {changed:?}");

    // Positive control: the same agent WITH agent_id writes (so the refusal
    // above is the principal gate, not a bad body).
    let marker = format!("e1a-principal-less-{}", Uuid::now_v7());
    let (name, value) = bearer(&rw);
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
}
