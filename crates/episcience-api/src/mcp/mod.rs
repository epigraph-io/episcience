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
//! Identity: every tool acts as the AUTHENTICATED CALLER. On the HTTP
//! transport `http::bearer_auth_middleware` validates the bearer and attaches
//! an [`http::McpCaller`]; [`ServerHandler::call_tool`] (below) refuses a call
//! with no caller, no `agent_id` or no scope for the tool, then hands the
//! tool an [`AuthContext`] through rmcp's `Extensions`. There is no server-wide
//! service identity: a stdio session or an unauthenticated development HTTP
//! session can list tools but cannot call one.

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::request::Parts;
use episcience_db::tenancy::{EpiscienceDb, RequestRefusal};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_router, ServerHandler};

use epigraph_embeddings::EmbeddingService;

pub mod blobs;
pub mod countersigns;
pub mod errors;
pub mod http;
pub mod list_countersignatures;
pub mod observations;
pub mod protocols;
pub mod queries;
pub mod synthesize;

use crate::auth::scopes::mcp_required_scope;
use crate::mcp::blobs::AttachBlobArgs;
use crate::mcp::countersigns::CountersignArgs;
use crate::mcp::errors::{invalid_request, McpError};
use crate::mcp::http::McpCaller;
use crate::mcp::list_countersignatures::ListCountersignaturesArgs;
use crate::mcp::observations::AddObservationArgs;
use crate::mcp::protocols::ProposeProtocolArgs;
use crate::mcp::queries::{GetSynthesisArgs, ListSynthesesArgs, RecallSynthesisArgs};
use crate::mcp::synthesize::SynthesizeArgs;
use crate::middleware::{AuthContext, CallerViewer, INSUFFICIENT_SCOPE, PRINCIPAL_REQUIRED};

