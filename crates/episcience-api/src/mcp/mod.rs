//! Episcience MCP server — exposes the synthesis pipeline as MCP tools.
//!
//! Phase 3 Tasks 3.6 / 3.7 / 3.8: a thin MCP wrapper around the same
//! repositories the REST routes call (`syntheses`, `synthesis_jobs`,
//! `synthesis_embeddings`). Tools mirror the REST surface:
//!
//!  - `synthesize` — `POST /syntheses` (with optional poll-to-completion).
//!  - `recall_synthesis` — `POST /syntheses/search`.
//!  - `get_synthesis` — `GET /syntheses/{id}`.
//!  - `list_syntheses` — `GET /syntheses`.
//!
//! The reference implementation is `epigraph-mcp` in the upstream EpiGraph
//! workspace — single `#[tool_router] impl` block, free-function delegate
//! handlers, `CallToolResult::success(vec![Content::text(json)])` returns,
//! `McpError = ErrorData` alias.
//!
//! Auth in v1 is service-level: `auth_agent_id` is set at construction (the
//! service's agent id), not pulled per-call. v2 should accept a per-call MCP
//! auth header and resolve a real agent id from JWT claims, matching the REST
//! middleware path.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use sqlx::PgPool;
use uuid::Uuid;

use epigraph_embeddings::EmbeddingService;
use episcience_db::EdgeWriter;

pub mod blobs;
pub mod countersigns;
pub mod errors;
pub mod observations;
pub mod protocols;
pub mod queries;
pub mod synthesize;

use crate::mcp::blobs::AttachBlobArgs;
use crate::mcp::countersigns::CountersignArgs;
use crate::mcp::errors::McpError;
use crate::mcp::observations::AddObservationArgs;
use crate::mcp::protocols::ProposeProtocolArgs;
use crate::mcp::queries::{GetSynthesisArgs, ListSynthesesArgs, RecallSynthesisArgs};
use crate::mcp::synthesize::SynthesizeArgs;

/// Conservative default cap for `attach_blob` payloads when the caller
/// doesn't pass an explicit `EPISCIENCE_MAX_UPLOAD_BYTES`. Mirrors the
/// `bin/server.rs` default so HTTP and MCP enforce the same ceiling.
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// MCP server for the EpiScience synthesis pipeline.
///
/// All shared mutable state is in [`PgPool`] (database) and [`EdgeWriter`]
/// (an HTTP client wrapper). Both are cheap to clone via `Arc` so the
/// `#[derive(Clone)]` impl is compatible with rmcp's per-request handler
/// cloning.
#[derive(Clone)]
pub struct EpiscienceServer {
    pub(crate) tool_router: ToolRouter<Self>,
    pub(crate) pool: PgPool,
    pub(crate) embedder: Arc<dyn EmbeddingService>,
    #[allow(dead_code)]
    pub(crate) edge_writer: Arc<dyn EdgeWriter>,
    pub(crate) llm_default_provider: String,
    pub(crate) llm_default_model: String,
    /// The auth agent_id for tool calls. v1: passed at construction (the
    /// service agent). v2 should pull this from a per-call MCP auth header.
    pub(crate) auth_agent_id: Uuid,
    /// Content-addressed blob storage root (e.g. `/var/lib/episcience/blobs`).
    /// Mirrors `ElnState::blob_dir`. Used by `attach_blob`.
    pub(crate) blob_dir: PathBuf,
    /// Maximum decoded blob size accepted by `attach_blob`. Mirrors
    /// `ElnState::max_upload_bytes`.
    pub(crate) max_upload_bytes: usize,
}

