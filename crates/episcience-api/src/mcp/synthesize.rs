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
//!  - The synthesis is authored by the authenticated caller and owned by a
//!    GROUP: `owner_group_id` if the caller may write it, else the caller's
//!    default group (a refinement of a non-public parent: the parent's
//!    group). Its job acts as the caller. Exactly as the REST route does.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use episcience_core::synthesis::SynthesisStatus;
use episcience_core::Ownership;
use episcience_db::{SynthesisJobsRepository, SynthesisRepository};

use crate::auth::tenancy::{child_ownership, root_ownership, RequestedVisibility};
use crate::mcp::errors::{from_api, internal_error, invalid_params, invalid_request, McpError};
use crate::mcp::{from_refusal, EpiscienceServer};
use crate::middleware::AuthContext;

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

    /// Visibility for the new synthesis row: `group` (default; `private` is
    /// accepted as its alias) or `public`. `shared` is retired: own the
    /// synthesis in a team group instead.
    #[schemars(
        description = "Visibility: group | public (\"private\" = group). Default: group. A public synthesis whose inputs are not all public is narrowed to group when it completes."
    )]
    #[serde(default = "default_visibility")]
    pub visibility: String,

    /// The owning group (must be a group the caller may write). Default:
    /// the caller's own default group.
    #[schemars(
        description = "Optional owner group id (a group the caller may write); default: the caller's own group"
    )]
    #[serde(default)]
    pub owner_group_id: Option<Uuid>,
}

fn default_timeout() -> u64 {
    POLL_TIMEOUT_CAP_SECS
}

fn default_visibility() -> String {
    "group".to_string()
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
    auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: SynthesizeArgs,
) -> Result<CallToolResult, McpError> {
    if args.query.trim().is_empty() {
        return Err(invalid_params("query cannot be empty"));
    }
    let visibility = RequestedVisibility::parse(&args.visibility)
        .map_err(invalid_params)?
        .resolve()
        .map_err(from_api)?;
    // Every read that decides this write, and the write (synthesis row + job
    // row), on ONE stamped transaction: either both land or neither, so the
    // worker never sees an orphaned synthesis row without a queued job.
    let mut tx = server.db.write_as(viewer).await.map_err(from_refusal)?;

    // A referenced parent or prerequisite must be readable by the caller.
    // Unreadable and missing ids get the same "not found" (as `get_synthesis`),
    // so the tool is not an existence oracle.
    for referenced in args
        .parent_synthesis_id
        .iter()
        .chain(args.prereq_synthesis_ids.iter())
    {
        if !SynthesisRepository::readable_by(&mut *tx, *referenced, viewer)
            .await
            .map_err(|e| internal_error(format!("readable_by: {e}")))?
        {
            return Err(invalid_request(format!("synthesis {referenced} not found")));
        }
    }
    let visibility = crate::auth::tenancy::narrow_for_prerequisites(
        &mut tx,
        viewer,
        visibility,
        &args.prereq_synthesis_ids,
    )
    .await
    .map_err(from_api)?;
    let owner = match args.parent_synthesis_id {
        Some(parent_id) => {
            let parent = SynthesisRepository::get_readable(&mut *tx, parent_id, viewer)
                .await
                .map_err(|e| internal_error(format!("read parent: {e}")))?;
            child_ownership(&mut tx, viewer, &parent, args.owner_group_id, visibility)
                .await
                .map_err(from_api)?
        }
        None => root_ownership(&mut tx, viewer, args.owner_group_id, visibility)
            .await
            .map_err(from_api)?,
    };

    let id = enqueue(
        &mut tx,
        server,
        auth,
        EnqueueSpec {
            query: &args.query,
            owner,
            // Phase 8 adds an MCP-surface skill_name argument; until then the
            // MCP path always defaults to `"baseline"`. Hard-coded here rather
            // than pulled from `args` because the public MCP schema cannot
            // accept the field yet (would mislead clients into thinking it's
            // wired through).
            skill_name: "baseline",
            parent_synthesis_id: args.parent_synthesis_id,
            prereq_synthesis_ids: &args.prereq_synthesis_ids,
            traversal_config: args.traversal_config,
            seed_theme_id: None,
        },
    )
    .await?;

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
        (result.status, result.narrative) =
            poll_until_terminal(server, viewer, id, args.timeout_seconds).await;
    }

    let body = serde_json::to_string_pretty(&result).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}