/// Conservative default cap for `attach_blob` payloads when the caller
/// doesn't pass an explicit `EPISCIENCE_MAX_UPLOAD_BYTES`. Mirrors the
/// `bin/server.rs` default so HTTP and MCP enforce the same ceiling.
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// MCP server for the EpiScience synthesis pipeline.
///
/// All shared mutable state is in the database, reached ONLY through
/// [`EpiscienceDb`] (stamped reads and writes on the `episcience_app`
/// application login; no raw pool); the handles are cheap to clone so the
/// `#[derive(Clone)]` impl is compatible with rmcp's per-request handler
/// cloning. The server holds no kernel service client (retired in E1f): it
/// writes no kernel edge.
#[derive(Clone)]
pub struct EpiscienceServer {
    pub(crate) tool_router: ToolRouter<Self>,
    pub(crate) db: EpiscienceDb,
    pub(crate) embedder: Arc<dyn EmbeddingService>,
    pub(crate) llm_default_provider: String,
    pub(crate) llm_default_model: String,
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
        db: EpiscienceDb,
        embedder: Arc<dyn EmbeddingService>,
        blob_dir: PathBuf,
        max_upload_bytes: usize,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            db,
            embedder,
            llm_default_provider: "anthropic".to_string(),
            llm_default_model: "claude-sonnet-4-6".to_string(),
            blob_dir,
            max_upload_bytes,
        }
    }

    /// The call's one authority decision (kernel parity), then the caller
    /// attached for the tool: resolve `auth`'s principal
    /// (`EpiscienceDb::resolve_principal`: an operated or unresolvable
    /// principal is refused here, before any tool runs) and put the
    /// [`AuthContext`] and the [`CallerViewer`] into `extensions`. Exactly
    /// what `call_tool` does after the bearer and scope checks; public so a
    /// test can drive a tool method the same way.
    ///
    /// # Errors
    /// The refusal ([`from_refusal`]).
    pub async fn attach_caller(
        &self,
        extensions: &mut Extensions,
        auth: AuthContext,
    ) -> Result<(), McpError> {
        let viewer = self
            .db
            .resolve_principal(Some(auth.agent_id))
            .await
            .map_err(from_refusal)?;
        extensions.insert(auth);
        extensions.insert(CallerViewer(Arc::new(viewer)));
        Ok(())
    }

    /// Number of tools this server exposes.
    ///
    /// Public because `bin/episcience-mcp-server.rs` is a separate crate from
    /// this lib and so cannot reach the `pub(crate)` `tool_router` field. It
    /// logs this at startup; deriving the count from the router is what keeps
    /// that line from going stale when a tool is added.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tool_router.list_all().len()
    }

    // ── Synthesize (Task 3.7) ────────────────────────────────────────────────

    #[tool(
        description = "Synthesize a Markdown narrative from EpiGraph claims matching the query. Enqueues a synthesis job owned by the authenticated caller and returns the new id; if wait_for_completion is true, polls until the job reaches a terminal state (timeout clamped to 600s)."
    )]
    pub async fn synthesize(
        &self,
        Parameters(args): Parameters<SynthesizeArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        synthesize::handle(self, &auth, &viewer, args).await
    }

    // ── Queries (Task 3.8) ───────────────────────────────────────────────────

    #[tool(
        description = "Semantic search over syntheses the authenticated caller can read (public, or owned by a group the caller belongs to). Returns synthesis_id + cosine similarity pairs."
    )]
    pub async fn recall_synthesis(
        &self,
        Parameters(args): Parameters<RecallSynthesisArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        queries::recall(self, &auth, &viewer, args).await
    }

    #[tool(
        description = "Get a single synthesis by id. Readable when public or owned by a group the authenticated caller belongs to; otherwise the error is indistinguishable from 'not found' (no existence leak)."
    )]
    pub async fn get_synthesis(
        &self,
        Parameters(args): Parameters<GetSynthesisArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        queries::get(self, &auth, &viewer, args).await
    }

    #[tool(
        description = "List syntheses the authenticated caller can read (public, or owned by a group the caller belongs to), most-recent first. Soft-deleted rows are excluded."
    )]
    pub async fn list_syntheses(
        &self,
        Parameters(args): Parameters<ListSynthesesArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        queries::list(self, &auth, &viewer, args).await
    }

    // ── ELN writes (Phase 8) ─────────────────────────────────────────────────

    #[tool(
        description = "Insert a new Protocol (versioned lab SOP). Title is required; steps is an ordered list of {order, instruction, optional duration_minutes/temperature_c/notes}. The authored_by agent is the authenticated caller; MCP clients cannot author as another agent. Returns id + content_hash (hex)."
    )]
    pub async fn propose_protocol(
        &self,
        Parameters(args): Parameters<ProposeProtocolArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        protocols::handle(self, &auth, &viewer, args).await
    }

    #[tool(
        description = "Attach a free-text observation claim to an existing sample. Inserts a claims row (truth_value=0.5) + a sample_claims link row in one transaction. The claim's agent_id is the authenticated caller. relationship defaults to 'observation'. Returns {claim_id, sample_id, relationship}."
    )]
    pub async fn add_observation(
        &self,
        Parameters(args): Parameters<AddObservationArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        observations::handle(self, &auth, &viewer, args).await
    }

    #[tool(
        description = "Countersign a claim the authenticated caller can read with an Ed25519 signature. signature_meaning ∈ {witnessed, approved, reviewed, certified, countersigned}. signature_hex is 128 hex chars (64-byte Ed25519 sig over claim_id|signer_id|signature_meaning|content, where signer_id is the agent whose key signed; default: the authenticated caller). public_key_hex (optional, 64 hex chars) must equal that agent's registered key. The row records the authenticated caller as the countersigner. Returns the countersignature row id."
    )]
    pub async fn countersign(
        &self,
        Parameters(args): Parameters<CountersignArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        countersigns::handle(self, &auth, &viewer, args).await
    }

    #[tool(
        description = "List every countersignature for a claim, oldest first. Used by the Phase 8 review-bot to check whether an approved/reviewed countersignature already exists for a candidate claim_id. Returns rows with hex-encoded content_hash, signature, and signer public_key."
    )]
    pub async fn list_countersignatures(
        &self,
        Parameters(args): Parameters<ListCountersignaturesArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        list_countersignatures::handle(self, &auth, &viewer, args).await
    }

    #[tool(
        description = "Store a content-addressed blob via base64-encoded bytes (MCP cannot do multipart). The blob's uploader_id is the authenticated caller. Optionally attach to a sample. Server enforces EPISCIENCE_MAX_UPLOAD_BYTES on the decoded payload. Returns id + content_hash (hex)."
    )]
    pub async fn attach_blob(
        &self,
        Parameters(args): Parameters<AttachBlobArgs>,
        extensions: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let (auth, viewer) = caller(&extensions)?;
        blobs::handle(self, &auth, &viewer, args).await
    }
}

/// The tool's [`AuthContext`] and resolved [`CallerViewer`], put in
/// `Extensions` by `call_tool`. A tool method reached any other way (no
/// caller attached) is refused.
pub(crate) fn caller(extensions: &Extensions) -> Result<(AuthContext, CallerViewer), McpError> {
    let auth = extensions.get::<AuthContext>().cloned().ok_or_else(|| {
        invalid_request("Unauthorized: no authenticated caller for this tool call")
    })?;
    let viewer = extensions
        .get::<CallerViewer>()
        .cloned()
        .ok_or_else(|| invalid_request("Unauthorized: no resolved caller for this tool call"))?;
    Ok((auth, viewer))
}

