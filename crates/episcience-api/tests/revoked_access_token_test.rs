//! Access-token revocation on BOTH bearer listeners (backlog b91ec7d3).
//!
//! The kernel's `POST /oauth/revoke` records a revoked access token's `jti`
//! in `revoked_access_tokens` (kernel migration 141). A revoked token keeps a
//! valid signature and `exp`, so a gate that only validates the token admits
//! it until it expires. Both EpiScience gates (REST
//! `middleware::bearer_auth_middleware` and MCP
//! `mcp::http::bearer_auth_middleware`) must refuse it with 401, and must
//! refuse EVERY token with 503 when the denylist cannot be read (fail closed).
//!
//! The token is revoked through the kernel's own write path
//! (`RevokedAccessTokenRepository::revoke`, the definer `/oauth/revoke`
//! calls). Every refusal is shown on a token the same server admitted a
//! moment before, so it cannot pass for an unrelated reason.
//!
//! Run under `scripts/e1-test-db.sh` (`DATABASE_URL` is the run's shared
//! clone; the fail-closed cases take a fresh clone each).
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use std::sync::Arc;

use axum::http::StatusCode;
use axum_test::TestServer;
use epigraph_db::RevokedAccessTokenRepository;
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::middleware::{
    JwtConfig, INSUFFICIENT_SCOPE, PRINCIPAL_REQUIRED, TOKEN_REVOKED,
};
use episcience_api::state::ElnState;
use sqlx::PgPool;
use uuid::Uuid;

#[path = "support/token.rs"]
mod token;
use token::{jwt_secret_bytes, mint, TokenSpec};

#[path = "support/mcp_http.rs"]
mod mcp_http;
use mcp_http::{bearer_auth, start_mcp, McpClient};

#[path = "support/write_routes.rs"]
mod write_routes;
use write_routes::bearer;

/// A REST route any valid read token may GET (no fixture needed).
const READ_ROUTE: &str = "/api/v1/eln/samples";

/// Revoke `token` exactly as the kernel's `/oauth/revoke` does: the `jti`,
/// client (`sub`) and expiry of the signature-verified token, through the
/// kernel's definer. Returns the `jti`.
async fn revoke(admin: &PgPool, token: &str) -> Uuid {
    let claims = JwtConfig::from_secret(&jwt_secret_bytes())
        .validate_token(token)
        .expect("the token to revoke is valid");
    let exp = chrono::DateTime::from_timestamp(claims.exp, 0).expect("exp is a timestamp");
    let written = RevokedAccessTokenRepository::revoke(admin, claims.jti, claims.sub, exp)
        .await
        .expect("the kernel's revoke definer");
    assert!(written, "a fresh revocation writes its row");
    claims.jti
}

async fn rest_server(admin: &PgPool) -> TestServer {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let state = ElnState {
        db: testdb::app_db_for(admin).await,
        blob_dir: std::env::temp_dir().join("episcience-revoked-token-blobs"),
        jwt_config: Arc::new(JwtConfig::from_secret(&jwt_secret_bytes())),
        max_upload_bytes: 1024 * 1024,
        embedder,
    };
    let _ = std::fs::create_dir_all(&state.blob_dir);
    TestServer::new(episcience_api::create_router(state)).expect("build TestServer")
}

async fn rest_get(server: &TestServer, token: &str) -> (StatusCode, String) {
    let (name, value) = bearer(token);
    let resp = server.get(READ_ROUTE).add_header(name, value).await;
    (resp.status_code(), resp.text())
}

async fn mcp_server(admin: &PgPool) -> (std::net::SocketAddr, tempfile::TempDir) {
    let blobs = tempfile::TempDir::new().expect("blob dir");
    let addr = start_mcp(
        admin.clone(),
        blobs.path().to_path_buf(),
        bearer_auth(&jwt_secret_bytes()),
    )
    .await;
    (addr, blobs)
}

/// A raw MCP `initialize` POST that keeps the response body (the shared
/// `McpClient` drops it on a non-2xx status). Returns `(status, body)`.
async fn mcp_initialize_raw(
    addr: std::net::SocketAddr,
    token: &str,
) -> (reqwest::StatusCode, String) {
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "episcience-test", "version": "0"}
                }
            })
            .to_string(),
        )
        .send()
        .await
        .expect("send MCP initialize");
    let status = resp.status();
    // Only refusals are read to the end; a 200 is an open SSE stream.
    let body = if status.is_success() {
        String::new()
    } else {
        resp.text().await.expect("read refusal body")
    };
    (status, body)
}

