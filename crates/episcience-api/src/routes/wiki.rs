//! Wiki page registry (plan 2026-10-08-wiki-phase-b-articles.md, Task 5),
//! the read API Phase C (Explorer `/wiki`) renders.
//!
//! - `GET /api/v1/eln/wiki`: every page the caller can read, each with its
//!   current (latest complete) article.
//! - `GET /api/v1/eln/wiki/:group_id/:wiki_key`: one page, its article text
//!   and its history (every generation, newest first, any status).
//!
//! Both read on the caller's stamped session. A malformed key, a group the
//! caller cannot read, and a key with no complete generation are all the
//! same 404, never an empty 200, so the route is not an existence oracle.

use axum::{
    extract::{Extension, Path, State},
    routing::get,
    Json, Router,
};
use uuid::Uuid;

use episcience_core::wiki::WikiKey;
use episcience_db::{WikiPageDetail, WikiPageRow, WikiRepository};

use crate::errors::ApiError;
use crate::middleware::CallerViewer;
use crate::state::ElnState;

async fn list_pages(
    State(state): State<ElnState>,
    Extension(viewer): Extension<CallerViewer>,
) -> Result<Json<Vec<WikiPageRow>>, ApiError> {
    let mut conn = state.db.read_as(&viewer).await?;
    let pages = WikiRepository::list_pages(&mut conn, &viewer).await?;
    Ok(Json(pages))
}

async fn get_page(
    State(state): State<ElnState>,
    Extension(viewer): Extension<CallerViewer>,
    Path((group_id, wiki_key)): Path<(Uuid, String)>,
) -> Result<Json<WikiPageDetail>, ApiError> {
    let not_found = || ApiError::NotFound(format!("wiki page {group_id}/{wiki_key} not found"));
    // Only the canonical slug names a page (`WikiKey::parse_slug` refuses
    // non-canonical spellings), checked before any read.
    if WikiKey::parse_slug(&wiki_key).is_none() {
        return Err(not_found());
    }
    let mut conn = state.db.read_as(&viewer).await?;
    WikiRepository::get_page(&mut conn, &viewer, group_id, &wiki_key)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/wiki", get(list_pages))
        .route("/api/v1/eln/wiki/:group_id/:wiki_key", get(get_page))
        .with_state(state)
}
