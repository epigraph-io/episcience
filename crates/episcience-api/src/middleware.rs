use axum::{body::Body, extract::State, http::Request, middleware::Next, response::Response};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::scopes::rest_required_scope;
use crate::errors::ApiError;
use crate::state::ElnState;

/// The only issuer EpiScience accepts. Mirrors the kernel's
/// `epigraph_auth::JwtConfig::validate_token`.
pub const EXPECTED_ISSUER: &str = "epigraph";

/// The only audience EpiScience accepts (the kernel mints exactly one).
pub const EXPECTED_AUDIENCE: &str = "epigraph-api";

/// Prefix of the 401 body when a valid token names no principal.
pub const PRINCIPAL_REQUIRED: &str = "principal_required";

/// Prefix of the 403 body when the token lacks the route's scope.
pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";

/// The subset of the kernel's access-token claims EpiScience reads. `iss`,
/// `aud` and `exp` are checked by [`JwtConfig::validate_token`] before this
/// struct is populated; unknown claims are ignored, so the struct stays
/// wire-compatible with the kernel's `EpiGraphClaims`.
#[derive(Debug, Deserialize)]
pub struct EpiGraphClaims {
    pub sub: Uuid,
    pub agent_id: Option<Uuid>,
    pub scopes: Vec<String>,
    pub client_type: String,
    pub exp: i64,
    pub jti: Uuid,
}

/// The authenticated caller of one request. `agent_id` is the token's
/// `agent_id` claim and is ALWAYS present: a token without one is refused
/// (there is no fallback to `sub`, which is an OAuth client id, not an agent).
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub agent_id: Uuid,
    pub client_id: Uuid,
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
            scopes: claims.scopes.clone(),
        })
    }

    /// Exact scope membership (the kernel's `has_scope` semantics).
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// HS256 verifier for kernel-minted access tokens.
///
/// Strict by construction: `iss` must be [`EXPECTED_ISSUER`], `aud` must be
/// [`EXPECTED_AUDIENCE`], `exp` is required and checked with zero leeway. There
/// is no configuration knob that relaxes any of these.
pub struct JwtConfig {
    decoding_key: DecodingKey,
}

impl JwtConfig {
    pub fn from_secret(secret: &[u8]) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret),
        }
    }

    pub fn validate_token(
        &self,
        token: &str,
    ) -> Result<EpiGraphClaims, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[EXPECTED_ISSUER]);
        validation.set_audience(&[EXPECTED_AUDIENCE]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_exp = true;
        validation.leeway = 0;
        let data = decode::<EpiGraphClaims>(token, &self.decoding_key, &validation)?;
        Ok(data.claims)
    }
}

/// REST bearer gate: a valid kernel token that names a principal and holds
/// the scope the request's method needs (`auth::scopes::rest_required_scope`).
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

    request.extensions_mut().insert(auth_ctx);
    Ok(next.run(request).await)
}
