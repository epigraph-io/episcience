//! REST surface for the synthesis pipeline.
//!
//! Ownership (E1d): a synthesis is owned by a GROUP, like the caller's kernel
//! data. Reads show it to members of its owner group (or to everyone when it
//! is `public`); edits (soft delete, visibility) need `admin` or `writer` in
//! the owner group. A synthesis the caller cannot read is a 404, exactly like
//! a missing one; one it can read but not edit is a 403.
//!
//! `POST /api/v1/eln/syntheses` inserts the `syntheses` row (owner: the
//! requested `owner_group_id` if the caller may write it, else the caller's
//! default group; visibility default `group`) and its `synthesis_jobs` row
//! (acting principal: the caller) in one transaction, and returns 202.
//! `POST …/:id/refine` creates a child; a child of a non-public parent is
//! owned by the parent's group, which the caller must be able to write.
//! Synthesis shares are retired (410): own the synthesis in a team group.

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use episcience_core::synthesis::{Cluster, StalenessEvent, Synthesis, SynthesisStatus};
use episcience_core::Ownership;
use episcience_db::errors::DbError;
use episcience_db::{
    SynthesisClustersRepository, SynthesisJobsRepository, SynthesisRepository,
    SynthesisStalenessRepository,
};

use crate::auth::tenancy::{child_ownership, root_ownership, RequestedVisibility, SHARES_RETIRED};
use crate::auth::viewer::caller_viewer;
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
    /// `group` (default; `private` is an alias) or `public`. `shared` is
    /// retired (410). A `public` synthesis whose inputs are not all public is
    /// narrowed to `group` when it completes.
    #[serde(default = "default_visibility")]
    pub visibility: RequestedVisibility,
    /// The owning group. Must be a group the caller may write (admin or
    /// writer); defaults to the caller's own default group.
    #[serde(default)]
    pub owner_group_id: Option<Uuid>,
    /// Optional skill selector. Defaults to `"baseline"` when omitted. Until
    /// Task 5.1 expands the `syntheses_skill_name_known` CHECK constraint, any
    /// value other than `"baseline"` will be rejected at the DB level —
    /// surfacing as a 500 here.
    #[serde(default)]
    pub skill_name: Option<String>,
    /// Optional EpiGraph workflow run correlation key. When set, the synthesis
    /// job will emit a `REFINES target_kind="workflow"` provo edge and include
    /// the id in `synthesis.complete` / `synthesis.failed` events so the
    /// triggering workflow can correlate the result. Omit for direct REST /
    /// MCP calls.
    #[serde(default)]
    pub workflow_run_id: Option<Uuid>,
    /// Autonomy level that produced this synthesis: "co_pilot" | "autopilot" | "autonomous".
    /// Stored as metadata; does not affect queue behavior directly.
    #[serde(default)]
    pub autonomy_level: Option<String>,
}

fn default_visibility() -> RequestedVisibility {
    RequestedVisibility::Group
}

