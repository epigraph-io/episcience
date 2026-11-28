use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use axum_extra::extract::multipart::Field;
use axum_extra::extract::Multipart;
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::tenancy::bound_identity;
use crate::auth::upload_token::{UploadTokenKey, UPLOAD_PATH, UPLOAD_TOKEN_HEADER};
use crate::errors::ApiError;
use crate::middleware::{AuthContext, CallerViewer};
use crate::state::ElnState;
use episcience_core::blob::{parse_hash_hex, validate_location_uri};
use episcience_core::{BlobRef, BlobStorage, HashAlgorithm};
use episcience_db::{
    BlobRepository, BlobStageError, NewExternalBlob, StagedBlob, VerifyObservation,
};

/// Room for the multipart framing and the small text fields around the file,
/// on top of `max_upload_bytes` (the file itself is capped as it streams).
const MULTIPART_OVERHEAD_BYTES: usize = 1024 * 1024;

/// The upload route's body limit: the file cap plus the multipart overhead,
/// replacing axum's 2 MB default (which `Multipart` otherwise applies).
pub fn upload_body_limit(max_upload_bytes: usize) -> DefaultBodyLimit {
    DefaultBodyLimit::max(max_upload_bytes.saturating_add(MULTIPART_OVERHEAD_BYTES))
}

fn stage_error(e: BlobStageError) -> ApiError {
    match e {
        BlobStageError::TooLarge { .. } => ApiError::Validation(e.to_string()),
        BlobStageError::HashMismatch { .. } => ApiError::Conflict(e.to_string()),
        BlobStageError::Io(m) => ApiError::Internal(m),
    }
}

/// Stream one multipart file field into a staging file in `blob_dir`,
/// hashing as it goes and refusing the first chunk past `max_bytes`, so the
/// upload is never held in memory whole and an oversized one is not read to
/// its end.
pub async fn stage_file_field(
    field: &mut Field,
    blob_dir: &std::path::Path,
    max_bytes: usize,
) -> Result<StagedBlob, ApiError> {
    let mut stager = BlobRepository::stager(blob_dir, max_bytes)
        .await
        .map_err(stage_error)?;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| ApiError::Validation(format!("failed to read file: {e}")))?
    {
        stager.push(&chunk).await.map_err(stage_error)?;
    }
    stager.finish().await.map_err(stage_error)
}

/// Cap on each non-file multipart field (`content_hash`, `uploader_id`,
/// `sample_id`, `labels`, `properties`). The route's body limit is sized for
/// the file, so without this a body with no file could hold ~`max_upload_bytes`
/// of `labels` or `properties` in memory, and then expand it further when
/// splitting or parsing it.
pub const MAX_TEXT_FIELD_BYTES: usize = 64 * 1024;

