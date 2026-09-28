//! `countersign` MCP tool — mirrors `POST /api/v1/eln/countersign`.
//!
//! Phase 8 ELN write parity. The Ed25519 verification logic mirrors the HTTP
//! route exactly (`routes/countersign.rs`):
//!
//! 1. Validate `signature_meaning` is one of the allow-listed strings.
//! 2. Fetch `claims.content` for the target `claim_id`.
//! 3. Recompute the version-2 canonical message
//!    `claim_id|signer_id|signature_meaning|content`.
//! 4. Verify the Ed25519 signature against `signer_id`'s REGISTERED key (a
//!    supplied `public_key_hex` must equal it).
//! 5. Insert the countersignature row via [`CountersignRepository::create`].
//!
//! Author / signer split: `signer_id` (default: the caller) holds the key
//! that signed; `countersigned_by` is always the authenticated caller. The
//! attestation's owner follows the claim (a group claim's attestation stays
//! in that group, which the caller must be able to write).

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_db::{CountersignRepository, KernelClaimRepository};

use crate::mcp::errors::{internal_error, invalid_params, McpError};
use crate::mcp::EpiscienceServer;
use crate::middleware::AuthContext;

const ALLOWED_MEANINGS: &[&str] = &[
    "witnessed",
    "approved",
    "reviewed",
    "certified",
    "countersigned",
];

const SIGNATURE_VERSION: i16 = 2;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CountersignArgs {
    /// Target claim id (the claim being countersigned).
    #[schemars(description = "Claim id being countersigned")]
    pub claim_id: Uuid,

    /// Meaning of the signature. One of: `witnessed`, `approved`,
    /// `reviewed`, `certified`, `countersigned`.
    #[schemars(
        description = "Signature meaning: witnessed | approved | reviewed | certified | countersigned"
    )]
    pub signature_meaning: String,

    /// Hex-encoded Ed25519 signature (128 hex chars = 64 bytes). Must be
    /// computed over `claim_id|signer_id|signature_meaning|content` where
    /// `signer_id` is the authenticated caller's agent id.
    #[schemars(description = "Hex-encoded 64-byte Ed25519 signature (128 hex chars)")]
    pub signature_hex: String,

    /// Hex-encoded Ed25519 public key (64 hex chars = 32 bytes). Used to
    /// verify the supplied signature.
    #[schemars(
        description = "Optional hex-encoded 32-byte Ed25519 public key; when given it must equal the signer's registered key"
    )]
    #[serde(default)]
    pub public_key_hex: Option<String>,

    #[schemars(description = "The agent whose key signed (default: the authenticated caller)")]
    #[serde(default)]
    pub signer_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct CountersignResult {
    pub id: Uuid,
    pub claim_id: Uuid,
    pub signer_id: Uuid,
    pub signature_meaning: String,
}

pub async fn handle(
    server: &EpiscienceServer,
    auth: &AuthContext,
    args: CountersignArgs,
) -> Result<CallToolResult, McpError> {
    // 1. Validate signature_meaning
    if !ALLOWED_MEANINGS.contains(&args.signature_meaning.as_str()) {
        return Err(invalid_params(format!(
            "signature_meaning must be one of: {}",
            ALLOWED_MEANINGS.join(", ")
        )));
    }

    let signer_id = args.signer_id.unwrap_or(auth.agent_id);

    // 2. Fetch the claim AS the caller (mirrors the HTTP route): a claim the
    //    caller cannot read is reported exactly like an absent one.
    let viewer = crate::mcp::errors::caller_viewer(&server.pool, auth).await?;
    let (content, claim_visibility, claim_owner) =
        KernelClaimRepository::content_and_pair_as(&server.pool, &viewer, args.claim_id)
            .await
            .map_err(|e| internal_error(format!("claim lookup: {e}")))?
            .ok_or_else(|| invalid_params(format!("claim {} not found", args.claim_id)))?;

    // 3. Parse hex-encoded signature and the optional key
    let sig_bytes: [u8; 64] = hex::decode(&args.signature_hex)
        .map_err(|e| invalid_params(format!("invalid signature hex: {e}")))?
        .try_into()
        .map_err(|_| invalid_params("signature must be 64 bytes (128 hex chars)"))?;
    let supplied_key: Option<[u8; 32]> = match &args.public_key_hex {
        None => None,
        Some(h) => Some(
            hex::decode(h)
                .map_err(|e| invalid_params(format!("invalid public key hex: {e}")))?
                .try_into()
                .map_err(|_| invalid_params("public key must be 32 bytes (64 hex chars)"))?,
        ),
    };

    // 4. Version-2 canonical message, verified against the signer's
    //    registered key (byte-identical with the HTTP route).
    let content_hash = crate::auth::tenancy::verify_countersignature(
        &server.pool,
        args.claim_id,
        signer_id,
        &args.signature_meaning,
        &content,
        &sig_bytes,
        supplied_key.as_ref(),
    )
    .await
    .map_err(crate::mcp::errors::from_api)?;
    let owner = crate::auth::tenancy::countersign_ownership(
        &server.pool,
        &viewer,
        &claim_visibility,
        claim_owner,
    )
    .await
    .map_err(crate::mcp::errors::from_api)?;

    // 5. Insert row via repository
    let cs = CountersignRepository::create(
        &server.pool,
        args.claim_id,
        signer_id,
        auth.agent_id,
        &args.signature_meaning,
        &content_hash,
        &sig_bytes,
        SIGNATURE_VERSION,
        owner,
    )
    .await
    .map_err(|e| internal_error(format!("create countersignature: {e}")))?;

    let body = CountersignResult {
        id: cs.id,
        claim_id: cs.claim_id,
        signer_id: cs.signer_id,
        signature_meaning: cs.signature_meaning,
    };
    let text = serde_json::to_string_pretty(&body).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
