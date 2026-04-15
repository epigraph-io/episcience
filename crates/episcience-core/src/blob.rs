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
        assert!(self.content_hash.len() >= 4, "content_hash must be at least 4 bytes");
        let hex = hex::encode(&self.content_hash);
        base_dir
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{}.blob", hex))
    }
}