/// Default skill name when the caller omits `skill_name` on
/// `POST /syntheses` (and the implicit choice for `refine_synthesis`
/// inheritance fallbacks).
const DEFAULT_SKILL_NAME: &str = "baseline";

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
    owner: Ownership,
    skill_name: &str,
    workflow_run_id: Option<Uuid>,
    autonomy_level: Option<&str>,
) -> Result<Uuid, ApiError> {
    let id = Uuid::now_v7();
    let payload = SynthesisJobPayload {
        synthesis_id: id,
        query: query.to_string(),
        traversal_config,
        agent_id,
        parent_synthesis_id,
        prereq_synthesis_ids: prereq_synthesis_ids.to_vec(),
        workflow_run_id,
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
        owner,
        skill_name,
        autonomy_level,
    )
    .await?;

    // The job acts as the caller (`agent_id` here is the authenticated
    // principal), supplied explicitly: the database refuses a job without one.
    SynthesisJobsRepository::enqueue_tx(&mut tx, id, agent_id, &payload_json).await?;

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
    let visibility = req.visibility.resolve()?;
    let viewer = caller_viewer(&state.pool, &auth).await?;

    // A referenced parent or prerequisite must be readable by the caller.
    // Unreadable and missing ids get the SAME 404, so the request is not an
    // existence oracle.
    for referenced in req
        .parent_synthesis_id
        .iter()
        .chain(req.prereq_synthesis_ids.iter())
    {
        if !SynthesisRepository::readable_by(&state.pool, *referenced, &viewer).await? {
            return Err(ApiError::NotFound(format!(
                "synthesis {referenced} not found"
            )));
        }
    }

    let owner = match req.parent_synthesis_id {
        Some(parent_id) => {
            let parent = SynthesisRepository::get_readable(&state.pool, parent_id, &viewer).await?;
            child_ownership(
                &state.pool,
                &viewer,
                &parent,
                req.owner_group_id,
                visibility,
            )
            .await?
        }
        None => root_ownership(&state.pool, &viewer, req.owner_group_id, visibility).await?,
    };

    let skill_name = req.skill_name.as_deref().unwrap_or(DEFAULT_SKILL_NAME);
    let id = enqueue_synthesis(
        &state,
        &req.query,
        auth.agent_id,
        req.parent_synthesis_id,
        &req.prereq_synthesis_ids,
        req.traversal_config,
        owner,
        skill_name,
        req.workflow_run_id,
        req.autonomy_level.as_deref(),
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
    // Invisible and missing rows are indistinguishable from the outside (both
    // 404), so the route is not an existence oracle.
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let s = SynthesisRepository::get_readable(&state.pool, id, &viewer)
        .await
        .map_err(|e| match e {
            DbError::NotFound { .. } => ApiError::NotFound(format!("synthesis {id} not found")),
            other => other.into(),
        })?;
    Ok(Json(s))
}

/// 404 unless `viewer` can read synthesis `id`.
async fn require_readable(
    state: &ElnState,
    viewer: &epigraph_db::Viewer,
    id: Uuid,
) -> Result<(), ApiError> {
    if SynthesisRepository::readable_by(&state.pool, id, viewer).await? {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("synthesis {id} not found")))
    }
}

// ─── Task 3.3 ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Include syntheses whose `stale_since IS NOT NULL`. Default `false` —
    /// stale rows are hidden from the default list to match the recall /
    /// search surfaces (Task 3.5 / 3.8). Clients that want to surface
    /// drifted rows must opt in explicitly.
    #[serde(default)]
    pub include_stale: bool,
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// Filter to syntheses with the given `skill_name`. Used by the
    /// Phase 8 review-bot to find candidates of a specific kind
    /// (e.g. `?skill_name=code_review`). Omit to disable filtering.
    #[serde(default)]
    pub skill_name: Option<String>,
}

fn default_list_limit() -> i64 {
    100
}