/// Read one non-file multipart field as UTF-8, refusing the first chunk that
/// takes it past `max_bytes` (422), so an oversized field is never buffered
/// whole.
pub async fn read_text_field(
    field: &mut Field,
    name: &str,
    max_bytes: usize,
) -> Result<String, ApiError> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| ApiError::Validation(format!("failed to read {name}: {e}")))?
    {
        if chunk.len() > max_bytes - buf.len() {
            return Err(ApiError::Validation(format!(
                "{name} too large: more than {max_bytes} bytes"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|e| ApiError::Validation(format!("{name} is not UTF-8: {e}")))
}

/// A declared `content_hash` field: 64 hex characters (BLAKE3).
fn parse_declared_hash(text: &str) -> Result<[u8; 32], ApiError> {
    let bytes = hex::decode(text.trim())
        .map_err(|e| ApiError::Validation(format!("invalid content_hash: {e}")))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        ApiError::Validation(format!(
            "invalid content_hash: {} bytes, expected 32 (BLAKE3)",
            bytes.len()
        ))
    })
}

// ── Upload (multipart) ────────────────────────────────────────────────
//
// Fields: `file` (required, capped at `max_upload_bytes` as it streams),
// `content_hash` (optional: the BLAKE3 the uploader expects, 64 hex; 409 when
// the received bytes differ), `uploader_id`, `sample_id`, `labels`,
// `properties` (each capped at `MAX_TEXT_FIELD_BYTES`).

async fn upload_blob(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Extension(viewer): Extension<CallerViewer>,
    mut multipart: Multipart,
) -> Result<Json<BlobRef>, ApiError> {
    let mut staged: Option<StagedBlob> = None;
    let mut declared_hash: Option<[u8; 32]> = None;
    let mut filename: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let mut uploader_id: Option<Uuid> = None;
    let mut sample_id: Option<Uuid> = None;
    let mut labels: Vec<String> = Vec::new();
    let mut properties: serde_json::Value = serde_json::Value::Object(Default::default());

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::Validation(format!("multipart error: {e}")))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                mime_type = field.content_type().map(|s| s.to_string());
                staged = Some(
                    stage_file_field(&mut field, &state.blob_dir, state.max_upload_bytes).await?,
                );
            }
            "content_hash" => {
                let text =
                    read_text_field(&mut field, "content_hash", MAX_TEXT_FIELD_BYTES).await?;
                declared_hash = Some(parse_declared_hash(&text)?);
            }
            "uploader_id" => {
                let text = read_text_field(&mut field, "uploader_id", MAX_TEXT_FIELD_BYTES).await?;
                uploader_id = Some(
                    text.trim()
                        .parse::<Uuid>()
                        .map_err(|e| ApiError::Validation(format!("invalid uploader_id: {e}")))?,
                );
            }
            "sample_id" => {
                let text = read_text_field(&mut field, "sample_id", MAX_TEXT_FIELD_BYTES).await?;
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    sample_id =
                        Some(trimmed.parse::<Uuid>().map_err(|e| {
                            ApiError::Validation(format!("invalid sample_id: {e}"))
                        })?);
                }
            }
            "labels" => {
                let text = read_text_field(&mut field, "labels", MAX_TEXT_FIELD_BYTES).await?;
                labels = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "properties" => {
                let text = read_text_field(&mut field, "properties", MAX_TEXT_FIELD_BYTES).await?;
                properties = serde_json::from_str(&text)
                    .map_err(|e| ApiError::Validation(format!("invalid properties JSON: {e}")))?;
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    // On any error from here on, `staged` drops and removes its staging file.
    let staged = staged.ok_or_else(|| ApiError::Validation("file field is required".into()))?;
    // Field order is the client's: the declared hash is checked once both
    // the file and the declaration have arrived.
    if let Some(declared) = declared_hash {
        staged.check_declared(&declared).map_err(stage_error)?;
    }

    let fname = filename.unwrap_or_else(|| "unnamed".to_string());
    let mtype = mime_type.unwrap_or_else(|| "application/octet-stream".to_string());
    // Optional: absent means the authenticated caller; different is 403.
    let uid = bound_identity("uploader_id", uploader_id, auth.agent_id)?;

    // Attaching to a sample requires write access to its owner group (404
    // otherwise, the same answer as for a missing sample).
    let mut tx = state.db.write_as(&viewer).await?;
    let sample = match sample_id {
        Some(sid) => {
            Some(episcience_db::SampleRepository::get_writable(&mut *tx, sid, &viewer).await?)
        }
        None => None,
    };
    let owner = crate::auth::tenancy::blob_ownership(&mut tx, &viewer, sample.as_ref()).await?;

    let blob = BlobRepository::store_staged(
        &mut tx,
        &state.blob_dir,
        staged,
        &fname,
        &mtype,
        uid,
        sample_id,
        &labels,
        &properties,
        owner,
    )
    .await?;
    tx.commit().await?;

    Ok(Json(blob))
}

// ── Out-of-band upload (PUT, raw body, upload token) ──────────────────
//
// The second step of MCP `attach_blob`'s upload mode (backlog efc5549c): the
// body is the file's bytes, the `X-Upload-Token` header the token MCP issued
// (`auth::upload_token`), and the bearer the same agent's. The token is
// checked before a byte of the body is read; the body is streamed to a
// staging file under the token's size (and `max_upload_bytes`), hashed with
// SHA-256 (the declared hash) and BLAKE3 (the stored `content_hash`) as it
// arrives. Write access to the sample is checked again, in the transaction
// that records the blob, after the body has arrived (no connection is held
// during the upload). The blob is stored under the token's blob id, so a
// replay of the same token and bytes returns the same blob.

/// 403 text for every token refusal: which check failed is not disclosed.
const TOKEN_REFUSED: &str = "upload token invalid or expired";

async fn upload_with_token(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Extension(viewer): Extension<CallerViewer>,
    key: Option<Extension<UploadTokenKey>>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<BlobRef>, ApiError> {
    let Some(Extension(key)) = key else {
        return Err(ApiError::ServiceUnavailable(
            "out-of-band upload is not configured on this server".into(),
        ));
    };
    let token = headers
        .get(UPLOAD_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Forbidden(format!("{UPLOAD_TOKEN_HEADER} is required")))?;
    let grant = key
        .verify(token, chrono::Utc::now().timestamp())
        .map_err(|_| ApiError::Forbidden(TOKEN_REFUSED.into()))?;
    if grant.agent_id != auth.agent_id {
        return Err(ApiError::Forbidden(TOKEN_REFUSED.into()));
    }
    let size = usize::try_from(grant.size_bytes)
        .ok()
        .filter(|n| *n <= state.max_upload_bytes)
        .ok_or_else(|| {
            ApiError::Validation(format!(
                "file too large: {} bytes (max {})",
                grant.size_bytes, state.max_upload_bytes
            ))
        })?;
    if let Some(len) = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        if len != grant.size_bytes {
            return Err(ApiError::Validation(format!(
                "the body is {len} bytes; the token declares {}",
                grant.size_bytes
            )));
        }
    }

    // On any error from here on, the stager (or `staged`) drops and removes
    // its staging file.
    let mut stager = BlobRepository::stager(&state.blob_dir, size)
        .await
        .map_err(stage_error)?;
    let mut sha256 = Sha256::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::Validation(format!("failed to read body: {e}")))?;
        stager.push(&chunk).await.map_err(stage_error)?;
        sha256.update(&chunk);
    }
    let staged = stager.finish().await.map_err(stage_error)?;
    if u64::try_from(staged.size_bytes()).ok() != Some(grant.size_bytes) {
        return Err(ApiError::Validation(format!(
            "the body is {} bytes; the token declares {}",
            staged.size_bytes(),
            grant.size_bytes
        )));
    }
    let received = hex::encode(sha256.finalize());
    if received != grant.sha256 {
        return Err(ApiError::Conflict(format!(
            "content hash mismatch: declared sha256 {}, received {received}",
            grant.sha256
        )));
    }

    let mut tx = state.db.write_as(&viewer).await?;
    let sample = match grant.sample_id {
        Some(sid) => {
            Some(episcience_db::SampleRepository::get_writable(&mut *tx, sid, &viewer).await?)
        }
        None => None,
    };
    let owner = crate::auth::tenancy::blob_ownership(&mut tx, &viewer, sample.as_ref()).await?;
    let blob = BlobRepository::store_staged_with_id(
        &mut tx,
        &state.blob_dir,
        staged,
        grant.blob_id,
        &viewer,
        &grant.filename,
        &grant.mime_type,
        auth.agent_id,
        grant.sample_id,
        &grant.labels,
        &grant.properties,
        owner,
    )
    .await
    .map_err(|e| match e {
        episcience_db::errors::DbError::Constraint(m) => ApiError::Conflict(m),
        other => other.into(),
    })?;
    tx.commit().await?;
    Ok(Json(blob))
}

