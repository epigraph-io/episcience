//! `add_observation` MCP tool — mirrors
//! `POST /api/v1/eln/samples/:id/observations`.
//!
//! Phase 8 ELN write parity. Delegates to
//! [`SampleRepository::add_observation`], the same helper the HTTP route
//! refactor calls. Inserts a `claims` row at `truth_value=0.5` plus a
//! `sample_claims` link row in one transaction.
//!
//! Auth: the claim's `agent_id` is pinned to
//! `EpiscienceServer::auth_agent_id`; MCP clients cannot post an observation
//! under another agent's identity.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::SampleRepository;

use crate::mcp::errors::{internal_error, invalid_params, McpError};
use crate::mcp::EpiscienceServer;

const DEFAULT_RELATIONSHIP: &str = "observation";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddObservationArgs {
    /// Target sample id. Must already exist.
    #[schemars(description = "Target sample id (must already exist)")]
    pub sample_id: Uuid,

    /// Free-text observation content. Becomes the `claims.content` value.
    #[schemars(description = "Free-text observation content (non-empty)")]
    pub content: String,

    /// Edge label written into `sample_claims.relationship`. Defaults to
    /// `"observation"`. Common values: `observation`, `measurement`,
    /// `note`.
    #[schemars(description = "sample_claims.relationship label (default: 'observation')")]
    #[serde(default)]
    pub relationship: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AddObservationResult {
    pub claim_id: Uuid,
    pub sample_id: Uuid,
    pub relationship: String,
}

pub async fn handle(
    server: &EpiscienceServer,
    args: AddObservationArgs,
) -> Result<CallToolResult, McpError> {
    if args.content.trim().is_empty() {
        return Err(invalid_params("content cannot be empty"));
    }

    // Verify sample exists (mirrors HTTP route's pre-check; surfaces a clean
    // NotFound rather than an FK violation).
    let _sample = SampleRepository::get_by_id(&server.pool, args.sample_id)
        .await
        .map_err(|e| internal_error(format!("sample lookup: {e}")))?;

    let relationship = args
        .relationship
        .unwrap_or_else(|| DEFAULT_RELATIONSHIP.to_string());

    let claim_id = SampleRepository::add_observation(
        &server.pool,
        args.sample_id,
        server.auth_agent_id,
        &args.content,
        &relationship,
    )
    .await
    .map_err(|e| internal_error(format!("add observation: {e}")))?;

    let body = AddObservationResult {
        claim_id,
        sample_id: args.sample_id,
        relationship,
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
