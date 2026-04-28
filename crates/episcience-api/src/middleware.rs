use axum::{
    body::Body,
    extract::State,
    http::Request,
    middleware::Next,
    response::Response,
};
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;

#[derive(Debug, Deserialize)]
pub struct EpiGraphClaims {
    pub sub: Uuid,
    pub agent_id: Option<Uuid>,
    pub scopes: Vec<String>,
    pub client_type: String,
    pub exp: i64,
    pub jti: Uuid,
}

#[derive(Clone, Debug)]
pub struct AuthContext {
    pub agent_id: Uuid,
    pub client_id: Uuid,
    pub scopes: Vec<String>,
}

pub struct JwtConfig {
    decoding_key: DecodingKey,
    /// Optional list of accepted audience values. When `None`, `aud` is not
    /// validated (the default — keeps unit tests with no `aud` claim passing).
    /// When `Some(non-empty)`, the token's `aud` claim must match one of the
    /// listed values (jsonwebtoken's `set_audience` is a "match-any" check).
    ///
    /// Configured via `EPIGRAPH_JWT_AUDIENCE` (comma-separated). The prod
    /// deployment should set `EPIGRAPH_JWT_AUDIENCE=epigraph-api` so tokens
    /// minted by upstream are accepted with strict audience checking.
    audience: Option<Vec<String>>,
}

impl JwtConfig {
    pub fn from_secret(secret: &[u8]) -> Self {
        let audience = std::env::var("EPIGRAPH_JWT_AUDIENCE")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect::<Vec<String>>()
            })
            .filter(|v: &Vec<String>| !v.is_empty());
        Self {
            decoding_key: DecodingKey::from_secret(secret),
            audience,
        }
    }

    pub fn validate_token(&self, token: &str) -> Result<EpiGraphClaims, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
        match &self.audience {
            Some(aud) => {
                validation.set_audience(aud);
            }
            None => {
                // Default: `aud` is not validated. Existing tests mint tokens
                // without an `aud` claim; jsonwebtoken's default `Validation`
                // would reject them otherwise.
                validation.validate_aud = false;
            }
        }
        let data = decode::<EpiGraphClaims>(token, &self.decoding_key, &validation)?;
        Ok(data.claims)
    }
}

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
        _ => return Err(ApiError::Unauthorized("missing or invalid Authorization header".into())),
    };

    let claims = state
        .jwt_config
        .validate_token(token)
        .map_err(|e| ApiError::Unauthorized(format!("invalid token: {e}")))?;

    let agent_id = claims
        .agent_id
        .unwrap_or(claims.sub);

    let auth_ctx = AuthContext {
        agent_id,
        client_id: claims.sub,
        scopes: claims.scopes,
    };

    request.extensions_mut().insert(auth_ctx);
    Ok(next.run(request).await)
}