/// What [`enqueue`] writes: the synthesis row's recipe and ownership, and the
/// job payload's seed.
pub(crate) struct EnqueueSpec<'a> {
    pub query: &'a str,
    /// The decided ownership pair (`root_ownership` / `child_ownership`).
    pub owner: Ownership,
    /// A registered skill (`episcience_core::synthesis::skills`).
    pub skill_name: &'a str,
    pub parent_synthesis_id: Option<Uuid>,
    pub prereq_synthesis_ids: &'a [Uuid],
    pub traversal_config: Option<serde_json::Value>,
    /// Theme-anchored Stage 1 seed (wiki articles); `None` = text recall.
    pub seed_theme_id: Option<Uuid>,
}

/// The one enqueue path of the MCP synthesis tools (`synthesize`,
/// `wiki_generate_article`): a pending `syntheses` row and its queued
/// `synthesis_jobs` row on the caller's stamped transaction `tx`, so either
/// both land or neither (the worker never sees an orphaned synthesis row
/// without a job). Mirrors `routes/syntheses.rs::enqueue_synthesis`. The row
/// is authored by, and the job acts as, the authenticated caller. Returns the
/// new synthesis id; the caller commits.
pub(crate) async fn enqueue(
    tx: &mut PgConnection,
    server: &EpiscienceServer,
    auth: &AuthContext,
    spec: EnqueueSpec<'_>,
) -> Result<Uuid, McpError> {
    let id = Uuid::now_v7();
    let mut payload = serde_json::json!({
        "synthesis_id": id,
        "query": spec.query,
        "traversal_config": spec.traversal_config,
        "agent_id": auth.agent_id,
        "parent_synthesis_id": spec.parent_synthesis_id,
        "prereq_synthesis_ids": spec.prereq_synthesis_ids,
    });
    if let Some(theme) = spec.seed_theme_id {
        payload["seed_theme_id"] = serde_json::json!(theme);
    }

    SynthesisRepository::create_pending_tx(
        &mut *tx,
        id,
        spec.query,
        auth.agent_id,
        spec.parent_synthesis_id,
        spec.prereq_synthesis_ids,
        &server.llm_default_provider,
        &server.llm_default_model,
        spec.owner,
        spec.skill_name,
        None, // autonomy_level: MCP path has no autonomy concept yet
    )
    .await
    .map_err(|e| internal_error(format!("create synthesis: {e}")))?;

    // The job acts as the caller, supplied explicitly (the database refuses a
    // job without a principal).
    SynthesisJobsRepository::enqueue_tx(&mut *tx, id, auth.agent_id, &payload)
        .await
        .map_err(|e| internal_error(format!("enqueue job: {e}")))?;
    Ok(id)
}

/// Poll synthesis `id` until it reaches a terminal state or
/// `timeout_seconds` (clamped to 600 s) elapses. Returns the status the
/// caller reports (`"queued"` on a timeout: the synthesis is still in the
/// queue and `get_synthesis` follows it up) and the narrative of a
/// `complete` row.
pub(crate) async fn poll_until_terminal(
    server: &EpiscienceServer,
    viewer: &epigraph_db::Viewer,
    id: Uuid,
    timeout_seconds: u64,
) -> (String, Option<String>) {
    let timeout = std::time::Duration::from_secs(timeout_seconds.min(POLL_TIMEOUT_CAP_SECS));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // A fresh stamped read per poll: no session is held across the
        // sleep (up to the 600 s cap).
        let polled = match server.db.read_as(viewer).await {
            Ok(mut conn) => SynthesisRepository::get_readable(&mut *conn, id, viewer).await,
            Err(e) => Err(episcience_db::errors::DbError::Constraint(e.to_string())),
        };
        match polled {
            Ok(synth) => match synth.status {
                SynthesisStatus::Complete => return ("complete".to_string(), synth.narrative),
                SynthesisStatus::Failed => return ("failed".to_string(), None),
                // Stage 6 verifier rejected the narrative. Terminal until
                // Phase 7 ships refinement.
                SynthesisStatus::Rejected => return ("rejected".to_string(), None),
                // Soft-deleted while we were waiting — treat as terminal.
                SynthesisStatus::Deleted => return ("deleted".to_string(), None),
                SynthesisStatus::Pending
                | SynthesisStatus::Running
                | SynthesisStatus::Verifying => {
                    // Still in flight — fall through to sleep.
                }
            },
            Err(_) => {
                // Transient DB error (or row not yet visible on a replica).
                // Treat the same as still-pending and try again.
            }
        }
        if std::time::Instant::now() >= deadline {
            // Timeout — the synthesis is still in the queue; the caller can
            // poll via `get_synthesis`.
            return ("queued".to_string(), None);
        }
        tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }
}