// ── Get metadata ──────────────────────────────────────────────────────

async fn get_blob_metadata(
    State(state): State<ElnState>,
    Extension(viewer): Extension<CallerViewer>,
    Path(id): Path<Uuid>,
) -> Result<Json<BlobRef>, ApiError> {
    let mut conn = state.db.read_as(&viewer).await?;
    let blob = BlobRepository::get_readable(&mut *conn, id, &viewer).await?;
    drop(conn);
    Ok(Json(blob))
}

// ── Download content ──────────────────────────────────────────────────

async fn download_blob(
    State(state): State<ElnState>,
    Extension(viewer): Extension<CallerViewer>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let mut conn = state.db.read_as(&viewer).await?;
    let blob = BlobRepository::get_readable(&mut *conn, id, &viewer).await?;
    drop(conn);
    // An external blob's hash was declared by whoever registered it: reading
    // the blob directory by it could return another tenant's bytes of that
    // hash. Its download is its location.
    if blob.storage != BlobStorage::Local {
        return Ok(external_location_response(&blob));
    }
    let content = BlobRepository::read_content(&state.blob_dir, &blob.content_hash).await?;
    let hash_hex = hex::encode(&blob.content_hash);

    let response = Response::builder()
        .header(header::CONTENT_TYPE, &blob.mime_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", blob.filename),
        )
        .header("X-Content-Hash", &hash_hex)
        .body(Body::from(content))
        .map_err(|e| ApiError::Internal(format!("failed to build response: {e}")))?;

    Ok(response)
}

