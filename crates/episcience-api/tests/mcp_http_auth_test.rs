//! MCP caller identity and token acceptance over the REAL HTTP stack
//! (batch E1a: T-A1 MCP, T-A3 MCP, T-A5, T-A9, and the development opt-out).
//!
//! Every case goes through `episcience_api::mcp::http::router`, the router the
//! binary serves: bearer layer -> rmcp session -> `ServerHandler::call_tool`
//! -> tool. Direct `server.tool(..)` calls would bypass `call_tool` and could
//! not catch a removed gate.
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.

use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint, mint_test_jwt, read_only_jwt, TokenSpec};

#[path = "support/mcp_http.rs"]
mod mcp_http;
use mcp_http::{bearer_auth, start_mcp, tool_json, McpClient};

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test";
const TOOL_COUNT: usize = 9;

async fn connect() -> PgPool {
    let dsn = std::env::var("DATABASE_URL").unwrap_or_else(|_| DSN.to_string());
    PgPool::connect(&dsn)
        .await
        .expect("connect (set DATABASE_URL to a migrated *_test database)")
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
    .bind(format!("mcp-http-auth-{id}"))
    .execute(pool)
    .await
    .expect("seed agent");
    id
}

async fn start(pool: &PgPool) -> (std::net::SocketAddr, tempfile::TempDir) {
    let blob_dir = tempfile::TempDir::new().expect("blob dir");
    let addr = start_mcp(
        pool.clone(),
        blob_dir.path().to_path_buf(),
        bearer_auth(&jwt_secret_bytes()),
    )
    .await;
    (addr, blob_dir)
}

// T-A1 (MCP). Kills: dropping the iss / aud / exp pins, or the bearer layer.
#[tokio::test]
async fn mcp_refuses_wrong_or_missing_iss_aud_and_expired_tokens() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let good = TokenSpec::valid(Uuid::now_v7());

    // Control: a valid token initializes.
    let mut client = McpClient::new(addr, Some(mint(&good)));
    assert_eq!(client.initialize().await, reqwest::StatusCode::OK);

    let cases = [
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
        (
            "expired 5s ago",
            TokenSpec {
                exp_offset_secs: -5,
                ..good.clone()
            },
        ),
    ];
    for (label, spec) in cases {
        let mut client = McpClient::new(addr, Some(mint(&spec)));
        assert_eq!(
            client.initialize().await,
            reqwest::StatusCode::UNAUTHORIZED,
            "{label}"
        );
    }
    // No bearer at all.
    let mut client = McpClient::new(addr, None);
    assert_eq!(client.initialize().await, reqwest::StatusCode::UNAUTHORIZED);
}

// T-A9. A valid principal-less token (the gateway discovery shape: no
// agent_id, a non-claims scope) initializes and lists every tool, and every
// tools/call is refused with principal_required. Kills: requiring the
// principal in the HTTP layer (discovery would break), or dropping it from
// call_tool (the call would run).
#[tokio::test]
async fn principal_less_token_lists_tools_but_cannot_call_them() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let discovery = mint(&TokenSpec {
        agent_id: None,
        scopes: vec!["episcience:tools".to_string()],
        ..TokenSpec::valid(Uuid::now_v7())
    });
    let mut client = McpClient::new(addr, Some(discovery));
    assert_eq!(client.initialize().await, reqwest::StatusCode::OK);

    let tools = client.list_tools().await;
    let listed = tools.result()["tools"]
        .as_array()
        .expect("tools array")
        .len();
    assert_eq!(listed, TOOL_COUNT, "discovery must see every tool");

    let reply = client.call_tool("list_syntheses", json!({})).await;
    assert!(
        reply.error_message().contains("principal_required"),
        "got: {}",
        reply.error_message()
    );

    // Even a principal-less token that holds claims scopes cannot call.
    let scoped = mint(&TokenSpec {
        agent_id: None,
        ..TokenSpec::valid(Uuid::now_v7())
    });
    let mut client = McpClient::new(addr, Some(scoped));
    assert_eq!(client.initialize().await, reqwest::StatusCode::OK);
    let reply = client.call_tool("list_syntheses", json!({})).await;
    assert!(reply.error_message().contains("principal_required"));
}

