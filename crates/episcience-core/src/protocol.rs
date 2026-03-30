use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A versioned lab protocol (SOP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Protocol {
    pub id: Uuid,
    pub title: String,
    pub version: i32,
    pub authored_by: Uuid,
    pub steps: Vec<ProtocolStep>,
    pub equipment: Vec<String>,
    pub safety_notes: Option<String>,
    pub supersedes: Option<Uuid>,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
    pub content_hash: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single step in a protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolStep {
    pub order: i32,
    pub instruction: String,
    #[serde(default)]
    pub duration_minutes: Option<f64>,
    #[serde(default)]
    pub temperature_c: Option<f64>,
    #[serde(default)]
    pub notes: Option<String>,
}
