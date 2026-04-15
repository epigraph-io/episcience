use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use epigraph_crypto::ContentHasher;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Quantity, Sample, SampleStatus, SampleType};
use episcience_db::SampleRepository;

#[derive(Deserialize)]
pub struct CreateSampleRequest {
    pub name: String,
    pub sample_type: String,
    pub prepared_by: Uuid,
    #[serde(default)]
    pub parent_sample_id: Option<Uuid>,
    #[serde(default)]
    pub storage_location: Option<String>,
    #[serde(default)]
    pub quantity_value: Option<f64>,
    #[serde(default)]
    pub quantity_unit: Option<String>,
    #[serde(default)]
    pub hazard_info: serde_json::Value,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub properties: serde_json::Value,
}

async fn create_sample(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<CreateSampleRequest>,
) -> Result<Json<Sample>, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::Validation("name cannot be empty".into()));
    }
    let sample_type: SampleType = req.sample_type.parse().map_err(|e: String| {
        ApiError::Validation(e)
    })?;
    if auth.agent_id != req.prepared_by {
        return Err(ApiError::Forbidden("agent mismatch".into()));
    }
    let quantity = match (req.quantity_value, req.quantity_unit) {
        (Some(v), Some(u)) => Some(Quantity { value: v, unit: u }),
        _ => None,
    };

    // BLAKE3 hash of the creation parameters
    let hash_input = format!("{}:{}:{}", req.name, req.sample_type, req.prepared_by);
    let hash = ContentHasher::hash(hash_input.as_bytes());

    let sample = SampleRepository::create(
        &state.pool,
        &req.name,
        sample_type,
        req.prepared_by,
        req.parent_sample_id,
        req.storage_location.as_deref(),
        quantity.as_ref(),
        &req.hazard_info,
        &req.labels,
        &req.properties,
        &hash[..],
    )
    .await?;

    Ok(Json(sample))
}

async fn get_sample(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Sample>, ApiError> {
    let sample = SampleRepository::get_by_id(&state.pool, id).await?;
    Ok(Json(sample))
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub sample_type: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    20
}

async fn list_samples(
    State(state): State<ElnState>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<Sample>>, ApiError> {
    let samples = SampleRepository::list(
        &state.pool,
        params.status.as_deref(),
        params.sample_type.as_deref(),
        params.limit.min(100),
        params.offset.max(0),
    )
    .await?;
    Ok(Json(samples))
}

#[derive(Deserialize)]
pub struct UpdateStatusRequest {
    pub status: String,
}

async fn update_status(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateStatusRequest>,
) -> Result<Json<Sample>, ApiError> {
    // Validate transition
    let current = SampleRepository::get_by_id(&state.pool, id).await?;
    let new_status: SampleStatus = req.status.parse().map_err(|e: String| {
        ApiError::Validation(e)
    })?;
    if !current.status.can_transition_to(new_status) {
        return Err(ApiError::Validation(format!(
            "Cannot transition from {} to {}",
            current.status, new_status
        )));
    }

    let updated = SampleRepository::update_status(&state.pool, id, new_status).await?;
    Ok(Json(updated))
}

#[derive(Deserialize)]
pub struct AddObservationRequest {
    pub content: String,
    pub agent_id: Uuid,
    #[serde(default = "default_relationship")]
    pub relationship: String,
}

fn default_relationship() -> String {
    "observation".into()
}

async fn add_observation(
    State(state): State<ElnState>,
    Path(sample_id): Path<Uuid>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<AddObservationRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Verify sample exists
    let _sample = SampleRepository::get_by_id(&state.pool, sample_id).await?;
    if auth.agent_id != req.agent_id {
        return Err(ApiError::Forbidden("agent mismatch".into()));
    }

    // Create the claim via direct SQL (Phase 0 — future: delegate to EpiGraph API)
    let claim_id = Uuid::now_v7();
    let hash = ContentHasher::hash(req.content.as_bytes());

    sqlx::query(
        r#"
        INSERT INTO claims (id, content, agent_id, truth_value, content_hash,
            is_current, created_at, updated_at)
        VALUES ($1, $2, $3, 0.5, $4, true, NOW(), NOW())
        "#,
    )
    .bind(claim_id)
    .bind(&req.content)
    .bind(req.agent_id)
    .bind(&hash[..])
    .execute(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Link sample to claim
    SampleRepository::link_claim(&state.pool, sample_id, claim_id, &req.relationship).await?;

    Ok(Json(serde_json::json!({
        "claim_id": claim_id,
        "sample_id": sample_id,
        "relationship": req.relationship,
    })))
}

pub fn router(state: ElnState) -> Router {
    let nested = Router::new()
        .route("/", get(get_sample))
        .route("/status", patch(update_status))
        .route("/observations", post(add_observation));

    Router::new()
        .route("/api/v1/eln/samples", post(create_sample).get(list_samples))
        .nest("/api/v1/eln/samples/:id", nested)
        .with_state(state)
}
