//! Phase 1 of the EpiClaw <-> episcience integration.
//!
//! `POST /api/v1/eln/workflow_runs` lets EpiClaw record a workflow-run
//! event as a `samples` row with `sample_type = 'workflow_run'`. The
//! row carries the EpiGraph workflow UUID (`properties.workflow_id`),
//! the canonical_name (also persisted as `samples.name`) and the
//! `started_at` timestamp; downstream observations / blobs /
//! countersignatures attach by `sample_id`.
//!
//! Authorisation: the request's `prepared_by` field must match
//! `auth.agent_id`; otherwise the handler returns 403.
//!
//! This route bypasses [`episcience_db::SampleRepository::create`] and
//! issues a direct INSERT so it can honour the caller's `started_at`
//! value as `preparation_date` (the repository helper stamps
//! `preparation_date = NOW()` unconditionally).
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use epigraph_crypto::ContentHasher;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::SampleType;

#[derive(Deserialize)]
pub struct CreateWorkflowRunRequest {
    pub workflow_id: Uuid,
    pub canonical_name: String,
    pub prepared_by: Uuid,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub labels: Vec<String>,
}

async fn create_workflow_run(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<CreateWorkflowRunRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if req.canonical_name.trim().is_empty() {
        return Err(ApiError::Validation(
            "canonical_name cannot be empty".into(),
        ));
    }
    if auth.agent_id != req.prepared_by {
        return Err(ApiError::Forbidden("agent mismatch".into()));
    }

    let started_at_rfc3339 = req.started_at.to_rfc3339();

    // content_hash = BLAKE3(canonical_name || workflow_id_bytes || started_at_rfc3339)
    let mut hash_input: Vec<u8> = Vec::new();
    hash_input.extend_from_slice(req.canonical_name.as_bytes());
    hash_input.extend_from_slice(req.workflow_id.as_bytes());
    hash_input.extend_from_slice(started_at_rfc3339.as_bytes());
    let hash = ContentHasher::hash(&hash_input);

    let properties = serde_json::json!({
        "workflow_id": req.workflow_id,
        "canonical_name": req.canonical_name,
        "started_at": started_at_rfc3339,
    });

    // labels = caller-supplied + "workflow_run" (plain append, no dedup)
    let mut labels = req.labels.clone();
    labels.push("workflow_run".to_string());

    let sample_id = Uuid::now_v7();
    let hazard_info = serde_json::json!({});
    let sample_type_str = SampleType::WorkflowRun.as_str();

    sqlx::query(
        r#"
        INSERT INTO samples (
            id, name, sample_type, status, parent_sample_id,
            prepared_by, preparation_date, storage_location,
            quantity_value, quantity_unit, hazard_info, labels, properties,
            content_hash, created_at, updated_at
        )
        VALUES ($1, $2, $3, 'prepared', NULL, $4, $5, NULL,
                1.0, 'run', $6, $7, $8, $9, $5, $5)
        "#,
    )
    .bind(sample_id)
    .bind(&req.canonical_name)
    .bind(sample_type_str)
    .bind(req.prepared_by)
    .bind(req.started_at)
    .bind(&hazard_info)
    .bind(&labels)
    .bind(&properties)
    .bind(&hash[..])
    .execute(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(format!("insert workflow_run sample: {e}")))?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "sample_id": sample_id,
            "sample_type": sample_type_str,
            "workflow_id": req.workflow_id,
        })),
    ))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/workflow_runs", post(create_workflow_run))
        .with_state(state)
}