#[tool_router]
impl EpiscienceServer {
    #[must_use]
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn EmbeddingService>,
        edge_writer: Arc<dyn EdgeWriter>,
        auth_agent_id: Uuid,
        blob_dir: PathBuf,
        max_upload_bytes: usize,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            pool,
            embedder,
            edge_writer,
            llm_default_provider: "anthropic".to_string(),
            llm_default_model: "claude-sonnet-4-6".to_string(),
            auth_agent_id,
            blob_dir,
            max_upload_bytes,
        }
    }

    // ── Synthesize (Task 3.7) ────────────────────────────────────────────────

    #[tool(
        description = "Synthesize a Markdown narrative from EpiGraph claims matching the query. Enqueues a synthesis job and returns the new id; if wait_for_completion is true, polls until the job reaches a terminal state (timeout clamped to 600s)."
    )]
    pub async fn synthesize(
        &self,
        Parameters(args): Parameters<SynthesizeArgs>,
    ) -> Result<CallToolResult, McpError> {
        synthesize::handle(self, args).await
    }

    // ── Queries (Task 3.8) ───────────────────────────────────────────────────

    #[tool(
        description = "Semantic search over syntheses readable by the calling agent (owner / public / explicit share). Returns synthesis_id + cosine similarity pairs."
    )]
    pub async fn recall_synthesis(
        &self,
        Parameters(args): Parameters<RecallSynthesisArgs>,
    ) -> Result<CallToolResult, McpError> {
        queries::recall(self, args).await
    }

    #[tool(
        description = "Get a single synthesis by id. Read-predicate gated — strangers receive an error indistinguishable from 'not found' to avoid existence leaks."
    )]
    pub async fn get_synthesis(
        &self,
        Parameters(args): Parameters<GetSynthesisArgs>,
    ) -> Result<CallToolResult, McpError> {
        queries::get(self, args).await
    }

    #[tool(
        description = "List syntheses readable by the calling agent (owner / public / shared), most-recent first. Soft-deleted rows are excluded."
    )]
    pub async fn list_syntheses(
        &self,
        Parameters(args): Parameters<ListSynthesesArgs>,
    ) -> Result<CallToolResult, McpError> {
        queries::list(self, args).await
    }

    // ── ELN writes (Phase 8) ─────────────────────────────────────────────────

    #[tool(
        description = "Insert a new Protocol (versioned lab SOP). Title is required; steps is an ordered list of {order, instruction, optional duration_minutes/temperature_c/notes}. The authored_by agent is the MCP-authenticated identity (auth_agent_id); MCP clients cannot impersonate another agent. Returns id + content_hash (hex)."
    )]
    pub async fn propose_protocol(
        &self,
        Parameters(args): Parameters<ProposeProtocolArgs>,
    ) -> Result<CallToolResult, McpError> {
        protocols::handle(self, args).await
    }

    #[tool(
        description = "Attach a free-text observation claim to an existing sample. Inserts a claims row (truth_value=0.5) + a sample_claims link row in one transaction. The claim's agent_id is the MCP-authenticated identity. relationship defaults to 'observation'. Returns {claim_id, sample_id, relationship}."
    )]
    pub async fn add_observation(
        &self,
        Parameters(args): Parameters<AddObservationArgs>,
    ) -> Result<CallToolResult, McpError> {
        observations::handle(self, args).await
    }

    #[tool(
        description = "Countersign a claim with an Ed25519 signature. signature_meaning ∈ {witnessed, approved, reviewed, certified, countersigned}. signature_hex is 128 hex chars (64-byte Ed25519 sig over claim_id|signer_id|signature_meaning|content where signer_id = MCP auth_agent_id). public_key_hex is 64 hex chars. Returns the countersignature row id."
    )]
    pub async fn countersign(
        &self,
        Parameters(args): Parameters<CountersignArgs>,
    ) -> Result<CallToolResult, McpError> {
        countersigns::handle(self, args).await
    }

    #[tool(
        description = "Store a content-addressed blob via base64-encoded bytes (MCP cannot do multipart). The blob's uploader_id is the MCP-authenticated identity. Optionally attach to a sample. Server enforces EPISCIENCE_MAX_UPLOAD_BYTES on the decoded payload. Returns id + content_hash (hex)."
    )]
    pub async fn attach_blob(
        &self,
        Parameters(args): Parameters<AttachBlobArgs>,
    ) -> Result<CallToolResult, McpError> {
        blobs::handle(self, args).await
    }
}

#[tool_handler]
impl ServerHandler for EpiscienceServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: Implementation {
                name: "episcience-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                description: Some("EpiScience synthesis + ELN write MCP server".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "EpiScience MCP server — synthesize narratives from EpiGraph claims, recall \
                 stored syntheses, and drive ELN writes (protocols, observations, blobs, \
                 countersignatures). Tools: synthesize, recall_synthesis, get_synthesis, \
                 list_syntheses, propose_protocol, add_observation, countersign, attach_blob."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