// ── Verify integrity ──────────────────────────────────────────────────

async fn verify_blob(
    State(state): State<ElnState>,
    Extension(viewer): Extension<CallerViewer>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut conn = state.db.read_as(&viewer).await?;
    let blob = BlobRepository::get_readable(&mut *conn, id, &viewer).await?;
    drop(conn);
    // An external blob is verified in place by whoever posts an observation
    // (POST .../verification); here only that recorded attestation is shown,
    // and the blob directory is never read by its declared hash.
    // `integrity_ok` keeps its one meaning, "the server re-hashed the bytes
    // and they match": the server never reads an external blob's bytes, so
    // it is always null here. The attestation is `verify_status`, with who
    // made it; `self_attested` says the registrant vouched for its own blob.
    if blob.storage != BlobStorage::Local {
        return Ok(Json(serde_json::json!({
            "id": blob.id,
            "storage": blob.storage,
            "content_hash": hex::encode(&blob.content_hash),
            "hash_algorithm": blob.hash_algorithm,
            "integrity_ok": serde_json::Value::Null,
            "verify_status": blob.verify_status,
            "last_verified_at": blob.last_verified_at,
            "last_verified_by": blob.last_verified_by,
            "uploader_id": blob.uploader_id,
            "self_attested": blob.last_verified_by.map(|v| v == blob.uploader_id),
        })));
    }
    let ok = BlobRepository::verify_integrity(&state.blob_dir, &blob.content_hash).await?;
    let hash_hex = hex::encode(&blob.content_hash);
    Ok(Json(serde_json::json!({
        "id": blob.id,
        "content_hash": hash_hex,
        "integrity_ok": ok,
    })))
}

// ── List by sample ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BySampleQuery {
    sample_id: Uuid,
}

async fn list_blobs_by_sample(
    State(state): State<ElnState>,
    Extension(viewer): Extension<CallerViewer>,
    Query(q): Query<BySampleQuery>,
) -> Result<Json<Vec<BlobRef>>, ApiError> {
    let mut conn = state.db.read_as(&viewer).await?;
    let blobs = BlobRepository::list_by_sample(&mut *conn, q.sample_id, &viewer).await?;
    Ok(Json(blobs))
}

// ── External blobs (registered by URI and hash, no bytes) ─────────────

/// The longest MIME type `blobs.mime_type` holds (`varchar(255)`).
const MAX_MIME_TYPE_CHARS: usize = 255;

/// A validated external-blob registration, shared by REST and MCP.
pub(crate) struct ExternalBlobInput {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub content_hash: Vec<u8>,
    pub hash_algorithm: HashAlgorithm,
    pub location_uri: String,
}

/// Validate the caller's description of an external blob (422 on any
/// defect; the 5043 CHECKs are only the backstop).
pub(crate) fn external_blob_input(
    filename: &str,
    mime_type: Option<&str>,
    size_bytes: i64,
    content_hash_hex: &str,
    hash_algorithm: &str,
    location_uri: &str,
) -> Result<ExternalBlobInput, String> {
    let filename = filename.trim();
    if filename.is_empty() {
        return Err("filename must not be empty".into());
    }
    let mime_type = mime_type
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream");
    if mime_type.chars().count() > MAX_MIME_TYPE_CHARS {
        return Err(format!(
            "mime_type is longer than {MAX_MIME_TYPE_CHARS} characters"
        ));
    }
    if size_bytes <= 0 {
        return Err(format!("size_bytes must be positive, got {size_bytes}"));
    }
    let content_hash = parse_hash_hex("content_hash", content_hash_hex)?;
    let hash_algorithm = hash_algorithm.trim().parse::<HashAlgorithm>()?;
    let location_uri = location_uri.trim();
    validate_location_uri(location_uri)?;
    Ok(ExternalBlobInput {
        filename: filename.to_string(),
        mime_type: mime_type.to_string(),
        size_bytes,
        content_hash,
        hash_algorithm,
        location_uri: location_uri.to_string(),
    })
}

