# EpiScience Phase 1: Production ELN Features

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend EpiScience with blob storage, PDF export, witness countersigning, and promote to production database — everything needed before the first lab experiment.

**Architecture:** Phase 1 adds three independent subsystems (blob store, PDF export, countersign workflow) as new modules in existing crates, plus two new migrations. Each subsystem has its own route handler. Blob store uses local filesystem with BLAKE3 content-addressing (no S3 dependency — 19GB free disk is sufficient for early lab work). PDF export uses the `printpdf` crate for deterministic, signed notebook pages. Countersign extends the existing `signature_meaning` field with a new `countersignatures` table and verification endpoint.

**Tech Stack:** Rust (axum 0.7, sqlx 0.7, tokio), printpdf 0.7, epigraph-crypto (Ed25519 + BLAKE3), PostgreSQL 16

**Repo:** `/home/jeremy/episcience/`
**Dev DB:** `postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev`
**Production DB:** `postgres://epigraph:epigraph@127.0.0.1:5432/epigraph`

---

## File Structure

### New Files
```
crates/episcience-core/src/blob.rs          — BlobRef model (metadata, not content)
crates/episcience-core/src/countersign.rs   — Countersignature model
crates/episcience-core/src/export.rs        — NotebookPage export model
crates/episcience-db/src/repos/blob.rs      — BlobRepository (filesystem + metadata DB)
crates/episcience-db/src/repos/countersign.rs — CountersignRepository
crates/episcience-api/src/routes/blobs.rs   — Upload/download handlers
crates/episcience-api/src/routes/countersign.rs — Countersign workflow handlers
crates/episcience-api/src/routes/export.rs  — PDF notebook export handler
migrations/5005_create_blobs.sql            — Blob metadata table
migrations/5006_create_countersignatures.sql — Countersignature table
tests/integration/blob_lifecycle.rs         — Blob upload/download/verify tests
tests/integration/countersign_workflow.rs   — Countersign flow tests
tests/integration/pdf_export.rs             — PDF generation tests
```

### Modified Files
```
crates/episcience-core/src/lib.rs           — Add new module exports
crates/episcience-db/src/lib.rs             — Add new repo exports
crates/episcience-db/src/repos/mod.rs       — Add new repo modules
crates/episcience-db/Cargo.toml             — Add tokio-fs dep
crates/episcience-api/src/lib.rs            — Add new routes to router
crates/episcience-api/src/routes/mod.rs     — Add new route modules
crates/episcience-api/Cargo.toml            — Add printpdf, axum-extra (multipart)
Cargo.toml                                  — Add printpdf to workspace deps
```

---

## Task 1: Blob Storage — Migration & Model

**Files:**
- Create: `migrations/5005_create_blobs.sql`
- Create: `crates/episcience-core/src/blob.rs`
- Modify: `crates/episcience-core/src/lib.rs`

- [ ] **Step 1: Write migration 5005**

```sql
-- migrations/5005_create_blobs.sql
-- Blob metadata table. Actual file content lives on filesystem at
-- EPISCIENCE_BLOB_DIR/{hash_hex[0:2]}/{hash_hex[2:4]}/{hash_hex}.blob
-- Content-addressed: duplicate uploads are deduplicated by BLAKE3 hash.

CREATE TABLE IF NOT EXISTS blobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    filename TEXT NOT NULL,
    mime_type VARCHAR(255) NOT NULL,
    size_bytes BIGINT NOT NULL,
    content_hash BYTEA NOT NULL,
    uploader_id UUID NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    sample_id UUID REFERENCES samples(id) ON DELETE SET NULL,
    labels TEXT[] NOT NULL DEFAULT '{}',
    properties JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT blobs_content_hash_length CHECK (octet_length(content_hash) = 32),
    CONSTRAINT blobs_size_positive CHECK (size_bytes > 0),
    CONSTRAINT blobs_filename_not_empty CHECK (length(trim(filename)) > 0)
);

CREATE INDEX IF NOT EXISTS idx_blobs_uploader ON blobs(uploader_id);
CREATE INDEX IF NOT EXISTS idx_blobs_sample ON blobs(sample_id) WHERE sample_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_blobs_hash ON blobs(content_hash);
CREATE INDEX IF NOT EXISTS idx_blobs_labels ON blobs USING GIN(labels);
CREATE INDEX IF NOT EXISTS idx_blobs_created ON blobs(created_at DESC);
```

- [ ] **Step 2: Run migration on dev DB**

Run: `PGPASSWORD=epigraph psql -h 127.0.0.1 -U epigraph -d epigraph_dev -f migrations/5005_create_blobs.sql`
Expected: CREATE TABLE, CREATE INDEX (×5)

- [ ] **Step 3: Write BlobRef model**

