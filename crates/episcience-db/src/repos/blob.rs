use epigraph_crypto::ContentHasher;
use episcience_core::BlobRef;
use sqlx::{PgPool, Row};
use std::path::Path;
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
        pool: &PgPool,
        blob_dir: &Path,
        filename: &str,
        mime_type: &str,
        content: &[u8],
        uploader_id: Uuid,
        sample_id: Option<Uuid>,
        labels: &[String],
        properties: &serde_json::Value,
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
        if !file_path.exists() {
            let mut file = tokio::fs::File::create(&file_path)
                .await
                .map_err(|e| DbError::Constraint(format!("Failed to write blob: {e}")))?;
            file.write_all(content)
                .await
                .map_err(|e| DbError::Constraint(format!("Failed to write blob: {e}")))?;
            file.flush().await.ok();
        }

        // Record metadata in DB
        let id = Uuid::now_v7();
        let row = sqlx::query(
            r#"
            INSERT INTO blobs (id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())
            RETURNING id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at
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
        .fetch_one(pool)
        .await?;

        Ok(row_to_blob(&row))
    }

    /// Read blob content from filesystem.
    pub async fn read_content(
        blob_dir: &Path,
        content_hash: &[u8],
    ) -> Result<Vec<u8>, DbError> {
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

        tokio::fs::read(&path)
            .await
            .map_err(|_e| DbError::NotFound {
                entity: "blob_file".into(),
                id: hex,
            })
    }

    /// Get blob metadata by ID.
    pub async fn get_by_id(pool: &PgPool, id: Uuid) -> Result<BlobRef, DbError> {
        let row = sqlx::query(
            r#"
            SELECT id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at
            FROM blobs WHERE id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| DbError::NotFound {
            entity: "blob".into(),
            id: id.to_string(),
        })?;

        Ok(row_to_blob(&row))
    }

    /// List blobs for a sample.
    pub async fn list_by_sample(
        pool: &PgPool,
        sample_id: Uuid,
    ) -> Result<Vec<BlobRef>, DbError> {
        let rows = sqlx::query(
            r#"
            SELECT id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at
            FROM blobs WHERE sample_id = $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(sample_id)
        .fetch_all(pool)
        .await?;

        Ok(rows.iter().map(row_to_blob).collect())
    }

    /// Verify blob integrity: re-hash file and compare to stored hash.
    pub async fn verify_integrity(
        blob_dir: &Path,
        stored_hash: &[u8],
    ) -> Result<bool, DbError> {
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
    }
}
