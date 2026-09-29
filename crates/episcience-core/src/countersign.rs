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
    pub prev_signature_hash: Option<Vec<u8>>,
    pub signature_version: i16,
    pub created_at: DateTime<Utc>,
    /// The principal that recorded this attestation (the authenticated
    /// caller); `signer_id` is the holder of the key that signed it.
    pub countersigned_by: Option<Uuid>,
    /// The owning group (kernel `groups.id`) and who may read the row
    /// (`None` only for a legacy row before the one-shot re-own).
    pub owner_group_id: Option<Uuid>,
    pub visibility: Option<crate::synthesis::Visibility>,
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