```rust
// crates/episcience-core/src/blob.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Metadata reference to a content-addressed blob on the filesystem.
/// The actual bytes live at EPISCIENCE_BLOB_DIR/{hash_hex[0:2]}/{hash_hex[2:4]}/{hash_hex}.blob
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRef {
    pub id: Uuid,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub content_hash: Vec<u8>,
    pub uploader_id: Uuid,
    pub sample_id: Option<Uuid>,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl BlobRef {
    /// Compute the filesystem path for this blob's content.
    pub fn storage_path(&self, base_dir: &std::path::Path) -> std::path::PathBuf {
        let hex = hex::encode(&self.content_hash);
        base_dir
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{}.blob", hex))
    }
}
```

- [ ] **Step 4: Add to lib.rs exports**

Add to `crates/episcience-core/src/lib.rs`:
```rust
pub mod blob;
pub use blob::BlobRef;
```

- [ ] **Step 5: Verify it compiles**

Run: `cd /home/jeremy/episcience && SQLX_OFFLINE=true cargo build -p episcience-core`
Expected: Compiles with 0 errors

- [ ] **Step 6: Commit**

```bash
git add migrations/5005_create_blobs.sql crates/episcience-core/src/blob.rs crates/episcience-core/src/lib.rs
git commit -m "feat(core): add blob metadata table and BlobRef model"
```

---

## Task 2: Blob Storage — Repository

**Files:**
- Create: `crates/episcience-db/src/repos/blob.rs`
- Modify: `crates/episcience-db/src/repos/mod.rs`
- Modify: `crates/episcience-db/src/lib.rs`
- Modify: `crates/episcience-db/Cargo.toml`

- [ ] **Step 1: Add tokio dep for async filesystem**

Add to `crates/episcience-db/Cargo.toml` under `[dependencies]`:
```toml
tokio.workspace = true
hex = "0.4"
```

- [ ] **Step 2: Write BlobRepository**

```rust
// crates/episcience-db/src/repos/blob.rs
use epigraph_crypto::ContentHasher;
use episcience_core::BlobRef;
use sqlx::{PgPool, Row};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::errors::DbError;

pub struct BlobRepository;

impl BlobRepository {
    /// Store blob: write content to filesystem, record metadata in DB.
    /// Returns the BlobRef. Content-addressed: if the same hash exists on
    /// disk, the file is not re-written (dedup).
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
        let hex = hex::encode(content_hash);
        let path = blob_dir
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.blob"));

        tokio::fs::read(&path)
            .await
            .map_err(|e| DbError::NotFound {
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
```

- [ ] **Step 3: Register module**

Add to `crates/episcience-db/src/repos/mod.rs`:
```rust
pub mod blob;
```

Add to `crates/episcience-db/src/lib.rs`:
```rust
pub use repos::blob::BlobRepository;
```

- [ ] **Step 4: Verify it compiles**

Run: `cd /home/jeremy/episcience && SQLX_OFFLINE=true cargo build -p episcience-db`
Expected: Compiles with 0 errors

- [ ] **Step 5: Commit**

```bash
git add crates/episcience-db/
git commit -m "feat(db): add BlobRepository with content-addressed filesystem storage"
```

---

## Task 3: Blob Storage — API Routes

**Files:**
- Create: `crates/episcience-api/src/routes/blobs.rs`
- Modify: `crates/episcience-api/src/routes/mod.rs`
- Modify: `crates/episcience-api/src/lib.rs`
- Modify: `crates/episcience-api/src/state.rs`
- Modify: `crates/episcience-api/Cargo.toml`
- Modify: `crates/episcience-api/src/bin/server.rs`

- [ ] **Step 1: Add multipart dep and hex to API Cargo.toml**

Add to `crates/episcience-api/Cargo.toml` under `[dependencies]`:
```toml
axum-extra = { version = "0.9", features = ["multipart"] }
hex = "0.4"
```

- [ ] **Step 2: Add blob_dir to ElnState**

Replace `crates/episcience-api/src/state.rs`:
```rust
use sqlx::PgPool;
use std::path::PathBuf;

#[derive(Clone)]
pub struct ElnState {
    pub pool: PgPool,
    pub blob_dir: PathBuf,
}
```

- [ ] **Step 3: Update server.rs to read EPISCIENCE_BLOB_DIR**

In `crates/episcience-api/src/bin/server.rs`, replace the state construction:
```rust
    let blob_dir = std::env::var("EPISCIENCE_BLOB_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/episcience/blobs"));
    tokio::fs::create_dir_all(&blob_dir)
        .await
        .expect("Failed to create blob directory");
    tracing::info!("Blob storage: {}", blob_dir.display());

    let state = ElnState { pool, blob_dir };
```

- [ ] **Step 4: Write blob route handlers**

