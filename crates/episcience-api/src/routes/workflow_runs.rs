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
//! Idempotent (backlog 015db9a5): a run is keyed on its `content_hash`,
//! BLAKE3(canonical_name || workflow_id || started_at as RFC 3339 in UTC),
//! for the calling agent. A POST whose key matches a `workflow_run` row the
//! caller prepared and can read returns that row with 200 and writes nothing
//! (labels included: a replay with other labels changes nothing); otherwise
//! the row is inserted and the answer is 201. The lookup runs on the
//! caller's stamped transaction, so a matching row under a tenancy the
//! caller cannot read is treated as absent (a new row, never the hidden id).
//! A transaction-scoped advisory lock on the key serialises concurrent POSTs
//! of the same run, so they cannot both miss the lookup and both insert.
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

/// The first key of the two-key advisory lock this route takes on a run's
/// `content_hash` (ASCII `WFRU`). The two-key form does not overlap the
/// one-key (`bigint`) lock space the kernel's definers use.
const WORKFLOW_RUN_LOCK_CLASS: i32 = 0x5746_5255;

#[derive(Deserialize)]
pub struct CreateWorkflowRunRequest {
    pub workflow_id: Uuid,
    pub canonical_name: String,
    /// Optional: absent means the authenticated caller; present and
    /// different is 403.
    #[serde(default)]
    pub prepared_by: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub labels: Vec<String>,
}

async fn create_workflow_run(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Extension(viewer): Extension<crate::middleware::CallerViewer>,
    Json(req): Json<CreateWorkflowRunRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if req.canonical_name.trim().is_empty() {
        return Err(ApiError::Validation(
            "canonical_name cannot be empty".into(),
        ));
    }
    let prepared_by =
        crate::auth::tenancy::bound_identity("prepared_by", req.prepared_by, auth.agent_id)?;

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

    let sample_type_str = SampleType::WorkflowRun.as_str();
    let mut tx = state.db.write_as(&viewer).await?;

    // Replay: hold the key's lock until commit, then look the run up on the
    // caller's stamped transaction (row security hides what the caller cannot
    // read). The fields are compared beside the hash, so two requests whose
    // concatenations happen to coincide are never taken for one run.
    let lock_key = i32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(WORKFLOW_RUN_LOCK_CLASS)
        .bind(lock_key)
        .execute(&mut *tx)
        .await
        .map_err(|e| ApiError::Internal(format!("lock workflow_run key: {e}")))?;
    let existing: Option<Uuid> = sqlx::query_scalar(
        r#"
        SELECT id FROM samples
         WHERE content_hash = $1
           AND sample_type = $2
           AND prepared_by = $3
           AND name = $4
           AND properties->>'workflow_id' = $5
           AND properties->>'started_at' = $6
         ORDER BY created_at, id
         LIMIT 1
        "#,
    )
    .bind(&hash[..])
    .bind(sample_type_str)
    .bind(prepared_by)
    .bind(&req.canonical_name)
    .bind(req.workflow_id.to_string())
    .bind(&started_at_rfc3339)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(format!("look up workflow_run sample: {e}")))?;
    if let Some(sample_id) = existing {
        tx.commit().await?;
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "sample_id": sample_id,
                "sample_type": sample_type_str,
                "workflow_id": req.workflow_id,
            })),
        ));
    }

    // A workflow-run sample is a ROOT row: `public`, owned by the caller's
    // default group (declared; the database refuses an undeclared root).
    let owner = crate::auth::tenancy::root_ownership(
        &mut tx,
        &viewer,
        None,
        episcience_core::Visibility::Public,
    )
    .await?;

    let sample_id = Uuid::now_v7();
    let hazard_info = serde_json::json!({});

    sqlx::query(
        r#"
        INSERT INTO samples (
            id, name, sample_type, status, parent_sample_id,
            prepared_by, preparation_date, storage_location,
            quantity_value, quantity_unit, hazard_info, labels, properties,
            content_hash, created_at, updated_at, owner_group_id, visibility
        )
        VALUES ($1, $2, $3, 'prepared', NULL, $4, $5, NULL,
                1.0, 'run', $6, $7, $8, $9, $5, $5, $10, $11)
        "#,
    )
    .bind(sample_id)
    .bind(&req.canonical_name)
    .bind(sample_type_str)
    .bind(prepared_by)
    .bind(req.started_at)
    .bind(&hazard_info)
    .bind(&labels)
    .bind(&properties)
    .bind(&hash[..])
    .bind(owner.owner_group_id)
    .bind(owner.visibility.as_str())
    .execute(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(format!("insert workflow_run sample: {e}")))?;
    tx.commit().await?;

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
