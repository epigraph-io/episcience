//! Client for upstream EpiGraph's `POST /api/v1/events` (the service
//! credential's event publishing).
//!
//! RETIRED in E1f: nothing constructs it. `synthesis.*` events are written in
//! process on the synthesis owner's transaction
//! (`episcience_db::synthesis::publish::publish_synthesis_event_conn`), and
//! the `belief.updated` long poll (and the `StalenessWorker` it fed) is
//! deleted: the worker's staleness recheck replaces it. The module is deleted
//! with the other client modules in E1h.

use super::service_token::ServiceToken;
use crate::errors::ApiError;
use reqwest::Client;
use serde::Serialize;
use std::sync::Arc;

/// Wire-format body for `POST /api/v1/events`.
#[derive(Debug, Serialize)]
struct CreateEventRequest<'a> {
    event_type: &'a str,
    payload: serde_json::Value,
}

/// HTTP client for `POST /api/v1/events` (retired; see the module docs).
pub struct EpigraphEventsClient {
    base_url: String,
    token: Arc<ServiceToken>,
    http: Client,
}

impl EpigraphEventsClient {
    /// Construct with a fixed bearer string (wrapped as a non-refreshing
    /// [`ServiceToken`]). Retained for the static-token path and unit tests.
    pub fn new(base_url: String, token: String) -> Self {
        Self::new_with_token(base_url, ServiceToken::static_token(token))
    }

    /// Construct with a shared [`ServiceToken`], which may auto-refresh via
    /// OAuth `client_credentials`.
    pub fn new_with_token(base_url: String, token: Arc<ServiceToken>) -> Self {
        Self {
            base_url,
            token,
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    /// POST `/api/v1/events` — publish an outbound event to EpiGraph's event bus.
    ///
    /// Sends `{ event_type, payload }` matching EpiGraph's `CreateEventRequest`.
    /// Returns `Ok(())` on 200 or 201. Maps 5xx responses to
    /// [`ApiError::ServiceUnavailable`] and all other error cases to
    /// [`ApiError::Internal`].
    pub async fn publish_event(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), ApiError> {
        let url = format!("{}/api/v1/events", self.base_url.trim_end_matches('/'));
        let body = CreateEventRequest {
            event_type,
            payload,
        };

        let bearer = self.token.bearer().await?;
        let resp = self
            .http
            .post(&url)
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::ServiceUnavailable(format!("epigraph events publish: {e}")))?;

        match resp.status().as_u16() {
            200 | 201 => Ok(()),
            500..=599 => Err(ApiError::ServiceUnavailable(format!(
                "epigraph {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ))),
            _ => Err(ApiError::Internal(format!(
                "unexpected {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn publish_event_posts_to_epigraph() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/events"))
            .and(header("authorization", "Bearer test-token"))
            .and(body_json(serde_json::json!({
                "event_type": "synthesis.complete",
                "payload": {
                    "synthesis_id": "11111111-1111-1111-1111-111111111111",
                    "workflow_run_id": null
                }
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = EpigraphEventsClient::new(server.uri(), "test-token".to_string());
        let result = client
            .publish_event(
                "synthesis.complete",
                serde_json::json!({
                    "synthesis_id": "11111111-1111-1111-1111-111111111111",
                    "workflow_run_id": null
                }),
            )
            .await;
        assert!(result.is_ok(), "publish_event should succeed on 200");
    }
}