```rust
// crates/episcience-api/src/routes/blobs.rs
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::Multipart;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::BlobRef;
use episcience_db::BlobRepository;

/// Upload a blob via multipart form.
/// Fields: file (required), uploader_id (required), sample_id (optional),
///         labels (optional, comma-separated), properties (optional, JSON string)
async fn upload_blob(
    State(state): State<ElnState>,
    mut multipart: Multipart,
) -> Result<Json<BlobRef>, ApiError> {
    let mut file_data: Option<(String, String, Vec<u8>)> = None;
    let mut uploader_id: Option<Uuid> = None;
    let mut sample_id: Option<Uuid> = None;
    let mut labels: Vec<String> = Vec::new();
    let mut properties = serde_json::Value::Object(Default::default());

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::Validation(format!("Multipart error: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let filename = field
                    .file_name()
                    .unwrap_or("unnamed")
                    .to_string();
                let mime = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::Validation(format!("Read error: {e}")))?;
                file_data = Some((filename, mime, bytes.to_vec()));
            }
            "uploader_id" => {
                let text = field.text().await.unwrap_or_default();
                uploader_id =
                    Some(text.parse().map_err(|_| ApiError::Validation("Invalid uploader_id UUID".into()))?);
            }
            "sample_id" => {
                let text = field.text().await.unwrap_or_default();
                if !text.is_empty() {
                    sample_id =
                        Some(text.parse().map_err(|_| ApiError::Validation("Invalid sample_id UUID".into()))?);
                }
            }
            "labels" => {
                let text = field.text().await.unwrap_or_default();
                labels = text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            "properties" => {
                let text = field.text().await.unwrap_or_default();
                if !text.is_empty() {
                    properties = serde_json::from_str(&text)
                        .map_err(|e| ApiError::Validation(format!("Invalid properties JSON: {e}")))?;
                }
            }
            _ => {} // ignore unknown fields
        }
    }

    let (filename, mime, content) =
        file_data.ok_or_else(|| ApiError::Validation("Missing 'file' field".into()))?;
    let uploader =
        uploader_id.ok_or_else(|| ApiError::Validation("Missing 'uploader_id' field".into()))?;

    let blob = BlobRepository::store(
        &state.pool,
        &state.blob_dir,
        &filename,
        &mime,
        &content,
        uploader,
        sample_id,
        &labels,
        &properties,
    )
    .await?;

    Ok(Json(blob))
}

/// Download blob content by ID.
async fn download_blob(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let blob = BlobRepository::get_by_id(&state.pool, id).await?;
    let content =
        BlobRepository::read_content(&state.blob_dir, &blob.content_hash).await?;

    let response = Response::builder()
        .header(header::CONTENT_TYPE, &blob.mime_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", blob.filename),
        )
        .header(
            "X-Content-Hash",
            hex::encode(&blob.content_hash),
        )
        .body(Body::from(content))
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(response)
}

/// Get blob metadata by ID.
async fn get_blob_metadata(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Json<BlobRef>, ApiError> {
    let blob = BlobRepository::get_by_id(&state.pool, id).await?;
    Ok(Json(blob))
}

/// Verify blob integrity.
async fn verify_blob(
    State(state): State<ElnState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let blob = BlobRepository::get_by_id(&state.pool, id).await?;
    let valid =
        BlobRepository::verify_integrity(&state.blob_dir, &blob.content_hash).await?;

    Ok(Json(serde_json::json!({
        "blob_id": id,
        "filename": blob.filename,
        "hash": hex::encode(&blob.content_hash),
        "integrity_valid": valid,
    })))
}

#[derive(Deserialize)]
pub struct ListBlobsParams {
    pub sample_id: Uuid,
}

async fn list_blobs_by_sample(
    State(state): State<ElnState>,
    Query(params): Query<ListBlobsParams>,
) -> Result<Json<Vec<BlobRef>>, ApiError> {
    let blobs =
        BlobRepository::list_by_sample(&state.pool, params.sample_id).await?;
    Ok(Json(blobs))
}

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
```

- [ ] **Step 5: Register blob routes**

Add to `crates/episcience-api/src/routes/mod.rs`:
```rust
pub mod blobs;
```

Add to `crates/episcience-api/src/lib.rs` in `create_router`:
```rust
        .merge(routes::blobs::router(state.clone()))
```

- [ ] **Step 6: Verify it compiles**

Run: `cd /home/jeremy/episcience && SQLX_OFFLINE=true cargo build`
Expected: Compiles with 0 errors

- [ ] **Step 7: Restart server and smoke test**

```bash
fuser -k 8081/tcp; sleep 1
EPISCIENCE_BLOB_DIR=/tmp/episcience-blobs \
DATABASE_URL="postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev" \
RUST_LOG=info ./target/release/episcience-server &>/tmp/episcience.log &
sleep 2

AGENT_ID=$(PGPASSWORD=epigraph psql -h 127.0.0.1 -U epigraph -d epigraph_dev -t -A -c "SELECT id FROM agents LIMIT 1;")

# Upload a test file
echo "Test AFM data 12345" > /tmp/test-afm.dat
curl -s -X POST http://127.0.0.1:8081/api/v1/eln/blobs \
  -F "file=@/tmp/test-afm.dat;type=application/octet-stream" \
  -F "uploader_id=$AGENT_ID" \
  -F "labels=afm,test"
```

