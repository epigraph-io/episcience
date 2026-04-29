//! Integration tests for the Phase 3 episcience MCP server (Tasks 3.6–3.8).
//!
//! Strategy: invoke the `EpiscienceServer` tool methods directly (they're
//! plain `async fn`s) rather than spinning up the stdio binary. This keeps
//! the tests fast and lets us seed/inspect the database without an MCP
//! transport round-trip. The visibility predicate, the readable-by gate,
//! and the synthesis_id+job tx are exercised end-to-end.
//!
//! The reference precedent is `epigraph-mcp` — its tests bypass the
//! `serve(stdio)` layer too. Tool methods are public on `EpiscienceServer`
//! by virtue of the `#[tool]` macro expanding them as ordinary methods.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//!     cargo test -p episcience-api --test mcp_tools_test
//!
//! Test inventory (mirrors the spec's items 1, 3, 4, 5, 6 — item 2 is
//! intentionally skipped because it would require a live worker; the
//! no-wait case in test 1 verifies the contract).

use std::sync::Arc;

use async_trait::async_trait;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::mcp::queries::{GetSynthesisArgs, ListSynthesesArgs, RecallSynthesisArgs};
use episcience_api::mcp::synthesize::SynthesizeArgs;
use episcience_api::mcp::EpiscienceServer;
use episcience_core::synthesis::Visibility;
use episcience_db::synthesis::edge_writer::{EdgeRequest, EdgeWriter, EdgeWriterError};
use episcience_db::{
    SynthesisEmbeddingsRepository, SynthesisRepository, SynthesisSharesRepository,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, RawContent};
use sqlx::PgPool;
use uuid::Uuid;

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis";

async fn connect() -> PgPool {
    let dsn = std::env::var("DATABASE_URL").unwrap_or_else(|_| DSN.to_string());
    PgPool::connect(&dsn)
        .await
        .expect("connect to epigraph_dev_synthesis (set DATABASE_URL to override)")
}

/// Stub edge writer for tests — the MCP synthesize tool only enqueues a job;
/// it never calls into the edge writer directly. The Stage 6 worker would,
/// but the worker isn't running in these tests, so a no-op stub is enough.
#[derive(Default)]
struct NoopEdgeWriter;

#[async_trait]
impl EdgeWriter for NoopEdgeWriter {
    async fn create_edge(&self, _req: EdgeRequest) -> Result<Uuid, EdgeWriterError> {
        Ok(Uuid::nil())
    }
}

/// Build an `(EpiscienceServer, MockProvider Arc)` pair so tests can use the
/// same embedder the server does to pre-seed deterministic embeddings.
fn build_server(pool: PgPool, auth_agent: Uuid) -> (EpiscienceServer, Arc<MockProvider>) {
    let mock = Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let embedder: Arc<dyn EmbeddingService> = mock.clone();
    let edge_writer: Arc<dyn EdgeWriter> = Arc::new(NoopEdgeWriter);
    let server = EpiscienceServer::new(pool, embedder, edge_writer, auth_agent);
    (server, mock)
}

