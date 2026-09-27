//! Streamable-HTTP transport for the EpiScience MCP server: the bearer layer
//! and the router. Lives in the library (not the binary) so the tests drive
//! the exact stack the binary serves.
//!
//! Authentication happens in two places, on purpose:
//!
//! 1. [`bearer_auth_middleware`] runs on EVERY HTTP request (`initialize`,
//!    `tools/list`, `ping`, `tools/call`, the SSE GET). It verifies the
//!    signature, `iss`, `aud` and `exp` and refuses anything else with 401.
//!    It does NOT require a principal, because the kernel gateway's discovery
//!    session authenticates with a service token that carries no `agent_id`
//!    and only lists tools.
//! 2. `ServerHandler::call_tool` (in `mcp/mod.rs`) requires a principal and
//!    the tool's scope before any tool runs.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use uuid::Uuid;

use crate::mcp::EpiscienceServer;
use crate::middleware::JwtConfig;

/// A caller whose bearer passed [`JwtConfig::validate_token`]. `agent_id` is
/// optional here (discovery tokens have none); `call_tool` refuses a call
/// without one.
#[derive(Clone, Debug)]
pub struct McpCaller {
    pub agent_id: Option<Uuid>,
    pub client_id: Uuid,
    pub scopes: Vec<String>,
}

/// How the HTTP transport authenticates.
#[derive(Clone)]
pub enum McpHttpAuth {
    /// Production: every request must carry a valid kernel bearer.
    Bearer(Arc<JwtConfig>),
    /// `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` (local development only). No
    /// caller is ever attached, so every `tools/call` is refused; the server
    /// can only initialize and list tools.
    Unauthenticated,
}

/// Bearer layer: validate the token on every request and attach the caller.
pub async fn bearer_auth_middleware(
    State(jwt_config): State<Arc<JwtConfig>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    let Some(claims) = token.and_then(|t| jwt_config.validate_token(t).ok()) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Unauthorized: missing or invalid Bearer token",
        )
            .into_response();
    };

    request.extensions_mut().insert(McpCaller {
        agent_id: claims.agent_id,
        client_id: claims.sub,
        scopes: claims.scopes,
    });
    next.run(request).await
}

/// The `/mcp` router the binary serves, with the chosen auth layer applied.
pub fn router(server: EpiscienceServer, auth: McpHttpAuth) -> axum::Router {
    // `EpiscienceServer` is `Clone` (all state is Arc/cheap), so the
    // per-session factory just clones the prebuilt server.
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    match auth {
        McpHttpAuth::Bearer(jwt_config) => router.layer(axum::middleware::from_fn_with_state(
            jwt_config,
            bearer_auth_middleware,
        )),
        McpHttpAuth::Unauthenticated => router,
    }
}
