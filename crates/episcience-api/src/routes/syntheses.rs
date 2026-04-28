//! REST surface for the synthesis pipeline.
//!
//! Phase 3 Task 3.1: `POST /api/v1/eln/syntheses`
//!     Atomically inserts a `syntheses` row in `pending` state and a
//!     `synthesis_jobs` row in `'queued'` state in a single transaction. The
//!     synthesis worker picks the job up on its next poll and drives the row
//!     through the 6-stage pipeline. Returns 202 Accepted with the new id.
//!
//! Phase 3 Task 3.2: `GET /api/v1/eln/syntheses/:id`
//!     Looks up a synthesis by id, gated by [`SynthesisRepository::readable_by`]
//!     (owner / public / explicit share). Strangers receive 404 — not 403 —
//!     to avoid leaking the existence of private syntheses.
//!
//! Phase 3 Task 3.3: list / refine / soft-delete / clusters / snapshot /
//!     staleness — six read-and-derive endpoints, each gated by the same read
//!     predicate (or, for delete, owner-only).
//!
//! Phase 3 Task 3.4: shares (grant / revoke / list) and visibility patch.
//!     Owner-only mutations; revoke additionally allows the recipient to
//!     remove their own share.

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use episcience_core::synthesis::{Cluster, StalenessEvent, Synthesis, SynthesisStatus, Visibility};
use episcience_db::{
    Share, SynthesisClustersRepository, SynthesisJobsRepository, SynthesisRepository,
    SynthesisSharesRepository, SynthesisStalenessRepository,
};

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

/// Internal helper shared by `create_synthesis` and `refine_synthesis`.
/// Generates an id, inserts the synthesis row + job row in one transaction,
/// and returns the new id. All caller-side validation (e.g. parent
/// readability) must happen before this is invoked.
#[allow(clippy::too_many_arguments)]
async fn enqueue_synthesis(
    state: &ElnState,
    query: &str,
    agent_id: Uuid,
    parent_synthesis_id: Option<Uuid>,
    prereq_synthesis_ids: &[Uuid],
    traversal_config: Option<serde_json::Value>,
    visibility: Visibility,
) -> Result<Uuid, ApiError> {
    let id = Uuid::now_v7();
    let payload = SynthesisJobPayload {
        synthesis_id: id,
        query: query.to_string(),
        traversal_config,
        agent_id,
        parent_synthesis_id,
        prereq_synthesis_ids: prereq_synthesis_ids.to_vec(),
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
        query,
        agent_id,
        parent_synthesis_id,
        prereq_synthesis_ids,
        DEFAULT_LLM_PROVIDER,
        DEFAULT_LLM_MODEL,
        visibility,
    )
    .await?;

    SynthesisJobsRepository::enqueue_tx(&mut tx, id, &payload_json).await?;

    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(format!("tx commit: {e}")))?;

    Ok(id)
}

async fn create_synthesis(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<CreateSynthesisRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if req.query.trim().is_empty() {
        return Err(ApiError::Validation("query cannot be empty".into()));
    }

    let id = enqueue_synthesis(
        &state,
        &req.query,
        auth.agent_id,
        req.parent_synthesis_id,
        &req.prereq_synthesis_ids,
        req.traversal_config,
        req.visibility,
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "id": id, "status": "queued" })),
    ))
}

