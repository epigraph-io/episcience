//! Long-poll client for upstream EpiGraph's `GET /api/v1/events` endpoint.
//!
//! Counterpart to `EpigraphEdgesClient` — same auth, same error mapping, same
//! wiremock test pattern. Used by the [`StalenessWorker`] to drain
//! `belief.updated` events and re-evaluate cited syntheses.
//!
//! ## Wire format note
//!
//! The upstream `GraphEvent` struct names its timestamp field `created_at`
//! (verified at `crates/epigraph-api/src/routes/events.rs` in the
//! `epigraph-wt-episcience-p0` worktree). The local `GraphEvent` struct here
//! mirrors that field name to keep deserialization straightforward; we expose
//! it back to callers as-is. If upstream ever renames it to `ts`, this is the
//! one place to update.
//!
//! ## P3 degradation gate
//!
//! Per `docs/superpowers/plans/p3-status.md`, only `belief.updated` events are
//! reliably persisted to the upstream Postgres `events` table (`feature = "db"`
//! is the production default). `edge.added`, `edge.deleted`, and
//! `claim.superseded` are pushed to the in-memory `EventStore` only and are NOT
//! visible via this HTTP polling path. `frame.changed` is not emitted at all.
//!
//! Phase 4 v1 polls only `belief.updated`. Other event types are deferred until
//! upstream dual-writes them.

use crate::errors::ApiError;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Mirror of upstream `GraphEvent` (epigraph-api). Field names match the wire
/// format exactly — including `created_at` (not `ts`, as an earlier draft of
/// this task had guessed).
#[derive(Debug, Clone, Deserialize)]
pub struct GraphEvent {
    pub id: Uuid,
    pub event_type: String,
    #[serde(default)]
    pub actor_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub graph_version: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct EventListResponse {
    events: Vec<GraphEvent>,
    #[serde(default)]
    #[allow(dead_code)]
    total: usize,
}

/// Wire-format body for `POST /api/v1/events`.
#[derive(Debug, Serialize)]
struct CreateEventRequest<'a> {
    event_type: &'a str,
    payload: serde_json::Value,
}

/// HTTP client for `GET /api/v1/events`. Polls for events created at or after
/// a watermark, optionally filtered by `event_type`.
pub struct EpigraphEventsClient {
    base_url: String,
    token: String,
    http: Client,
}

