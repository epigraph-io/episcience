//! `propose_protocol` MCP tool — mirrors `POST /api/v1/eln/protocols`.
//!
//! Phase 8 ELN write parity. Delegates to [`ProtocolRepository::create`] (the
//! same path the REST route uses) and reuses [`ContentHasher`] over the
//! serialized steps so HTTP and MCP produce byte-identical `content_hash`
//! values for the same input.
//!
//! Auth: the `authored_by` field is pinned to `EpiscienceServer::auth_agent_id`
//! — MCP clients cannot author a protocol under another agent's identity.
//!
//! `ProtocolStep` lives in `episcience-core` and does not derive `JsonSchema`,
//! so we mirror it as [`ProtocolStepArg`] here with a `From` impl. Adding
//! `schemars` to `episcience-core` just for this would pull a heavy dep into
//! every downstream crate.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use epigraph_crypto::ContentHasher;
use episcience_core::ProtocolStep;
use episcience_db::ProtocolRepository;

use crate::mcp::errors::{internal_error, invalid_params, McpError};
use crate::mcp::EpiscienceServer;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProtocolStepArg {
    /// 1-based step ordinal.
    pub order: i32,
    /// Human-readable instruction text for the step.
    pub instruction: String,
    /// Optional planned duration of the step in minutes.
    #[serde(default)]
    pub duration_minutes: Option<f64>,
    /// Optional set-point temperature in degrees Celsius.
    #[serde(default)]
    pub temperature_c: Option<f64>,
    /// Optional free-text notes for the step.
    #[serde(default)]
    pub notes: Option<String>,
}

impl From<ProtocolStepArg> for ProtocolStep {
    fn from(a: ProtocolStepArg) -> Self {
        ProtocolStep {
            order: a.order,
            instruction: a.instruction,
            duration_minutes: a.duration_minutes,
            temperature_c: a.temperature_c,
            notes: a.notes,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProposeProtocolArgs {
    /// Human-readable protocol title (non-empty).
    #[schemars(description = "Protocol title (required, non-empty)")]
    pub title: String,

    /// Ordered list of steps (`order`, `instruction`, optional
    /// `duration_minutes` / `temperature_c` / `notes`).
    #[schemars(description = "Ordered list of protocol steps")]
    pub steps: Vec<ProtocolStepArg>,

    /// Equipment names required by the protocol (free-form strings).
    #[schemars(description = "Equipment names required by the protocol")]
    #[serde(default)]
    pub equipment: Vec<String>,

    /// Optional free-text safety notes.
    #[schemars(description = "Free-text safety notes")]
    #[serde(default)]
    pub safety_notes: Option<String>,

    /// Optional id of a prior protocol this one supersedes. The new
    /// protocol's `version` is `prior.version + 1`.
    #[schemars(description = "Optional id of prior protocol this supersedes")]
    #[serde(default)]
    pub supersedes: Option<Uuid>,

    /// Tag-style labels for the protocol.
    #[schemars(description = "Tag-style labels")]
    #[serde(default)]
    pub labels: Vec<String>,

    /// Free-form JSON properties.
    #[schemars(description = "Free-form JSON properties object")]
    #[serde(default)]
    pub properties: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ProposeProtocolResult {
    pub id: Uuid,
    pub version: i32,
    pub content_hash: String,
}

pub async fn handle(
    server: &EpiscienceServer,
    args: ProposeProtocolArgs,
) -> Result<CallToolResult, McpError> {
    if args.title.trim().is_empty() {
        return Err(invalid_params("title cannot be empty"));
    }

    let steps: Vec<ProtocolStep> = args.steps.into_iter().map(Into::into).collect();

    // Mirror the HTTP route: hash the JSON-serialized steps. Both paths
    // must produce identical content_hash values for the same payload.
    let hash_input = serde_json::to_string(&steps).unwrap_or_default();
    let hash = ContentHasher::hash(hash_input.as_bytes());

    let properties = if args.properties.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        args.properties
    };

    let protocol = ProtocolRepository::create(
        &server.pool,
        &args.title,
        server.auth_agent_id,
        &steps,
        &args.equipment,
        args.safety_notes.as_deref(),
        args.supersedes,
        &args.labels,
        &properties,
        &hash[..],
    )
    .await
    .map_err(|e| internal_error(format!("create protocol: {e}")))?;

    let body = ProposeProtocolResult {
        id: protocol.id,
        version: protocol.version,
        content_hash: hex::encode(&protocol.content_hash),
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