async fn get_synthesis(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Synthesis>, ApiError> {
    // Read-predicate gate. Strangers and missing rows are indistinguishable
    // from the outside (both 404) — this is intentional, to avoid leaking
    // the existence of private syntheses.
    if !SynthesisRepository::readable_by(&state.pool, id, auth.agent_id).await? {
        return Err(ApiError::NotFound(format!("synthesis {id} not found")));
    }
    let s = SynthesisRepository::get_by_id(&state.pool, id).await?;
    Ok(Json(s))
}

// ─── Task 3.3 ────────────────────────────────────────────────────────────────

/// `GET /syntheses` — list all syntheses readable by the auth agent.
///
/// Read-gated by [`SynthesisRepository::list_readable_by`]: owner rows,
/// `visibility = 'public'` rows, and rows with an explicit share to the
/// requesting agent are returned. Soft-deleted rows are excluded.
async fn list_syntheses(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<Synthesis>>, ApiError> {
    let s = SynthesisRepository::list_readable_by(&state.pool, auth.agent_id, 100, 0).await?;
    Ok(Json(s))
}

#[derive(Debug, Deserialize)]
pub struct RefineRequest {
    /// Optional override for the new synthesis's query. If omitted, the
    /// parent's query is reused — the common case for "re-narrate this with
    /// updated beliefs".
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub traversal_config: Option<serde_json::Value>,
    /// Visibility for the refined synthesis row. Defaults to `private`,
    /// matching `POST /syntheses`. The parent's visibility is intentionally
    /// not inherited — the caller may want to re-narrate a public synthesis
    /// privately or vice versa.
    #[serde(default = "default_visibility")]
    pub visibility: Visibility,
}

/// `POST /syntheses/{id}/refine` — create a NEW synthesis with
/// `parent_synthesis_id = {id}` and re-run the pipeline.
///
/// The parent must be readable by the requesting agent (owner / public /
/// shared); otherwise 404 to avoid existence leakage. Returns 202 with the
/// new synthesis id.
async fn refine_synthesis(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(parent_id): Path<Uuid>,
    Json(req): Json<RefineRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if !SynthesisRepository::readable_by(&state.pool, parent_id, auth.agent_id).await? {
        return Err(ApiError::NotFound(format!(
            "synthesis {parent_id} not found"
        )));
    }
    let parent = SynthesisRepository::get_by_id(&state.pool, parent_id).await?;
    let query = req.query.as_deref().unwrap_or(&parent.query);

    let new_id = enqueue_synthesis(
        &state,
        query,
        auth.agent_id,
        Some(parent_id),
        &[],
        req.traversal_config,
        req.visibility,
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "id": new_id,
            "parent_synthesis_id": parent_id,
            "status": "queued"
        })),
    ))
}