#[derive(Deserialize)]
struct RegisterExternalBody {
    filename: String,
    #[serde(default)]
    mime_type: Option<String>,
    size_bytes: i64,
    /// 64 hex digits, computed with `hash_algorithm`.
    content_hash: String,
    hash_algorithm: String,
    location_uri: String,
    #[serde(default)]
    is_manifest: bool,
    #[serde(default)]
    uploader_id: Option<Uuid>,
    #[serde(default)]
    sample_id: Option<Uuid>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    properties: Option<serde_json::Value>,
}

/// Register data that stays where it is: a metadata row naming its location
/// and declared hash. No bytes are sent, and none are written.
async fn register_external_blob(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Extension(viewer): Extension<CallerViewer>,
    Json(body): Json<RegisterExternalBody>,
) -> Result<Json<BlobRef>, ApiError> {
    let input = external_blob_input(
        &body.filename,
        body.mime_type.as_deref(),
        body.size_bytes,
        &body.content_hash,
        &body.hash_algorithm,
        &body.location_uri,
    )
    .map_err(ApiError::Validation)?;
    let uid = bound_identity("uploader_id", body.uploader_id, auth.agent_id)?;
    let properties = match body.properties {
        None | Some(serde_json::Value::Null) => serde_json::Value::Object(Default::default()),
        Some(p) => p,
    };

    // Attaching to a sample requires write access to its owner group (404
    // otherwise), exactly as for an upload.
    let mut tx = state.db.write_as(&viewer).await?;
    let sample = match body.sample_id {
        Some(sid) => {
            Some(episcience_db::SampleRepository::get_writable(&mut *tx, sid, &viewer).await?)
        }
        None => None,
    };
    let owner = crate::auth::tenancy::blob_ownership(&mut tx, &viewer, sample.as_ref()).await?;
    let blob = BlobRepository::register_external(
        &mut *tx,
        &NewExternalBlob {
            filename: &input.filename,
            mime_type: &input.mime_type,
            size_bytes: input.size_bytes,
            content_hash: &input.content_hash,
            hash_algorithm: input.hash_algorithm,
            location_uri: &input.location_uri,
            is_manifest: body.is_manifest,
            uploader_id: uid,
            sample_id: body.sample_id,
            labels: &body.labels,
            properties: &properties,
        },
        owner,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(blob))
}

#[derive(Deserialize)]
struct VerificationBody {
    /// The hash the verifier computed over the bytes at the location (64
    /// hex digits). Exactly one of this and `unreachable`.
    #[serde(default)]
    observed_hash: Option<String>,
    /// The algorithm the verifier used; must be the blob's. Required with
    /// `observed_hash`.
    #[serde(default)]
    hash_algorithm: Option<String>,
    /// The verifier could not read the bytes.
    #[serde(default)]
    unreachable: bool,
}

/// Record a verification of an external blob by a worker with access to its
/// store. The worker reports what it observed; the server decides `ok` or
/// `mismatch` against the row's own hash. Needs write access to the blob's
/// owner group (404 otherwise, as for a missing blob).
async fn record_blob_verification(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Extension(viewer): Extension<CallerViewer>,
    Path(id): Path<Uuid>,
    Json(body): Json<VerificationBody>,
) -> Result<Json<BlobRef>, ApiError> {
    let observation = match (body.observed_hash.as_deref(), body.unreachable) {
        (Some(_), true) | (None, false) => {
            return Err(ApiError::Validation(
                "give exactly one of observed_hash (with hash_algorithm) and unreachable: true"
                    .into(),
            ))
        }
        (Some(h), false) => VerifyObservation::Hashed(
            parse_hash_hex("observed_hash", h).map_err(ApiError::Validation)?,
        ),
        (None, true) => VerifyObservation::Unreachable,
    };
    let algorithm = match body.hash_algorithm.as_deref() {
        Some(a) => Some(
            a.trim()
                .parse::<HashAlgorithm>()
                .map_err(ApiError::Validation)?,
        ),
        None if matches!(observation, VerifyObservation::Hashed(_)) => {
            return Err(ApiError::Validation(
                "observed_hash needs the hash_algorithm it was computed with".into(),
            ))
        }
        None => None,
    };

    let mut tx = state.db.write_as(&viewer).await?;
    let blob = BlobRepository::get_writable(&mut *tx, id, &viewer).await?;
    if blob.storage != BlobStorage::External {
        return Err(ApiError::Validation(
            "a local blob is verified by the server from its own bytes (GET .../verify)".into(),
        ));
    }
    if let Some(a) = algorithm {
        if a != blob.hash_algorithm {
            return Err(ApiError::Validation(format!(
                "this blob's content_hash is {}; the observation was computed with {}",
                blob.hash_algorithm.as_str(),
                a.as_str()
            )));
        }
    }
    let updated =
        BlobRepository::record_verification(&mut *tx, id, &viewer, &observation, auth.agent_id)
            .await?;
    tx.commit().await?;
    Ok(Json(updated))
}

