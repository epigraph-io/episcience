use epigraph_crypto::ContentHasher;
use epigraph_db::Viewer;
use episcience_core::{BlobRef, Ownership, Visibility};
use sqlx::Row;
use std::path::{Path, PathBuf};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::errors::DbError;

pub struct BlobRepository;

impl BlobRepository {
    /// Store blob from bytes already in memory (the MCP base64 path): the
    /// same staging and placement as a streamed upload
    /// ([`Self::stager`] then [`Self::store_staged`]), with no size cap of its
    /// own (the caller enforces it on the decoded payload).
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
        let mut stager = Self::stager(blob_dir, usize::MAX)
            .await
            .map_err(DbError::from)?;
        stager.push(content).await.map_err(DbError::from)?;
        let staged = stager.finish().await.map_err(DbError::from)?;
        Self::store_staged(
            conn,
            blob_dir,
            staged,
            filename,
            mime_type,
            uploader_id,
            sample_id,
            labels,
            properties,
            owner,
        )
        .await
    }

    /// Start staging an upload of at most `max_bytes`: a private temp file
    /// inside `blob_dir` (same filesystem, so the final rename is atomic),
    /// hashed with BLAKE3 as it is written.
    pub async fn stager(blob_dir: &Path, max_bytes: usize) -> Result<BlobStager, BlobStageError> {
        tokio::fs::create_dir_all(blob_dir)
            .await
            .map_err(|e| BlobStageError::Io(format!("create blob dir: {e}")))?;
        let tmp_path = blob_dir.join(format!(".upload-{}.part", Uuid::now_v7()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| BlobStageError::Io(format!("create staging file: {e}")))?;
        Ok(BlobStager {
            file,
            guard: TempGuard(Some(tmp_path)),
            hasher: blake3::Hasher::new(),
            size: 0,
            max_bytes,
        })
    }

    /// Record a staged upload: insert the metadata row, move the staging
    /// file to its content-addressed path, then release a savepoint. Callers
    /// run this inside their own `write_as` transaction, so the row becomes
    /// durable only when the CALLER commits; the file is placed before that,
    /// so a rollback or commit failure leaves at worst an unreferenced
    /// content-addressed file (harmless: the next upload of the same bytes
    /// reuses it). Content-addressed: if the hash's file already exists, it is
    /// not re-written (dedup), and the staging file is removed on drop. Each
    /// upload stages under its own `create_new` name, so a leftover or
    /// concurrent upload of the same content can never have a partial file
    /// renamed into place.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_staged(
        conn: &mut sqlx::PgConnection,
        blob_dir: &Path,
        mut staged: StagedBlob,
        filename: &str,
        mime_type: &str,
        uploader_id: Uuid,
        sample_id: Option<Uuid>,
        labels: &[String],
        properties: &serde_json::Value,
        owner: Ownership,
    ) -> Result<BlobRef, DbError> {
        let hex = hex::encode(staged.content_hash);
        let id = Uuid::now_v7();
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        // On any early return below, `staged` drops and removes its temp file.
        let row = sqlx::query(
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
        .bind(staged.size_bytes)
        .bind(&staged.content_hash[..])
        .bind(uploader_id)
        .bind(sample_id)
        .bind(labels)
        .bind(properties)
        .bind(owner.owner_group_id)
        .bind(owner.visibility.as_str())
        .fetch_one(&mut *tx)
        .await?;

        let dir = blob_dir.join(&hex[0..2]).join(&hex[2..4]);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| DbError::Io(format!("Failed to create blob dir: {e}")))?;
        let file_path = dir.join(format!("{hex}.blob"));
        if !file_path.exists() {
            let tmp = staged.guard.path().to_path_buf();
            tokio::fs::rename(&tmp, &file_path)
                .await
                .map_err(|e| DbError::Io(format!("blob rename failed: {e}")))?;
            staged.guard.disarm();
        }
        tx.commit().await?;
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

/// Why staging an upload stopped.
#[derive(Debug, thiserror::Error)]
pub enum BlobStageError {
    /// The upload passed its cap; staging stopped at the chunk that crossed
    /// it, without reading the rest.
    #[error("file too large: more than {max} bytes")]
    TooLarge { max: usize },
    /// The uploader declared a content hash the received bytes do not have.
    #[error("content hash mismatch: declared {declared}, received {actual}")]
    HashMismatch { declared: String, actual: String },
    #[error("blob staging I/O: {0}")]
    Io(String),
}

impl From<BlobStageError> for DbError {
    fn from(e: BlobStageError) -> Self {
        match e {
            BlobStageError::Io(m) => DbError::Io(m),
            other => DbError::Constraint(other.to_string()),
        }
    }
}

/// Removes a staging file on drop unless disarmed (it was renamed into place).
#[derive(Debug)]
struct TempGuard(Option<PathBuf>);

impl TempGuard {
    fn path(&self) -> &Path {
        self.0.as_deref().expect("staging guard already disarmed")
    }
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// An upload being written, chunk by chunk, to its staging file. Dropping it
/// (on any error) removes the file.
#[derive(Debug)]
pub struct BlobStager {
    file: tokio::fs::File,
    guard: TempGuard,
    hasher: blake3::Hasher,
    size: usize,
    max_bytes: usize,
}

impl BlobStager {
    /// Append one chunk. Refuses, before writing it, the chunk that would
    /// take the upload past its cap, so an oversized upload is never read
    /// (or held) in full.
    pub async fn push(&mut self, chunk: &[u8]) -> Result<(), BlobStageError> {
        if chunk.len() > self.max_bytes - self.size {
            return Err(BlobStageError::TooLarge {
                max: self.max_bytes,
            });
        }
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| BlobStageError::Io(format!("blob write failed: {e}")))?;
        self.hasher.update(chunk);
        self.size += chunk.len();
        Ok(())
    }

    /// Bytes accepted so far.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Flush the staging file and seal the hash.
    pub async fn finish(mut self) -> Result<StagedBlob, BlobStageError> {
        self.file
            .flush()
            .await
            .map_err(|e| BlobStageError::Io(format!("blob flush failed: {e}")))?;
        self.file
            .sync_all()
            .await
            .map_err(|e| BlobStageError::Io(format!("blob sync failed: {e}")))?;
        let content_hash: [u8; 32] = self.hasher.finalize().into();
        let guard = TempGuard(self.guard.0.take());
        Ok(StagedBlob {
            guard,
            content_hash,
            size_bytes: self.size as i64,
        })
    }
}

/// A fully received upload, in its staging file, not yet recorded.
/// Dropping it removes the staging file.
#[derive(Debug)]
pub struct StagedBlob {
    guard: TempGuard,
    content_hash: [u8; 32],
    size_bytes: i64,
}

impl StagedBlob {
    /// BLAKE3 of the received bytes (the same digest as
    /// `ContentHasher::hash` over them).
    pub fn content_hash(&self) -> &[u8; 32] {
        &self.content_hash
    }

    pub fn size_bytes(&self) -> i64 {
        self.size_bytes
    }

    /// The staging file (tests and diagnostics).
    pub fn staging_path(&self) -> &Path {
        self.guard.path()
    }

    /// Refuse the upload when the uploader declared a different hash.
    pub fn check_declared(&self, declared: &[u8]) -> Result<(), BlobStageError> {
        if declared == self.content_hash {
            Ok(())
        } else {
            Err(BlobStageError::HashMismatch {
                declared: hex::encode(declared),
                actual: hex::encode(self.content_hash),
            })
        }
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    // The digest of a streamed upload is the one every other blob path
    // (dedup, verify_integrity) computes over the same bytes.
    #[tokio::test]
    async fn chunked_hash_equals_content_hasher_over_the_whole() {
        let dir = tempfile::TempDir::new().unwrap();
        let data: Vec<u8> = (0..200_003u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut s = BlobRepository::stager(dir.path(), data.len())
            .await
            .unwrap();
        for c in data.chunks(7_919) {
            s.push(c).await.unwrap();
        }
        let staged = s.finish().await.unwrap();
        assert_eq!(staged.content_hash(), &ContentHasher::hash(&data));
        assert_eq!(staged.size_bytes(), data.len() as i64);
        assert_eq!(std::fs::read(staged.staging_path()).unwrap(), data);
    }

    // The cap is enforced on the chunk that crosses it (not after the whole
    // upload arrived), exactly at the boundary, and the staging file is gone.
    #[tokio::test]
    async fn cap_refuses_the_crossing_chunk_and_removes_the_staging_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = BlobRepository::stager(dir.path(), 10).await.unwrap();
        s.push(b"12345").await.unwrap();
        s.push(b"67890").await.unwrap(); // exactly at the cap: accepted
        let err = s.push(b"x").await.unwrap_err();
        assert!(
            matches!(err, BlobStageError::TooLarge { max: 10 }),
            "{err:?}"
        );
        assert_eq!(s.size(), 10, "the refused chunk must not be counted");
        drop(s);
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }

    #[tokio::test]
    async fn declared_hash_mismatch_is_refused_and_leaves_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = BlobRepository::stager(dir.path(), 64).await.unwrap();
        s.push(b"payload").await.unwrap();
        let staged = s.finish().await.unwrap();
        staged
            .check_declared(&ContentHasher::hash(b"payload"))
            .expect("the true hash is accepted");
        let err = staged
            .check_declared(&ContentHasher::hash(b"other"))
            .unwrap_err();
        assert!(
            matches!(err, BlobStageError::HashMismatch { .. }),
            "{err:?}"
        );
        drop(staged);
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }
}