/// `DELETE /syntheses/{id}` — soft-delete a synthesis.
///
/// Owner-only — share recipients cannot delete. Sets `status = 'deleted'`.
/// Note: the `syntheses_check` constraint enforces
/// `(status='complete') = (narrative IS NOT NULL)`, so soft-deleting a row
/// that has already completed (with narrative populated) will fail at the DB
/// level. This is acceptable for v1 — a follow-up migration can loosen the
/// check, or a future version can null out the narrative on delete.
async fn soft_delete_synthesis(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let s = SynthesisRepository::get_by_id(&state.pool, id).await?;
    if s.agent_id != auth.agent_id {
        return Err(ApiError::Forbidden("only owner can delete".into()));
    }
    SynthesisRepository::update_status(&state.pool, id, SynthesisStatus::Deleted).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /syntheses/{id}/clusters` — list clusters for a synthesis.
async fn list_clusters(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Cluster>>, ApiError> {
    if !SynthesisRepository::readable_by(&state.pool, id, auth.agent_id).await? {
        return Err(ApiError::NotFound(format!("synthesis {id} not found")));
    }
    let clusters = SynthesisClustersRepository::list_by_synthesis(&state.pool, id).await?;
    Ok(Json(clusters))
}

/// `GET /syntheses/{id}/snapshot` — return the SubgraphSnapshot JSON.
async fn get_snapshot(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !SynthesisRepository::readable_by(&state.pool, id, auth.agent_id).await? {
        return Err(ApiError::NotFound(format!("synthesis {id} not found")));
    }
    let snap: serde_json::Value =
        sqlx::query_scalar("SELECT subgraph_snapshot FROM syntheses WHERE id = $1")
            .bind(id)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| ApiError::Internal(format!("snapshot: {e}")))?;
    Ok(Json(snap))
}

/// `GET /syntheses/{id}/staleness` — list staleness events for a synthesis.
async fn list_staleness(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<StalenessEvent>>, ApiError> {
    if !SynthesisRepository::readable_by(&state.pool, id, auth.agent_id).await? {
        return Err(ApiError::NotFound(format!("synthesis {id} not found")));
    }
    let events = SynthesisStalenessRepository::list_for_synthesis(&state.pool, id).await?;
    Ok(Json(events))
}

// ─── Task 3.4 ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GrantRequest {
    pub shared_with_agent_id: Uuid,
    /// Permission. v1 supports only `"read"`; other values are rejected.
    /// Forward-compatible: future permissions (e.g. `"write"`) can be added
    /// without changing the wire format.
    #[serde(default = "default_permission")]
    pub permission: String,
}

fn default_permission() -> String {
    "read".to_string()
}

/// `POST /syntheses/{id}/shares` — grant a share to another agent.
///
/// Owner-only. v1 only accepts `permission = "read"`.
async fn grant_share(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<GrantRequest>,
) -> Result<StatusCode, ApiError> {
    if req.permission != "read" {
        return Err(ApiError::Validation(format!(
            "unsupported permission '{}'; only 'read' is supported",
            req.permission
        )));
    }
    let s = SynthesisRepository::get_by_id(&state.pool, id).await?;
    if s.agent_id != auth.agent_id {
        return Err(ApiError::Forbidden("only owner can grant shares".into()));
    }
    SynthesisSharesRepository::grant(&state.pool, id, req.shared_with_agent_id, auth.agent_id)
        .await?;
    Ok(StatusCode::CREATED)
}

/// `DELETE /syntheses/{id}/shares/{agent_id}` — revoke a share.
///
/// Owner can revoke any share; the recipient can revoke their own share;
/// everyone else gets 403. Idempotent: revoking a non-existent share is a
/// no-op (still 204).
async fn revoke_share(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path((id, agent_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let s = SynthesisRepository::get_by_id(&state.pool, id).await?;
    let is_owner = s.agent_id == auth.agent_id;
    let is_self_revoke = agent_id == auth.agent_id;
    if !is_owner && !is_self_revoke {
        return Err(ApiError::Forbidden(
            "not authorized to revoke this share".into(),
        ));
    }
    SynthesisSharesRepository::revoke(&state.pool, id, agent_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /syntheses/{id}/shares` — list all share rows for a synthesis.
///
/// Owner-only — recipients can read the synthesis but not enumerate the
/// other recipients.
async fn list_shares(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Share>>, ApiError> {
    let s = SynthesisRepository::get_by_id(&state.pool, id).await?;
    if s.agent_id != auth.agent_id {
        return Err(ApiError::Forbidden("only owner can list shares".into()));
    }
    let shares = SynthesisSharesRepository::list(&state.pool, id).await?;
    Ok(Json(shares))
}

#[derive(Debug, Deserialize)]
pub struct VisibilityPatch {
    pub visibility: Visibility,
}

/// `PATCH /syntheses/{id}/visibility` — update the visibility column.
///
/// Owner-only. The new visibility value is validated by the
/// [`Visibility`] deserialiser (private/shared/public).
async fn update_visibility(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<VisibilityPatch>,
) -> Result<StatusCode, ApiError> {
    let s = SynthesisRepository::get_by_id(&state.pool, id).await?;
    if s.agent_id != auth.agent_id {
        return Err(ApiError::Forbidden(
            "only owner can change visibility".into(),
        ));
    }
    SynthesisRepository::update_visibility(&state.pool, id, req.visibility).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route(
            "/api/v1/eln/syntheses",
            post(create_synthesis).get(list_syntheses),
        )
        .route(
            "/api/v1/eln/syntheses/:id",
            get(get_synthesis).delete(soft_delete_synthesis),
        )
        .route("/api/v1/eln/syntheses/:id/refine", post(refine_synthesis))
        .route("/api/v1/eln/syntheses/:id/clusters", get(list_clusters))
        .route("/api/v1/eln/syntheses/:id/snapshot", get(get_snapshot))
        .route("/api/v1/eln/syntheses/:id/staleness", get(list_staleness))
        .route(
            "/api/v1/eln/syntheses/:id/shares",
            post(grant_share).get(list_shares),
        )
        .route(
            "/api/v1/eln/syntheses/:id/shares/:agent_id",
            delete(revoke_share),
        )
        .route(
            "/api/v1/eln/syntheses/:id/visibility",
            patch(update_visibility),
        )
        .with_state(state)
}
