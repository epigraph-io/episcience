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

const TOOL_COUNT: usize = 9;

async fn connect() -> PgPool {
    // No default DSN: a stray run without the gate env must fail, not reach
    // whatever database listens on a default port.
    let dsn = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must name a migrated throwaway *_test database (no default)");
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

// T-A5 (synthesize twin). Two callers on ONE server each call `synthesize`;
// the synthesis row AND its job payload are owned by that caller, the owner
// sees the private row, and the other caller cannot get it. Kills: the
// synthesize arm ignoring the handed-over caller for the row owner
// (`create_pending_tx`) or for the job payload's `agent_id` (the worker acts
// as that agent), and any server-wide identity.
#[tokio::test]
async fn synthesize_is_owned_by_the_calling_agent() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let mut created = Vec::new();
    for _ in 0..2 {
        let agent = seed_agent(&pool).await;
        let mut client = McpClient::new(addr, Some(mint_test_jwt(agent)));
        assert_eq!(client.initialize().await, reqwest::StatusCode::OK);
        let marker = format!("e1a-synth-owner-{}", Uuid::now_v7());
        let reply = client
            .call_tool("synthesize", json!({"query": marker}))
            .await;
        let id: Uuid = tool_json(reply.result())["synthesis_id"]
            .as_str()
            .expect("synthesis_id")
            .parse()
            .expect("uuid");
        let (owner, visibility): (Uuid, String) =
            sqlx::query_as("SELECT agent_id, visibility FROM syntheses WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("synthesis row");
        assert_eq!(owner, agent, "syntheses.agent_id must be the caller");
        assert_eq!(visibility, "private", "default visibility");
        let payload_agent: Option<String> =
            sqlx::query_scalar("SELECT payload->>'agent_id' FROM synthesis_jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("job row");
        assert_eq!(
            payload_agent.as_deref(),
            Some(agent.to_string().as_str()),
            "the job payload's agent_id must be the caller"
        );
        created.push((agent, id, client));
    }

    // The owner reads its private synthesis; the other caller cannot.
    let (_, h1_synthesis, _) = &created[0];
    let h1_synthesis = *h1_synthesis;
    {
        let (_, _, h1) = &mut created[0];
        let listed = h1.call_tool("list_syntheses", json!({"limit": 500})).await;
        let ids: Vec<String> = tool_json(listed.result())
            .as_array()
            .expect("array")
            .iter()
            .map(|r| r["id"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(
            ids.contains(&h1_synthesis.to_string()),
            "the owner must list its own private synthesis"
        );
        let got = h1
            .call_tool("get_synthesis", json!({"synthesis_id": h1_synthesis}))
            .await;
        assert_eq!(
            tool_json(got.result())["id"].as_str(),
            Some(h1_synthesis.to_string().as_str())
        );
    }
    let (_, _, h2) = &mut created[1];
    let got = h2
        .call_tool("get_synthesis", json!({"synthesis_id": h1_synthesis}))
        .await;
    assert!(
        got.error_message().contains("not found"),
        "another caller must not read the private synthesis, got {}",
        got.error_message()
    );
}

/// Arguments that make each read tool SUCCEED for `synthesis` (owned by the
/// caller). Panics on a read tool with no entry, so a new read tool must be
/// added here (and so is covered by the scope tests below).
fn read_tool_args(tool: &str, synthesis: Uuid) -> serde_json::Value {
    match tool {
        "recall_synthesis" => json!({"query": "scope coverage"}),
        "get_synthesis" => json!({"synthesis_id": synthesis}),
        "list_syntheses" => json!({"limit": 500}),
        "list_countersignatures" => json!({"claim_id": Uuid::now_v7()}),
        other => panic!("read tool {other} has no arguments in read_tool_args"),
    }
}

// T-A3 (MCP read half). Every tool the scope table maps to `claims:read`
// SUCCEEDS over HTTP with a `claims:read`-only token, and is refused with
// insufficient_scope for a `claims:write`-only token (write does not imply
// read). Kills: mapping any read tool to `claims:write` (read-only callers
// would break), or skipping the scope check for reads.
#[tokio::test]
async fn read_tools_need_exactly_claims_read() {
    use episcience_api::auth::scopes::{CLAIMS_READ, MCP_TOOL_SCOPES};
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let agent = seed_agent(&pool).await;

    // A synthesis the caller owns, so get_synthesis has a row to return.
    let mut rw = McpClient::new(addr, Some(mint_test_jwt(agent)));
    assert_eq!(rw.initialize().await, reqwest::StatusCode::OK);
    let created = rw
        .call_tool("synthesize", json!({"query": "read scope fixture"}))
        .await;
    let synthesis: Uuid = tool_json(created.result())["synthesis_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let read_tools: Vec<&str> = MCP_TOOL_SCOPES
        .iter()
        .filter(|(_, scope)| *scope == CLAIMS_READ)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        read_tools.len(),
        4,
        "the scope table lists four read tools: {read_tools:?}"
    );

    let mut ro = McpClient::new(addr, Some(read_only_jwt(agent)));
    assert_eq!(ro.initialize().await, reqwest::StatusCode::OK);
    let write_only = mint(&TokenSpec {
        scopes: vec![token::CLAIMS_WRITE.to_string()],
        ..TokenSpec::valid(agent)
    });
    let mut wo = McpClient::new(addr, Some(write_only));
    assert_eq!(wo.initialize().await, reqwest::StatusCode::OK);

    for tool in read_tools {
        let args = read_tool_args(tool, synthesis);
        // `result()` panics on a JSON-RPC error, so this asserts success.
        let ok = ro.call_tool(tool, args.clone()).await;
        let _ = tool_json(ok.result());
        let refused = wo.call_tool(tool, args).await;
        assert!(
            refused.error_message().contains("insufficient_scope"),
            "{tool}: a claims:write-only token must be refused, got {}",
            refused.error_message()
        );
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
