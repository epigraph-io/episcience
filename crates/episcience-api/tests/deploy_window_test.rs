//! The E1d binary on the 5034 schema: the deploy window between the expand
//! step and the contract step, when the E1d binary is installed but 5035's
//! row guards do not exist yet. The application must hold the tenancy rules
//! itself there (the guards are its backstop from 5035 on), so each case runs
//! the REAL REST router over a kernel-only clone migrated to 5034 exactly.
//!
//! Cast: H1 and H2 (personal groups), a bystander with no membership.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_core::TenancyDecl;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_db::ledger;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use testdb::TestDb;
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

async fn at_5034() -> TestDb {
    let db = TestDb::fresh_kernel_only().await;
    let mut c = ledger::connect_with(db.admin_options())
        .await
        .expect("ledger connection");
    ledger::run_to(&mut c, Some(ledger::TENANCY_EXPAND_VERSION))
        .await
        .expect("episcience migrations up to 5034");
    db
}

fn server(pool: PgPool, blobs: &std::path::Path) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    TestServer::new(episcience_api::create_router(ElnState {
        pool,
        blob_dir: blobs.to_path_buf(),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    }))
    .expect("TestServer")
}

/// A declared synthesis written on the admin pool.
async fn synthesis(pool: &PgPool, author: Uuid, vis: &str, owner: Uuid, query: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id) \
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), $4, $5)",
    )
    .bind(id)
    .bind(query)
    .bind(author)
    .bind(vis)
    .bind(owner)
    .execute(pool)
    .await
    .expect("synthesis fixture");
    id
}

async fn visibility_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT visibility::text FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// R5 (the application half): at 5034, a PUBLIC refinement of a GROUP
/// synthesis is stored `group` (a bystander cannot read it, so the parent's
/// query does not leak), and so is a synthesis asked public with a GROUP
/// prerequisite; a synthesis with no non-public input stays public. Kills:
/// child_ownership honouring the requested visibility under a non-public
/// parent, or narrow_for_prerequisites removed.
#[tokio::test]
async fn a_public_synthesis_with_a_group_input_is_born_group_before_the_guards_exist() {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let bystander = testdb::principal(a, "bystander").await;
    let blobs = tempfile::TempDir::new().unwrap();
    let srv = server(a.clone(), blobs.path());
    let parent = synthesis(a, h1.agent, "group", h1.personal_group, "a secret query").await;

    let (hn, hv) = bearer(h1.agent);
    let resp = srv
        .post(&format!("/api/v1/eln/syntheses/{parent}/refine"))
        .add_header(hn, hv)
        .json(&json!({"visibility": "public"}))
        .await;
    assert_eq!(resp.status_code(), StatusCode::ACCEPTED, "{}", resp.text());
    let child: Uuid = resp.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(visibility_of(a, child).await, "group");
    let (hn, hv) = bearer(bystander.agent);
    let resp = srv
        .get(&format!("/api/v1/eln/syntheses/{child}"))
        .add_header(hn, hv)
        .await;
    assert_eq!(resp.status_code(), StatusCode::NOT_FOUND);
    assert!(!resp.text().contains("a secret query"));

    for (prereqs, want) in [(vec![parent], "group"), (vec![], "public")] {
        let (hn, hv) = bearer(h1.agent);
        let resp = srv
            .post("/api/v1/eln/syntheses")
            .add_header(hn, hv)
            .json(&json!({"query": "q", "prereq_synthesis_ids": prereqs, "visibility": "public"}))
            .await;
        assert_eq!(resp.status_code(), StatusCode::ACCEPTED, "{}", resp.text());
        let id: Uuid = resp.json::<serde_json::Value>()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(visibility_of(a, id).await, want, "prereqs {prereqs:?}");
    }
}

/// R6 (the application half): at 5034, widening a synthesis that cites a
/// GROUP claim is refused (403, the widening guard's own words) and the row
/// is unchanged; one whose members are all public widens. Kills: the
/// publishability check removed from `set_visibility_as` (the widen would
/// succeed until 5035 existed).
#[tokio::test]
async fn widening_a_synthesis_with_a_group_member_is_refused_before_the_guards_exist() {
    let db = at_5034().await;
    let a = &db.admin;
    let h1 = testdb::principal(a, "h1").await;
    let g = h1.personal_group;
    let blobs = tempfile::TempDir::new().unwrap();
    let srv = server(a.clone(), blobs.path());
    let own = testdb::claim(
        a,
        h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(g),
    )
    .await;
    let public = testdb::claim(
        a,
        h1.agent,
        &format!("pub {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(g),
    )
    .await;
    let blocked = synthesis(a, h1.agent, "group", g, "q").await;
    let open = synthesis(a, h1.agent, "group", g, "q").await;
    for (s, c) in [(blocked, own), (open, public)] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(s)
        .bind(c)
        .execute(a)
        .await
        .unwrap();
    }
    for (s, want) in [
        (blocked, StatusCode::FORBIDDEN),
        (open, StatusCode::NO_CONTENT),
    ] {
        let (hn, hv) = bearer(h1.agent);
        let resp = srv
            .patch(&format!("/api/v1/eln/syntheses/{s}/visibility"))
            .add_header(hn, hv)
            .json(&json!({"visibility": "public"}))
            .await;
        assert_eq!(resp.status_code(), want, "{}", resp.text());
    }
    assert_eq!(visibility_of(a, blocked).await, "group");
    assert_eq!(visibility_of(a, open).await, "public");
}
