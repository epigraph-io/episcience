//! T-R4s: EpiScience's own reads of kernel `claims` apply the kernel's viewer
//! predicate (the `/* {VISIBILITY:c} */` splice), at the splice level: the
//! process still reads on a row-level-security-bypassing role, so the splice is
//! the only thing that hides another owner's group-owned claim.
//!
//! Cast (fresh template clone per test): H1 and H2, each with its personal
//! group. H1 authors a PUBLIC claim and a GROUP claim owned by H1's personal
//! group; H2 authors a PUBLIC claim. All three carry the same unique search
//! word and label. Kernel semantics: H1 reads all three; H2 reads the two
//! public ones and never H1's group claim.
//!
//! This replaces batch E1a's interim own-authored-only filter (T-R0), which
//! also hid PUBLIC claims by other authors.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::TestServer;
use chrono::{DateTime, Utc};
use epigraph_core::TenancyDecl;
use epigraph_crypto::ContentHasher;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::mcp::countersigns::CountersignArgs;
use episcience_api::mcp::list_countersignatures::ListCountersignaturesArgs;
use episcience_api::mcp::EpiscienceServer;
use episcience_api::middleware::{AuthContext, JwtConfig};
use episcience_api::state::ElnState;
use episcience_db::synthesis::edge_writer::{EdgeRequest, EdgeWriter, EdgeWriterError};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::Extensions;
use sqlx::PgPool;
use std::sync::Arc;
use testdb::{Principal, TestDb};
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

fn rest_server(pool: PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        pool,
        blob_dir: std::env::temp_dir().join("episcience-viewer-splice-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024,
        embedder,
    };
    TestServer::new(episcience_api::create_router(state)).expect("build TestServer")
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

struct Seeded {
    id: Uuid,
    content: String,
    truth_value: f64,
    created_at: DateTime<Utc>,
}

/// A declared claim (kernel repository) carrying `label`, read back for the
/// export hash.
async fn seed(
    pool: &PgPool,
    author: Uuid,
    word: &str,
    label: &str,
    decl: TenancyDecl,
    tag: &str,
) -> Seeded {
    let content = format!("viewer splice observation {word} {tag}");
    let id = testdb::claim(pool, author, &content, 0.7, decl).await;
    let (truth_value, created_at): (f64, DateTime<Utc>) = sqlx::query_as(
        "UPDATE claims SET labels = ARRAY[$2] WHERE id = $1 RETURNING truth_value, created_at",
    )
    .bind(id)
    .bind(label)
    .fetch_one(pool)
    .await
    .expect("label the fixture claim");
    Seeded {
        id,
        content,
        truth_value,
        created_at,
    }
}

/// The export route's integrity hash for exactly `entries`, in export order
/// (`created_at` ascending).
fn export_hash(entries: &mut [&Seeded]) -> String {
    entries.sort_by_key(|e| e.created_at);
    let mut input = String::new();
    for e in entries.iter() {
        input.push_str(&e.id.to_string());
        input.push_str(&e.content);
        input.push_str(&e.truth_value.to_string());
        input.push_str(&e.created_at.to_rfc3339());
    }
    hex::encode(ContentHasher::hash(input.as_bytes()))
}

struct Cast {
    h1: Principal,
    h2: Principal,
    word: String,
    label: String,
    h1_public: Seeded,
    h1_group: Seeded,
    h2_public: Seeded,
}

async fn cast(pool: &PgPool) -> Cast {
    let h1 = testdb::principal(pool, "h1").await;
    let h2 = testdb::principal(pool, "h2").await;
    let word = unique_word();
    let label = format!("t-r4s-{word}");
    let h1_public = seed(
        pool,
        h1.agent,
        &word,
        &label,
        TenancyDecl::public(h1.personal_group),
        "h1-public",
    )
    .await;
    let h1_group = seed(
        pool,
        h1.agent,
        &word,
        &label,
        TenancyDecl::group(h1.personal_group),
        "h1-group",
    )
    .await;
    let h2_public = seed(
        pool,
        h2.agent,
        &word,
        &label,
        TenancyDecl::public(h2.personal_group),
        "h2-public",
    )
    .await;
    assert_eq!(
        testdb::claim_pair(pool, h1_group.id).await,
        ("group".to_string(), h1.personal_group),
        "the private fixture must really be group-owned, or every exclusion below is vacuous"
    );
    Cast {
        h1,
        h2,
        word,
        label,
        h1_public,
        h1_group,
        h2_public,
    }
}

async fn search_ids(server: &TestServer, agent: Uuid, word: &str) -> Vec<Uuid> {
    let (n, v) = bearer(&mint_test_jwt(agent));
    let resp = server
        .get("/api/v1/eln/search/fulltext")
        .add_query_param("q", word)
        .add_header(n, v)
        .await;
    assert_eq!(resp.status_code(), StatusCode::OK);
    let mut ids: Vec<Uuid> = resp
        .json::<serde_json::Value>()
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["claim_id"].as_str().unwrap().parse().unwrap())
        .collect();
    ids.sort();
    ids
}

