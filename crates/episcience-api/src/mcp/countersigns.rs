//! `countersign` MCP tool — mirrors `POST /api/v1/eln/countersign`.
//!
//! Phase 8 ELN write parity. The Ed25519 verification logic mirrors the HTTP
//! route exactly (`routes/countersign.rs`):
//!
//! 1. Validate `signature_meaning` is one of the allow-listed strings.
//! 2. Fetch `claims.content` for the target `claim_id`.
//! 3. Recompute the version-2 canonical message
//!    `claim_id|signer_id|signature_meaning|content`.
//! 4. Verify the Ed25519 signature with the supplied `public_key_hex`.
//! 5. Insert the countersignature row via [`CountersignRepository::create`].
//!
//! Auth: the `signer_id` is always `EpiscienceServer::auth_agent_id`. MCP
//! tools cannot countersign on behalf of a third party — the HTTP route
//! rejects this with a 403 (`auth.agent_id != req.signer_id`); MCP enforces
//! the constraint by construction (no `signer_id` arg).

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use epigraph_crypto::{ContentHasher, SignatureVerifier};
use episcience_db::CountersignRepository;

use crate::mcp::errors::{internal_error, invalid_params, McpError};
use crate::mcp::EpiscienceServer;

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
    /// `signer_id` is the MCP-authenticated agent id.
    #[schemars(description = "Hex-encoded 64-byte Ed25519 signature (128 hex chars)")]
    pub signature_hex: String,

    /// Hex-encoded Ed25519 public key (64 hex chars = 32 bytes). Used to
    /// verify the supplied signature.
    #[schemars(description = "Hex-encoded 32-byte Ed25519 public key (64 hex chars)")]
    pub public_key_hex: String,
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
    args: CountersignArgs,
) -> Result<CallToolResult, McpError> {
    // 1. Validate signature_meaning
    if !ALLOWED_MEANINGS.contains(&args.signature_meaning.as_str()) {
        return Err(invalid_params(format!(
            "signature_meaning must be one of: {}",
            ALLOWED_MEANINGS.join(", ")
        )));
    }

    let signer_id = server.auth_agent_id;

    // 2. Fetch claim content (mirror HTTP route's SQL exactly)
    let claim_row = sqlx::query("SELECT content FROM claims WHERE id = $1")
        .bind(args.claim_id)
        .fetch_optional(&server.pool)
        .await
        .map_err(|e| internal_error(format!("claim lookup: {e}")))?
        .ok_or_else(|| invalid_params(format!("claim {} not found", args.claim_id)))?;
    let content: String = claim_row.get("content");

    // 3. Parse hex-encoded signature and public key
    let sig_bytes: [u8; 64] = hex::decode(&args.signature_hex)
        .map_err(|e| invalid_params(format!("invalid signature hex: {e}")))?
        .try_into()
        .map_err(|_| invalid_params("signature must be 64 bytes (128 hex chars)"))?;

    let pub_bytes: [u8; 32] = hex::decode(&args.public_key_hex)
        .map_err(|e| invalid_params(format!("invalid public key hex: {e}")))?
        .try_into()
        .map_err(|_| invalid_params("public key must be 32 bytes (64 hex chars)"))?;

    // 4. Version-2 canonical message — byte-identical with HTTP route.
    let canonical = format!(
        "{}|{}|{}|{}",
        args.claim_id, signer_id, args.signature_meaning, content
    );
    let content_hash = ContentHasher::hash(canonical.as_bytes());
    let valid = SignatureVerifier::verify(&pub_bytes, canonical.as_bytes(), &sig_bytes)
        .map_err(|e| invalid_params(format!("verification error: {e}")))?;
    if !valid {
        return Err(invalid_params(
            "Ed25519 signature verification failed".to_string(),
        ));
    }

    // 5. Insert row via repository
    let cs = CountersignRepository::create(
        &server.pool,
        args.claim_id,
        signer_id,
        &args.signature_meaning,
        &content_hash,
        &sig_bytes,
        SIGNATURE_VERSION,
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