/// Minimal RFC3986 query-component encoder for the few characters that appear
/// in an RFC3339 timestamp (`:` and `+`). Avoids pulling a new crate dep just
/// for one URL parameter.
fn url_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl EpigraphEventsClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url,
            token,
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Poll events created at or after `since`, optionally filtered by
    /// `event_types`.
    ///
    /// The upstream filter accepts a single `event_type` parameter; for
    /// multi-type polling we pass the first type as a server-side filter and
    /// re-filter client-side, but Phase 4 v1 only ever passes one type
    /// (`belief.updated`).
    ///
    /// Returns events in upstream order (ascending `graph_version`). An empty
    /// result is valid (no new events).
    pub async fn poll_since(
        &self,
        since: Option<DateTime<Utc>>,
        event_types: &[&str],
        limit: usize,
    ) -> Result<Vec<GraphEvent>, ApiError> {
        let mut url = format!(
            "{}/api/v1/events?limit={}",
            self.base_url.trim_end_matches('/'),
            limit
        );
        if let Some(ts) = since {
            url.push_str(&format!("&since={}", url_encode_query(&ts.to_rfc3339())));
        }
        // Server-side filter only when a single type is requested. Multi-type
        // polling falls back to client-side filter — Phase 4 v1 never hits
        // this branch.
        if event_types.len() == 1 {
            url.push_str(&format!("&event_type={}", url_encode_query(event_types[0])));
        }

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ApiError::ServiceUnavailable(format!("epigraph events: {e}")))?;

        match resp.status().as_u16() {
            200 => {
                let body: EventListResponse = resp
                    .json()
                    .await
                    .map_err(|e| ApiError::Internal(format!("decode: {e}")))?;
                let events = if event_types.len() > 1 {
                    body.events
                        .into_iter()
                        .filter(|e| event_types.iter().any(|t| *t == e.event_type))
                        .collect()
                } else {
                    body.events
                };
                Ok(events)
            }
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
        let body = CreateEventRequest { event_type, payload };

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
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
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_belief_updated_event() -> serde_json::Value {
        serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "event_type": "belief.updated",
            "actor_id": null,
            "created_at": "2026-04-28T12:00:00Z",
            "payload": {
                "claim_id": "22222222-2222-2222-2222-222222222222",
                "frame_id": null,
                "old_belief": 0.5,
                "new_belief": 0.6,
                "old_plausibility": 0.9,
                "new_plausibility": 0.95,
                "pignistic_prob": 0.55,
                "combination_method": "dempster",
                "total_sources": 2,
                "perspective_id": null
            },
            "graph_version": 42
        })
    }

    fn sample_claim_created_event() -> serde_json::Value {
        serde_json::json!({
            "id": "33333333-3333-3333-3333-333333333333",
            "event_type": "claim.created",
            "actor_id": null,
            "created_at": "2026-04-28T12:00:01Z",
            "payload": {"claim_id": "44444444-4444-4444-4444-444444444444"},
            "graph_version": 43
        })
    }

    #[tokio::test]
    async fn poll_returns_events_with_filter() {
        let server = MockServer::start().await;
        // Server-side filter applied: upstream is responsible for honouring
        // event_type=belief.updated, so the mock returns only one event in
        // that case to mirror the contract.
        Mock::given(method("GET"))
            .and(path("/api/v1/events"))
            .and(query_param("event_type", "belief.updated"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "events": [sample_belief_updated_event()],
                "total": 1
            })))
            .mount(&server)
            .await;

        let client = EpigraphEventsClient::new(server.uri(), "test-token".to_string());
        let events = match client.poll_since(None, &["belief.updated"], 100).await {
            Ok(v) => v,
            Err(_) => panic!("poll_since unexpectedly failed"),
        };
        assert_eq!(events.len(), 1, "should return only the filtered event");
        assert_eq!(events[0].event_type, "belief.updated");
    }

    #[tokio::test]
    async fn poll_multi_type_filters_client_side() {
        let server = MockServer::start().await;
        // No event_type query param → upstream returns everything; client
        // re-filters to the requested set.
        Mock::given(method("GET"))
            .and(path("/api/v1/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "events": [sample_belief_updated_event(), sample_claim_created_event()],
                "total": 2
            })))
            .mount(&server)
            .await;

        let client = EpigraphEventsClient::new(server.uri(), "test-token".to_string());
        let events = match client
            .poll_since(None, &["belief.updated", "edge.added"], 100)
            .await
        {
            Ok(v) => v,
            Err(_) => panic!("poll_since unexpectedly failed"),
        };
        assert_eq!(
            events.len(),
            1,
            "claim.created should be filtered out client-side"
        );
        assert_eq!(events[0].event_type, "belief.updated");
    }

    #[tokio::test]
    async fn poll_503_maps_to_service_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/events"))
            .respond_with(ResponseTemplate::new(503).set_body_string("epigraph down"))
            .mount(&server)
            .await;

        let client = EpigraphEventsClient::new(server.uri(), "test-token".to_string());
        let result = client.poll_since(None, &["belief.updated"], 100).await;
        match result {
            Err(ApiError::ServiceUnavailable(msg)) => {
                assert!(
                    msg.contains("503") || msg.contains("epigraph"),
                    "unexpected ServiceUnavailable message: {msg}"
                );
            }
            other => panic!("expected ServiceUnavailable, got Err? {}", other.is_err()),
        }
    }

    #[tokio::test]
    async fn poll_empty_response_returns_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "events": [],
                "total": 0
            })))
            .mount(&server)
            .await;

        let client = EpigraphEventsClient::new(server.uri(), "test-token".to_string());
        let events = match client.poll_since(None, &["belief.updated"], 100).await {
            Ok(v) => v,
            Err(_) => panic!("poll_since unexpectedly failed"),
        };
        assert!(events.is_empty());
    }

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
        assert!(
            result.is_ok(),
            "publish_event should succeed on 200, got {result:?}"
        );
    }

    #[tokio::test]
    async fn poll_includes_since_query_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/events"))
            .and(query_param("since", "2026-04-28T11:30:00+00:00"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "events": [],
                "total": 0
            })))
            .mount(&server)
            .await;

        let client = EpigraphEventsClient::new(server.uri(), "test-token".to_string());
        let since = chrono::DateTime::parse_from_rfc3339("2026-04-28T11:30:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        // wiremock returns 404 if no Mock matches; the matcher above asserts
        // `since=...` is present in the URL, so a successful 200 here implies
        // the parameter made it across the wire.
        match client
            .poll_since(Some(since), &["belief.updated"], 100)
            .await
        {
            Ok(_) => {}
            Err(_) => panic!("poll_since failed — `since` query param likely missing"),
        }
    }
}