/// An external blob's download: where its bytes are, never bytes. JSON
/// with no `Content-Disposition`, so a client cannot save it as the data file.
fn external_location_response(blob: &BlobRef) -> Response {
    let mut resp = Json(serde_json::json!({
        "id": blob.id,
        "storage": blob.storage,
        "location_uri": blob.location_uri,
        "content_hash": hex::encode(&blob.content_hash),
        "hash_algorithm": blob.hash_algorithm,
        "size_bytes": blob.size_bytes,
        "is_manifest": blob.is_manifest,
        "verify_status": blob.verify_status,
        "last_verified_at": blob.last_verified_at,
    }))
    .into_response();
    resp.headers_mut().insert(
        "X-Blob-Storage",
        axum::http::HeaderValue::from_static("external"),
    );
    resp
}

// ── Router ────────────────────────────────────────────────────────────

pub fn router(state: ElnState) -> Router {
    // axum's default 2 MB body limit would refuse any upload past it whatever
    // `max_upload_bytes` says; the file is capped as it streams instead.
    let upload_limit = upload_body_limit(state.max_upload_bytes);
    let nested = Router::new()
        .route("/", get(get_blob_metadata))
        .route("/download", get(download_blob))
        .route("/verify", get(verify_blob))
        .route("/verification", post(record_blob_verification));

    Router::new()
        .route("/api/v1/eln/blobs", post(upload_blob).layer(upload_limit))
        .route("/api/v1/eln/blobs/external", post(register_external_blob))
        .route(UPLOAD_PATH, put(upload_with_token))
        .route("/api/v1/eln/blobs/by-sample", get(list_blobs_by_sample))
        .nest("/api/v1/eln/blobs/:id", nested)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::FromRequest;
    use axum::http::{header::CONTENT_TYPE, Request, StatusCode};
    use futures_util::StreamExt;
    use std::time::Duration;

    const BOUNDARY: &str = "b73boundary";

    fn file_part_head() -> String {
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
    }

    fn request(body: Body) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .unwrap()
    }

    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    // An upload past the cap is refused while it is still arriving: the body
    // here sends 3x the cap and then never ends, so a handler that buffers
    // the field to its end (the old `field.bytes()`) never answers.
    #[tokio::test]
    async fn oversized_upload_is_refused_before_the_body_ends() {
        let dir = tempfile::TempDir::new().unwrap();
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(file_part_head())),
            Ok(Bytes::from(vec![7u8; 1000])),
            Ok(Bytes::from(vec![7u8; 1000])),
            Ok(Bytes::from(vec![7u8; 1000])),
        ];
        let stalls = futures_util::stream::iter(chunks).chain(futures_util::stream::pending());
        let mut mp = Multipart::from_request(request(Body::from_stream(stalls)), &())
            .await
            .unwrap();
        let mut field = mp.next_field().await.unwrap().expect("file field");
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            stage_file_field(&mut field, dir.path(), 1000),
        )
        .await
        .expect("the cap must be enforced before the upload ends");
        match outcome {
            Err(ApiError::Validation(m)) => assert!(m.contains("file too large"), "{m}"),
            Err(_) => panic!("expected the too-large refusal, got another error"),
            Ok(_) => panic!("an upload past the cap was staged"),
        }
        assert!(
            dir_entries(dir.path()).is_empty(),
            "{:?}",
            dir_entries(dir.path())
        );
    }

    // A non-file field past its cap is refused while it is still arriving:
    // 3x the cap of `labels` and then a body that never ends, so a reader
    // that buffers the field to its end (`field.text()`) never answers.
    #[tokio::test]
    async fn oversized_text_field_is_refused_before_the_body_ends() {
        let head =
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"labels\"\r\n\r\n");
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(head)),
            Ok(Bytes::from("a,".repeat(500))),
            Ok(Bytes::from("a,".repeat(500))),
            Ok(Bytes::from("a,".repeat(500))),
        ];
        let stalls = futures_util::stream::iter(chunks).chain(futures_util::stream::pending());
        let mut mp = Multipart::from_request(request(Body::from_stream(stalls)), &())
            .await
            .unwrap();
        let mut field = mp.next_field().await.unwrap().expect("labels field");
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            read_text_field(&mut field, "labels", 1000),
        )
        .await
        .expect("the text-field cap must be enforced before the field ends");
        match outcome {
            Err(ApiError::Validation(m)) => assert!(m.contains("labels too large"), "{m}"),
            Err(_) => panic!("expected the too-large refusal, got another error"),
            Ok(t) => panic!("a {}-byte field past the cap was accepted", t.len()),
        }
    }

    // A text field at exactly its cap is read whole (the cap is inclusive).
    #[tokio::test]
    async fn text_field_at_the_cap_is_read_whole() {
        let value = "x".repeat(1000);
        let body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"labels\"\r\n\r\n\
             {value}\r\n--{BOUNDARY}--\r\n"
        );
        let mut mp = Multipart::from_request(request(Body::from(body)), &())
            .await
            .unwrap();
        let mut field = mp.next_field().await.unwrap().expect("labels field");
        match read_text_field(&mut field, "labels", 1000).await {
            Ok(t) => assert_eq!(t, value),
            Err(_) => panic!("a field at exactly the cap must be accepted"),
        }
    }

    // A complete upload streams to a staging file holding exactly its bytes,
    // under the digest every other blob path computes.
    #[tokio::test]
    async fn complete_upload_is_staged_with_its_content_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();
        let mut body = file_part_head().into_bytes();
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        let mut mp = Multipart::from_request(request(Body::from(body)), &())
            .await
            .unwrap();
        let mut field = mp.next_field().await.unwrap().expect("file field");
        let staged = match stage_file_field(&mut field, dir.path(), data.len()).await {
            Ok(s) => s,
            Err(_) => panic!("a file at exactly the cap must be accepted"),
        };
        assert_eq!(
            staged.content_hash(),
            &epigraph_crypto::ContentHasher::hash(&data)
        );
        assert_eq!(std::fs::read(staged.staging_path()).unwrap(), data);
    }

    // The upload route's body limit follows `max_upload_bytes`: without it,
    // axum's 2 MB default refuses a 3 MB file even under a 4 MB cap.
    #[tokio::test]
    async fn body_limit_admits_files_past_axums_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let blob_dir = dir.path().to_path_buf();
        let app = Router::new().route(
            "/",
            post(move |mut mp: Multipart| {
                let blob_dir = blob_dir.clone();
                async move {
                    let mut field = mp.next_field().await.unwrap().unwrap();
                    match stage_file_field(&mut field, &blob_dir, 4 * 1024 * 1024).await {
                        Ok(s) => (StatusCode::OK, s.size_bytes().to_string()),
                        Err(_) => (StatusCode::UNPROCESSABLE_ENTITY, String::new()),
                    }
                }
            })
            .layer(upload_body_limit(4 * 1024 * 1024)),
        );
        let server = axum_test::TestServer::new(app).unwrap();
        let three_mb = vec![1u8; 3 * 1024 * 1024];
        let form = axum_test::multipart::MultipartForm::new().add_part(
            "file",
            axum_test::multipart::Part::bytes(three_mb).file_name("big.bin"),
        );
        let res = server.post("/").multipart(form).await;
        assert_eq!(res.status_code(), StatusCode::OK, "{}", res.text());
        assert_eq!(res.text(), (3 * 1024 * 1024).to_string());
    }
}