/// Hard-delete a synthesis and its dependents. Idempotent.
async fn cleanup_synthesis(pool: &PgPool, id: Uuid) {
    sqlx::query("DELETE FROM synthesis_shares WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM synthesis_embeddings WHERE synthesis_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM synthesis_jobs WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .ok();
}

/// Pull the JSON text payload out of a `CallToolResult::success(...)` return.
///
/// Our tools always emit `vec![Content::text(json_string)]`, so the test
/// helper can be terse about the variant.
fn body_json(result: &CallToolResult) -> serde_json::Value {
    let content = result
        .content
        .first()
        .expect("tool result has at least one content item");
    match &content.raw {
        RawContent::Text(t) => serde_json::from_str(&t.text).expect("body is JSON"),
        other => panic!("unexpected content variant: {other:?}"),
    }
}

/// Seed a synthesis row + an embedding row using the given mock provider so
/// that calling `recall` with the same `text` produces a cosine = 1.0 hit.
async fn seed_synthesis_with_embedding(
    pool: &PgPool,
    mock: &MockProvider,
    id: Uuid,
    owner: Uuid,
    visibility: Visibility,
    text: &str,
) {
    SynthesisRepository::create_pending(
        pool,
        id,
        text,
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        visibility,
    )
    .await
    .expect("seed synthesis");

    let embedding = mock.generate(text).await.expect("mock embed text");
    SynthesisEmbeddingsRepository::upsert(
        pool,
        id,
        &embedding,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .expect("upsert embedding");
}

/// Seed a synthesis row without an embedding (for list/get tests).
async fn seed_synthesis(pool: &PgPool, id: Uuid, owner: Uuid, visibility: Visibility, query: &str) {
    SynthesisRepository::create_pending(
        pool,
        id,
        query,
        owner,
        None,
        &[],
        "anthropic",
        "claude-sonnet-4-6",
        visibility,
    )
    .await
    .expect("seed synthesis");
}

// ─── Test 1: synthesize returns queued when not waiting ──────────────────────

#[tokio::test]
async fn synthesize_returns_queued_when_no_wait() {
    let pool = connect().await;
    let agent = Uuid::now_v7();
    let (server, _) = build_server(pool.clone(), agent);

    let result = server
        .synthesize(Parameters(SynthesizeArgs {
            query: "DNA origami thermal stability".to_string(),
            traversal_config: None,
            parent_synthesis_id: None,
            prereq_synthesis_ids: vec![],
            wait_for_completion: false,
            timeout_seconds: 0,
            visibility: "private".to_string(),
        }))
        .await
        .expect("synthesize tool call");

    let body = body_json(&result);
    let id_str = body
        .get("synthesis_id")
        .and_then(|v| v.as_str())
        .expect("body has synthesis_id");
    let id: Uuid = id_str.parse().expect("synthesis_id parses as UUID");
    assert_eq!(
        body.get("status").and_then(|v| v.as_str()),
        Some("queued"),
        "status should be 'queued' without wait_for_completion"
    );
    // Body should NOT include narrative when not waiting.
    assert!(
        body.get("narrative").is_none() || body["narrative"].is_null(),
        "narrative should be absent when not waiting"
    );

    // Both rows should exist after a successful tx commit.
    let synth_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("count syntheses");
    assert_eq!(synth_count, 1, "exactly 1 row in syntheses");

    let job_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM synthesis_jobs WHERE id = $1 AND state = 'queued'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("count synthesis_jobs");
    assert_eq!(job_count, 1, "exactly 1 queued row in synthesis_jobs");

    cleanup_synthesis(&pool, id).await;
}

// ─── Test 2 (skipped per spec) ───────────────────────────────────────────────
//
// `synthesize_with_wait_returns_complete_after_poll` would require either a
// running worker or a way to inject a synthesis_id externally. The synthesize
// tool mints its own id internally, so a unit test can't cheaply pre-seed
// the row to `complete`. The contract that the tool blocks on a poll loop is
// covered by code review; the no-wait path covers the success contract.

// ─── Test 3: recall returns visible hits ─────────────────────────────────────

#[tokio::test]
async fn recall_synthesis_returns_visible_hits() {
    let pool = connect().await;
    let agent = Uuid::now_v7();
    let (server, mock) = build_server(pool.clone(), agent);

    let id_a = Uuid::now_v7();
    let id_b = Uuid::now_v7();
    let query = format!("mcp recall test {}", Uuid::now_v7());

    // Both syntheses are owned by the auth agent and produced by the same
    // mock embedder seed text → cosine = 1.0 against the recall query.
    seed_synthesis_with_embedding(&pool, &mock, id_a, agent, Visibility::Private, &query).await;
    seed_synthesis_with_embedding(&pool, &mock, id_b, agent, Visibility::Public, &query).await;

    let result = server
        .recall_synthesis(Parameters(RecallSynthesisArgs {
            query: query.clone(),
            limit: Some(50),
            min_score: Some(0.99),
            include_stale: Some(false),
        }))
        .await
        .expect("recall tool call");

    let body = body_json(&result);
    let arr = body.as_array().expect("recall body is an array");
    let ids: Vec<Uuid> = arr
        .iter()
        .map(|v| v["synthesis_id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert!(
        ids.contains(&id_a) && ids.contains(&id_b),
        "expected both seeded ids in hits, got {ids:?}"
    );

    cleanup_synthesis(&pool, id_a).await;
    cleanup_synthesis(&pool, id_b).await;
}

// ─── Test 4: get_synthesis owner reads ───────────────────────────────────────

#[tokio::test]
async fn get_synthesis_owner_reads() {
    let pool = connect().await;
    let agent = Uuid::now_v7();
    let (server, _) = build_server(pool.clone(), agent);

    let id = Uuid::now_v7();
    seed_synthesis(&pool, id, agent, Visibility::Private, "owner read test").await;

    let result = server
        .get_synthesis(Parameters(GetSynthesisArgs { synthesis_id: id }))
        .await
        .expect("get_synthesis tool call");

    let body = body_json(&result);
    assert_eq!(body["id"].as_str().unwrap().parse::<Uuid>().unwrap(), id);
    assert_eq!(body["query"].as_str().unwrap(), "owner read test");

    cleanup_synthesis(&pool, id).await;
}

// ─── Test 5: get_synthesis stranger gets invalid_request ─────────────────────

#[tokio::test]
async fn get_synthesis_stranger_returns_invalid_request() {
    let pool = connect().await;
    let owner = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    let (server, _) = build_server(pool.clone(), stranger);

    let id = Uuid::now_v7();
    // Seed as `owner`, ask as `stranger` with no share — should look identical
    // to "not found" from the outside.
    seed_synthesis(&pool, id, owner, Visibility::Private, "stranger probe").await;

    let result = server
        .get_synthesis(Parameters(GetSynthesisArgs { synthesis_id: id }))
        .await;
    assert!(
        result.is_err(),
        "stranger must not be able to read a private synthesis"
    );
    let err = result.unwrap_err();
    assert!(
        err.message.to_lowercase().contains("not found"),
        "error should look like 'not found' to avoid existence leak; got: {}",
        err.message
    );

    cleanup_synthesis(&pool, id).await;
}

// ─── Test 6: list_syntheses returns readable rows ────────────────────────────

#[tokio::test]
async fn list_syntheses_returns_readable() {
    let pool = connect().await;
    let agent = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    let (server, _) = build_server(pool.clone(), agent);

    let id_owned = Uuid::now_v7();
    let id_public = Uuid::now_v7();
    let id_shared = Uuid::now_v7();
    let id_unrelated = Uuid::now_v7();

    seed_synthesis(&pool, id_owned, agent, Visibility::Private, "list owned").await;
    seed_synthesis(
        &pool,
        id_public,
        stranger,
        Visibility::Public,
        "list public",
    )
    .await;
    seed_synthesis(
        &pool,
        id_shared,
        stranger,
        Visibility::Shared,
        "list shared",
    )
    .await;
    seed_synthesis(
        &pool,
        id_unrelated,
        stranger,
        Visibility::Private,
        "list unrelated",
    )
    .await;
    SynthesisSharesRepository::grant(&pool, id_shared, agent, stranger)
        .await
        .expect("grant share");

    let result = server
        .list_syntheses(Parameters(ListSynthesesArgs {
            limit: Some(500),
            offset: Some(0),
            include_stale: Some(false),
        }))
        .await
        .expect("list_syntheses tool call");

    let body = body_json(&result);
    let ids: Vec<Uuid> = body
        .as_array()
        .expect("list body is array")
        .iter()
        .map(|v| v["id"].as_str().unwrap().parse().unwrap())
        .collect();

    assert!(ids.contains(&id_owned), "owner row missing");
    assert!(ids.contains(&id_public), "public row missing");
    assert!(ids.contains(&id_shared), "shared row missing");
    assert!(
        !ids.contains(&id_unrelated),
        "unrelated private row leaked: {ids:?}"
    );

    cleanup_synthesis(&pool, id_owned).await;
    cleanup_synthesis(&pool, id_public).await;
    cleanup_synthesis(&pool, id_shared).await;
    cleanup_synthesis(&pool, id_unrelated).await;
}