async fn export_hash_for(server: &TestServer, agent: Uuid, label: &str) -> String {
    let today = Utc::now().date_naive();
    let (n, v) = bearer(&mint_test_jwt(agent));
    let resp = server
        .get("/api/v1/eln/export/notebook.pdf")
        .add_query_param("from", today.pred_opt().unwrap().to_string())
        .add_query_param("to", today.succ_opt().unwrap().to_string())
        .add_query_param("label", label)
        .add_header(n, v)
        .await;
    assert_eq!(resp.status_code(), StatusCode::OK);
    resp.headers()
        .get("x-content-hash")
        .expect("x-content-hash header")
        .to_str()
        .unwrap()
        .to_string()
}

// T-R4s (search). Kills: dropping the splice (or its group bind) from
// `NotebookRepository::fulltext_search` (H2 would see H1's group claim), and
// restoring E1a's own-authored filter (H1 would lose H2's public claim).
#[tokio::test]
async fn fulltext_search_returns_what_the_kernel_would_show_the_caller() {
    let db = TestDb::fresh().await;
    let c = cast(&db.admin).await;
    let server = rest_server(db.admin.clone());

    let mut all = vec![c.h1_public.id, c.h1_group.id, c.h2_public.id];
    all.sort();
    assert_eq!(search_ids(&server, c.h1.agent, &c.word).await, all);

    let mut public = vec![c.h1_public.id, c.h2_public.id];
    public.sort();
    let h2_hits = search_ids(&server, c.h2.agent, &c.word).await;
    assert_eq!(h2_hits, public);
    assert!(!h2_hits.contains(&c.h1_group.id));
}

// T-R4s (export). Kills: dropping the splice from the notebook export, or
// restoring E1a's own-authored filter.
#[tokio::test]
async fn notebook_export_covers_what_the_kernel_would_show_the_caller() {
    let db = TestDb::fresh().await;
    let c = cast(&db.admin).await;
    let server = rest_server(db.admin.clone());

    assert_eq!(
        export_hash_for(&server, c.h1.agent, &c.label).await,
        export_hash(&mut [&c.h1_public, &c.h1_group, &c.h2_public]),
        "H1's export covers its own claims and H2's public one"
    );
    let h2 = export_hash_for(&server, c.h2.agent, &c.label).await;
    assert_eq!(
        h2,
        export_hash(&mut [&c.h1_public, &c.h2_public]),
        "H2's export covers the two public claims only"
    );
    assert_ne!(
        h2,
        export_hash(&mut [&c.h1_public, &c.h1_group, &c.h2_public])
    );
}