Expected: JSON with blob ID, filename, content_hash, size_bytes

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(api): add blob upload/download/verify endpoints with content-addressed filesystem storage"
```

---

## Task 4: Countersignature — Migration & Model

**Files:**
- Create: `migrations/5006_create_countersignatures.sql`
- Create: `crates/episcience-core/src/countersign.rs`
- Modify: `crates/episcience-core/src/lib.rs`

- [ ] **Step 1: Write migration 5006**

```sql
-- migrations/5006_create_countersignatures.sql
-- Countersignatures: a second agent signs an existing claim to attest
-- they witnessed, reviewed, or approved the content. Patent-defensible
-- witnessing requires: original author signature + witness signature +
-- timestamps for both.

CREATE TABLE IF NOT EXISTS countersignatures (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    claim_id UUID NOT NULL REFERENCES claims(id) ON DELETE RESTRICT,
    signer_id UUID NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    signature_meaning VARCHAR(50) NOT NULL
        CHECK (signature_meaning IN ('witnessed', 'approved', 'reviewed', 'certified', 'countersigned')),
    content_hash BYTEA NOT NULL,
    signature BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT cs_content_hash_length CHECK (octet_length(content_hash) = 32),
    CONSTRAINT cs_signature_length CHECK (octet_length(signature) = 64),
    CONSTRAINT cs_unique_signer_claim UNIQUE (claim_id, signer_id, signature_meaning)
);

CREATE INDEX IF NOT EXISTS idx_cs_claim ON countersignatures(claim_id);
CREATE INDEX IF NOT EXISTS idx_cs_signer ON countersignatures(signer_id);
CREATE INDEX IF NOT EXISTS idx_cs_created ON countersignatures(created_at DESC);
```

- [ ] **Step 2: Run migration on dev DB**

Run: `PGPASSWORD=epigraph psql -h 127.0.0.1 -U epigraph -d epigraph_dev -f migrations/5006_create_countersignatures.sql`
Expected: CREATE TABLE, CREATE INDEX (×3)

- [ ] **Step 3: Write Countersignature model**

```rust
// crates/episcience-core/src/countersign.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A countersignature attesting to a claim's content.
/// The signer affirms the signature_meaning (witnessed, approved, etc.)
/// over the BLAKE3 hash of the claim content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Countersignature {
    pub id: Uuid,
    pub claim_id: Uuid,
    pub signer_id: Uuid,
    pub signature_meaning: String,
    pub content_hash: Vec<u8>,
    pub signature: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

/// Verification result for a countersignature.
#[derive(Debug, Serialize)]
pub struct VerificationResult {
    pub countersignature_id: Uuid,
    pub claim_id: Uuid,
    pub signer_id: Uuid,
    pub signature_meaning: String,
    pub content_hash_valid: bool,
    pub signature_valid: bool,
}
```

- [ ] **Step 4: Add to lib.rs**

Add to `crates/episcience-core/src/lib.rs`:
```rust
pub mod countersign;
pub use countersign::{Countersignature, VerificationResult};
```

- [ ] **Step 5: Compile check**

Run: `cd /home/jeremy/episcience && SQLX_OFFLINE=true cargo build -p episcience-core`
Expected: 0 errors

- [ ] **Step 6: Commit**

```bash
git add migrations/5006_create_countersignatures.sql crates/episcience-core/src/countersign.rs crates/episcience-core/src/lib.rs
git commit -m "feat(core): add countersignature table and model for patent-defensible witnessing"
```

---

## Task 5: Countersignature — Repository & Routes

**Files:**
- Create: `crates/episcience-db/src/repos/countersign.rs`
- Create: `crates/episcience-api/src/routes/countersign.rs`
- Modify: `crates/episcience-db/src/repos/mod.rs`
- Modify: `crates/episcience-db/src/lib.rs`
- Modify: `crates/episcience-api/src/routes/mod.rs`
- Modify: `crates/episcience-api/src/lib.rs`

- [ ] **Step 1: Write CountersignRepository**

```rust
// crates/episcience-db/src/repos/countersign.rs
use episcience_core::Countersignature;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct CountersignRepository;

impl CountersignRepository {
    pub async fn create(
        pool: &PgPool,
        claim_id: Uuid,
        signer_id: Uuid,
        signature_meaning: &str,
        content_hash: &[u8],
        signature: &[u8],
    ) -> Result<Countersignature, DbError> {
        let id = Uuid::now_v7();
        let row = sqlx::query(
            r#"
            INSERT INTO countersignatures (id, claim_id, signer_id, signature_meaning,
                content_hash, signature, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, NOW())
            RETURNING id, claim_id, signer_id, signature_meaning,
                content_hash, signature, created_at
            "#,
        )
        .bind(id)
        .bind(claim_id)
        .bind(signer_id)
        .bind(signature_meaning)
        .bind(content_hash)
        .bind(signature)
        .fetch_one(pool)
        .await?;

        Ok(row_to_cs(&row))
    }