/// `GET /syntheses` — list the syntheses the caller can read: public ones and
/// those owned by any of the caller's groups. Soft-deleted rows are excluded;
/// stale rows unless `?include_stale=true`.
async fn list_syntheses(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Synthesis>>, ApiError> {
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let s = SynthesisRepository::list_readable_by(
        &state.pool,
        &viewer,
        q.limit,
        q.offset,
        q.include_stale,
        q.skill_name.as_deref(),
    )
    .await?;
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
    /// Visibility for the refined synthesis row. Defaults to `group`,
    /// matching `POST /syntheses`.
    #[serde(default = "default_visibility")]
    pub visibility: RequestedVisibility,
    /// The owning group. A refinement of a non-public parent is owned by the
    /// parent's group (naming another is 403).
    #[serde(default)]
    pub owner_group_id: Option<Uuid>,
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
    let visibility = req.visibility.resolve()?;
    let viewer = caller_viewer(&state.pool, &auth).await?;
    let parent = SynthesisRepository::get_readable(&state.pool, parent_id, &viewer)
        .await
        .map_err(|e| match e {
            DbError::NotFound { .. } => {
                ApiError::NotFound(format!("synthesis {parent_id} not found"))
            }
            other => other.into(),
        })?;
    let owner = child_ownership(
        &state.pool,
        &viewer,
        &parent,
        req.owner_group_id,
        visibility,
    )
    .await?;
    let query = req.query.as_deref().unwrap_or(&parent.query);

    // Inherit the parent's skill_name so a refinement re-runs the same skill
    // by default. `Synthesis` itself doesn't carry `skill_name`, so read it
    // directly off the (already readable) row.
    let parent_skill: String = sqlx::query_scalar("SELECT skill_name FROM syntheses WHERE id = $1")
        .bind(parent_id)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(format!("read parent skill_name: {e}")))?;

    let new_id = enqueue_synthesis(
        &state,
        query,
        auth.agent_id,
        Some(parent_id),
        &[],
        req.traversal_config,
        owner,
        &parent_skill,
        None,
        None,
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
/// Needs `admin` or `writer` in the owner group (403 for a reader; 404 when
/// the caller cannot read it). Sets `status = 'deleted'`. Note: the
/// `syntheses_check` constraint enforces `(status='complete') = (narrative
/// IS NOT NULL)`, so soft-deleting a row that already completed fails at the
/// DB level (accepted for v1).
async fn soft_delete_synthesis(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let viewer = caller_viewer(&state.pool, &auth).await?;
    require_readable(&state, &viewer, id).await?;
    match SynthesisRepository::update_status_as(&state.pool, id, SynthesisStatus::Deleted, &viewer)
        .await
    {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(DbError::NotFound { .. }) => Err(ApiError::Forbidden(
            "deleting a synthesis needs write access to its owner group".into(),
        )),
        Err(e) => Err(e.into()),
    }
}

/// `GET /syntheses/{id}/clusters` — list clusters for a synthesis.
async fn list_clusters(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Cluster>>, ApiError> {
    let viewer = caller_viewer(&state.pool, &auth).await?;
    require_readable(&state, &viewer, id).await?;
    let clusters = SynthesisClustersRepository::list_by_synthesis(&state.pool, id).await?;
    Ok(Json(clusters))
}

/// `GET /syntheses/{id}/snapshot` — return the SubgraphSnapshot JSON.
async fn get_snapshot(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let viewer = caller_viewer(&state.pool, &auth).await?;
    require_readable(&state, &viewer, id).await?;
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
    let viewer = caller_viewer(&state.pool, &auth).await?;
    require_readable(&state, &viewer, id).await?;
    let events = SynthesisStalenessRepository::list_for_synthesis(&state.pool, id).await?;
    Ok(Json(events))
}

// ─── Shares (retired) and visibility ────────────────────────────────────────

/// Every synthesis-share route: 410. The kernel's only sharing primitive is
/// group ownership; the `synthesis_shares` table is frozen.
async fn shares_retired() -> ApiError {
    ApiError::Gone(SHARES_RETIRED.into())
}

#[derive(Debug, Deserialize)]
pub struct VisibilityPatch {
    pub visibility: RequestedVisibility,
}

/// `PATCH /syntheses/{id}/visibility` — set `group` or `public`.
///
/// Needs `admin` or `writer` in the owner group (403 for a reader; 404 when
/// the caller cannot read it). `shared` is 410. Widening to `public` sets the
/// transaction-local widening interlock the database requires, and the
/// database refuses (403) unless every input is public (member claims, the
/// parent, every prerequisite). It also releases the synthesis' deferred
/// outbox rows.
async fn update_visibility(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<VisibilityPatch>,
) -> Result<StatusCode, ApiError> {
    let visibility = req.visibility.resolve()?;
    let viewer = caller_viewer(&state.pool, &auth).await?;
    require_readable(&state, &viewer, id).await?;
    match SynthesisRepository::set_visibility_as(&state.pool, id, visibility, &viewer).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(DbError::NotFound { .. }) => Err(ApiError::Forbidden(
            "changing visibility needs write access to the owner group".into(),
        )),
        Err(e) => Err(e.into()),
    }
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
            post(shares_retired).get(shares_retired),
        )
        .route(
            "/api/v1/eln/syntheses/:id/shares/:agent_id",
            delete(shares_retired),
        )
        .route(
            "/api/v1/eln/syntheses/:id/visibility",
            patch(update_visibility),
        )
        .with_state(state)
}
