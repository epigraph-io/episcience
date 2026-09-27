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
//!
//! A session is also BOUND to the caller that opened it
//! ([`session_binding_middleware`]): a request carrying another caller's
//! `mcp-session-id` gets rmcp's own unknown-session answer, so one valid token
//! cannot drive or tear down another caller's session. The kernel gateway is
//! unaffected: it lists tools on its own discovery session and invokes each
//! tool on a fresh per-call session opened with the caller's bearer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

/// rmcp's session header (`rmcp::transport::common::http_header::HEADER_SESSION_ID`).
const SESSION_HEADER: &str = "mcp-session-id";

/// Upper bound on remembered session owners. Sessions the client deletes are
/// forgotten at once; this only bounds sessions that expire without a DELETE.
/// On overflow the oldest bindings are dropped (those sessions become unbound,
/// which is the pre-binding behaviour, never a refusal of a live session).
pub const MAX_BOUND_SESSIONS: usize = 10_000;

/// Who opened a session: the token's OAuth client and its agent (`None` for a
/// principal-less discovery token). A refreshed token of the same client and
/// agent keeps the session.
type SessionOwner = (Uuid, Option<Uuid>);

/// Session id -> the caller that opened it, with a bind sequence number (the
/// eviction order).
#[derive(Clone, Default)]
pub struct SessionOwners(Arc<Mutex<BoundSessions>>);

#[derive(Default)]
pub struct BoundSessions {
    by_id: HashMap<String, (SessionOwner, u64)>,
    next_seq: u64,
}

impl SessionOwners {
    fn lock(&self) -> std::sync::MutexGuard<'_, BoundSessions> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn owner_of(&self, session: &str) -> Option<SessionOwner> {
        self.lock().by_id.get(session).map(|(owner, _)| *owner)
    }

    fn bind(&self, session: String, owner: SessionOwner) {
        let mut inner = self.lock();
        if inner.by_id.len() >= MAX_BOUND_SESSIONS {
            let mut by_age: Vec<(u64, String)> = inner
                .by_id
                .iter()
                .map(|(k, (_, seq))| (*seq, k.clone()))
                .collect();
            by_age.sort_unstable();
            for (_, k) in by_age.into_iter().take(MAX_BOUND_SESSIONS / 10 + 1) {
                inner.by_id.remove(&k);
            }
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.by_id.insert(session, (owner, seq));
    }

    fn forget(&self, session: &str) {
        self.lock().by_id.remove(session);
    }

    /// Number of remembered bindings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().by_id.len()
    }

    /// `true` when no session is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Session binding layer (runs INSIDE the bearer layer, so the caller is
/// already attached). A request that names a session opened by a different
/// caller is answered exactly like an unknown session (rmcp: 401
/// "Unauthorized: Session not found") and never reaches the session. The
/// session a successful `initialize` creates is bound to its caller; a
/// successful DELETE forgets it.
pub async fn session_binding_middleware(
    State(owners): State<SessionOwners>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(caller) = request.extensions().get::<McpCaller>().cloned() else {
        // Unreachable behind the bearer layer; refuse rather than run unbound.
        return (StatusCode::UNAUTHORIZED, "Unauthorized: no caller").into_response();
    };
    let owner: SessionOwner = (caller.client_id, caller.agent_id);
    let session = request
        .headers()
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(session) = session.as_deref() {
        if owners.owner_of(session).is_some_and(|bound| bound != owner) {
            return (StatusCode::UNAUTHORIZED, "Unauthorized: Session not found").into_response();
        }
    }
    let is_delete = request.method() == axum::http::Method::DELETE;
    let response = next.run(request).await;
    if response.status().is_success() {
        match session {
            Some(session) if is_delete => owners.forget(&session),
            Some(_) => {}
            None => {
                if let Some(created) = response
                    .headers()
                    .get(SESSION_HEADER)
                    .and_then(|v| v.to_str().ok())
                {
                    owners.bind(created.to_owned(), owner);
                }
            }
        }
    }
    response
}

/// The `/mcp` router the binary serves, with the chosen auth layer applied.
pub fn router(server: EpiscienceServer, auth: McpHttpAuth) -> axum::Router {
    let owners = SessionOwners::default();
    // `EpiscienceServer` is `Clone` (all state is Arc/cheap), so the
    // per-session factory just clones the prebuilt server.
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    match auth {
        // Layers wrap outward: the bearer layer (added last) runs first and
        // attaches the caller the binding layer reads.
        McpHttpAuth::Bearer(jwt_config) => router
            .layer(axum::middleware::from_fn_with_state(
                owners,
                session_binding_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                jwt_config,
                bearer_auth_middleware,
            )),
        McpHttpAuth::Unauthenticated => router,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Kills: an unbounded table, or evicting the NEWEST bindings (a live,
    // just-opened session would lose its binding first).
    #[test]
    fn bound_sessions_are_capped_and_the_oldest_go_first() {
        let owners = SessionOwners::default();
        let who = (Uuid::nil(), None);
        let total = MAX_BOUND_SESSIONS + 5;
        for i in 0..total {
            owners.bind(format!("s{i}"), who);
        }
        assert!(owners.len() <= MAX_BOUND_SESSIONS, "{}", owners.len());
        assert!(owners.owner_of("s0").is_none(), "the oldest is evicted");
        assert_eq!(owners.owner_of(&format!("s{}", total - 1)), Some(who));
        owners.forget(&format!("s{}", total - 1));
        assert!(owners.owner_of(&format!("s{}", total - 1)).is_none());
    }
}
