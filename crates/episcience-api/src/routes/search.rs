use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_db::NotebookRepository;

#[derive(Deserialize)]
pub struct FullTextParams {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    20
}

#[derive(Serialize)]
pub struct SearchResult {
    pub claim_id: Uuid,
    pub content: String,
    pub rank: f32,
}

async fn fulltext_search(
    State(state): State<ElnState>,
    Extension(_auth): Extension<crate::middleware::AuthContext>,
    Query(params): Query<FullTextParams>,
) -> Result<Json<Vec<SearchResult>>, ApiError> {
    if params.q.trim().is_empty() {
        return Err(ApiError::Validation("query cannot be empty".into()));
    }

    let results = NotebookRepository::fulltext_search(
        &state.pool,
        &params.q,
        params.limit.min(100),
    )
    .await?;

    Ok(Json(
        results
            .into_iter()
            .map(|r| SearchResult {
                claim_id: r.claim_id,
                content: r.content,
                rank: r.rank,
            })
            .collect(),
    ))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/search/fulltext", get(fulltext_search))
        .with_state(state)
}
