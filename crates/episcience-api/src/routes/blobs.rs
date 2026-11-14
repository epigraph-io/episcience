use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::Multipart;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::tenancy::bound_identity;
use crate::errors::ApiError;
use crate::middleware::{AuthContext, CallerViewer};
use crate::state::ElnState;
use episcience_core::blob::{parse_hash_hex, validate_location_uri};
use episcience_core::{BlobRef, BlobStorage, HashAlgorithm};
use episcience_db::{BlobRepository, NewExternalBlob, VerifyObservation};

// ── Upload (multipart) ────────────────────────────────────────────────

async fn upload_blob(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Extension(viewer): Extension<CallerViewer>,
    mut multipart: Multipart,
) -> Result<Json<BlobRef>, ApiError> {
    let mut file_data: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let mut uploader_id: Option<Uuid> = None;
    let mut sample_id: Option<Uuid> = None;
    let mut labels: Vec<String> = Vec::new();
    let mut properties: serde_json::Value = serde_json::Value::Object(Default::default());

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::Validation(format!("multipart error: {e}")))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                mime_type = field.content_type().map(|s| s.to_string());
                file_data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::Validation(format!("failed to read file: {e}")))?
                        .to_vec(),
                );
            }
            "uploader_id" => {
                let text = field.text().await.map_err(|e| {
                    ApiError::Validation(format!("failed to read uploader_id: {e}"))
                })?;
                uploader_id = Some(
                    text.trim()
                        .parse::<Uuid>()
                        .map_err(|e| ApiError::Validation(format!("invalid uploader_id: {e}")))?,
                );
            }
            "sample_id" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::Validation(format!("failed to read sample_id: {e}")))?;
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    sample_id =
                        Some(trimmed.parse::<Uuid>().map_err(|e| {
                            ApiError::Validation(format!("invalid sample_id: {e}"))
                        })?);
                }
            }
            "labels" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::Validation(format!("failed to read labels: {e}")))?;
                labels = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "properties" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::Validation(format!("failed to read properties: {e}")))?;
                properties = serde_json::from_str(&text)
                    .map_err(|e| ApiError::Validation(format!("invalid properties JSON: {e}")))?;
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    let content = file_data.ok_or_else(|| ApiError::Validation("file field is required".into()))?;

    if content.len() > state.max_upload_bytes {
        return Err(ApiError::Validation(format!(
            "file too large: {} bytes (max {})",
            content.len(),
            state.max_upload_bytes
        )));
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

    let blob = BlobRepository::store(
        &mut tx,
        &state.blob_dir,
        &fname,
        &mtype,
        &content,
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
    let nested = Router::new()
        .route("/", get(get_blob_metadata))
        .route("/download", get(download_blob))
        .route("/verify", get(verify_blob))
        .route("/verification", post(record_blob_verification));

    Router::new()
        .route("/api/v1/eln/blobs", post(upload_blob))
        .route("/api/v1/eln/blobs/external", post(register_external_blob))
        .route("/api/v1/eln/blobs/by-sample", get(list_blobs_by_sample))
        .nest("/api/v1/eln/blobs/:id", nested)
        .with_state(state)
}
