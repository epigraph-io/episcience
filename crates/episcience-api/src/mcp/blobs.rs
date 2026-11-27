//! `attach_blob` MCP tool — mirrors `POST /api/v1/eln/blobs` (multipart).
//!
//! Phase 8 ELN write parity. MCP cannot do multipart uploads, so the tool
//! has two modes:
//!
//! * inline: a base64-encoded `file_bytes_base64` payload, decoded
//!   server-side. The rest of the ingest path — BLAKE3 hash, content-addressed
//!   filesystem write, metadata row insert — is delegated to
//!   [`BlobRepository::store`], which stages the decoded bytes and records
//!   them through [`BlobRepository::store_staged`], the same placement the
//!   streamed HTTP route uses. The bytes pass through the model's context, so
//!   this suits small files only.
//! * upload (backlog 4a1abad4): `content_hash` (the file's SHA-256) and
//!   `size_bytes` instead of the bytes. Nothing is stored; the result is an
//!   upload URL and a short-lived token (`auth::upload_token`) for
//!   `PUT /api/v1/eln/blobs/upload`, which stores the blob when the bytes
//!   arrive and match.
//!
//! Auth: `uploader_id` is the authenticated caller (`AuthContext.agent_id`).
//! MCP clients cannot upload a blob under another agent's identity; an upload
//! token is bound to the caller and is refused under another agent's bearer.
//!
//! `register_external_blob` mirrors `POST /api/v1/eln/blobs/external`: data
//! that stays where it is, registered by location URI and declared hash, with
//! no bytes sent (migration 5043). It shares the REST route's validation.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::BlobRepository;

use crate::auth::upload_token::{
    normalize_sha256_hex, upload_url, UploadGrant, MAX_TOKEN_LIFETIME_SECS,
};
use crate::mcp::errors::{internal_error, invalid_params, invalid_request, McpError};
use crate::mcp::EpiscienceServer;
use crate::middleware::AuthContext;

const DEFAULT_FILENAME: &str = "unnamed";
const DEFAULT_MIME: &str = "application/octet-stream";
/// The upload route's token header
/// ([`crate::auth::upload_token::UPLOAD_TOKEN_HEADER`]) as a client spells it.
const UPLOAD_TOKEN_HEADER_NAME: &str = "X-Upload-Token";

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct AttachBlobArgs {
    /// Base64-encoded file content (standard alphabet, with padding), for the
    /// inline mode. MCP does not support multipart uploads; this is how the
    /// byte payload reaches the server inline. Omit it (and give
    /// `content_hash` and `size_bytes`) for the upload mode.
    #[schemars(
        description = "Inline mode: base64-encoded file content (standard alphabet). Omit for upload mode."
    )]
    #[serde(default)]
    pub file_bytes_base64: Option<String>,

    /// Upload mode: the SHA-256 of the file, 64 hex digits. Not the stored
    /// `content_hash` (BLAKE3): it is what the PUT's bytes are checked
    /// against.
    #[schemars(
        description = "Upload mode: SHA-256 of the file as 64 hex digits (e.g. sha256sum). The stored blob's content_hash is BLAKE3, not this."
    )]
    #[serde(default)]
    pub content_hash: Option<String>,

    /// Upload mode: the exact size of the file in bytes.
    #[schemars(description = "Upload mode: exact size of the file in bytes")]
    #[serde(default)]
    pub size_bytes: Option<u64>,

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

/// Upload mode's answer: where and how to send the bytes.
#[derive(Debug, Serialize)]
pub struct AttachBlobUpload {
    pub mode: &'static str,
    pub upload_url: String,
    pub method: &'static str,
    pub token_header: &'static str,
    pub token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// The id the blob will have once the PUT succeeds.
    pub blob_id: Uuid,
    /// The declared SHA-256 (lowercase hex), echoed.
    pub sha256: String,
    pub size_bytes: u64,
    pub sample_id: Option<Uuid>,
}

/// The display filename and MIME type, defaulted as both modes store them.
fn name_and_mime(args: &AttachBlobArgs) -> (String, String) {
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
    (filename, mime_type)
}

/// The blob's ownership, after checking that a sample (if any) is one the
/// caller may write; a sample it cannot write gets the same answer as a
/// missing one.
async fn sample_and_owner(
    conn: &mut sqlx::PgConnection,
    viewer: &epigraph_db::Viewer,
    sample_id: Option<Uuid>,
) -> Result<episcience_core::Ownership, McpError> {
    let sample = match sample_id {
        Some(sample_id) => Some(
            episcience_db::SampleRepository::get_writable(&mut *conn, sample_id, viewer)
                .await
                .map_err(|e| match e {
                    episcience_db::errors::DbError::NotFound { .. } => {
                        invalid_request(format!("sample {sample_id} not found"))
                    }
                    other => internal_error(format!("sample lookup: {other}")),
                })?,
        ),
        None => None,
    };
    crate::auth::tenancy::blob_ownership(conn, viewer, sample.as_ref())
        .await
        .map_err(crate::mcp::errors::from_api)
}

