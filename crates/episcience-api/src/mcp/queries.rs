//! Query MCP tools — `recall_synthesis`, `get_synthesis`, `list_syntheses`.
//!
//! Read-only MCP wrappers around the same repos the REST routes use. The
//! caller's viewer (public, or owned by one of the caller's groups) is spliced
//! into every read inside the repo helpers.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::{SynthesisEmbeddingsRepository, SynthesisRepository};

use crate::mcp::errors::{internal_error, invalid_params, invalid_request, McpError};
use crate::mcp::EpiscienceServer;
use crate::middleware::AuthContext;

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
    _auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
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
    let mut conn = server
        .db
        .read_as(viewer)
        .await
        .map_err(crate::mcp::from_refusal)?;
    let hits = SynthesisEmbeddingsRepository::search(
        &mut *conn,
        &embedding,
        args.limit.unwrap_or(DEFAULT_RECALL_LIMIT),
        args.min_score.unwrap_or(0.0),
        viewer,
        args.include_stale.unwrap_or(false),
    )
    .await
    .map_err(|e| internal_error(format!("search: {e}")))?;

    let result: Vec<RecallHit> = hits
        .into_iter()
        .map(|(synthesis_id, score)| RecallHit {
            synthesis_id,
            score,
        })
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
    _auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: GetSynthesisArgs,
) -> Result<CallToolResult, McpError> {
    // Invisible and missing rows are indistinguishable ('not found'), so the
    // tool is not an existence oracle.
    let mut conn = server
        .db
        .read_as(viewer)
        .await
        .map_err(crate::mcp::from_refusal)?;
    let synth = match SynthesisRepository::get_readable(&mut *conn, args.synthesis_id, viewer).await
    {
        Ok(s) => s,
        Err(episcience_db::errors::DbError::NotFound { .. }) => {
            return Err(invalid_request(format!(
                "synthesis {} not found",
                args.synthesis_id
            )))
        }
        Err(e) => return Err(internal_error(format!("get_readable: {e}"))),
    };
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

    /// Include syntheses that have been marked stale. Default false —
    /// matches the default behaviour of `recall_synthesis` and the REST
    /// `GET /syntheses` route.
    #[schemars(description = "Include stale syntheses (default false)")]
    #[serde(default)]
    pub include_stale: Option<bool>,

    /// Filter to syntheses produced by the given skill (e.g.
    /// `"code_review"`). Used by the Phase 8 review-bot to find
    /// candidates of a specific kind without scanning all readable
    /// syntheses. Omit to disable the filter.
    #[schemars(description = "Filter to syntheses with the given skill_name (e.g. code_review)")]
    #[serde(default)]
    pub skill_name: Option<String>,
}

pub async fn list(
    server: &EpiscienceServer,
    _auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: ListSynthesesArgs,
) -> Result<CallToolResult, McpError> {
    let mut conn = server
        .db
        .read_as(viewer)
        .await
        .map_err(crate::mcp::from_refusal)?;
    let rows = SynthesisRepository::list_readable_by(
        &mut *conn,
        viewer,
        args.limit.unwrap_or(DEFAULT_LIST_LIMIT),
        args.offset.unwrap_or(0),
        args.include_stale.unwrap_or(false),
        args.skill_name.as_deref(),
    )
    .await
    .map_err(|e| internal_error(format!("list_readable_by: {e}")))?;
    let body = serde_json::to_string_pretty(&rows).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
