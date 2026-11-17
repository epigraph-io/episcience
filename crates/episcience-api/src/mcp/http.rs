//! Streamable-HTTP transport for the EpiScience MCP server: the bearer layer
//! and the router. Lives in the library (not the binary) so the tests drive
//! the exact stack the binary serves.
//!
//! Authentication happens in two places, on purpose:
//!
//! 1. [`bearer_auth_middleware`] runs on EVERY HTTP request (`initialize`,
//!    `tools/list`, `ping`, `tools/call`, the SSE GET). It verifies the
//!    signature, `iss`, `aud` and `exp` and refuses anything else with 401,
//!    then refuses a token whose `jti` the kernel has revoked (401) and every
//!    token while the revocation denylist cannot be read (503, fail closed).
//!    Two audiences are admitted here and RECORDED on the caller
//!    ([`TokenAudience`]): `episcience` (the short-lived token the kernel
//!    gateway obtains by RFC 8693 token exchange and forwards on `tools/call`)
//!    and the kernel's `epigraph-api` (the gateway's discovery session still
//!    sends one to initialize and list tools).
//!    Because it runs on every request, a session opened before its token was
//!    revoked is refused from the next request on. It does NOT require a
//!    principal, because the kernel gateway's discovery session
//!    authenticates with a service token that carries no `agent_id` and only
//!    lists tools.
//! 2. `ServerHandler::call_tool` (in `mcp/mod.rs`) requires an
//!    `episcience`-audience token, a principal and the tool's scope before any
//!    tool runs. A kernel-audience token is refused on `tools/call` unless the
//!    deploy-transition switch [`KernelAudienceOnCall::Admit`]
//!    (`EPISCIENCE_MCP_ACCEPT_KERNEL_AUDIENCE_ON_CALL`, default off) is set.
//!
//! A session is also BOUND to the caller that opened it
//! ([`session_binding_middleware`]): a well-formed request carrying another
//! caller's `mcp-session-id` gets the same answer rmcp gives for an unknown
//! session id and never reaches the session, so one valid token cannot drive
//! or tear down another caller's session. The kernel gateway is unaffected: it
//! lists tools on its own discovery session and invokes each tool on a fresh
//! per-call session opened with the caller's bearer.

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
use crate::middleware::{EpiGraphClaims, JwtConfig, EXPECTED_ISSUER};
use episcience_db::tenancy::EpiscienceDb;

/// The audience of the token the kernel gateway forwards on `tools/call`: the
/// gateway exchanges the caller's kernel token (RFC 8693) for one minted with
/// `aud` = the extension's mount name, which is `episcience`.
pub const EXTENSION_AUDIENCE: &str = "episcience";

/// Prefix of the `tools/call` refusal for a kernel-audience token in strict
/// mode (`authorize_tool_call`).
pub const EXTENSION_AUDIENCE_REQUIRED: &str = "extension_audience_required";

/// Which audience admitted a bearer at the HTTP layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenAudience {
    /// `aud = episcience`: a token minted for this server (by the kernel's
    /// token exchange). The only audience `tools/call` accepts by default.
    Extension,
    /// `aud = epigraph-api`: a kernel token. Always enough to initialize and
    /// list tools (the gateway's discovery session); enough for `tools/call`
    /// only under [`KernelAudienceOnCall::Admit`].
    Kernel,
}

/// The deploy-transition switch for kernel-audience tokens on `tools/call`
/// (`config::KERNEL_AUDIENCE_ON_CALL_VAR`). [`Self::Refuse`] is the default
/// and the end state; [`Self::Admit`] exists only so this server can be
/// deployed before the kernel gateway that exchanges tokens, and is turned off
/// once that gateway is live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KernelAudienceOnCall {
    /// Strict: `tools/call` requires an `episcience`-audience token.
    #[default]
    Refuse,
    /// Transition: `tools/call` also accepts a kernel-audience token.
    Admit,
}

