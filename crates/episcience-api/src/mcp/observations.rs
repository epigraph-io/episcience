//! `add_observation` MCP tool — mirrors
//! `POST /api/v1/eln/samples/:id/observations`.
//!
//! Phase 8 ELN write parity. Delegates to
//! [`SampleRepository::add_observation`], the same helper the HTTP route
//! refactor calls. Inserts a `claims` row at `truth_value=0.5` plus a
//! `sample_claims` link row in one transaction.
//!
//! Auth: the claim's `agent_id` is the authenticated caller
//! (`AuthContext.agent_id`); MCP clients cannot post an observation under
//! another agent's identity.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::SampleRepository;

use crate::mcp::errors::{internal_error, invalid_params, invalid_request, McpError};
use crate::mcp::EpiscienceServer;
use crate::middleware::AuthContext;

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
    auth: &AuthContext,
    args: AddObservationArgs,
) -> Result<CallToolResult, McpError> {
    if args.content.trim().is_empty() {
        return Err(invalid_params("content cannot be empty"));
    }

    // The target sample must be prepared by the caller. A sample owned by
    // anyone else gets the same answer as a missing one (mirrors the HTTP
    // route's 404).
    SampleRepository::get_owned_by(&server.pool, args.sample_id, auth.agent_id)
        .await
        .map_err(|e| match e {
            episcience_db::errors::DbError::NotFound { .. } => {
                invalid_request(format!("sample {} not found", args.sample_id))
            }
            other => internal_error(format!("sample lookup: {other}")),
        })?;

    let relationship = args
        .relationship
        .unwrap_or_else(|| DEFAULT_RELATIONSHIP.to_string());

    let claim_id = SampleRepository::add_observation(
        &server.pool,
        args.sample_id,
        auth.agent_id,
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