// T-R4s (REST countersign reads). A claim the caller cannot read is 404 on
// the countersign create, list and verify routes, exactly like an absent
// claim; the owner gets past the claim read. Kills: reverting any of the
// three routes to an unspliced `SELECT content FROM claims`.
#[tokio::test]
async fn countersign_routes_treat_an_invisible_claim_as_absent() {
    let db = TestDb::fresh().await;
    let c = cast(&db.admin).await;
    let server = rest_server(db.admin.clone());
    let path = |id: Uuid| format!("/api/v1/eln/claims/{id}/countersignatures");

    for (agent, want) in [
        (c.h2.agent, StatusCode::NOT_FOUND),
        (c.h1.agent, StatusCode::OK),
    ] {
        for suffix in ["", "/verify"] {
            let (n, v) = bearer(&mint_test_jwt(agent));
            let resp = server
                .get(&format!("{}{suffix}", path(c.h1_group.id)))
                .add_header(n, v)
                .await;
            assert_eq!(resp.status_code(), want, "GET countersignatures{suffix}");
        }
    }

    // Create: H2 signing H1's group claim is refused at the claim read (404)
    // before any signature check; an absent claim answers the same.
    for claim in [c.h1_group.id, Uuid::now_v7()] {
        let (n, v) = bearer(&mint_test_jwt(c.h2.agent));
        let resp = server
            .post("/api/v1/eln/countersign")
            .add_header(n, v)
            .json(&serde_json::json!({
                "claim_id": claim,
                "signer_id": c.h2.agent,
                "signature_meaning": "witnessed",
                "signature_hex": "00".repeat(64),
                "public_key_hex": "00".repeat(32),
            }))
            .await;
        assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    }
    // The owner passes the claim read (whatever the dummy signature then
    // does): proof the 404 above is the viewer's doing.
    let (n, v) = bearer(&mint_test_jwt(c.h1.agent));
    let resp = server
        .post("/api/v1/eln/countersign")
        .add_header(n, v)
        .json(&serde_json::json!({
            "claim_id": c.h1_group.id,
            "signer_id": c.h1.agent,
            "signature_meaning": "witnessed",
            "signature_hex": "00".repeat(64),
            "public_key_hex": "00".repeat(32),
        }))
        .await;
    assert_ne!(resp.status_code(), StatusCode::NOT_FOUND);
}

#[derive(Default)]
struct NoopEdgeWriter;

#[async_trait::async_trait]
impl EdgeWriter for NoopEdgeWriter {
    async fn create_edge(&self, _req: EdgeRequest) -> Result<Uuid, EdgeWriterError> {
        Ok(Uuid::nil())
    }
}

fn as_caller(agent: Uuid) -> Extensions {
    let mut ext = Extensions::new();
    ext.insert(AuthContext {
        agent_id: agent,
        client_id: Uuid::new_v4(),
        owner_id: None,
        client_type: "human".to_string(),
        scopes: vec!["claims:read".to_string(), "claims:write".to_string()],
    });
    ext
}

// T-R4s (MCP countersign reads). `list_countersignatures` and `countersign`
// report an invisible claim as not found, and serve the owner. Kills:
// reverting either MCP claim read to an unspliced lookup (or dropping it).
#[tokio::test]
async fn mcp_list_countersignatures_treats_an_invisible_claim_as_absent() {
    let db = TestDb::fresh().await;
    let c = cast(&db.admin).await;
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let server = EpiscienceServer::new(
        db.admin.clone(),
        embedder,
        Arc::new(NoopEdgeWriter),
        std::env::temp_dir().join(format!("episcience-splice-{}", Uuid::now_v7())),
        1024,
    );
    let args = || {
        Parameters(ListCountersignaturesArgs {
            claim_id: c.h1_group.id,
        })
    };
    let refused = server
        .list_countersignatures(args(), as_caller(c.h2.agent))
        .await
        .expect_err("H2 cannot read H1's group claim");
    assert!(refused.message.contains("not found"), "{}", refused.message);
    server
        .list_countersignatures(args(), as_caller(c.h1.agent))
        .await
        .expect("the owner lists the countersignatures of its own claim");

    // `countersign`: the claim read is refused as "not found" for H2 before
    // any signature check.
    let sign = || {
        Parameters(CountersignArgs {
            claim_id: c.h1_group.id,
            signature_meaning: "witnessed".to_string(),
            signature_hex: "00".repeat(64),
            public_key_hex: "00".repeat(32),
        })
    };
    let refused = server
        .countersign(sign(), as_caller(c.h2.agent))
        .await
        .expect_err("H2 cannot countersign a claim it cannot read");
    assert!(refused.message.contains("not found"), "{}", refused.message);
    // The owner gets past the claim read. The all-zero key is a small-order
    // point, so the dummy signature may verify or fail depending on the
    // message: either outcome proves the claim read did not refuse.
    if let Err(owner) = server.countersign(sign(), as_caller(c.h1.agent)).await {
        assert!(!owner.message.contains("not found"), "{}", owner.message);
    }
}
