use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::multipart::Field;
use axum_extra::extract::Multipart;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::tenancy::bound_identity;
use crate::errors::ApiError;
use crate::middleware::{AuthContext, CallerViewer};
use crate::state::ElnState;
use episcience_core::BlobRef;
use episcience_db::{BlobRepository, BlobStageError, StagedBlob};

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
// Fields: `file` (required), `content_hash` (optional: the BLAKE3 the
// uploader expects, 64 hex; 409 when the received bytes differ),
// `uploader_id`, `sample_id`, `labels`, `properties`.

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
                let text = field.text().await.map_err(|e| {
                    ApiError::Validation(format!("failed to read content_hash: {e}"))
                })?;
                declared_hash = Some(parse_declared_hash(&text)?);
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

// ── Router ────────────────────────────────────────────────────────────

pub fn router(state: ElnState) -> Router {
    // axum's default 2 MB body limit would refuse any upload past it whatever
    // `max_upload_bytes` says; the file is capped as it streams instead.
    let upload_limit = upload_body_limit(state.max_upload_bytes);
    let nested = Router::new()
        .route("/", get(get_blob_metadata))
        .route("/download", get(download_blob))
        .route("/verify", get(verify_blob));

    Router::new()
        .route("/api/v1/eln/blobs", post(upload_blob).layer(upload_limit))
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
