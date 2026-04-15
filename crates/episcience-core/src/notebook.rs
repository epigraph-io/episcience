use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A notebook entry — thin wrapper around an EpiGraph claim with ELN context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookEntry {
    /// The underlying EpiGraph claim ID.
    pub claim_id: Uuid,
    /// Human-readable title for the entry.
    pub title: Option<String>,
    /// The agent who authored this entry.
    pub author_id: Uuid,
    /// Optional linked sample.
    pub sample_id: Option<Uuid>,
    /// Optional linked protocol.
    pub protocol_id: Option<Uuid>,
    /// Entry content (mirrors claim content).
    pub content: String,
    /// Signature meaning (authored, witnessed, approved, etc.)
    pub signature_meaning: Option<String>,
    pub created_at: DateTime<Utc>,
}