    pub async fn list_for_claim(
        pool: &PgPool,
        claim_id: Uuid,
    ) -> Result<Vec<Countersignature>, DbError> {
        let rows = sqlx::query(
            r#"
            SELECT id, claim_id, signer_id, signature_meaning,
                content_hash, signature, created_at
            FROM countersignatures WHERE claim_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(claim_id)
        .fetch_all(pool)
        .await?;

        Ok(rows.iter().map(row_to_cs).collect())
    }
}

fn row_to_cs(row: &sqlx::postgres::PgRow) -> Countersignature {
    Countersignature {
        id: row.get("id"),
        claim_id: row.get("claim_id"),
        signer_id: row.get("signer_id"),
        signature_meaning: row.get("signature_meaning"),
        content_hash: row.get("content_hash"),
        signature: row.get("signature"),
        created_at: row.get("created_at"),
    }
}
```

- [ ] **Step 2: Register in db crate**

Add to `crates/episcience-db/src/repos/mod.rs`:
```rust
pub mod countersign;
```

Add to `crates/episcience-db/src/lib.rs`:
```rust
pub use repos::countersign::CountersignRepository;
```

- [ ] **Step 3: Write countersign route handlers**

```rust
// crates/episcience-api/src/routes/countersign.rs
use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use epigraph_crypto::{ContentHasher, SignatureVerifier};
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Countersignature, VerificationResult};
use episcience_db::CountersignRepository;

#[derive(Deserialize)]
pub struct CountersignRequest {
    pub claim_id: Uuid,
    pub signer_id: Uuid,
    pub signature_meaning: String,
    /// Hex-encoded Ed25519 signature (128 hex chars = 64 bytes)
    pub signature_hex: String,
    /// Hex-encoded Ed25519 public key of the signer (64 hex chars = 32 bytes)
    pub public_key_hex: String,
}

async fn create_countersignature(
    State(state): State<ElnState>,
    Json(req): Json<CountersignRequest>,
) -> Result<Json<Countersignature>, ApiError> {
    // Validate signature_meaning
    let valid_meanings = ["witnessed", "approved", "reviewed", "certified", "countersigned"];
    if !valid_meanings.contains(&req.signature_meaning.as_str()) {
        return Err(ApiError::Validation(format!(
            "Invalid signature_meaning '{}'. Must be one of: {}",
            req.signature_meaning,
            valid_meanings.join(", ")
        )));
    }

    // Fetch the claim content to compute hash
    let claim_row = sqlx::query("SELECT content FROM claims WHERE id = $1")
        .bind(req.claim_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("Claim {} not found", req.claim_id)))?;

    let content: String = sqlx::Row::get(&claim_row, "content");
    let content_hash = ContentHasher::hash(content.as_bytes());

    // Parse and verify the Ed25519 signature
    let sig_bytes: [u8; 64] = hex::decode(&req.signature_hex)
        .map_err(|e| ApiError::Validation(format!("Invalid signature hex: {e}")))?
        .try_into()
        .map_err(|_| ApiError::Validation("Signature must be 64 bytes".into()))?;

    let pub_bytes: [u8; 32] = hex::decode(&req.public_key_hex)
        .map_err(|e| ApiError::Validation(format!("Invalid public key hex: {e}")))?
        .try_into()
        .map_err(|_| ApiError::Validation("Public key must be 32 bytes".into()))?;

    let valid = SignatureVerifier::verify(&pub_bytes, content.as_bytes(), &sig_bytes)
        .map_err(|e| ApiError::Validation(format!("Verification error: {e}")))?;

    if !valid {
        return Err(ApiError::Validation(
            "Signature verification failed: signature does not match claim content".into(),
        ));
    }

    // Store the countersignature
    let cs = CountersignRepository::create(
        &state.pool,
        req.claim_id,
        req.signer_id,
        &req.signature_meaning,
        &content_hash[..],
        &sig_bytes[..],
    )
    .await?;

    Ok(Json(cs))
}

async fn list_countersignatures(
    State(state): State<ElnState>,
    Path(claim_id): Path<Uuid>,
) -> Result<Json<Vec<Countersignature>>, ApiError> {
    let sigs =
        CountersignRepository::list_for_claim(&state.pool, claim_id).await?;
    Ok(Json(sigs))
}