/// The 503 body must not carry the database error the gate logged.
fn assert_no_db_detail(body: &str) {
    assert!(
        !body.contains("permission denied") && !body.contains("revoked_access_tokens"),
        "the 503 body leaks no database detail: {body}"
    );
}

/// The federation gateway's discovery shape: no `agent_id`, a service client.
fn discovery_token() -> String {
    mint(&TokenSpec {
        agent_id: None,
        scopes: vec!["episcience:tools".to_string()],
        client_type: "service".to_string(),
        ..TokenSpec::valid(Uuid::now_v7())
    })
}

/// Make the denylist unreadable for the application login (what a database
/// without kernel migration 141, or without its SELECT grant, looks like to
/// the gates), and prove that it is: the application login reads it before
/// and cannot after. The before half pins the grant the gates depend on:
/// `episcience_app` reads the table through its `epigraph_app` membership.
async fn break_the_denylist(admin: &PgPool) {
    let readable = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT has_table_privilege('episcience_app', 'public.revoked_access_tokens', 'SELECT')",
        )
        .fetch_one(admin)
        .await
        .expect("privilege probe")
    };
    assert!(
        readable().await,
        "the application login must be able to read the denylist on an unmutated clone"
    );
    sqlx::query("REVOKE SELECT ON public.revoked_access_tokens FROM epigraph_app")
        .execute(admin)
        .await
        .expect("revoke the application role's SELECT");
    assert!(
        !readable().await,
        "after the REVOKE no other grant may still let the application login read it"
    );
}

