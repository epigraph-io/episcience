//! `synthesize` MCP tool — mirrors `POST /api/v1/eln/syntheses`.
//!
//! Atomically inserts a `syntheses` row in `pending` state and a
//! `synthesis_jobs` row in `'queued'` state in a single transaction (same
//! repo helpers the REST route uses), then optionally polls until the
//! synthesis reaches a terminal state.
//!
//! v1 limitations:
//!  - Polling timeout is clamped to 600s; most MCP clients have shorter call
//!    timeouts than that. For long-running syntheses prefer the no-wait form
//!    and use `get_synthesis` to follow up.
//!  - `agent_id` is the service's `auth_agent_id` (set at construction). v2
//!    should resolve a real agent from per-call MCP auth headers.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_core::synthesis::{SynthesisStatus, Visibility};
use episcience_db::{SynthesisJobsRepository, SynthesisRepository};

use crate::mcp::errors::{internal_error, invalid_params, McpError};
use crate::mcp::EpiscienceServer;

/// Polling cadence for `wait_for_completion`. The same 2 s rhythm the manual
/// `curl` smoke loop uses — fast enough that a small synthesis returns
/// promptly, slow enough that we don't hammer the DB.
const POLL_INTERVAL_SECS: u64 = 2;

/// Hard ceiling on `timeout_seconds`. The MCP transport itself often has a
/// shorter timeout than this; the cap is a safety net, not a SLA.
const POLL_TIMEOUT_CAP_SECS: u64 = 600;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SynthesizeArgs {
    /// Natural-language query for synthesis (e.g. "consensus on DNA origami
    /// thermal stability").
    #[schemars(description = "Natural-language query for synthesis")]
    pub query: String,

    /// Optional traversal config override. If omitted the worker uses
    /// pipeline defaults.
    #[schemars(
        description = "Optional traversal config (TraversalConfig JSON); omit for pipeline defaults"
    )]
    #[serde(default)]
    pub traversal_config: Option<serde_json::Value>,

    /// Optional parent synthesis to refine. The new synthesis records
    /// `parent_synthesis_id` and re-runs the pipeline.
    #[schemars(description = "Optional parent synthesis id to refine")]
    #[serde(default)]
    pub parent_synthesis_id: Option<Uuid>,

    /// Optional prerequisite syntheses. The worker waits for these to
    /// complete (or fail) before running this synthesis.
    #[schemars(description = "Optional prerequisite synthesis ids")]
    #[serde(default)]
    pub prereq_synthesis_ids: Vec<Uuid>,

    /// If true, poll until the synthesis reaches a terminal state and
    /// return the full narrative. Default `false` (returns immediately
    /// with `status='queued'`).
    #[schemars(description = "Block until terminal state. Default: false (returns 'queued').")]
    #[serde(default)]
    pub wait_for_completion: bool,

    /// Polling timeout in seconds. Clamped to 600s. Only consulted when
    /// `wait_for_completion=true`.
    #[schemars(description = "Polling timeout (s) when wait_for_completion=true. Clamp: 600.")]
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,

    /// Visibility for the new synthesis row. One of `private` | `shared` |
    /// `public`. Default `private`.
    #[schemars(description = "Visibility: private | shared | public. Default: private.")]
    #[serde(default = "default_visibility")]
    pub visibility: String,
}

fn default_timeout() -> u64 {
    POLL_TIMEOUT_CAP_SECS
}

fn default_visibility() -> String {
    "private".to_string()
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SynthesizeResult {
    pub synthesis_id: Uuid,
    pub status: String,
    /// Populated only when `wait_for_completion=true` and the row reaches
    /// `status='complete'`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
}

/// Free-function delegate (the `#[tool]` method on `EpiscienceServer` is a
/// thin wrapper). Mirrors the `tools::claims::submit_claim(server, params)`
/// shape from epigraph-mcp.
pub async fn handle(
    server: &EpiscienceServer,
    args: SynthesizeArgs,
) -> Result<CallToolResult, McpError> {
    if args.query.trim().is_empty() {
        return Err(invalid_params("query cannot be empty"));
    }
    let visibility: Visibility = args
        .visibility
        .parse()
        .map_err(|e: String| invalid_params(format!("visibility: {e}")))?;

    let id = Uuid::now_v7();
    let payload = serde_json::json!({
        "synthesis_id": id,
        "query": args.query,
        "traversal_config": args.traversal_config,
        "agent_id": server.auth_agent_id,
        "parent_synthesis_id": args.parent_synthesis_id,
        "prereq_synthesis_ids": args.prereq_synthesis_ids,
    });

    // ── Atomic insert: synthesis row + job row in one transaction ────────────
    //
    // Mirrors `routes/syntheses.rs::enqueue_synthesis`. Either both land or
    // neither, so the worker never sees an orphaned synthesis row without a
    // queued job (or vice versa).
    let mut tx = server
        .pool
        .begin()
        .await
        .map_err(|e| internal_error(format!("tx begin: {e}")))?;

    SynthesisRepository::create_pending_tx(
        &mut tx,
        id,
        &args.query,
        server.auth_agent_id,
        args.parent_synthesis_id,
        &args.prereq_synthesis_ids,
        &server.llm_default_provider,
        &server.llm_default_model,
        visibility,
    )
    .await
    .map_err(|e| internal_error(format!("create synthesis: {e}")))?;

    SynthesisJobsRepository::enqueue_tx(&mut tx, id, &payload)
        .await
        .map_err(|e| internal_error(format!("enqueue job: {e}")))?;

    tx.commit()
        .await
        .map_err(|e| internal_error(format!("tx commit: {e}")))?;

    // ── Optional poll-to-completion ──────────────────────────────────────────
    let mut result = SynthesizeResult {
        synthesis_id: id,
        status: "queued".to_string(),
        narrative: None,
    };

    if args.wait_for_completion {
        let timeout = std::time::Duration::from_secs(args.timeout_seconds.min(POLL_TIMEOUT_CAP_SECS));
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match SynthesisRepository::get_by_id(&server.pool, id).await {
                Ok(synth) => match synth.status {
                    SynthesisStatus::Complete => {
                        result.status = "complete".to_string();
                        result.narrative = synth.narrative;
                        break;
                    }
                    SynthesisStatus::Failed => {
                        result.status = "failed".to_string();
                        break;
                    }
                    SynthesisStatus::Deleted => {
                        // Soft-deleted while we were waiting — treat as terminal.
                        result.status = "deleted".to_string();
                        break;
                    }
                    SynthesisStatus::Pending | SynthesisStatus::Running => {
                        // Still in flight — fall through to sleep.
                    }
                },
                Err(_) => {
                    // Transient DB error (or row not yet visible on a replica).
                    // Treat the same as still-pending and try again.
                }
            }
            if std::time::Instant::now() >= deadline {
                // Timeout — leave status as 'queued' (or whatever non-terminal
                // state the caller saw last). The synthesis is still in the
                // queue; the caller can poll via `get_synthesis`.
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    let body = serde_json::to_string_pretty(&result).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
