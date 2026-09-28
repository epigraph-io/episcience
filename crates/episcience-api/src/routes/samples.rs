use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use epigraph_crypto::ContentHasher;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::tenancy::{observation_decl, root_ownership, RequestedVisibility};
use crate::auth::viewer::caller_viewer;
use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Ownership, Quantity, Sample, SampleStatus, SampleType, Visibility};
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
    /// The owning group (a group the caller may write); default: the caller's
    /// default group. A child of a `group` sample is owned by the parent's
    /// group regardless.
    #[serde(default)]
    pub owner_group_id: Option<Uuid>,
    /// `public` (default) or `group`.
    #[serde(default)]
    pub visibility: Option<RequestedVisibility>,
}

async fn create_sample(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<CreateSampleRequest>,
) -> Result<Json<Sample>, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::Validation("name cannot be empty".into()));
    }
    let sample_type: SampleType = req
        .sample_type
        .parse()
        .map_err(|e: String| ApiError::Validation(e))?;
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

    let visibility = req
        .visibility
        .unwrap_or(RequestedVisibility::Public)
        .resolve()?;
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let owner = match req.parent_sample_id {
        // A child of a GROUP sample stays in the parent's group: the caller
        // must be able to write it, and may not name another (the database
        // refuses the same). A public parent does not constrain the child.
        Some(parent_id) => {
            let parent = SampleRepository::get_readable(&state.pool, parent_id, &viewer).await?;
            match (parent.visibility, parent.owner_group_id) {
                (Some(Visibility::Public), _) => {
                    root_ownership(&state.pool, &viewer, req.owner_group_id, visibility).await?
                }
                (_, Some(g)) => {
                    if req.owner_group_id.is_some_and(|r| r != g)
                        || !viewer.writable_groups().contains(&g)
                    {
                        return Err(ApiError::Forbidden(
                            "a child of a group sample is owned by the parent's group, which the caller must be able to write"
                                .into(),
                        ));
                    }
                    Ownership::group(g)
                }
                (_, None) => {
                    return Err(ApiError::NotFound(format!("sample {parent_id} not found")))
                }
            }
        }
        None => root_ownership(&state.pool, &viewer, req.owner_group_id, visibility).await?,
    };

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
        owner,
    )
    .await?;

    Ok(Json(sample))
}

async fn get_sample(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Sample>, ApiError> {
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let sample = SampleRepository::get_readable(&state.pool, id, &viewer).await?;
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
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<Sample>>, ApiError> {
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let samples = SampleRepository::list(
        &state.pool,
        &viewer,
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
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<UpdateStatusRequest>,
) -> Result<Json<Sample>, ApiError> {
    // Only a writer of the sample's owner group may change its status; anyone
    // else gets the same 404 as for a missing sample.
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let current = SampleRepository::get_writable(&state.pool, id, &viewer).await?;
    let new_status: SampleStatus = req
        .status
        .parse()
        .map_err(|e: String| ApiError::Validation(e))?;
    if !current.status.can_transition_to(new_status) {
        return Err(ApiError::Validation(format!(
            "Cannot transition from {} to {}",
            current.status, new_status
        )));
    }

    let updated = SampleRepository::update_status_as(&state.pool, id, new_status, &viewer).await?;
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
    // The caller must be able to write the sample's owner group (404
    // otherwise, the same answer as for a missing sample).
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let sample = SampleRepository::get_writable(&state.pool, sample_id, &viewer).await?;
    if auth.agent_id != req.agent_id {
        return Err(ApiError::Forbidden("agent mismatch".into()));
    }
    let decl = observation_decl(&state.pool, &sample, auth.agent_id).await?;

    let claim_id = SampleRepository::add_observation(
        &state.pool,
        sample_id,
        req.agent_id,
        &req.content,
        &req.relationship,
        decl,
    )
    .await?;

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
