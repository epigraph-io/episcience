//! `list_countersignatures` MCP tool — mirrors
//! `GET /api/v1/eln/claims/{claim_id}/countersignatures`.
//!
//! Phase 8 review-bot read path. The bot needs to check whether an
//! `approved` countersignature already exists for a claim before scoring
//! it as "unreviewed", and to look at signer/meaning when verifying a
//! merge gate. The HTTP route returns raw byte arrays (`Vec<u8>`)
//! serialised as JSON arrays of `u8`; that wire shape is awkward for MCP
//! consumers, so this tool hex-encodes `content_hash`, `signature`, and
//! the signer's `public_key` (looked up from the `agents` table). The
//! HTTP route is intentionally left unchanged to avoid breaking
//! Phase 3 / Phase 8 HTTP clients.
//!
//! No auth gate beyond the per-call MCP `auth_agent_id`: countersignatures
//! are conceptually public attestations and the HTTP route is also
//! ungated. If a private-countersignature predicate is ever introduced,
//! it should be enforced inside the repo / route uniformly, not duplicated
//! here.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use episcience_db::CountersignRepository;

use crate::mcp::errors::{internal_error, McpError};
use crate::mcp::EpiscienceServer;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCountersignaturesArgs {
    /// Target claim id. Returns every countersignature row whose
    /// `claim_id` matches, ordered by `created_at ASC` (oldest first).
    #[schemars(description = "Claim id whose countersignatures to list")]
    pub claim_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct CountersignatureView {
    pub id: Uuid,
    pub claim_id: Uuid,
    pub signer_id: Uuid,
    pub signature_meaning: String,
    pub content_hash_hex: String,
    pub signature_hex: String,
    /// Hex-encoded Ed25519 public key of the signer, looked up from the
    /// `agents` table. `None` only if the signer agent row has been
    /// hard-deleted — normal flows always populate this.
    pub public_key_hex: Option<String>,
    pub signature_version: i16,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn handle(
    server: &EpiscienceServer,
    args: ListCountersignaturesArgs,
) -> Result<CallToolResult, McpError> {
    let sigs = CountersignRepository::list_for_claim(&server.pool, args.claim_id)
        .await
        .map_err(|e| internal_error(format!("list_for_claim: {e}")))?;

    let mut out = Vec::with_capacity(sigs.len());
    for cs in sigs {
        // Look up the signer's public key. One query per row is fine —
        // the review-bot expects O(few) signatures per claim. A join in
        // the repo would couple `countersignatures` to `agents`, which we
        // avoid until the schema settles.
        let pub_row = sqlx::query("SELECT public_key FROM agents WHERE id = $1")
            .bind(cs.signer_id)
            .fetch_optional(&server.pool)
            .await
            .map_err(|e| internal_error(format!("signer public_key lookup: {e}")))?;
        let public_key_hex = pub_row.map(|r| {
            let bytes: Vec<u8> = r.get("public_key");
            hex::encode(bytes)
        });

        out.push(CountersignatureView {
            id: cs.id,
            claim_id: cs.claim_id,
            signer_id: cs.signer_id,
            signature_meaning: cs.signature_meaning,
            content_hash_hex: hex::encode(&cs.content_hash),
            signature_hex: hex::encode(&cs.signature),
            public_key_hex,
            signature_version: cs.signature_version,
            created_at: cs.created_at,
        });
    }

    let body = serde_json::to_string_pretty(&out).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