async fn verify_countersignatures(
    State(state): State<ElnState>,
    Path(claim_id): Path<Uuid>,
) -> Result<Json<Vec<VerificationResult>>, ApiError> {
    // Get claim content
    let claim_row = sqlx::query("SELECT content FROM claims WHERE id = $1")
        .bind(claim_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("Claim {} not found", claim_id)))?;

    let content: String = sqlx::Row::get(&claim_row, "content");
    let expected_hash = ContentHasher::hash(content.as_bytes());

    let sigs =
        CountersignRepository::list_for_claim(&state.pool, claim_id).await?;

    let mut results = Vec::new();
    for cs in &sigs {
        let hash_valid = cs.content_hash[..] == expected_hash[..];

        // Look up signer public key
        let agent_row = sqlx::query("SELECT public_key FROM agents WHERE id = $1")
            .bind(cs.signer_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

        let sig_valid = if let Some(row) = agent_row {
            let pk_hex: String = sqlx::Row::get(&row, "public_key");
            if let Ok(pk_bytes) = hex::decode(&pk_hex) {
                if let Ok(pk_arr) = <[u8; 32]>::try_from(pk_bytes.as_slice()) {
                    if let Ok(sig_arr) = <[u8; 64]>::try_from(cs.signature.as_slice()) {
                        SignatureVerifier::verify(&pk_arr, content.as_bytes(), &sig_arr)
                            .unwrap_or(false)
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        results.push(VerificationResult {
            countersignature_id: cs.id,
            claim_id: cs.claim_id,
            signer_id: cs.signer_id,
            signature_meaning: cs.signature_meaning.clone(),
            content_hash_valid: hash_valid,
            signature_valid: sig_valid,
        });
    }

    Ok(Json(results))
}

pub fn router(state: ElnState) -> Router {
    let nested = Router::new()
        .route("/", get(list_countersignatures))
        .route("/verify", get(verify_countersignatures));

    Router::new()
        .route("/api/v1/eln/countersign", post(create_countersignature))
        .nest("/api/v1/eln/claims/:claim_id/countersignatures", nested)
        .with_state(state)
}
```

- [ ] **Step 4: Register in API crate**

Add to `crates/episcience-api/src/routes/mod.rs`:
```rust
pub mod countersign;
```

Add to `crates/episcience-api/src/lib.rs` in `create_router`:
```rust
        .merge(routes::countersign::router(state.clone()))
```

- [ ] **Step 5: Compile and test**

Run: `cd /home/jeremy/episcience && SQLX_OFFLINE=true cargo build`
Expected: 0 errors

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(api): add countersignature workflow with Ed25519 verification"
```

---

## Task 6: PDF Notebook Export

**Files:**
- Create: `crates/episcience-api/src/routes/export.rs`
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/episcience-api/Cargo.toml`
- Modify: `crates/episcience-api/src/routes/mod.rs`
- Modify: `crates/episcience-api/src/lib.rs`

- [ ] **Step 1: Add printpdf dependency**

Add to workspace `Cargo.toml` under `[workspace.dependencies]`:
```toml
printpdf = "0.7"
```

Add to `crates/episcience-api/Cargo.toml` under `[dependencies]`:
```toml
printpdf.workspace = true
```

- [ ] **Step 2: Write PDF export handler**

```rust
// crates/episcience-api/src/routes/export.rs
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, NaiveDate, Utc};
use epigraph_crypto::ContentHasher;
use printpdf::*;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;

#[derive(Deserialize)]
pub struct ExportParams {
    /// Start date (inclusive), YYYY-MM-DD
    pub from: String,
    /// End date (inclusive), YYYY-MM-DD
    pub to: String,
    /// Agent ID to filter by (optional — all agents if omitted)
    #[serde(default)]
    pub agent_id: Option<Uuid>,
    /// Label filter (optional)
    #[serde(default)]
    pub label: Option<String>,
}

struct NotebookRow {
    id: Uuid,
    content: String,
    agent_id: Uuid,
    agent_name: String,
    truth_value: f64,
    labels: Vec<String>,
    created_at: DateTime<Utc>,
}

async fn export_notebook_pdf(
    State(state): State<ElnState>,
    Query(params): Query<ExportParams>,
) -> Result<Response, ApiError> {
    let from = NaiveDate::parse_from_str(&params.from, "%Y-%m-%d")
        .map_err(|_| ApiError::Validation("Invalid 'from' date, use YYYY-MM-DD".into()))?;
    let to = NaiveDate::parse_from_str(&params.to, "%Y-%m-%d")
        .map_err(|_| ApiError::Validation("Invalid 'to' date, use YYYY-MM-DD".into()))?;

    let from_ts = from.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let to_ts = to.and_hms_opt(23, 59, 59).unwrap().and_utc();

    // Query claims in date range
    let rows = sqlx::query(
        r#"
        SELECT c.id, c.content, c.agent_id, c.truth_value, c.labels, c.created_at,
               COALESCE(a.display_name, c.agent_id::text) AS agent_name
        FROM claims c
        LEFT JOIN agents a ON a.id = c.agent_id
        WHERE c.created_at >= $1 AND c.created_at <= $2
          AND ($3::uuid IS NULL OR c.agent_id = $3)
          AND ($4::text IS NULL OR c.labels @> ARRAY[$4::text])
        ORDER BY c.created_at ASC
        "#,
    )
    .bind(from_ts)
    .bind(to_ts)
    .bind(params.agent_id)
    .bind(params.label.as_deref())
    .fetch_all(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    let entries: Vec<NotebookRow> = rows
        .iter()
        .map(|r| NotebookRow {
            id: sqlx::Row::get(r, "id"),
            content: sqlx::Row::get(r, "content"),
            agent_id: sqlx::Row::get(r, "agent_id"),
            agent_name: sqlx::Row::get(r, "agent_name"),
            truth_value: sqlx::Row::get(r, "truth_value"),
            labels: sqlx::Row::get(r, "labels"),
            created_at: sqlx::Row::get(r, "created_at"),
        })
        .collect();

    // Generate PDF
    let pdf_bytes = generate_notebook_pdf(&params.from, &params.to, &entries)?;

    // Compute BLAKE3 hash of the PDF for integrity verification
    let pdf_hash = ContentHasher::hash(&pdf_bytes);
    let hash_hex = hex::encode(pdf_hash);

    let response = Response::builder()
        .header(header::CONTENT_TYPE, "application/pdf")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"notebook_{}_{}.pdf\"",
                params.from, params.to
            ),
        )
        .header("X-Content-Hash", &hash_hex)
        .body(Body::from(pdf_bytes))
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(response)
}

fn generate_notebook_pdf(
    from: &str,
    to: &str,
    entries: &[NotebookRow],
) -> Result<Vec<u8>, ApiError> {
    let (doc, page1, layer1) = PdfDocument::new(
        &format!("EpiScience Lab Notebook — {} to {}", from, to),
        Mm(210.0),
        Mm(297.0),
        "Layer 1",
    );

    let font = doc.add_builtin_font(BuiltinFont::Helvetica).unwrap();
    let font_bold = doc.add_builtin_font(BuiltinFont::HelveticaBold).unwrap();
    let font_mono = doc.add_builtin_font(BuiltinFont::Courier).unwrap();

    let mut current_layer = doc.get_page(page1).get_layer(layer1);
    let mut y = Mm(280.0);
    let left = Mm(15.0);
    let page_width = Mm(180.0);

    // Title
    current_layer.use_text(
        "EpiScience Lab Notebook",
        16.0,
        left,
        y,
        &font_bold,
    );
    y -= Mm(7.0);
    current_layer.use_text(
        &format!("Period: {} to {} | Entries: {}", from, to, entries.len()),
        10.0,
        left,
        y,
        &font,
    );
    y -= Mm(5.0);

    let generated = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    current_layer.use_text(
        &format!("Generated: {}", generated),
        8.0,
        left,
        y,
        &font,
    );
    y -= Mm(3.0);

    // Separator
    y -= Mm(3.0);
    let line = Line {
        points: vec![
            (Point::new(left, y), false),
            (Point::new(left + page_width, y), false),
        ],
        is_closed: false,
    };
    current_layer.add_line(line);
    y -= Mm(6.0);

    for entry in entries {
        // Check if we need a new page
        if y < Mm(30.0) {
            let (new_page, new_layer) =
                doc.add_page(Mm(210.0), Mm(297.0), "Layer 1");
            current_layer = doc.get_page(new_page).get_layer(new_layer);
            y = Mm(280.0);
        }

        // Timestamp + author
        let ts = entry.created_at.format("%Y-%m-%d %H:%M UTC").to_string();
        current_layer.use_text(
            &format!("[{}] {}", ts, entry.agent_name),
            9.0,
            left,
            y,
            &font_bold,
        );
        y -= Mm(4.5);

        // Claim ID + truth value
        current_layer.use_text(
            &format!(
                "ID: {} | Truth: {:.3} | Labels: [{}]",
                &entry.id.to_string()[..8],
                entry.truth_value,
                entry.labels.join(", ")
            ),
            7.0,
            left,
            y,
            &font_mono,
        );
        y -= Mm(4.0);

        // Content (wrap at ~90 chars per line)
        let content = &entry.content;
        for chunk in content.as_bytes().chunks(90) {
            let line_text = String::from_utf8_lossy(chunk);
            current_layer.use_text(&line_text, 8.0, left + Mm(3.0), y, &font);
            y -= Mm(3.5);

            if y < Mm(30.0) {
                let (new_page, new_layer) =
                    doc.add_page(Mm(210.0), Mm(297.0), "Layer 1");
                current_layer = doc.get_page(new_page).get_layer(new_layer);
                y = Mm(280.0);
            }
        }

        y -= Mm(3.0); // spacing between entries
    }

    // Footer on last page: integrity hash
    let all_content: String = entries
        .iter()
        .map(|e| format!("{}:{}", e.id, e.content))
        .collect::<Vec<_>>()
        .join("|");
    let content_hash = ContentHasher::hash(all_content.as_bytes());
    let hash_hex = hex::encode(content_hash);

    current_layer.use_text(
        &format!("Content integrity hash (BLAKE3): {}", hash_hex),
        7.0,
        left,
        Mm(10.0),
        &font_mono,
    );

    doc.save_to_bytes()
        .map_err(|e| ApiError::Internal(format!("PDF generation failed: {e}")))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/export/notebook.pdf", get(export_notebook_pdf))
        .with_state(state)
}
```

- [ ] **Step 3: Register export routes**

Add to `crates/episcience-api/src/routes/mod.rs`:
```rust
pub mod export;
```

Add to `crates/episcience-api/src/lib.rs` in `create_router`:
```rust
        .merge(routes::export::router(state.clone()))
```

- [ ] **Step 4: Compile and test**

```bash
cd /home/jeremy/episcience && SQLX_OFFLINE=true cargo build
```
Expected: 0 errors

Smoke test:
```bash
curl -s "http://127.0.0.1:8081/api/v1/eln/export/notebook.pdf?from=2026-03-01&to=2026-03-31" \
  -o /tmp/test-notebook.pdf
file /tmp/test-notebook.pdf
```
Expected: `PDF document`

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(api): add PDF notebook export with BLAKE3 integrity hash"
```

---

## Task 7: Production Migration

**Files:**
- No new files — applies existing migrations to production database

- [ ] **Step 1: Verify all migrations are idempotent**

Each migration uses `IF NOT EXISTS` / `IF NOT EXISTS`. Verify:
```bash
for f in migrations/500*.sql; do
  echo "--- $f ---"
  grep -c "IF NOT EXISTS\|IF NOT EXISTS\|ADD COLUMN IF NOT EXISTS" "$f"
done
```
Expected: Each file has at least 1 idempotent guard

- [ ] **Step 2: Backup production database**

```bash
PGPASSWORD=epigraph pg_dump -Fc -h 127.0.0.1 -U epigraph epigraph \
  > /home/jeremy/EpigraphV2/EpiGraphV2/backups/epigraph_pre_episcience_$(date +%Y%m%d).dump
ls -la /home/jeremy/EpigraphV2/EpiGraphV2/backups/epigraph_pre_episcience_*.dump
```
Expected: Dump file created, size > 0

- [ ] **Step 3: Apply migrations 5001-5006 to production**

```bash
for f in migrations/5001_signature_meaning.sql \
         migrations/5002_claims_fulltext_search.sql \
         migrations/5003_create_samples.sql \
         migrations/5004_create_protocols.sql \
         migrations/5005_create_blobs.sql \
         migrations/5006_create_countersignatures.sql; do
  echo "=== Applying $f ==="
  PGPASSWORD=epigraph psql -h 127.0.0.1 -U epigraph -d epigraph -f "$f"
done
```
Expected: All CREATE TABLE / ALTER TABLE / CREATE INDEX succeed

- [ ] **Step 4: Verify production schema**

```bash
PGPASSWORD=epigraph_ro psql -h 127.0.0.1 -U epigraph_ro -d epigraph -c "
SELECT table_name FROM information_schema.tables
WHERE table_schema = 'public'
  AND table_name IN ('samples', 'sample_claims', 'protocols', 'blobs', 'countersignatures')
ORDER BY table_name;"
```
Expected: All 5 tables listed

- [ ] **Step 5: Verify tsvector and signature_meaning**

```bash
PGPASSWORD=epigraph_ro psql -h 127.0.0.1 -U epigraph_ro -d epigraph -c "
SELECT column_name, data_type FROM information_schema.columns
WHERE table_name = 'claims' AND column_name = 'content_tsv';"

PGPASSWORD=epigraph_ro psql -h 127.0.0.1 -U epigraph_ro -d epigraph -c "
SELECT column_name FROM information_schema.columns
WHERE table_name = 'provenance_log' AND column_name = 'signature_meaning';"
```
Expected: Both columns exist

- [ ] **Step 6: Commit (no code changes, just document)**

```bash
git commit --allow-empty -m "chore: applied EpiScience migrations 5001-5006 to production epigraph database"
```

---

## Summary

| Task | What | New Endpoints |
|------|------|---------------|
| 1-3 | Blob storage (filesystem + DB metadata) | POST /blobs, GET /blobs/:id, GET /blobs/:id/download, GET /blobs/:id/verify, GET /blobs/by-sample |
| 4-5 | Countersignature workflow | POST /countersign, GET /claims/:id/countersignatures, GET /claims/:id/countersignatures/verify |
| 6 | PDF notebook export | GET /export/notebook.pdf |
| 7 | Production migration | (no endpoints) |

Total: 8 new endpoints, 2 new migrations, 4 new source files.
