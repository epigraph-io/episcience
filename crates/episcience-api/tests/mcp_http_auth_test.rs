//! MCP caller identity and token acceptance over the REAL HTTP stack
//! (batch E1a: T-A1 MCP, T-A3 MCP, T-A5, T-A9, and the development opt-out).
//!
//! Every case goes through `episcience_api::mcp::http::router`, the router the
//! binary serves: bearer layer -> rmcp session -> `ServerHandler::call_tool`
//! -> tool. Direct `server.tool(..)` calls would bypass `call_tool` and could
//! not catch a removed gate.
//!
//! Run with `DATABASE_URL` pointing at a migrated throwaway `*_test` database.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

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

/// The run's shared clone of the E1 template (scripts/e1-test-db.sh). Refuses
/// port 5432 and any database name not ending in `_test`; no default DSN.
async fn connect() -> PgPool {
    testdb::shared_pool("DATABASE_URL").await
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

/// The federation gateway's discovery-token shape: a SERVICE client's token
/// with no `agent_id`, no `owner_id` and a non-claims scope, carrying every
/// claim the kernel's `EpiGraphClaims` requires (`sub` as a uuid, `iss`,
/// `aud`, `exp`, `iat`, `nbf`, `jti`, `scopes`, `client_type`).
fn discovery_spec() -> TokenSpec {
    TokenSpec {
        agent_id: None,
        scopes: vec!["episcience:tools".to_string()],
        client_type: "service".to_string(),
        ..TokenSpec::valid(Uuid::now_v7())
    }
}

// T-A9. A valid principal-less token (the gateway discovery shape above)
// initializes and lists every tool, and every tools/call is refused with
// principal_required. Kills: requiring the principal (or an owner) in the
// HTTP layer (discovery would break), or dropping it from call_tool (the call
// would run).
#[tokio::test]
async fn principal_less_token_lists_tools_but_cannot_call_them() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let discovery = mint(&discovery_spec());
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

// The claims the kernel's token type REQUIRES are required here too: a
// discovery-shaped token missing any one of `iat`, `nbf`, `jti` or
// `client_type` (or with a non-uuid `sub`) is refused at `initialize` with
// 401, while the complete shape is accepted. This is why a hand-minted
// gateway discovery token must carry them all (docs/kernel-pin.md). Kills: a
// validator that silently accepts a token the kernel's own validator refuses
// (for example a local claims type with serde defaults), which would make the
// deploy precondition on the discovery token's shape wrong.
#[tokio::test]
async fn discovery_token_missing_a_required_claim_is_refused() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;

    let mut client = McpClient::new(addr, Some(mint(&discovery_spec())));
    assert_eq!(client.initialize().await, reqwest::StatusCode::OK);

    for claim in ["iat", "nbf", "jti", "client_type"] {
        let token = mint(&TokenSpec {
            omit: vec![claim],
            ..discovery_spec()
        });
        let mut client = McpClient::new(addr, Some(token));
        assert_eq!(
            client.initialize().await,
            reqwest::StatusCode::UNAUTHORIZED,
            "a discovery token without `{claim}` must be refused"
        );
    }

    // `sub` present but not a uuid.
    let mut payload: serde_json::Value = serde_json::from_slice(&base64_url_decode(
        mint(&discovery_spec()).split('.').nth(1).expect("payload"),
    ))
    .expect("payload JSON");
    payload["sub"] = json!("gateway-discovery");
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &payload,
        &jsonwebtoken::EncodingKey::from_secret(&jwt_secret_bytes()),
    )
    .expect("re-sign");
    let mut client = McpClient::new(addr, Some(token));
    assert_eq!(
        client.initialize().await,
        reqwest::StatusCode::UNAUTHORIZED,
        "a non-uuid sub must be refused"
    );
}

