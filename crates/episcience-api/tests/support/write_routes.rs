//! Every REST write route, as re-sendable requests, for the auth-gate tests
//! (T-A2: a principal-less token writes nothing; T-A3: a read-only token is
//! refused). One list, so a route added here is covered by both.
//!
//! Included with `#[path = "support/write_routes.rs"] mod write_routes;`.
#![allow(dead_code)]

use axum::http::header::{HeaderName, HeaderValue, AUTHORIZATION};
use axum_test::multipart::{MultipartForm, Part};
use axum_test::{TestResponse, TestServer};
use uuid::Uuid;

pub fn bearer(token: &str) -> (HeaderName, HeaderValue) {
    (
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
    )
}

/// One write request, re-buildable so it can be sent with several tokens.
#[derive(Clone)]
pub enum Write {
    Json(&'static str, String, serde_json::Value),
    Blob(Uuid),
}

pub async fn send(server: &TestServer, w: &Write, token: &str) -> TestResponse {
    let (name, value) = bearer(token);
    match w {
        Write::Json(method, path, body) => {
            let req = match *method {
                "POST" => server.post(path),
                "PATCH" => server.patch(path),
                "DELETE" => server.delete(path),
                other => panic!("unexpected method {other}"),
            };
            req.add_header(name, value).json(body).await
        }
        Write::Blob(uploader) => {
            let form = MultipartForm::new()
                .add_part(
                    "file",
                    Part::bytes(b"auth gate payload".to_vec())
                        .file_name("gate.txt")
                        .mime_type("text/plain"),
                )
                .add_text("uploader_id", uploader.to_string());
            server
                .post("/api/v1/eln/blobs")
                .add_header(name, value)
                .multipart(form)
                .await
        }
    }
}

/// Every REST write route. `agent` owns `sample` and `synthesis`;
/// `public_key_hex` is `agent`'s Ed25519 public key.
pub fn write_routes(
    agent: Uuid,
    sample: Uuid,
    synthesis: Uuid,
    public_key_hex: &str,
) -> Vec<(&'static str, Write)> {
    vec![
        (
            "create sample",
            Write::Json(
                "POST",
                "/api/v1/eln/samples".into(),
                serde_json::json!({"name": format!("gate-{agent}"), "sample_type": "biological", "prepared_by": agent}),
            ),
        ),
        (
            "sample status",
            Write::Json(
                "PATCH",
                format!("/api/v1/eln/samples/{sample}/status"),
                serde_json::json!({"status": "in_use"}),
            ),
        ),
        (
            "add observation",
            Write::Json(
                "POST",
                format!("/api/v1/eln/samples/{sample}/observations"),
                serde_json::json!({"content": "gate observation", "agent_id": agent}),
            ),
        ),
        (
            "create protocol",
            Write::Json(
                "POST",
                "/api/v1/eln/protocols".into(),
                serde_json::json!({"title": "gate", "authored_by": agent, "steps": [{"order": 1, "instruction": "x"}]}),
            ),
        ),
        ("upload blob", Write::Blob(agent)),
        (
            "create workflow run",
            Write::Json(
                "POST",
                "/api/v1/eln/workflow_runs".into(),
                serde_json::json!({"workflow_id": Uuid::now_v7(), "canonical_name": "gate", "prepared_by": agent, "started_at": chrono::Utc::now()}),
            ),
        ),
        (
            "create synthesis",
            Write::Json(
                "POST",
                "/api/v1/eln/syntheses".into(),
                serde_json::json!({"query": "gate"}),
            ),
        ),
        (
            "refine synthesis",
            Write::Json(
                "POST",
                format!("/api/v1/eln/syntheses/{synthesis}/refine"),
                serde_json::json!({}),
            ),
        ),
        (
            "grant share",
            Write::Json(
                "POST",
                format!("/api/v1/eln/syntheses/{synthesis}/shares"),
                serde_json::json!({"shared_with_agent_id": agent}),
            ),
        ),
        (
            "revoke share",
            Write::Json(
                "DELETE",
                format!("/api/v1/eln/syntheses/{synthesis}/shares/{agent}"),
                serde_json::json!({}),
            ),
        ),
        (
            "update visibility",
            Write::Json(
                "PATCH",
                format!("/api/v1/eln/syntheses/{synthesis}/visibility"),
                serde_json::json!({"visibility": "public"}),
            ),
        ),
        (
            "synthesis search (POST)",
            Write::Json(
                "POST",
                "/api/v1/eln/syntheses/search".into(),
                serde_json::json!({"query": "gate"}),
            ),
        ),
        (
            "countersign",
            Write::Json(
                "POST",
                "/api/v1/eln/countersign".into(),
                serde_json::json!({"claim_id": Uuid::now_v7(), "signer_id": agent, "signature_meaning": "approved", "signature_hex": "00".repeat(64), "public_key_hex": public_key_hex}),
            ),
        ),
        (
            "delete synthesis",
            Write::Json(
                "DELETE",
                format!("/api/v1/eln/syntheses/{synthesis}"),
                serde_json::json!({}),
            ),
        ),
    ]
}
