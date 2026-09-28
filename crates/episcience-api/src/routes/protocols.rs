use axum::extract::{Path, State};
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use epigraph_crypto::ContentHasher;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Protocol, ProtocolSections, ProtocolStep};
use episcience_db::ProtocolRepository;

/// Response header carrying non-fatal warnings emitted by the protocols
/// route (e.g. off-vocabulary section keys preserved under `extras`).
///
/// Mirrors SciLink's loader warning behaviour: surface the issue, don't
/// reject the payload.
pub const PROTOCOL_WARNINGS_HEADER: &str = "x-episcience-protocol-warnings";

#[derive(Deserialize)]
pub struct CreateProtocolRequest {
    pub title: String,
    /// Optional; defaults to the caller. When present it must equal the
    /// caller's agent id (403 otherwise).
    #[serde(default)]
    pub authored_by: Option<Uuid>,
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
    /// Optional structured section vocabulary. Raw JSON object — the
    /// handler runs [`ProtocolSections::from_value`] and surfaces a
    /// warning header for any off-vocabulary keys.
    #[serde(default)]
    pub sections: Option<serde_json::Value>,
    /// The owning group (a group the caller may write); default: the caller's
    /// default group. Protocols are `public`.
    #[serde(default)]
    pub owner_group_id: Option<Uuid>,
}

async fn create_protocol(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<CreateProtocolRequest>,
) -> Result<(HeaderMap, Json<Protocol>), ApiError> {
    if req.title.trim().is_empty() {
        return Err(ApiError::Validation("title cannot be empty".into()));
    }
    // The author is the caller. A body naming anyone else is refused rather
    // than trusted.
    let authored_by = match req.authored_by {
        None => auth.agent_id,
        Some(a) if a == auth.agent_id => a,
        Some(_) => return Err(ApiError::Forbidden("agent mismatch".into())),
    };

    let raw_sections = req.sections.unwrap_or_else(|| serde_json::json!({}));
    let (sections, off_vocab) = ProtocolSections::from_value(&raw_sections);

    let hash_input = serde_json::to_string(&req.steps).unwrap_or_default();
    let hash = ContentHasher::hash(hash_input.as_bytes());

    let viewer = crate::auth::viewer::caller_viewer(&state.pool, &auth).await?;
    let owner = crate::auth::tenancy::protocol_ownership(
        &state.pool,
        &viewer,
        req.supersedes,
        req.owner_group_id,
    )
    .await?;

    let protocol = ProtocolRepository::create(
        &mut *state
            .pool
            .acquire()
            .await
            .map_err(episcience_db::errors::DbError::from)?,
        &req.title,
        authored_by,
        &req.steps,
        &req.equipment,
        req.safety_notes.as_deref(),
        req.supersedes,
        &req.labels,
        &req.properties,
        &hash[..],
        &sections,
        owner,
    )
    .await?;

    let mut headers = HeaderMap::new();
    if !off_vocab.is_empty() {
        let value = format!("extras_dropped={}", off_vocab.join(","));
        // Header name is a compile-time-known lowercase ASCII constant; the
        // value is user-supplied off-vocab keys which we coerce safely.
        let name = HeaderName::from_static(PROTOCOL_WARNINGS_HEADER);
        if let Ok(val) = HeaderValue::from_str(&value) {
            headers.insert(name, val);
        }
    }

    Ok((headers, Json(protocol)))
}

async fn get_protocol(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Protocol>, ApiError> {
    let viewer = crate::auth::viewer::caller_viewer(&state.pool, &auth).await?;
    let protocol = ProtocolRepository::get_readable(&state.pool, id, &viewer).await?;
    Ok(Json(protocol))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/protocols", post(create_protocol))
        .nest(
            "/api/v1/eln/protocols/:id",
            Router::new().route("/", get(get_protocol)),
        )
        .with_state(state)
}
