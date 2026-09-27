//! Shared test-token minting for the episcience-api integration tests.
//!
//! Included with `#[path = "support/token.rs"] mod token;` so every test binary
//! mints tokens the same way the kernel does (`iss = "epigraph"`,
//! `aud = "epigraph-api"`, HS256). Not every binary uses every helper.
#![allow(dead_code)]

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use uuid::Uuid;

pub const ISSUER: &str = "epigraph";
pub const AUDIENCE: &str = "epigraph-api";
pub const CLAIMS_READ: &str = "claims:read";
pub const CLAIMS_WRITE: &str = "claims:write";

/// The HMAC secret the tests sign with and the test servers verify with.
pub fn jwt_secret_bytes() -> Vec<u8> {
    std::env::var("EPIGRAPH_JWT_SECRET")
        .map(|s| s.into_bytes())
        .unwrap_or_else(|_| b"epigraph-dev-secret-change-in-production!!".to_vec())
}

/// Every knob a test may want to turn. `None` for `iss` / `aud` /
/// `agent_id` OMITS the claim from the payload entirely.
#[derive(Clone, Debug)]
pub struct TokenSpec {
    /// The OAuth client id (`sub`). `None` = a fresh random uuid.
    pub sub: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub scopes: Vec<String>,
    pub iss: Option<String>,
    pub aud: Option<String>,
    /// When set, `aud` is sent as this JSON ARRAY instead of `aud`'s string.
    pub aud_list: Option<Vec<String>>,
    /// Seconds relative to now. Negative = already expired.
    pub exp_offset_secs: i64,
    pub secret: Vec<u8>,
}

impl TokenSpec {
    /// A well-formed kernel-shaped token for `agent_id` with read + write.
    pub fn valid(agent_id: Uuid) -> Self {
        Self {
            sub: None,
            agent_id: Some(agent_id),
            scopes: vec![CLAIMS_READ.to_string(), CLAIMS_WRITE.to_string()],
            iss: Some(ISSUER.to_string()),
            aud: Some(AUDIENCE.to_string()),
            aud_list: None,
            exp_offset_secs: 3600,
            secret: jwt_secret_bytes(),
        }
    }
}

#[derive(Serialize)]
struct Claims {
    sub: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    iss: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aud: Option<serde_json::Value>,
    exp: i64,
    iat: i64,
    nbf: i64,
    jti: Uuid,
    scopes: Vec<String>,
    client_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<Uuid>,
}

pub fn mint(spec: &TokenSpec) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        // The kernel's `sub` is the OAuth CLIENT id, never an agent id; a
        // fresh uuid keeps any accidental `sub` fallback observable.
        sub: spec.sub.unwrap_or_else(Uuid::new_v4),
        iss: spec.iss.clone(),
        aud: match &spec.aud_list {
            Some(list) => Some(serde_json::json!(list)),
            None => spec.aud.clone().map(serde_json::Value::String),
        },
        exp: now + spec.exp_offset_secs,
        iat: now,
        nbf: now,
        jti: Uuid::now_v7(),
        scopes: spec.scopes.clone(),
        client_type: "human".to_string(),
        agent_id: spec.agent_id,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(&spec.secret),
    )
    .expect("mint JWT")
}

/// Valid read + write token for `agent_id` (the default for route tests).
pub fn mint_test_jwt(agent_id: Uuid) -> String {
    mint(&TokenSpec::valid(agent_id))
}

/// Valid token for `agent_id` holding ONLY `claims:read`.
pub fn read_only_jwt(agent_id: Uuid) -> String {
    mint(&TokenSpec {
        scopes: vec![CLAIMS_READ.to_string()],
        ..TokenSpec::valid(agent_id)
    })
}