/// Validates a bearer minted for ANOTHER resource server than the kernel,
/// exactly as the kernel validates its own.
///
/// This is a mirror of `epigraph_auth::JwtConfig::validate_token_for_audience`
/// (epigraph-io/epigraph#931): same crate and version of `jsonwebtoken`, HS256
/// only, `iss = "epigraph"`, `aud` equal to the given audience (a single
/// string; an array fails to decode), a required `exp` with zero leeway, and
/// the kernel's own `EpiGraphClaims` type (every claim the kernel mints is
/// required). It is local only because the kernel pin predates #931: moving
/// the pin is one rev for every kernel crate and is bounded by the deployed
/// kernel schema (`docs/kernel-pin.md`). When the pin includes #931, replace
/// this type with `JwtConfig::validate_token_for_audience`. The unit test
/// `extension_validator_decides_exactly_as_the_kernel_validator` keeps the two
/// in step until then.
#[derive(Clone)]
pub struct ExtensionAudienceValidator {
    decoding_key: jsonwebtoken_kernel::DecodingKey,
}

impl ExtensionAudienceValidator {
    /// From the shared HMAC secret (the one `JwtConfig::from_secret` takes).
    #[must_use]
    pub fn from_secret(secret: &[u8]) -> Self {
        Self {
            decoding_key: jsonwebtoken_kernel::DecodingKey::from_secret(secret),
        }
    }

    /// `JwtConfig::validate_token_for_audience` (see the type's docs).
    ///
    /// # Errors
    /// The `jsonwebtoken` error the kernel's validator would return.
    pub fn validate_token_for_audience(
        &self,
        token: &str,
        audience: &str,
    ) -> Result<EpiGraphClaims, jsonwebtoken_kernel::errors::Error> {
        use jsonwebtoken_kernel::{decode, Algorithm, Validation};
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[EXPECTED_ISSUER]);
        validation.set_audience(&[audience]);
        validation.leeway = 0;
        let data = decode::<EpiGraphClaims>(token, &self.decoding_key, &validation)?;
        Ok(data.claims)
    }
}

/// A caller whose bearer passed the bearer layer, with the audience that
/// admitted it. `agent_id` is optional here (discovery tokens have none);
/// `call_tool` refuses a call without one.
#[derive(Clone, Debug)]
pub struct McpCaller {
    pub agent_id: Option<Uuid>,
    pub client_id: Uuid,
    pub owner_id: Option<Uuid>,
    pub client_type: String,
    pub scopes: Vec<String>,
    /// The audience that admitted the bearer.
    pub audience: TokenAudience,
    /// The listener's transition switch, as configured at boot.
    pub kernel_audience_on_call: KernelAudienceOnCall,
}

/// Production bearer configuration: the kernel's validator, the extension
/// audience validator over the same secret, and the transition switch.
#[derive(Clone)]
pub struct BearerAuth {
    pub kernel: Arc<JwtConfig>,
    pub extension: Arc<ExtensionAudienceValidator>,
    pub kernel_audience_on_call: KernelAudienceOnCall,
}

impl BearerAuth {
    /// Both validators from the one shared HMAC secret.
    #[must_use]
    pub fn from_secret(secret: &[u8], kernel_audience_on_call: KernelAudienceOnCall) -> Self {
        Self {
            kernel: Arc::new(JwtConfig::from_secret(secret)),
            extension: Arc::new(ExtensionAudienceValidator::from_secret(secret)),
            kernel_audience_on_call,
        }
    }

    /// Validate `token` for this server first, then as a kernel token, and
    /// say which audience admitted it. `None` when neither does.
    #[must_use]
    pub fn admit(&self, token: &str) -> Option<(EpiGraphClaims, TokenAudience)> {
        if let Ok(claims) = self
            .extension
            .validate_token_for_audience(token, EXTENSION_AUDIENCE)
        {
            return Some((claims, TokenAudience::Extension));
        }
        self.kernel
            .validate_token(token)
            .ok()
            .map(|claims| (claims, TokenAudience::Kernel))
    }
}

/// How the HTTP transport authenticates.
#[derive(Clone)]
pub enum McpHttpAuth {
    /// Production: every request must carry a valid bearer for this server
    /// (`episcience`) or the kernel (`epigraph-api`).
    Bearer(BearerAuth),
    /// `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` (local development only). No
    /// caller is ever attached, so every `tools/call` is refused; the server
    /// can only initialize and list tools.
    Unauthenticated,
}

