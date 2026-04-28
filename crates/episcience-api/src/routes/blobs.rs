use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::Multipart;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::BlobRef;
use episcience_db::BlobRepository;

// ── Upload (multipart) ────────────────────────────────────────────────

async fn upload_blob(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
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
    let uid =
        uploader_id.ok_or_else(|| ApiError::Validation("uploader_id field is required".into()))?;

    if auth.agent_id != uid {
        return Err(ApiError::Forbidden("agent mismatch".into()));
    }

    let blob = BlobRepository::store(
        &state.pool,
        &state.blob_dir,
        &fname,
        &mtype,
        &content,
        uid,
        sample_id,
        &labels,
        &properties,
    )
    .await?;

    Ok(Json(blob))
}

// ── Get metadata ──────────────────────────────────────────────────────

async fn get_blob_metadata(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Json<BlobRef>, ApiError> {
    let blob = BlobRepository::get_by_id(&state.pool, id).await?;
    Ok(Json(blob))
}

// ── Download content ──────────────────────────────────────────────────

async fn download_blob(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let blob = BlobRepository::get_by_id(&state.pool, id).await?;
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
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let blob = BlobRepository::get_by_id(&state.pool, id).await?;
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
    Query(q): Query<BySampleQuery>,
) -> Result<Json<Vec<BlobRef>>, ApiError> {
    let blobs = BlobRepository::list_by_sample(&state.pool, q.sample_id).await?;
    Ok(Json(blobs))
}

// ── Router ────────────────────────────────────────────────────────────

pub fn router(state: ElnState) -> Router {
    let nested = Router::new()
        .route("/", get(get_blob_metadata))
        .route("/download", get(download_blob))
        .route("/verify", get(verify_blob));

    Router::new()
        .route("/api/v1/eln/blobs", post(upload_blob))
        .route("/api/v1/eln/blobs/by-sample", get(list_blobs_by_sample))
        .nest("/api/v1/eln/blobs/:id", nested)
        .with_state(state)
}
