//! REST surface for the synthesis pipeline.
//!
//! Phase 3 Task 3.1: `POST /api/v1/eln/syntheses`
//!     Atomically inserts a `syntheses` row in `pending` state and a
//!     `synthesis_jobs` row in `'queued'` state in a single transaction. The
//!     synthesis worker picks the job up on its next poll and drives the row
//!     through the 6-stage pipeline. Returns 202 Accepted with the new id.

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use episcience_core::synthesis::Visibility;
use episcience_db::{SynthesisJobsRepository, SynthesisRepository};

use crate::errors::ApiError;
use crate::jobs::synthesis_job::SynthesisJobPayload;
use crate::middleware::AuthContext;
use crate::state::ElnState;

// Default LLM provider/model for newly created syntheses. The worker honours
// these strings only as audit metadata on the `syntheses` row — the actual
// LLM is configured at server startup. Once we have per-request override, the
// request body can override these.
const DEFAULT_LLM_PROVIDER: &str = "anthropic";
const DEFAULT_LLM_MODEL: &str = "claude-sonnet-4-6";

#[derive(Debug, Deserialize)]
pub struct CreateSynthesisRequest {
    pub query: String,
    #[serde(default)]
    pub traversal_config: Option<serde_json::Value>,
    #[serde(default)]
    pub parent_synthesis_id: Option<Uuid>,
    #[serde(default)]
    pub prereq_synthesis_ids: Vec<Uuid>,
    #[serde(default = "default_visibility")]
    pub visibility: Visibility,
}

fn default_visibility() -> Visibility {
    Visibility::Private
}

async fn create_synthesis(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<CreateSynthesisRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if req.query.trim().is_empty() {
        return Err(ApiError::Validation("query cannot be empty".into()));
    }

    let id = Uuid::now_v7();
    let payload = SynthesisJobPayload {
        synthesis_id: id,
        query: req.query.clone(),
        traversal_config: req.traversal_config.clone(),
        agent_id: auth.agent_id,
        parent_synthesis_id: req.parent_synthesis_id,
        prereq_synthesis_ids: req.prereq_synthesis_ids.clone(),
    };
    let payload_json = serde_json::to_value(&payload)
        .map_err(|e| ApiError::Internal(format!("payload serialize: {e}")))?;

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| ApiError::Internal(format!("tx begin: {e}")))?;

    SynthesisRepository::create_pending_tx(
        &mut tx,
        id,
        &req.query,
        auth.agent_id,
        req.parent_synthesis_id,
        &req.prereq_synthesis_ids,
        DEFAULT_LLM_PROVIDER,
        DEFAULT_LLM_MODEL,
        req.visibility,
    )
    .await?;

    SynthesisJobsRepository::enqueue_tx(&mut tx, id, &payload_json).await?;

    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(format!("tx commit: {e}")))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "id": id, "status": "queued" })),
    ))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/syntheses", post(create_synthesis))
        .with_state(state)
}
