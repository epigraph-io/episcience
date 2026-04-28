use crate::errors::ApiError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct EdgeRequest {
    pub source_type: String,
    pub source_id: Uuid,
    pub target_type: String,
    pub target_id: Uuid,
    pub relationship: String,
}

#[derive(Debug, Deserialize)]
struct EdgeResponse {
    id: Uuid,
}

pub struct EpigraphEdgesClient {
    base_url: String,
    token: String,
    http: Client,
}

impl EpigraphEdgesClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url,
            token,
            http: Client::new(),
        }
    }

    pub async fn create_edge(&self, req: EdgeRequest) -> Result<Uuid, ApiError> {
        let resp = self
            .http
            .post(format!("{}/edges", self.base_url))
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await
            .map_err(|e| ApiError::ServiceUnavailable(format!("epigraph edges: {e}")))?;
        match resp.status().as_u16() {
            201 | 200 => {
                let body: EdgeResponse = resp
                    .json()
                    .await
                    .map_err(|e| ApiError::Internal(format!("decode: {e}")))?;
                Ok(body.id)
            }
            422 => Err(ApiError::Validation(resp.text().await.unwrap_or_default())),
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
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_request() -> EdgeRequest {
        EdgeRequest {
            source_type: "synthesis".to_string(),
            source_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap(),
            target_type: "claim".to_string(),
            target_id: Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap(),
            relationship: "wasDerivedFrom".to_string(),
        }
    }

    #[tokio::test]
    async fn create_edge_succeeds_returns_edge_id() {
        let server = MockServer::start().await;
        let edge_id = "11111111-1111-1111-1111-111111111111";

        Mock::given(method("POST"))
            .and(path("/edges"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": edge_id })),
            )
            .mount(&server)
            .await;

        let client = EpigraphEdgesClient::new(server.uri(), "test-token".to_string());
        let result = client.create_edge(sample_request()).await;

        let id = match result {
            Ok(id) => id,
            Err(_) => panic!("expected Ok(uuid), got Err"),
        };
        assert_eq!(id, Uuid::parse_str(edge_id).unwrap());
    }

    #[tokio::test]
    async fn create_edge_503_maps_to_service_unavailable() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/edges"))
            .respond_with(ResponseTemplate::new(503).set_body_string("epigraph down"))
            .mount(&server)
            .await;

        let client = EpigraphEdgesClient::new(server.uri(), "test-token".to_string());
        let result = client.create_edge(sample_request()).await;

        match result {
            Err(ApiError::ServiceUnavailable(msg)) => {
                assert!(
                    msg.contains("503") || msg.contains("epigraph"),
                    "unexpected ServiceUnavailable message: {msg}"
                );
            }
            other => panic!("expected ServiceUnavailable, got {:?}", other.is_err()),
        }
    }

    #[tokio::test]
    async fn create_edge_422_maps_to_validation() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/edges"))
            .respond_with(ResponseTemplate::new(422).set_body_string("invalid relationship"))
            .mount(&server)
            .await;

        let client = EpigraphEdgesClient::new(server.uri(), "test-token".to_string());
        let result = client.create_edge(sample_request()).await;

        match result {
            Err(ApiError::Validation(msg)) => {
                assert!(
                    msg.contains("invalid"),
                    "unexpected Validation message: {msg}"
                );
            }
            other => panic!("expected Validation, got {:?}", other.is_err()),
        }
    }
}