fn base64_url_decode(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .expect("base64url")
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
        assert_eq!(
            visibility, "group",
            "default visibility (the kernel vocabulary)"
        );
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
        // A PUBLIC claim every caller can read (scripts/ci-seed.sql): the
        // tool answers "not found" for a claim the caller cannot read.
        "list_countersignatures" => {
            json!({"claim_id": Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa)})
        }
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

// A session is bound to the caller that opened it. Another caller's valid
// token on that session id gets rmcp's unknown-session answer (401 for
// tools/list and tools/call, 202 for DELETE), writes nothing, closes nothing,
// and the owner's session keeps working; a refreshed token of the SAME client and agent keeps the
// session; a caller cannot ride a principal-less (discovery) session; the
// owner's DELETE ends the session. Kills: removing the binding layer (the
// foreign call would run as the foreign caller and the foreign DELETE would
// tear the session down), binding on the raw token instead of the caller, or
// binding on the OAuth client alone (the same client with another agent).
#[tokio::test]
async fn a_session_is_bound_to_the_caller_that_opened_it() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let (h1, h2) = (seed_agent(&pool).await, seed_agent(&pool).await);
    let h1_client_id = Uuid::now_v7();
    let h1_token = |_: ()| {
        mint(&TokenSpec {
            sub: Some(h1_client_id),
            ..TokenSpec::valid(h1)
        })
    };

    let mut owner = McpClient::new(addr, Some(h1_token(())));
    assert_eq!(owner.initialize().await, reqwest::StatusCode::OK);
    let session = owner.session().expect("session id");

    let marker = format!("e1a-session-{}", Uuid::now_v7());
    let mut intruder = McpClient::on_session(addr, Some(mint_test_jwt(h2)), session.clone());
    assert_eq!(
        intruder.list_tools().await.status,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let call = intruder
        .call_tool(
            "propose_protocol",
            json!({"title": marker, "steps": [{"order": 1, "instruction": "x"}]}),
        )
        .await;
    assert_eq!(call.status, reqwest::StatusCode::UNAUTHORIZED);
    // rmcp answers DELETE of an unknown id with 202, so a foreign DELETE gets
    // 202 too, but closes nothing: the refreshed owner below still lists, and
    // the same-client call below is still refused (the binding survived).
    assert_eq!(
        intruder.delete_session().await,
        reqwest::StatusCode::ACCEPTED
    );
    // The SAME OAuth client carrying a different agent is a different
    // principal: refused too (the binding is (client, agent), not the client).
    let mut same_client = McpClient::on_session(
        addr,
        Some(mint(&TokenSpec {
            sub: Some(h1_client_id),
            ..TokenSpec::valid(h2)
        })),
        session.clone(),
    );
    let call = same_client
        .call_tool(
            "propose_protocol",
            json!({"title": marker, "steps": [{"order": 1, "instruction": "x"}]}),
        )
        .await;
    assert_eq!(call.status, reqwest::StatusCode::UNAUTHORIZED);
    let written: i64 = sqlx::query_scalar("SELECT count(*) FROM protocols WHERE title = $1")
        .bind(&marker)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(written, 0, "a foreign caller must not act on the session");

    // The owner's session survived, including with a refreshed token (new jti
    // and exp, same client and agent).
    let mut refreshed = McpClient::on_session(addr, Some(h1_token(())), session.clone());
    let tools = refreshed.list_tools().await;
    assert_eq!(
        tools.result()["tools"].as_array().unwrap().len(),
        TOOL_COUNT
    );

    // A caller cannot ride a principal-less discovery session.
    let mut discovery = McpClient::new(
        addr,
        Some(mint(&TokenSpec {
            agent_id: None,
            scopes: vec!["episcience:tools".to_string()],
            ..TokenSpec::valid(Uuid::now_v7())
        })),
    );
    assert_eq!(discovery.initialize().await, reqwest::StatusCode::OK);
    let mut rider = McpClient::on_session(
        addr,
        Some(mint_test_jwt(h2)),
        discovery.session().expect("discovery session"),
    );
    let ride = rider
        .call_tool(
            "propose_protocol",
            json!({"title": marker, "steps": [{"order": 1, "instruction": "x"}]}),
        )
        .await;
    assert_eq!(ride.status, reqwest::StatusCode::UNAUTHORIZED);

    // The owner ends its own session.
    assert!(owner.delete_session().await.is_success());
    assert_eq!(
        refreshed.list_tools().await.status,
        reqwest::StatusCode::UNAUTHORIZED,
        "a deleted session is gone"
    );
}

