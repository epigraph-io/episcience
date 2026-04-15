use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A countersignature attesting to a claim's content.
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
