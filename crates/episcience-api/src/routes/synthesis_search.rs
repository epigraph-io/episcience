//! Phase 3 Task 3.5: `POST /api/v1/eln/syntheses/search`
//!
//! Semantic search over `synthesis_embeddings`. The route embeds the caller's
//! query string with the shared [`epigraph_embeddings::EmbeddingService`] (the
//! same instance the worker uses to embed at write time), then delegates to
//! [`SynthesisEmbeddingsRepository::search`], which already enforces the
//! visibility predicate (owner / public / explicit share).
//!
//! Defaults: `limit = 20`, `min_score = 0.0`, `include_stale = false`. Stale
//! syntheses are excluded by default — clients that want to surface drifted
//! syntheses must opt in explicitly.

use axum::{
    extract::{Extension, State},
    routing::post,
    Json, Router,
};
use episcience_db::SynthesisEmbeddingsRepository;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::ApiError;
use crate::middleware::AuthContext;
use crate::state::ElnState;

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_min_score")]
    pub min_score: f64,
    #[serde(default)]
    pub include_stale: bool,
}

fn default_limit() -> usize {
    20
}

fn default_min_score() -> f64 {
    0.0
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub synthesis_id: Uuid,
    pub score: f64,
}

async fn search(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<Vec<SearchHit>>, ApiError> {
    if req.query.trim().is_empty() {
        return Err(ApiError::Validation("query cannot be empty".into()));
    }
    let embedding = state
        .embedder
        .generate_query(&req.query)
        .await
        .map_err(|e| ApiError::Internal(format!("embed query: {e}")))?;
    let hits = SynthesisEmbeddingsRepository::search(
        &state.pool,
        &embedding,
        req.limit,
        req.min_score,
        auth.agent_id,
        req.include_stale,
    )
    .await?;
    Ok(Json(
        hits.into_iter()
            .map(|(synthesis_id, score)| SearchHit {
                synthesis_id,
                score,
            })
            .collect(),
    ))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/syntheses/search", post(search))
        .with_state(state)
}
