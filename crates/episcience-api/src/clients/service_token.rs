//! Auto-refreshing EpiGraph service token.
//!
//! EpiGraph access tokens are short-lived (`client_type: service` → 1h TTL).
//! The edge/event clients previously held a single static bearer string read
//! once at boot, so writes began 401'ing an hour after every restart. This
//! provider mints an access token on first use via the OAuth2
//! `client_credentials` grant against `{base_url}/oauth/token` and re-mints
//! transparently once the cached token is within 60s of expiry — mirroring the
//! sibling `epiclaw-host` `HttpApiClient::mint_or_cached_token` pattern.
//!
//! A `static_token` mode preserves the legacy behavior (verbatim token, never
//! refreshed) for the `EPIGRAPH_SERVICE_TOKEN` fallback and for unit tests.

use crate::errors::ApiError;
use chrono::{DateTime, Utc};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A minted access token paired with its absolute expiry.
#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

/// Client-credentials material + the cached token guarded for re-mint.
struct OauthCreds {
    /// Root URL (no trailing slash); the mint endpoint is `{base_url}/oauth/token`.
    base_url: String,
    client_id: String,
    client_secret: String,
    scope: String,
    http: Client,
    cached: Mutex<Option<CachedToken>>,
}

enum Mode {
    Static(String),
    Oauth(OauthCreds),
}

/// A source of EpiGraph bearer tokens. Clone-free sharing via `Arc`.
pub struct ServiceToken {
    mode: Mode,
}

impl ServiceToken {
    /// A fixed token returned verbatim and never refreshed. Backs the
    /// `EPIGRAPH_SERVICE_TOKEN` fallback and unit tests.
    pub fn static_token(token: String) -> Arc<Self> {
        Arc::new(Self {
            mode: Mode::Static(token),
        })
    }

    /// An auto-refreshing token minted via `client_credentials`. Mints lazily
    /// on first `bearer()` and re-mints when the cached token is within 60s of
    /// expiry.
    pub fn oauth(
        base_url: String,
        client_id: String,
        client_secret: String,
        scope: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            mode: Mode::Oauth(OauthCreds {
                base_url: base_url.trim_end_matches('/').to_string(),
                client_id,
                client_secret,
                scope,
                http: Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .expect("reqwest client"),
                cached: Mutex::new(None),
            }),
        })
    }

    /// Return a currently-valid bearer token, minting or refreshing as needed.
    pub async fn bearer(&self) -> Result<String, ApiError> {
        match &self.mode {
            Mode::Static(t) => Ok(t.clone()),
            Mode::Oauth(c) => c.bearer().await,
        }
    }
}

impl OauthCreds {
    async fn bearer(&self) -> Result<String, ApiError> {
        let mut guard = self.cached.lock().await;
        if let Some(tok) = guard.as_ref() {
            // 60s skew guard: refresh before the token can expire mid-request.
            if tok.expires_at > Utc::now() + chrono::Duration::seconds(60) {
                return Ok(tok.token.clone());
            }
        }

        let url = format!("{}/oauth/token", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "grant_type": "client_credentials",
                "client_id": self.client_id,
                "client_secret": self.client_secret,
                "scope": self.scope,
            }))
            .send()
            .await
            .map_err(|e| ApiError::ServiceUnavailable(format!("epigraph oauth: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Unauthorized(format!(
                "oauth mint {status}: {body}"
            )));
        }

        let parsed: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Internal(format!("oauth decode: {e}")))?;
        let access = parsed["access_token"]
            .as_str()
            .ok_or_else(|| ApiError::Internal("oauth response missing access_token".to_string()))?
            .to_string();
        let expires_in = parsed["expires_in"].as_i64().unwrap_or(900);
        let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);

        *guard = Some(CachedToken {
            token: access.clone(),
            expires_at,
        });
        Ok(access)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ApiError intentionally does not derive Debug, so `.unwrap()` won't compile
    // on `Result<_, ApiError>`; `.ok().as_deref()` keeps assertions Debug-free.
    #[tokio::test]
    async fn static_token_returns_value_without_network() {
        let t = ServiceToken::static_token("abc".to_string());
        assert_eq!(t.bearer().await.ok().as_deref(), Some("abc"));
        assert_eq!(t.bearer().await.ok().as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn oauth_mints_on_first_use() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "minted-1",
                "token_type": "Bearer",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let t = ServiceToken::oauth(
            server.uri(),
            "cid".to_string(),
            "sec".to_string(),
            "edges:write".to_string(),
        );
        assert_eq!(t.bearer().await.ok().as_deref(), Some("minted-1"));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "should mint exactly once"
        );
    }

    #[tokio::test]
    async fn oauth_reuses_cached_token_within_ttl() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "minted-1",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let t = ServiceToken::oauth(
            server.uri(),
            "cid".to_string(),
            "sec".to_string(),
            "s".to_string(),
        );
        assert!(t.bearer().await.is_ok());
        assert!(t.bearer().await.is_ok());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "second call within TTL must reuse the cached token"
        );
    }

    #[tokio::test]
    async fn oauth_remints_when_token_near_expiry() {
        let server = MockServer::start().await;
        // expires_in=30 is inside the 60s skew guard → every call is treated as
        // stale and re-mints.
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "minted",
                "expires_in": 30
            })))
            .mount(&server)
            .await;

        let t = ServiceToken::oauth(
            server.uri(),
            "cid".to_string(),
            "sec".to_string(),
            "s".to_string(),
        );
        assert!(t.bearer().await.is_ok());
        assert!(t.bearer().await.is_ok());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "near-expiry token must be re-minted on the next call"
        );
    }

    #[tokio::test]
    async fn oauth_mint_failure_surfaces_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad creds"))
            .mount(&server)
            .await;

        let t = ServiceToken::oauth(
            server.uri(),
            "cid".to_string(),
            "sec".to_string(),
            "s".to_string(),
        );
        assert!(t.bearer().await.is_err(), "mint 401 must surface as Err");
    }
}