// REST: a token the server admits is refused with 401 once its jti is
// revoked; a different, unrevoked token of the same agent is still admitted.
// Kills: a gate that never consults the denylist, one that keys it on
// anything but the token's own jti, or one that refuses with another status.
#[tokio::test]
async fn rest_refuses_a_revoked_token_and_admits_an_unrevoked_one() {
    let admin = testdb::shared_pool("DATABASE_URL").await;
    let server = rest_server(&admin).await;
    let agent = Uuid::now_v7();
    let token = mint(&TokenSpec::valid(agent));
    let other = mint(&TokenSpec::valid(agent));

    assert_eq!(
        rest_get(&server, &token).await.0,
        StatusCode::OK,
        "control: the token is admitted before it is revoked"
    );
    revoke(&admin, &token).await;

    let (status, body) = rest_get(&server, &token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked token: {body}");
    assert!(
        body.contains(TOKEN_REVOKED),
        "the refusal names the cause ({TOKEN_REVOKED}): {body}"
    );
    assert_eq!(
        rest_get(&server, &other).await.0,
        StatusCode::OK,
        "an unrevoked token is unaffected"
    );
}

// REST, fail closed: when the denylist cannot be read, a token the server
// admitted a moment before is refused with 503, never admitted; the body
// carries no database detail. Kills: treating a lookup error as "not
// revoked" (fail open), or answering it as 401/500.
#[tokio::test]
async fn rest_refuses_every_token_with_503_when_the_revocation_lookup_fails() {
    let db = testdb::TestDb::fresh().await;
    let server = rest_server(&db.admin).await;
    let token = mint(&TokenSpec::valid(Uuid::now_v7()));
    assert_eq!(rest_get(&server, &token).await.0, StatusCode::OK, "control");

    break_the_denylist(&db.admin).await;

    let (status, body) = rest_get(&server, &token).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_no_db_detail(&body);
    // A request with no valid token is still a 401 (the lookup runs only on a
    // signature-verified token).
    let (status, _) = rest_get(&server, "not-a-jwt").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// REST, placement: the revocation check runs right after signature
// validation, before the principal and scope checks. A principal-less token
// and a token without the route's scope are each refused for THAT reason
// before revocation, and with 401 token_revoked after it. Kills: moving the
// check behind `AuthContext::from_claims`, the scope check or
// `resolve_principal` (the revoked token would then still be refused, but as
// principal_required / insufficient_scope).
#[tokio::test]
async fn rest_checks_revocation_before_the_principal_and_scope_checks() {
    let admin = testdb::shared_pool("DATABASE_URL").await;
    let server = rest_server(&admin).await;
    let principal_less = mint(&TokenSpec {
        agent_id: None,
        ..TokenSpec::valid(Uuid::now_v7())
    });
    let scopeless = mint(&TokenSpec {
        scopes: vec![],
        ..TokenSpec::valid(Uuid::now_v7())
    });

    for (token, before_status, before_cause) in [
        (
            &principal_less,
            StatusCode::UNAUTHORIZED,
            PRINCIPAL_REQUIRED,
        ),
        (&scopeless, StatusCode::FORBIDDEN, INSUFFICIENT_SCOPE),
    ] {
        let (status, body) = rest_get(&server, token).await;
        assert_eq!(status, before_status, "control ({before_cause}): {body}");
        assert!(
            body.contains(before_cause) && !body.contains(TOKEN_REVOKED),
            "control: refused as {before_cause} before revocation: {body}"
        );
        revoke(&admin, token).await;
        let (status, body) = rest_get(&server, token).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{before_cause} token, revoked: {body}"
        );
        assert!(
            body.contains(TOKEN_REVOKED),
            "a revoked {before_cause} token is refused as revoked: {body}"
        );
    }
}

// MCP: a revoked token cannot open a session, and a session opened with it
// BEFORE the revocation is refused on its next request (the bearer layer runs
// on every HTTP request, so no session outlives its token's revocation). An
// unrevoked token still initializes. Kills: checking only at initialize, or
// not at all.
#[tokio::test]
async fn mcp_refuses_a_revoked_token_on_new_and_existing_sessions() {
    let admin = testdb::shared_pool("DATABASE_URL").await;
    let (addr, _blobs) = mcp_server(&admin).await;
    let token = mint(&TokenSpec::valid(Uuid::now_v7()));

    let mut open = McpClient::new(addr, Some(token.clone()));
    assert_eq!(
        open.initialize().await,
        reqwest::StatusCode::OK,
        "control: the token opens a session before it is revoked"
    );
    let session = open.session().expect("initialize assigned a session");
    assert_eq!(
        open.list_tools().await.status,
        reqwest::StatusCode::OK,
        "control: the session serves before the revocation"
    );

    revoke(&admin, &token).await;

    let mut same_session = McpClient::on_session(addr, Some(token.clone()), session);
    assert_eq!(
        same_session.list_tools().await.status,
        reqwest::StatusCode::UNAUTHORIZED,
        "the existing session is refused once its token is revoked"
    );
    let (status, body) = mcp_initialize_raw(addr, &token).await;
    assert_eq!(
        status,
        reqwest::StatusCode::UNAUTHORIZED,
        "a revoked token cannot open a session: {body}"
    );
    assert!(
        body.contains(TOKEN_REVOKED),
        "the MCP refusal names the cause ({TOKEN_REVOKED}): {body}"
    );
    let mut unrevoked = McpClient::new(addr, Some(mint(&TokenSpec::valid(Uuid::now_v7()))));
    assert_eq!(unrevoked.initialize().await, reqwest::StatusCode::OK);
}

// MCP: the principal-less discovery token is checked too (the bearer layer,
// not `call_tool`, owns revocation). Kills: putting the check behind the
// principal requirement, where a discovery token never reaches it.
#[tokio::test]
async fn mcp_refuses_a_revoked_discovery_token() {
    let admin = testdb::shared_pool("DATABASE_URL").await;
    let (addr, _blobs) = mcp_server(&admin).await;
    let token = discovery_token();

    let mut client = McpClient::new(addr, Some(token.clone()));
    assert_eq!(
        client.initialize().await,
        reqwest::StatusCode::OK,
        "control"
    );
    revoke(&admin, &token).await;
    let (status, body) = mcp_initialize_raw(addr, &token).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{body}");
    assert!(
        body.contains(TOKEN_REVOKED),
        "the discovery token is refused as revoked: {body}"
    );
}

// MCP, fail closed: an unreadable denylist refuses a token the server
// admitted a moment before with 503. Kills: a fail-open lookup on the MCP
// listener.
#[tokio::test]
async fn mcp_refuses_every_token_with_503_when_the_revocation_lookup_fails() {
    let db = testdb::TestDb::fresh().await;
    let (addr, _blobs) = mcp_server(&db.admin).await;
    let token = mint(&TokenSpec::valid(Uuid::now_v7()));
    let discovery = discovery_token();
    for (what, t) in [("agent token", &token), ("discovery token", &discovery)] {
        let mut client = McpClient::new(addr, Some(t.clone()));
        assert_eq!(
            client.initialize().await,
            reqwest::StatusCode::OK,
            "control: the {what} is admitted while the denylist is readable"
        );
    }

    break_the_denylist(&db.admin).await;

    for (what, t) in [("agent token", &token), ("discovery token", &discovery)] {
        let (status, body) = mcp_initialize_raw(addr, t).await;
        assert_eq!(
            status,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "the {what} is refused when the lookup fails: {body}"
        );
        assert_no_db_detail(&body);
    }
}
