use epigraph_crypto::ContentHasher;
use epigraph_db::Viewer;
use episcience_core::{BlobRef, BlobStorage, HashAlgorithm, Ownership, VerifyStatus, Visibility};
use sqlx::Row;
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::errors::DbError;

pub struct BlobRepository;

/// Every column of a blob row, as `row_to_blob` reads it, on alias `b`.
const BLOB_COLS: &str = "b.id, b.filename, b.mime_type, b.size_bytes, b.content_hash,
    b.uploader_id, b.sample_id, b.labels, b.properties, b.created_at,
    b.owner_group_id, b.visibility, b.storage, b.location_uri, b.hash_algorithm,
    b.is_manifest, b.verify_status, b.last_verified_at, b.last_verified_by,
    b.last_observed_hash";

/// An external blob to register (migration 5043): bytes that stay at
/// `location_uri`, described by the caller. `content_hash` is DECLARED, not
/// computed: it is what a later verification compares against, and it never
/// names a file in the local blob directory.
pub struct NewExternalBlob<'a> {
    pub filename: &'a str,
    pub mime_type: &'a str,
    pub size_bytes: i64,
    pub content_hash: &'a [u8],
    pub hash_algorithm: HashAlgorithm,
    pub location_uri: &'a str,
    pub is_manifest: bool,
    pub uploader_id: Uuid,
    pub sample_id: Option<Uuid>,
    pub labels: &'a [String],
    pub properties: &'a serde_json::Value,
}

/// What a verifier with access to an external blob's store observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyObservation {
    /// The bytes at the location hash (with the blob's algorithm) to this.
    Hashed(Vec<u8>),
    /// The verifier could not read the bytes.
    Unreachable,
}

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
        let result = sqlx::query(&format!(
            r#"
            INSERT INTO blobs AS b (id, filename, mime_type, size_bytes, content_hash,
                uploader_id, sample_id, labels, properties, created_at,
                owner_group_id, visibility)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW(), $10, $11)
            RETURNING {BLOB_COLS}
            "#
        ))
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
        let row = sqlx::query(&format!("SELECT {BLOB_COLS} FROM blobs b WHERE b.id = $1"))
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
            &format!("SELECT {BLOB_COLS} FROM blobs b WHERE b.id = $1 /* {{VISIBILITY:b}} */"),
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
            &format!(
                "SELECT {BLOB_COLS} FROM blobs b WHERE b.sample_id = $1 /* {{VISIBILITY:b}} */
                  ORDER BY b.created_at DESC"
            ),
            2,
        );
        let mut q = sqlx::query(&sql).bind(sample_id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(executor).await?;
        Ok(rows.iter().map(row_to_blob).collect())
    }

    /// Register an external blob: a metadata row only, no bytes written
    /// anywhere. The row's tenancy pair is `owner` (the sample's pair when
    /// attached, as for an upload).
    pub async fn register_external<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        new: &NewExternalBlob<'_>,
        owner: Ownership,
    ) -> Result<BlobRef, DbError> {
        let row = sqlx::query(&format!(
            "INSERT INTO blobs AS b (id, filename, mime_type, size_bytes, content_hash,
                 uploader_id, sample_id, labels, properties, created_at,
                 owner_group_id, visibility, storage, location_uri, hash_algorithm, is_manifest)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), $10, $11,
                     'external', $12, $13, $14)
             RETURNING {BLOB_COLS}"
        ))
        .bind(Uuid::now_v7())
        .bind(new.filename)
        .bind(new.mime_type)
        .bind(new.size_bytes)
        .bind(new.content_hash)
        .bind(new.uploader_id)
        .bind(new.sample_id)
        .bind(new.labels)
        .bind(new.properties)
        .bind(owner.owner_group_id)
        .bind(owner.visibility.as_str())
        .bind(new.location_uri)
        .bind(new.hash_algorithm.as_str())
        .bind(new.is_manifest)
        .fetch_one(executor)
        .await?;
        Ok(row_to_blob(&row))
    }

    /// Blob metadata if `viewer` can WRITE it (its owner group is one the
    /// viewer may write); a blob the viewer cannot write is reported exactly
    /// like a missing one.
    pub async fn get_writable<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
        viewer: &Viewer,
    ) -> Result<BlobRef, DbError> {
        let sql = viewer.splice_write(
            &format!("SELECT {BLOB_COLS} FROM blobs b WHERE b.id = $1 /* {{WRITABLE:b}} */"),
            2,
        );
        let mut q = sqlx::query(&sql).bind(id);
        if let Some(groups) = viewer.writable_bind() {
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

    /// Record an in-place verification of an EXTERNAL blob the viewer may
    /// write. The status is computed here, in the same statement, from the
    /// row's own `content_hash`: `ok` when the observed hash equals it,
    /// `mismatch` otherwise (the observed hash is kept), `unreachable` when the
    /// verifier could not read the bytes. A local blob, or one the viewer
    /// cannot write, is not found (local blobs are verified by the server
    /// itself, from the blob directory).
    pub async fn record_verification<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
        viewer: &Viewer,
        observation: &VerifyObservation,
        verifier: Uuid,
    ) -> Result<BlobRef, DbError> {
        let observed: Option<&[u8]> = match observation {
            VerifyObservation::Hashed(h) => {
                if h.len() != 32 {
                    return Err(DbError::Constraint(format!(
                        "observed hash must be 32 bytes, got {}",
                        h.len()
                    )));
                }
                Some(h)
            }
            VerifyObservation::Unreachable => None,
        };
        let sql = viewer.splice_write(
            &format!(
                "UPDATE blobs AS b
                    SET verify_status = CASE WHEN $2::bytea IS NULL THEN 'unreachable'
                                             WHEN $2::bytea = b.content_hash THEN 'ok'
                                             ELSE 'mismatch' END,
                        last_observed_hash = $2::bytea,
                        last_verified_at = now(),
                        last_verified_by = $3
                  WHERE b.id = $1 AND b.storage = 'external' /* {{WRITABLE:b}} */
              RETURNING {BLOB_COLS}"
            ),
            4,
        );
        let mut q = sqlx::query(&sql).bind(id).bind(observed).bind(verifier);
        if let Some(groups) = viewer.writable_bind() {
            q = q.bind(groups);
        }
        let row = q
            .fetch_optional(executor)
            .await?
            .ok_or_else(|| DbError::NotFound {
                entity: "external blob".into(),
                id: id.to_string(),
            })?;
        Ok(row_to_blob(&row))
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
        // An unreadable storage kind reads as EXTERNAL (never local): a local
        // blob is read from the blob directory by its hash, which must only
        // happen for a hash the server computed.
        storage: row
            .get::<String, _>("storage")
            .parse::<BlobStorage>()
            .unwrap_or(BlobStorage::External),
        location_uri: row.get("location_uri"),
        hash_algorithm: row
            .get::<String, _>("hash_algorithm")
            .parse::<HashAlgorithm>()
            .unwrap_or_default(),
        is_manifest: row.get("is_manifest"),
        verify_status: row
            .get::<Option<String>, _>("verify_status")
            .and_then(|v| v.parse::<VerifyStatus>().ok()),
        last_verified_at: row.get("last_verified_at"),
        last_verified_by: row.get("last_verified_by"),
        last_observed_hash: row.get("last_observed_hash"),
    }
}