/// A request-path refusal as an MCP error: the caller's problem (no
/// principal, operated, unresolvable, may write no group) is
/// `invalid_request` with the refusal's words; a session that cannot be
/// opened is internal.
pub fn from_refusal(r: RequestRefusal) -> McpError {
    match r {
        RequestRefusal::PrincipalRequired => invalid_request(format!("Unauthorized: {r}")),
        RequestRefusal::Operated(_)
        | RequestRefusal::Unresolvable(_)
        | RequestRefusal::NoWritableGroup => invalid_request(format!("Forbidden: {r}")),
        RequestRefusal::Session(_) => crate::mcp::errors::internal_error(r),
    }
}

/// Decide whether `caller` may call `tool`: a validated bearer, a principal
/// (`agent_id`), and the tool's scope from `auth::scopes`. Returns the
/// [`AuthContext`] the tool runs as.
pub fn authorize_tool_call(
    caller: Option<&McpCaller>,
    tool: &str,
) -> Result<AuthContext, McpError> {
    let Some(caller) = caller else {
        return Err(invalid_request(
            "Unauthorized: tools/call requires a validated Bearer token that carries an \
             agent_id (stdio and unauthenticated sessions can only list tools)",
        ));
    };
    let Some(agent_id) = caller.agent_id else {
        return Err(invalid_request(format!(
            "Unauthorized: {PRINCIPAL_REQUIRED}: the token carries no agent_id"
        )));
    };
    let Some(required) = mcp_required_scope(tool) else {
        return Err(invalid_request(format!(
            "Forbidden: tool '{tool}' is not authorized (no scope mapping)"
        )));
    };
    let auth = AuthContext {
        agent_id,
        client_id: caller.client_id,
        owner_id: caller.owner_id,
        client_type: caller.client_type.clone(),
        scopes: caller.scopes.clone(),
    };
    if !auth.has_scope(required) {
        return Err(invalid_request(format!(
            "Forbidden: {INSUFFICIENT_SCOPE}: tool '{tool}' requires scope '{required}'"
        )));
    }
    Ok(auth)
}

// Manual `ServerHandler` impl (in lieu of `#[tool_handler]`) so `call_tool`
// can gate every invocation on the caller. `list_tools` and `get_tool` mirror
// the macro's expansion (rmcp-macros 0.15 `tool_handler.rs`), the same shape
// the kernel's `epigraph-mcp` server uses.
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
                 list_syntheses, propose_protocol, add_observation, countersign, \
                 list_countersignatures, attach_blob. Every tool acts as the authenticated \
                 caller."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // The HTTP bearer layer puts the validated caller into the request's
        // extensions; rmcp's `StreamableHttpService` forwards the request's
        // `http::request::Parts` into `context.extensions`. stdio has no
        // `Parts`, hence no caller, hence no tool call.
        let caller = context
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<McpCaller>())
            .cloned();
        let auth = authorize_tool_call(caller.as_ref(), &request.name)?;
        // Only `tools/call` resolves: discovery (`initialize`, `tools/list`)
        // stays principal-less.
        let mut context = context;
        self.attach_caller(&mut context.extensions, auth).await?;
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::scopes::{CLAIMS_READ, CLAIMS_WRITE, MCP_TOOL_SCOPES};
    use uuid::Uuid;

    /// Every tool the router serves has exactly one scope entry, and the
    /// table names no tool the router lacks. Kills: adding a tool without a
    /// scope (it would be refused at runtime, but silently).
    #[test]
    fn scope_table_covers_exactly_the_tool_router() {
        let mut routed: Vec<String> = EpiscienceServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        routed.sort();
        let mut table: Vec<String> = MCP_TOOL_SCOPES.iter().map(|(n, _)| n.to_string()).collect();
        table.sort();
        assert_eq!(routed, table);
    }

    fn caller(agent: Option<Uuid>, scopes: &[&str]) -> McpCaller {
        McpCaller {
            agent_id: agent,
            client_id: Uuid::new_v4(),
            owner_id: None,
            client_type: "human".to_string(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn authorize_requires_caller_principal_and_scope() {
        let agent = Uuid::now_v7();
        assert!(authorize_tool_call(None, "list_syntheses").is_err());
        let err =
            authorize_tool_call(Some(&caller(None, &[CLAIMS_READ])), "list_syntheses").unwrap_err();
        assert!(err.message.contains(PRINCIPAL_REQUIRED), "{}", err.message);
        let err = authorize_tool_call(Some(&caller(Some(agent), &[CLAIMS_READ])), "synthesize")
            .unwrap_err();
        assert!(err.message.contains(INSUFFICIENT_SCOPE), "{}", err.message);
        let err = authorize_tool_call(
            Some(&caller(Some(agent), &[CLAIMS_READ, CLAIMS_WRITE])),
            "no_such_tool",
        )
        .unwrap_err();
        assert!(err.message.contains("no scope mapping"), "{}", err.message);
        let ok = authorize_tool_call(Some(&caller(Some(agent), &[CLAIMS_WRITE])), "synthesize")
            .expect("write scope admits a write tool");
        assert_eq!(ok.agent_id, agent);
    }
}
