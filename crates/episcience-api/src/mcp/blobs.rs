//! `attach_blob` MCP tool — mirrors `POST /api/v1/eln/blobs` (multipart).
//!
//! Phase 8 ELN write parity. MCP cannot do multipart uploads, so the tool
//! accepts a base64-encoded `file_bytes_base64` payload and decodes it
//! server-side. The rest of the ingest path — BLAKE3 hash, content-addressed
//! filesystem write, metadata row insert — is delegated to
//! [`BlobRepository::store`], the same helper the HTTP route uses.
//!
//! Auth: `uploader_id` is pinned to `EpiscienceServer::auth_agent_id`. MCP
//! clients cannot upload a blob under another agent's identity.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::BlobRepository;

use crate::mcp::errors::{internal_error, invalid_params, McpError};
use crate::mcp::EpiscienceServer;

const DEFAULT_FILENAME: &str = "unnamed";
const DEFAULT_MIME: &str = "application/octet-stream";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachBlobArgs {
    /// Base64-encoded file content (standard alphabet, with padding).
    /// MCP does not support multipart uploads; this is how the byte
    /// payload reaches the server.
    #[schemars(description = "Base64-encoded file content (standard alphabet)")]
    pub file_bytes_base64: String,

    /// Display filename (e.g. `gel_image.png`). Stored verbatim — not
    /// trusted for path resolution. Defaults to `"unnamed"` if absent or
    /// empty after trim.
    #[schemars(description = "Display filename (default: 'unnamed')")]
    #[serde(default)]
    pub filename: Option<String>,

    /// MIME type (e.g. `image/png`). Defaults to
    /// `"application/octet-stream"`.
    #[schemars(description = "MIME type (default: 'application/octet-stream')")]
    #[serde(default)]
    pub mime_type: Option<String>,

    /// Optional sample id to attach the blob to.
    #[schemars(description = "Optional sample id to attach the blob to")]
    #[serde(default)]
    pub sample_id: Option<Uuid>,

    /// Tag-style labels for the blob.
    #[schemars(description = "Tag-style labels")]
    #[serde(default)]
    pub labels: Vec<String>,

    /// Free-form JSON properties.
    #[schemars(description = "Free-form JSON properties object")]
    #[serde(default)]
    pub properties: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct AttachBlobResult {
    pub id: Uuid,
    pub content_hash: String,
    pub size_bytes: i64,
    pub filename: String,
    pub mime_type: String,
    pub sample_id: Option<Uuid>,
}

pub async fn handle(
    server: &EpiscienceServer,
    args: AttachBlobArgs,
) -> Result<CallToolResult, McpError> {
    if args.file_bytes_base64.trim().is_empty() {
        return Err(invalid_params("file_bytes_base64 cannot be empty"));
    }

    // Decode upfront so size enforcement applies to the post-decode payload
    // (which is what the HTTP multipart path also checks).
    let bytes = BASE64_STANDARD
        .decode(args.file_bytes_base64.as_bytes())
        .map_err(|e| invalid_params(format!("invalid base64: {e}")))?;

    if bytes.len() > server.max_upload_bytes {
        return Err(invalid_params(format!(
            "file too large: {} bytes (max {})",
            bytes.len(),
            server.max_upload_bytes
        )));
    }

    let filename = args
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_FILENAME)
        .to_string();
    let mime_type = args
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_MIME)
        .to_string();

    let properties = if args.properties.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        args.properties
    };

    let blob = BlobRepository::store(
        &server.pool,
        &server.blob_dir,
        &filename,
        &mime_type,
        &bytes,
        server.auth_agent_id,
        args.sample_id,
        &args.labels,
        &properties,
    )
    .await
    .map_err(|e| internal_error(format!("store blob: {e}")))?;

    let body = AttachBlobResult {
        id: blob.id,
        content_hash: hex::encode(&blob.content_hash),
        size_bytes: blob.size_bytes,
        filename: blob.filename,
        mime_type: blob.mime_type,
        sample_id: blob.sample_id,
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
