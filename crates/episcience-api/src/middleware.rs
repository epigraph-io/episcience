use std::sync::Arc;

use axum::{body::Body, extract::State, http::Request, middleware::Next, response::Response};
use epigraph_db::Viewer;
use uuid::Uuid;

/// The kernel's own access-token types. EpiScience validates exactly what the
/// kernel validates: `epigraph_auth::JwtConfig::validate_token` pins HS256,
/// `iss = "epigraph"`, `aud = "epigraph-api"` (a single string; an array
/// audience fails to decode), a required `exp` with zero leeway, and
/// `EpiGraphClaims` requires every claim the kernel mints. There is no local
/// copy of either to drift.
pub use epigraph_auth::{EpiGraphClaims, JwtConfig};

use crate::auth::scopes::rest_required_scope;
use crate::errors::ApiError;
use crate::state::ElnState;

/// The only issuer EpiScience accepts (the kernel's).
pub const EXPECTED_ISSUER: &str = "epigraph";

/// The only audience EpiScience accepts (the kernel mints exactly one).
pub const EXPECTED_AUDIENCE: &str = "epigraph-api";

/// Prefix of the 401 body when a valid token names no principal.
pub const PRINCIPAL_REQUIRED: &str = "principal_required";

/// Prefix of the 403 body when the token lacks the route's scope.
pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";

/// The authenticated caller of one request. `agent_id` is the token's
/// `agent_id` claim and is ALWAYS present: a token without one is refused
/// (there is no fallback to `sub`, which is an OAuth client id, not an agent).
/// `owner_id` and `client_type` are carried as the kernel minted them.
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub agent_id: Uuid,
    pub client_id: Uuid,
    pub owner_id: Option<Uuid>,
    pub client_type: String,
    pub scopes: Vec<String>,
}

impl AuthContext {
    /// Build the caller from validated claims, or `None` when the token
    /// carries no `agent_id` (a principal-less token).
    #[must_use]
    pub fn from_claims(claims: &EpiGraphClaims) -> Option<Self> {
        claims.agent_id.map(|agent_id| Self {
            agent_id,
            client_id: claims.sub,
            owner_id: claims.owner_id,
            client_type: claims.client_type.clone(),
            scopes: claims.scopes.clone(),
        })
    }

    /// Exact scope membership (the kernel's `has_scope` semantics).
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// The caller's resolved read and write authority (the kernel's viewer),
/// attached by [`bearer_auth_middleware`] next to the [`AuthContext`]. Every
/// handler reads on `state.db.read_as(&viewer)` and writes on
/// `state.db.write_as(&viewer)`.
#[derive(Clone, Debug)]
pub struct CallerViewer(pub Arc<Viewer>);

impl std::ops::Deref for CallerViewer {
    type Target = Viewer;
    fn deref(&self) -> &Viewer {
        &self.0
    }
}

/// REST bearer gate: a valid kernel token that names a principal and holds
/// the scope the request's method needs (`auth::scopes::rest_required_scope`),
/// then the request's one authority decision
/// (`EpiscienceDb::resolve_principal`: an operated or unresolvable principal
/// is 403). The handler receives both the [`AuthContext`] and the
/// [`CallerViewer`].
pub async fn bearer_auth_middleware(
    State(state): State<ElnState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    let auth_header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h["Bearer ".len()..],
        _ => {
            return Err(ApiError::Unauthorized(
                "missing or invalid Authorization header".into(),
            ))
        }
    };

    let claims = state
        .jwt_config
        .validate_token(token)
        .map_err(|e| ApiError::Unauthorized(format!("invalid token: {e}")))?;

    let auth_ctx = AuthContext::from_claims(&claims).ok_or_else(|| {
        ApiError::Unauthorized(format!(
            "{PRINCIPAL_REQUIRED}: the token carries no agent_id"
        ))
    })?;

    let required = rest_required_scope(request.method());
    if !auth_ctx.has_scope(required) {
        return Err(ApiError::Forbidden(format!(
            "{INSUFFICIENT_SCOPE}: this request requires scope '{required}'"
        )));
    }

    let viewer = state.db.resolve_principal(Some(auth_ctx.agent_id)).await?;
    request.extensions_mut().insert(auth_ctx);
    request
        .extensions_mut()
        .insert(CallerViewer(Arc::new(viewer)));
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

    const SECRET: &[u8] = b"a-test-secret-that-is-long-enough-32b";

    /// A token the KERNEL mints validates here and carries every field the
    /// caller context keeps. Kills: a local validator or claims struct that
    /// drifts from the kernel's (e.g. a different audience or a renamed claim).
    #[test]
    fn kernel_minted_token_becomes_the_caller() {
        let cfg = JwtConfig::from_secret(SECRET);
        let (client, owner, agent) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (token, _jti) = cfg
            .issue_access_token(
                client,
                vec!["claims:read".into()],
                "human",
                Some(owner),
                Some(agent),
                chrono::Duration::hours(1),
            )
            .expect("mint");
        let claims = cfg.validate_token(&token).expect("kernel token validates");
        assert_eq!(claims.iss, EXPECTED_ISSUER);
        assert_eq!(claims.aud, EXPECTED_AUDIENCE);
        let caller = AuthContext::from_claims(&claims).expect("has a principal");
        assert_eq!(caller.agent_id, agent);
        assert_eq!(caller.client_id, client);
        assert_eq!(caller.owner_id, Some(owner));
        assert_eq!(caller.client_type, "human");
        assert!(caller.has_scope("claims:read") && !caller.has_scope("claims:write"));
    }

    /// The kernel's claims struct requires every claim it mints: a token
    /// without `nbf`/`iat`/`jti` is refused even with a valid signature, iss,
    /// aud and exp. Kills: reverting to a permissive local claims struct.
    #[test]
    fn token_missing_kernel_claims_is_refused() {
        let now = chrono::Utc::now().timestamp();
        let body = serde_json::json!({
            "sub": Uuid::new_v4(), "iss": "epigraph", "aud": "epigraph-api",
            "exp": now + 600, "scopes": ["claims:read"], "client_type": "human",
            "agent_id": Uuid::new_v4(),
        });
        let token = encode(
            &Header::new(Algorithm::HS256),
            &body,
            &EncodingKey::from_secret(SECRET),
        )
        .expect("mint");
        assert!(JwtConfig::from_secret(SECRET)
            .validate_token(&token)
            .is_err());
    }
}
