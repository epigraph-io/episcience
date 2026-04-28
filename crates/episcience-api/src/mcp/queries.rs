//! Query MCP tools — `recall_synthesis`, `get_synthesis`, `list_syntheses`.
//!
//! Phase 3 Task 3.8: read-only MCP wrappers around the same repos the REST
//! routes use. The visibility predicate (owner / public / explicit share) is
//! enforced inside the repo helpers, not here — these wrappers are pure
//! plumbing.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::{SynthesisEmbeddingsRepository, SynthesisRepository};

use crate::mcp::errors::{internal_error, invalid_params, invalid_request, McpError};
use crate::mcp::EpiscienceServer;

const DEFAULT_RECALL_LIMIT: usize = 20;
const DEFAULT_LIST_LIMIT: i64 = 100;

// ─── recall_synthesis ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallSynthesisArgs {
    /// Natural-language query. Embedded with the same provider the worker
    /// uses at write time so cosine scores are comparable.
    #[schemars(description = "Natural-language query for semantic search")]
    pub query: String,

    /// Maximum number of hits. Default 20.
    #[schemars(description = "Maximum number of hits (default 20)")]
    #[serde(default)]
    pub limit: Option<usize>,

    /// Minimum cosine similarity. Default 0.0.
    #[schemars(description = "Minimum cosine similarity (default 0.0)")]
    #[serde(default)]
    pub min_score: Option<f64>,

    /// Include syntheses that have been marked stale. Default false.
    #[schemars(description = "Include stale syntheses (default false)")]
    #[serde(default)]
    pub include_stale: Option<bool>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RecallHit {
    pub synthesis_id: Uuid,
    pub score: f64,
}

pub async fn recall(
    server: &EpiscienceServer,
    args: RecallSynthesisArgs,
) -> Result<CallToolResult, McpError> {
    if args.query.trim().is_empty() {
        return Err(invalid_params("query cannot be empty"));
    }
    let embedding = server
        .embedder
        .generate_query(&args.query)
        .await
        .map_err(|e| internal_error(format!("embed query: {e}")))?;
    let hits = SynthesisEmbeddingsRepository::search(
        &server.pool,
        &embedding,
        args.limit.unwrap_or(DEFAULT_RECALL_LIMIT),
        args.min_score.unwrap_or(0.0),
        server.auth_agent_id,
        args.include_stale.unwrap_or(false),
    )
    .await
    .map_err(|e| internal_error(format!("search: {e}")))?;

    let result: Vec<RecallHit> = hits
        .into_iter()
        .map(|(synthesis_id, score)| RecallHit { synthesis_id, score })
        .collect();
    let body = serde_json::to_string_pretty(&result).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}

// ─── get_synthesis ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSynthesisArgs {
    #[schemars(description = "Synthesis id (UUID)")]
    pub synthesis_id: Uuid,
}

pub async fn get(
    server: &EpiscienceServer,
    args: GetSynthesisArgs,
) -> Result<CallToolResult, McpError> {
    // Read-predicate gate. Strangers and missing rows are indistinguishable
    // from the outside (both 'not found') — this is intentional, to avoid
    // leaking the existence of private syntheses.
    if !SynthesisRepository::readable_by(&server.pool, args.synthesis_id, server.auth_agent_id)
        .await
        .map_err(|e| internal_error(format!("readable_by: {e}")))?
    {
        return Err(invalid_request(format!(
            "synthesis {} not found",
            args.synthesis_id
        )));
    }
    let synth = SynthesisRepository::get_by_id(&server.pool, args.synthesis_id)
        .await
        .map_err(|e| internal_error(format!("get_by_id: {e}")))?;
    let body = serde_json::to_string_pretty(&synth).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}

// ─── list_syntheses ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSynthesesArgs {
    /// Maximum number of rows. Default 100.
    #[schemars(description = "Maximum number of rows (default 100)")]
    #[serde(default)]
    pub limit: Option<i64>,

    /// Offset into the result set (for pagination). Default 0.
    #[schemars(description = "Offset (default 0)")]
    #[serde(default)]
    pub offset: Option<i64>,
}

pub async fn list(
    server: &EpiscienceServer,
    args: ListSynthesesArgs,
) -> Result<CallToolResult, McpError> {
    let rows = SynthesisRepository::list_readable_by(
        &server.pool,
        server.auth_agent_id,
        args.limit.unwrap_or(DEFAULT_LIST_LIMIT),
        args.offset.unwrap_or(0),
    )
    .await
    .map_err(|e| internal_error(format!("list_readable_by: {e}")))?;
    let body = serde_json::to_string_pretty(&rows).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
