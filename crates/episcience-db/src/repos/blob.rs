use epigraph_crypto::ContentHasher;
use epigraph_db::Viewer;
use episcience_core::{BlobRef, Ownership, Visibility};
use sqlx::Row;
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::errors::DbError;

pub struct BlobRepository;

impl BlobRepository {
    /// Store blob: write content to filesystem, record metadata in DB.
    /// Returns the BlobRef. Content-addressed: if the same hash exists on
    /// disk, the file is not re-written (dedup).
    #[allow(clippy::too_many_arguments)]
    pub async fn store(
        conn: &mut sqlx::PgConnection,
        blob_dir: &Path,
        filename: &str,
        mime_type: &str,
        content: &[u8],
        uploader_id: Uuid,
        sample_id: Option<Uuid>,
        labels: &[String],
        properties: &serde_json::Value,
        owner: Ownership,
    ) -> Result<BlobRef, DbError> {
        let content_hash = ContentHasher::hash(content);
        let hex = hex::encode(content_hash);
        let size_bytes = content.len() as i64;

        // Write to filesystem (content-addressed path)
        let dir = blob_dir.join(&hex[0..2]).join(&hex[2..4]);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| DbError::Constraint(format!("Failed to create blob dir: {e}")))?;

        let file_path = dir.join(format!("{hex}.blob"));
        let tmp_path = dir.join(format!("{hex}.blob.tmp"));

        // Write to tmp file atomically
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content).await {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(DbError::Io(format!("blob write failed: {e}")));
                }
                if let Err(e) = file.flush().await {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(DbError::Io(format!("blob flush failed: {e}")));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Tmp file left from a crashed previous attempt — treat as non-fatal,
                // the DB INSERT below will be the authoritative dedup check.
            }
            Err(e) => return Err(DbError::Io(format!("blob create failed: {e}"))),
        }

        // Record metadata in DB — within a transaction so file+row stay in sync
        let id = Uuid::now_v7();
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let result = sqlx::query(
            r#"
            INSERT INTO blobs (id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at,
                owner_group_id, visibility)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW(), $10, $11)
            RETURNING id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at,
                owner_group_id, visibility
            "#,
        )
        .bind(id)
        .bind(filename)
        .bind(mime_type)
        .bind(size_bytes)
        .bind(&content_hash[..])
        .bind(uploader_id)
        .bind(sample_id)
        .bind(labels)
        .bind(properties)
        .bind(owner.owner_group_id)
        .bind(owner.visibility.as_str())
        .fetch_one(&mut *tx)
        .await;

        let row = match result {
            Ok(r) => r,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(DbError::Sqlx(e));
            }
        };

        // Commit then atomically rename tmp → final
        if let Err(e) = tx.commit().await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(DbError::Sqlx(e));
        }

        // If the blob file already exists (dedup), just remove tmp
        if file_path.exists() {
            let _ = tokio::fs::remove_file(&tmp_path).await;
        } else {
            tokio::fs::rename(&tmp_path, &file_path)
                .await
                .map_err(|e| DbError::Io(format!("blob rename failed: {e}")))?;
        }

        Ok(row_to_blob(&row))
    }

    /// Read blob content from filesystem.
    pub async fn read_content(blob_dir: &Path, content_hash: &[u8]) -> Result<Vec<u8>, DbError> {
        if content_hash.len() < 4 {
            return Err(DbError::Constraint(format!(
                "content_hash too short: {} bytes",
                content_hash.len()
            )));
        }
        let hex = hex::encode(content_hash);
        let path = blob_dir
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.blob"));

        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DbError::NotFound {
                    entity: "blob_file".into(),
                    id: hex.clone(),
                }
            } else {
                tracing::warn!(path = %path.display(), error = %e, "blob read failed");
                DbError::Io(e.to_string())
            }
        })
    }

    /// Blob metadata by ID, UNFILTERED (internal use; request handlers use
    /// [`Self::get_readable`]).
    pub async fn get_by_id<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
    ) -> Result<BlobRef, DbError> {
        let row = sqlx::query(
            r#"
            SELECT id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at,
                owner_group_id, visibility
            FROM blobs WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(executor)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "blob".into(),
            id: id.to_string(),
        })?;

        Ok(row_to_blob(&row))
    }

    /// Blob metadata if `viewer` can read it; an invisible blob is reported
    /// exactly like a missing one.
    pub async fn get_readable<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
        viewer: &Viewer,
    ) -> Result<BlobRef, DbError> {
        let sql = viewer.splice(
            "SELECT b.id, b.filename, b.mime_type, b.size_bytes, b.content_hash,
                    b.uploader_id, b.sample_id, b.labels, b.properties, b.created_at,
                    b.owner_group_id, b.visibility
               FROM blobs b WHERE b.id = $1 /* {VISIBILITY:b} */",
            2,
        );
        let mut q = sqlx::query(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let row = q
            .fetch_optional(executor)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "blob".into(),
                id: id.to_string(),
            })?;
        Ok(row_to_blob(&row))
    }

    /// The blobs of a sample that `viewer` can read.
    pub async fn list_by_sample<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        sample_id: Uuid,
        viewer: &Viewer,
    ) -> Result<Vec<BlobRef>, DbError> {
        let sql = viewer.splice(
            "SELECT b.id, b.filename, b.mime_type, b.size_bytes, b.content_hash,
                    b.uploader_id, b.sample_id, b.labels, b.properties, b.created_at,
                    b.owner_group_id, b.visibility
               FROM blobs b WHERE b.sample_id = $1 /* {VISIBILITY:b} */
              ORDER BY b.created_at DESC",
            2,
        );
        let mut q = sqlx::query(&sql).bind(sample_id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(executor).await?;
        Ok(rows.iter().map(row_to_blob).collect())
    }

    /// Verify blob integrity: re-hash file and compare to stored hash.
    pub async fn verify_integrity(blob_dir: &Path, stored_hash: &[u8]) -> Result<bool, DbError> {
        let content = Self::read_content(blob_dir, stored_hash).await?;
        let actual = ContentHasher::hash(&content);
        Ok(actual[..] == stored_hash[..])
    }
}

fn row_to_blob(row: &sqlx::postgres::PgRow) -> BlobRef {
    BlobRef {
        id: row.get("id"),
        filename: row.get("filename"),
        mime_type: row.get("mime_type"),
        size_bytes: row.get("size_bytes"),
        content_hash: row.get("content_hash"),
        uploader_id: row.get("uploader_id"),
        sample_id: row.get("sample_id"),
        labels: row.get("labels"),
        properties: row.get("properties"),
        created_at: row.get("created_at"),
        owner_group_id: row
            .try_get::<Option<Uuid>, _>("owner_group_id")
            .ok()
            .flatten(),
        visibility: row
            .try_get::<Option<String>, _>("visibility")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<Visibility>().ok()),
    }
}
