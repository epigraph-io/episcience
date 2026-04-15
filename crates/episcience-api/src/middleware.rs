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
}

impl JwtConfig {
    pub fn from_secret(secret: &[u8]) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret),
        }
    }

    pub fn validate_token(&self, token: &str) -> Result<EpiGraphClaims, jsonwebtoken::errors::Error> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;
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