/// One raw HTTP exchange on `/mcp`, reduced to what a caller can observe:
/// status, the sorted header names and values (minus `date`), and the body.
async fn raw_exchange(
    addr: std::net::SocketAddr,
    method: reqwest::Method,
    token: &str,
    session: &str,
    body: Option<serde_json::Value>,
) -> (reqwest::StatusCode, Vec<(String, String)>, Vec<u8>) {
    let mut req = reqwest::Client::new()
        .request(method, format!("http://{addr}/mcp"))
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session);
    if let Some(b) = body {
        req = req
            .header("content-type", "application/json")
            .body(b.to_string());
    }
    let resp = req.send().await.expect("send");
    let status = resp.status();
    let mut headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str() != "date")
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("?").to_string()))
        .collect();
    headers.sort();
    // A request that reaches a live session's SSE stream never completes;
    // bound the read so that case fails instead of hanging the suite.
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), resp.bytes())
        .await
        .unwrap_or_else(|_| panic!("no complete answer within 10 s (status {status}): the request reached a live session"))
        .expect("body")
        .to_vec();
    (status, headers, body)
}

// Review E1a-D3: for a well-formed POST (tools/list, tools/call), GET (SSE)
// and DELETE, a session bound to ANOTHER caller is answered exactly as rmcp
// answers a session id that does not exist (status, headers, body), so a
// caller holding a candidate id cannot learn that it exists; and the foreign
// DELETE closes nothing. Kills: the refusal built as a `(StatusCode, &str)`
// tuple (adds a `content-type`), a foreign DELETE answered 401 (rmcp says 202
// for an unknown id), and a foreign DELETE forwarded to rmcp (it would close
// the owner's session).
#[tokio::test]
async fn a_foreign_session_is_answered_like_an_unknown_one() {
    let pool = connect().await;
    let (addr, _blobs) = start(&pool).await;
    let (h1, h2) = (seed_agent(&pool).await, seed_agent(&pool).await);

    let mut owner = McpClient::new(addr, Some(mint_test_jwt(h1)));
    assert_eq!(owner.initialize().await, reqwest::StatusCode::OK);
    let bound = owner.session().expect("session id");
    let unknown = Uuid::new_v4().to_string();
    let intruder = mint_test_jwt(h2);

    let list = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {}});
    let call = json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call",
        "params": {"name": "list_syntheses", "arguments": {}}});
    let cases = [
        ("POST tools/list", reqwest::Method::POST, Some(list)),
        ("POST tools/call", reqwest::Method::POST, Some(call)),
        ("GET", reqwest::Method::GET, None),
        ("DELETE", reqwest::Method::DELETE, None),
    ];
    for (name, method, body) in cases {
        let on_unknown =
            raw_exchange(addr, method.clone(), &intruder, &unknown, body.clone()).await;
        let on_bound = raw_exchange(addr, method, &intruder, &bound, body).await;
        assert!(
            on_unknown.0.is_client_error() || on_unknown.0 == reqwest::StatusCode::ACCEPTED,
            "{name}: control must be rmcp's unknown-session answer, got {}",
            on_unknown.0
        );
        assert_eq!(on_bound, on_unknown, "{name}: foreign != unknown");
    }

    // Nothing was closed: the owner still lists on its session.
    let tools = owner.list_tools().await;
    assert_eq!(
        tools.result()["tools"].as_array().unwrap().len(),
        TOOL_COUNT
    );
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
    let server =
        episcience_api::mcp::EpiscienceServer::new(pool, embedder, std::env::temp_dir(), 1024);
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
