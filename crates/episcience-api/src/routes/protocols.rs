use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use epigraph_crypto::ContentHasher;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Protocol, ProtocolStep};
use episcience_db::ProtocolRepository;

#[derive(Deserialize)]
pub struct CreateProtocolRequest {
    pub title: String,
    pub authored_by: Uuid,
    pub steps: Vec<ProtocolStep>,
    #[serde(default)]
    pub equipment: Vec<String>,
    #[serde(default)]
    pub safety_notes: Option<String>,
    #[serde(default)]
    pub supersedes: Option<Uuid>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub properties: serde_json::Value,
}

async fn create_protocol(
    State(state): State<ElnState>,
    Json(req): Json<CreateProtocolRequest>,
) -> Result<Json<Protocol>, ApiError> {
    if req.title.trim().is_empty() {
        return Err(ApiError::Validation("title cannot be empty".into()));
    }

    let hash_input = serde_json::to_string(&req.steps).unwrap_or_default();
    let hash = ContentHasher::hash(hash_input.as_bytes());

    let protocol = ProtocolRepository::create(
        &state.pool,
        &req.title,
        req.authored_by,
        &req.steps,
        &req.equipment,
        req.safety_notes.as_deref(),
        req.supersedes,
        &req.labels,
        &req.properties,
        &hash[..],
    )
    .await?;

    Ok(Json(protocol))
}

async fn get_protocol(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Protocol>, ApiError> {
    let protocol = ProtocolRepository::get_by_id(&state.pool, id).await?;
    Ok(Json(protocol))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/protocols", post(create_protocol))
        .nest("/api/v1/eln/protocols/:id", Router::new().route("/", get(get_protocol)))
        .with_state(state)
}