/// What the bearer layer checks a token against: the two validators and the
/// database holding the kernel's revocation denylist (the server's own
/// [`EpiscienceDb`], so the check reads the database the tools run on).
#[derive(Clone)]
pub struct BearerState {
    pub auth: BearerAuth,
    pub db: EpiscienceDb,
}

/// Bearer layer: validate the token on every request, refuse a revoked one
/// (whichever audience admitted it), and attach the caller with its audience.
pub async fn bearer_auth_middleware(
    State(BearerState { auth, db }): State<BearerState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    let Some((claims, audience)) = token.and_then(|t| auth.admit(t)) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Unauthorized: missing or invalid Bearer token",
        )
            .into_response();
    };

    // The REST gate's check and wording (`middleware::refuse_revoked`): 401
    // when revoked, 503 when the lookup fails (logged there, never returned).
    if let Err(refusal) = crate::middleware::refuse_revoked(&db, claims.jti).await {
        return refusal.into_response();
    }

    request.extensions_mut().insert(McpCaller {
        agent_id: claims.agent_id,
        client_id: claims.sub,
        owner_id: claims.owner_id,
        client_type: claims.client_type,
        scopes: claims.scopes,
        audience,
        kernel_audience_on_call: auth.kernel_audience_on_call,
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

/// rmcp 0.15's answer to a well-formed request naming an unknown session id
/// (`streamable_http_server/tower.rs`): `POST`/`GET` get 401 with this body
/// and no `content-type`; `DELETE` gets 202 with an empty body, because
/// `LocalSessionManager::close_session` accepts an unknown id. Built with
/// `Response::builder` so no `content-type` is added (a `(StatusCode, &str)`
/// tuple would add `text/plain`).
fn unknown_session_answer(method: &axum::http::Method) -> Response {
    let (status, body) = if method == axum::http::Method::DELETE {
        (StatusCode::ACCEPTED, Body::empty())
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Body::from("Unauthorized: Session not found"),
        )
    };
    Response::builder()
        .status(status)
        .body(body)
        .unwrap_or_else(|_| StatusCode::UNAUTHORIZED.into_response())
}

/// Session binding layer (runs INSIDE the bearer layer, so the caller is
/// already attached). A `POST`, `GET` or `DELETE` that names a session opened
/// by a different caller never reaches the session: it gets the status,
/// headers and body rmcp gives a well-formed request for an unknown session id
/// ([`unknown_session_answer`]; a foreign DELETE gets 202 and closes
/// nothing). The session and its binding are untouched. A malformed request
/// (for example a GET without `Accept: text/event-stream`) is refused here
/// with that same answer, where rmcp would first reject its form, so only
/// well-formed requests are indistinguishable. Other methods pass through to
/// rmcp's 405, which touches no session. The session a successful
/// `initialize` creates is bound to its caller; a successful DELETE by that
/// caller forgets it.
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
    let method = request.method().clone();
    let session_method = matches!(
        method,
        axum::http::Method::POST | axum::http::Method::GET | axum::http::Method::DELETE
    );
    if let Some(session) = session.as_deref() {
        if session_method && owners.owner_of(session).is_some_and(|bound| bound != owner) {
            return unknown_session_answer(&method);
        }
    }
    let is_delete = method == axum::http::Method::DELETE;
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
    let db = server.db.clone();
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
        McpHttpAuth::Bearer(auth) => router
            .layer(axum::middleware::from_fn_with_state(
                owners,
                session_binding_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                BearerState { auth, db },
                bearer_auth_middleware,
            )),
        McpHttpAuth::Unauthenticated => router,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken_kernel::{encode, Algorithm, EncodingKey, Header};
    use serde_json::{json, Value};

    const SECRET: &[u8] = b"mcp-http-unit-secret-at-least-32-bytes!!";

    fn payload(aud: Value) -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({
            "sub": Uuid::new_v4(), "iss": "epigraph", "aud": aud,
            "exp": now + 600, "iat": now, "nbf": now, "jti": Uuid::new_v4(),
            "scopes": ["claims:read"], "client_type": "human", "agent_id": Uuid::new_v4(),
        })
    }

    fn sign(payload: &Value, alg: Algorithm, secret: &[u8]) -> String {
        encode(
            &Header::new(alg),
            payload,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    /// A token matrix around the kernel shape: every case changes one thing.
    fn matrix() -> Vec<(String, String)> {
        let now = chrono::Utc::now().timestamp();
        let base = payload(json!("epigraph-api"));
        let mut out = vec![(
            "kernel token".to_string(),
            sign(&base, Algorithm::HS256, SECRET),
        )];
        let mut with = |label: &str, f: &dyn Fn(&mut Value)| {
            let mut p = base.clone();
            f(&mut p);
            out.push((label.to_string(), sign(&p, Algorithm::HS256, SECRET)));
        };
        with("aud episcience", &|p| p["aud"] = json!("episcience"));
        with("aud other-ext", &|p| p["aud"] = json!("other-ext"));
        with("aud array", &|p| p["aud"] = json!(["epigraph-api"]));
        with("wrong iss", &|p| p["iss"] = json!("not-epigraph"));
        with("expired 5s ago", &|p| p["exp"] = json!(now - 5));
        with("nbf in the future", &|p| p["nbf"] = json!(now + 300));
        with("sub not a uuid", &|p| p["sub"] = json!("gateway-discovery"));
        with("scopes not a list", &|p| p["scopes"] = json!("claims:read"));
        for claim in [
            "sub",
            "iss",
            "aud",
            "exp",
            "iat",
            "nbf",
            "jti",
            "scopes",
            "client_type",
            "agent_id",
        ] {
            with(&format!("without {claim}"), &|p| {
                p.as_object_mut().unwrap().remove(claim);
            });
        }
        out.push((
            "wrong secret".to_string(),
            sign(
                &base,
                Algorithm::HS256,
                b"another-secret-also-at-least-32-bytes!!",
            ),
        ));
        out.push(("HS384".to_string(), sign(&base, Algorithm::HS384, SECRET)));
        out.push(("not a JWT".to_string(), "not.a.jwt".to_string()));
        out
    }

    // The local mirror and the kernel's validator must reach the same verdict
    // (and the same error kind) on every token when both check the kernel
    // audience. Kills: drift in the mirror, for example a non-zero leeway, an
    // extra allowed algorithm, a dropped `iss` pin, a local claims type with
    // defaults, or accepting an array `aud`.
    #[test]
    fn extension_validator_decides_exactly_as_the_kernel_validator() {
        let kernel = JwtConfig::from_secret(SECRET);
        let mirror = ExtensionAudienceValidator::from_secret(SECRET);
        let mut admitted = 0;
        for (label, token) in matrix() {
            let k = kernel.validate_token(&token);
            let m = mirror.validate_token_for_audience(&token, "epigraph-api");
            match (k, m) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "{label}");
                    admitted += 1;
                }
                (Err(a), Err(b)) => assert_eq!(a.kind(), b.kind(), "{label}"),
                (k, m) => panic!("{label}: kernel {k:?} vs mirror {m:?}"),
            }
        }
        // The matrix is not all refusals: the kernel token, the future nbf
        // (the kernel does not check nbf) and the token without agent_id
        // (optional) pass both.
        assert_eq!(admitted, 3);
    }

    // Kills: classifying by anything but the validated `aud`, admitting a
    // third audience, or validating the extension audience without the
    // signature.
    #[test]
    fn the_bearer_layer_records_which_audience_admitted_the_token() {
        let auth = BearerAuth::from_secret(SECRET, KernelAudienceOnCall::Refuse);
        let ext = sign(&payload(json!("episcience")), Algorithm::HS256, SECRET);
        let kern = sign(&payload(json!("epigraph-api")), Algorithm::HS256, SECRET);
        let other = sign(&payload(json!("other-ext")), Algorithm::HS256, SECRET);
        let forged = sign(
            &payload(json!("episcience")),
            Algorithm::HS256,
            b"another-secret-also-at-least-32-bytes!!",
        );
        assert_eq!(
            auth.admit(&ext).map(|a| a.1),
            Some(TokenAudience::Extension)
        );
        assert_eq!(auth.admit(&kern).map(|a| a.1), Some(TokenAudience::Kernel));
        assert!(auth.admit(&other).is_none(), "another extension's token");
        assert!(
            auth.admit(&forged).is_none(),
            "a token signed with another secret"
        );
    }

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