pub async fn handle(
    server: &EpiscienceServer,
    auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: AttachBlobArgs,
) -> Result<CallToolResult, McpError> {
    match (&args.file_bytes_base64, &args.content_hash, args.size_bytes) {
        (Some(_), None, None) => handle_inline(server, auth, viewer, args).await,
        (None, Some(_), Some(_)) => handle_upload(server, auth, viewer, args).await,
        _ => Err(invalid_params(
            "give either file_bytes_base64 (inline), or content_hash (the file's SHA-256) and \
             size_bytes (upload: returns an upload URL and token), not both",
        )),
    }
}

async fn handle_inline(
    server: &EpiscienceServer,
    auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: AttachBlobArgs,
) -> Result<CallToolResult, McpError> {
    let b64 = args.file_bytes_base64.as_deref().unwrap_or_default();
    if b64.trim().is_empty() {
        return Err(invalid_params("file_bytes_base64 cannot be empty"));
    }

    // Decode upfront so size enforcement applies to the post-decode payload
    // (which is what the HTTP multipart path also checks).
    let bytes = BASE64_STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| invalid_params(format!("invalid base64: {e}")))?;

    if bytes.len() > server.max_upload_bytes {
        return Err(invalid_params(format!(
            "file too large: {} bytes (max {})",
            bytes.len(),
            server.max_upload_bytes
        )));
    }

    let (filename, mime_type) = name_and_mime(&args);

    // Attaching to a sample requires write access to its owner group.
    let mut tx = server
        .db
        .write_as(viewer)
        .await
        .map_err(crate::mcp::from_refusal)?;
    let owner = sample_and_owner(&mut tx, viewer, args.sample_id).await?;

    let properties = if args.properties.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        args.properties
    };

    let blob = BlobRepository::store(
        &mut tx,
        &server.blob_dir,
        &filename,
        &mime_type,
        &bytes,
        auth.agent_id,
        args.sample_id,
        &args.labels,
        &properties,
        owner,
    )
    .await
    .map_err(|e| crate::mcp::errors::from_api(e.into()))?;

    tx.commit().await.map_err(internal_error)?;

    let body = AttachBlobResult {
        id: blob.id,
        content_hash: hex::encode(&blob.content_hash),
        size_bytes: blob.size_bytes,
        filename: blob.filename,
        mime_type: blob.mime_type,
        sample_id: blob.sample_id,
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

/// Upload mode: check what can be checked now (the declaration, the size
/// cap, write access to the sample), then sign a grant for the PUT. Nothing
/// is written.
async fn handle_upload(
    server: &EpiscienceServer,
    auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: AttachBlobArgs,
) -> Result<CallToolResult, McpError> {
    let key = server.upload_tokens.as_ref().ok_or_else(|| {
        invalid_request(
            "upload mode is not configured on this server; send the bytes inline as \
             file_bytes_base64",
        )
    })?;
    let sha256 = normalize_sha256_hex(args.content_hash.as_deref().unwrap_or_default())
        .map_err(invalid_params)?;
    let size_bytes = args.size_bytes.unwrap_or_default();
    if size_bytes == 0 {
        return Err(invalid_params("size_bytes must be positive"));
    }
    if usize::try_from(size_bytes).map_or(true, |n| n > server.max_upload_bytes) {
        return Err(invalid_params(format!(
            "file too large: {size_bytes} bytes (max {})",
            server.max_upload_bytes
        )));
    }

    let (filename, mime_type) = name_and_mime(&args);

    // Fail now, not after the upload, when the caller may not write the
    // sample (or no group). The PUT checks again; this transaction writes
    // nothing and is rolled back.
    let mut tx = server
        .db
        .write_as(viewer)
        .await
        .map_err(crate::mcp::from_refusal)?;
    sample_and_owner(&mut tx, viewer, args.sample_id).await?;
    tx.rollback()
        .await
        .map_err(|e| internal_error(format!("rollback: {e}")))?;

    let properties = if args.properties.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        args.properties
    };
    let now = chrono::Utc::now();
    let exp = now.timestamp() + MAX_TOKEN_LIFETIME_SECS;
    let grant = UploadGrant {
        blob_id: Uuid::now_v7(),
        agent_id: auth.agent_id,
        sample_id: args.sample_id,
        sha256: sha256.clone(),
        size_bytes,
        exp,
        filename,
        mime_type,
        labels: args.labels,
        properties,
    };
    let token = key.sign(&grant).map_err(invalid_params)?;

    let body = AttachBlobUpload {
        mode: "upload",
        upload_url: upload_url(server.upload_base_url.as_deref()),
        method: "PUT",
        token_header: UPLOAD_TOKEN_HEADER_NAME,
        token,
        expires_at: chrono::DateTime::from_timestamp(exp, 0).unwrap_or(now),
        blob_id: grant.blob_id,
        sha256,
        size_bytes,
        sample_id: grant.sample_id,
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

// ── register_external_blob: mirrors `POST /api/v1/eln/blobs/external` ──

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterExternalBlobArgs {
    /// Where the bytes live, `<scheme>://<location>` (e.g.
    /// `pvc://<volume>/<path>`, `s3://<bucket>/<key>`, `file://<host>/<path>`).
    #[schemars(description = "Where the bytes live: <scheme>://<location>")]
    pub location_uri: String,

    /// The hash of the bytes at the location, 64 hex digits, computed with
    /// `hash_algorithm`. Declared, not checked: a verifier with access to the
    /// store checks it later.
    #[schemars(description = "Hash of the bytes at the location (64 hex digits)")]
    pub content_hash: String,

    /// `blake3` or `sha256`.
    #[schemars(description = "The algorithm of content_hash: blake3 or sha256")]
    pub hash_algorithm: String,

    /// Size of the data in bytes (positive).
    #[schemars(description = "Size of the data in bytes")]
    pub size_bytes: i64,

    /// Display filename.
    #[schemars(description = "Display filename")]
    pub filename: String,

    /// MIME type (default `application/octet-stream`).
    #[schemars(description = "MIME type (default: 'application/octet-stream')")]
    #[serde(default)]
    pub mime_type: Option<String>,

    /// The data is a manifest (a list of files and their hashes).
    #[schemars(description = "The data is a manifest listing files and their hashes")]
    #[serde(default)]
    pub is_manifest: bool,

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
pub struct RegisterExternalBlobResult {
    pub id: Uuid,
    pub storage: episcience_core::BlobStorage,
    pub location_uri: Option<String>,
    pub content_hash: String,
    pub hash_algorithm: episcience_core::HashAlgorithm,
    pub size_bytes: i64,
    pub sample_id: Option<Uuid>,
}

/// Register data that stays where it is: one metadata row, no bytes. The
/// uploader is the authenticated caller; attaching to a sample needs write
/// access to it, exactly as for `attach_blob`.
pub async fn handle_register_external(
    server: &EpiscienceServer,
    auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: RegisterExternalBlobArgs,
) -> Result<CallToolResult, McpError> {
    let input = crate::routes::blobs::external_blob_input(
        &args.filename,
        args.mime_type.as_deref(),
        args.size_bytes,
        &args.content_hash,
        &args.hash_algorithm,
        &args.location_uri,
    )
    .map_err(invalid_params)?;

    let mut tx = server
        .db
        .write_as(viewer)
        .await
        .map_err(crate::mcp::from_refusal)?;
    let sample = match args.sample_id {
        Some(sample_id) => Some(
            episcience_db::SampleRepository::get_writable(&mut *tx, sample_id, viewer)
                .await
                .map_err(|e| match e {
                    episcience_db::errors::DbError::NotFound { .. } => {
                        invalid_request(format!("sample {sample_id} not found"))
                    }
                    other => internal_error(format!("sample lookup: {other}")),
                })?,
        ),
        None => None,
    };
    let owner = crate::auth::tenancy::blob_ownership(&mut tx, viewer, sample.as_ref())
        .await
        .map_err(crate::mcp::errors::from_api)?;
    let properties = if args.properties.is_null() {
        serde_json::Value::Object(Default::default())
    } else {
        args.properties
    };

    let blob = BlobRepository::register_external(
        &mut *tx,
        &episcience_db::NewExternalBlob {
            filename: &input.filename,
            mime_type: &input.mime_type,
            size_bytes: input.size_bytes,
            content_hash: &input.content_hash,
            hash_algorithm: input.hash_algorithm,
            location_uri: &input.location_uri,
            is_manifest: args.is_manifest,
            uploader_id: auth.agent_id,
            sample_id: args.sample_id,
            labels: &args.labels,
            properties: &properties,
        },
        owner,
    )
    .await
    .map_err(|e| crate::mcp::errors::from_api(e.into()))?;
    tx.commit().await.map_err(internal_error)?;

    let body = RegisterExternalBlobResult {
        id: blob.id,
        storage: blob.storage,
        location_uri: blob.location_uri,
        content_hash: hex::encode(&blob.content_hash),
        hash_algorithm: blob.hash_algorithm,
        size_bytes: blob.size_bytes,
        sample_id: blob.sample_id,
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}