// T-A3 (MCP). A claims:read-only token is refused on every write tool with
// insufficient_scope, and may call a read tool. Kills: removing the scope
// check from call_tool, or mapping a write tool to claims:read.
#[tokio::test]
async fn read_only_token_is_refused_on_every_write_tool() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let agent = seed_agent(&pool).await;
    let mut client = McpClient::new(addr, Some(read_only_jwt(agent)));
    assert_eq!(client.initialize().await, reqwest::StatusCode::OK);

    let marker = format!("e1a-ro-{}", Uuid::now_v7());
    let writes = [
        ("synthesize", json!({"query": marker})),
        (
            "propose_protocol",
            json!({"title": marker, "steps": [{"order": 1, "instruction": "x"}]}),
        ),
        (
            "add_observation",
            json!({"sample_id": Uuid::now_v7(), "content": marker}),
        ),
        (
            "countersign",
            json!({"claim_id": Uuid::now_v7(), "signature_meaning": "approved",
                   "signature_hex": "00".repeat(64), "public_key_hex": "00".repeat(32)}),
        ),
        ("attach_blob", json!({"file_bytes_base64": "aGVsbG8="})),
    ];
    for (tool, args) in writes {
        let reply = client.call_tool(tool, args).await;
        assert!(
            reply.error_message().contains("insufficient_scope"),
            "{tool}: got {}",
            reply.error_message()
        );
    }
    let written: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM protocols WHERE title = $1)
              + (SELECT count(*) FROM syntheses WHERE query = $1)",
    )
    .bind(&marker)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(written, 0, "a refused write tool must write nothing");

    // Control: the same token may call a read tool.
    let reply = client.call_tool("list_syntheses", json!({})).await;
    reply.result();
}

// T-A5. propose_protocol over HTTP stores authored_by = the token's agent,
// for two different callers on the same server. Kills: any server-wide
// identity (a service-agent fallback, a constant, the first caller cached).
#[tokio::test]
async fn propose_protocol_is_authored_by_the_calling_agent() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    for _ in 0..2 {
        let agent = seed_agent(&pool).await;
        let mut client = McpClient::new(addr, Some(mint_test_jwt(agent)));
        assert_eq!(client.initialize().await, reqwest::StatusCode::OK);
        let reply = client
            .call_tool(
                "propose_protocol",
                json!({"title": "caller identity", "steps": [{"order": 1, "instruction": "x"}]}),
            )
            .await;
        let id: Uuid = tool_json(reply.result())["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let authored_by: Uuid =
            sqlx::query_scalar("SELECT authored_by FROM protocols WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("protocol row");
        assert_eq!(authored_by, agent, "authored_by must be the caller");
    }
}

// The development opt-out attaches no caller: tools list, calls are refused.
// Kills: an opt-out that injects a permissive or service caller.
#[tokio::test]
async fn unauthenticated_mode_can_list_but_never_call() {
    let pool = connect().await;
    let blob_dir = tempfile::TempDir::new().expect("blob dir");
    let addr = start_mcp(
        pool.clone(),
        blob_dir.path().to_path_buf(),
        episcience_api::mcp::http::McpHttpAuth::Unauthenticated,
    )
    .await;
    let mut client = McpClient::new(addr, None);
    assert_eq!(client.initialize().await, reqwest::StatusCode::OK);
    let tools = client.list_tools().await;
    assert_eq!(
        tools.result()["tools"].as_array().unwrap().len(),
        TOOL_COUNT
    );
    let reply = client.call_tool("list_syntheses", json!({})).await;
    assert!(
        reply.error_message().starts_with("Unauthorized"),
        "got: {}",
        reply.error_message()
    );
}

// Defence in depth: a tool method reached without `call_tool` (no caller in
// the extensions) refuses instead of running as anyone.
#[tokio::test]
async fn tool_method_without_a_caller_is_refused() {
    use std::sync::Arc;
    let pool = connect().await;
    let embedder: Arc<dyn epigraph_embeddings::EmbeddingService> = Arc::new(
        epigraph_embeddings::MockProvider::new(epigraph_embeddings::EmbeddingConfig::openai(1536)),
    );
    let server = episcience_api::mcp::EpiscienceServer::new(
        pool,
        embedder,
        Arc::new(mcp_http::NoopEdgeWriter),
        std::env::temp_dir(),
        1024,
    );
    let err = server
        .list_syntheses(
            Parameters(episcience_api::mcp::queries::ListSynthesesArgs {
                limit: None,
                offset: None,
                include_stale: None,
                skill_name: None,
            }),
            rmcp::model::Extensions::new(),
        )
        .await
        .expect_err("no caller, no call");
    assert!(err.message.contains("no authenticated caller"));
}
