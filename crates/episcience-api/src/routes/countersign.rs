use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use epigraph_crypto::ContentHasher;
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Countersignature, VerificationResult};
use episcience_db::{CountersignRepository, KernelClaimRepository};

const ALLOWED_MEANINGS: &[&str] = &[
    "witnessed",
    "approved",
    "reviewed",
    "certified",
    "countersigned",
];

#[derive(Deserialize)]
pub struct CountersignRequest {
    pub claim_id: Uuid,
    /// The agent whose key signed. Need not be the caller: the caller
    /// (`countersigned_by`) records an attestation `signer_id` signed, proven
    /// by the signature verifying against `signer_id`'s REGISTERED key.
    pub signer_id: Uuid,
    pub signature_meaning: String,
    pub signature_hex: String,
    /// Optional; when given it must equal the signer's registered key.
    #[serde(default)]
    pub public_key_hex: Option<String>,
}

async fn create_countersignature(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Json(req): Json<CountersignRequest>,
) -> Result<Json<Countersignature>, ApiError> {
    // 1. Validate signature_meaning
    if !ALLOWED_MEANINGS.contains(&req.signature_meaning.as_str()) {
        return Err(ApiError::Validation(format!(
            "signature_meaning must be one of: {}",
            ALLOWED_MEANINGS.join(", ")
        )));
    }

    // 2. Fetch the claim AS the caller: a claim it cannot read is 404,
    //    exactly like an absent one.
    let viewer = crate::auth::viewer::caller_viewer(&state.pool, &auth).await?;
    let (content, claim_visibility, claim_owner) =
        KernelClaimRepository::content_and_pair_as(&state.pool, &viewer, req.claim_id)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("claim {} not found", req.claim_id)))?;

    // 3. Parse the signature (and the optional supplied key).
    let sig_bytes: [u8; 64] = hex::decode(&req.signature_hex)
        .map_err(|e| ApiError::Validation(format!("Invalid signature hex: {e}")))?
        .try_into()
        .map_err(|_| ApiError::Validation("Signature must be 64 bytes (128 hex chars)".into()))?;
    let supplied_key: Option<[u8; 32]> = match &req.public_key_hex {
        None => None,
        Some(h) => Some(
            hex::decode(h)
                .map_err(|e| ApiError::Validation(format!("Invalid public key hex: {e}")))?
                .try_into()
                .map_err(|_| {
                    ApiError::Validation("Public key must be 32 bytes (64 hex chars)".into())
                })?,
        ),
    };

    // 4. Version 2: the signature binds claim_id + signer_id + meaning +
    //    content and must verify against the signer's registered key.
    let content_hash = crate::auth::tenancy::verify_countersignature(
        &state.pool,
        req.claim_id,
        req.signer_id,
        &req.signature_meaning,
        &content,
        &sig_bytes,
        supplied_key.as_ref(),
    )
    .await?;

    // 5. The attestation's owner follows the claim (brief 7.1).
    let owner = crate::auth::tenancy::countersign_ownership(
        &state.pool,
        &viewer,
        &claim_visibility,
        claim_owner,
    )
    .await?;

    // 6. Store: the caller recorded it, the signer signed it.
    let cs = CountersignRepository::create(
        &state.pool,
        req.claim_id,
        req.signer_id,
        auth.agent_id,
        &req.signature_meaning,
        &content_hash,
        &sig_bytes,
        2i16,
        owner,
    )
    .await?;

    Ok(Json(cs))
}

async fn list_countersignatures(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Path(claim_id): Path<Uuid>,
) -> Result<Json<Vec<Countersignature>>, ApiError> {
    // The claim must be readable by the caller; otherwise 404, like an absent
    // claim (its countersignatures would otherwise reveal that it exists).
    let viewer = crate::auth::viewer::caller_viewer(&state.pool, &auth).await?;
    if KernelClaimRepository::content_as(&state.pool, &viewer, claim_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound(format!("claim {claim_id} not found")));
    }
    let sigs = CountersignRepository::list_for_claim(&state.pool, claim_id, &viewer).await?;
    Ok(Json(sigs))
}

async fn verify_countersignatures(
    State(state): State<ElnState>,
    Extension(auth): Extension<crate::middleware::AuthContext>,
    Path(claim_id): Path<Uuid>,
) -> Result<Json<Vec<VerificationResult>>, ApiError> {
    // Fetch claim content AS the caller (invisible == absent == 404).
    let viewer = crate::auth::viewer::caller_viewer(&state.pool, &auth).await?;
    let content = KernelClaimRepository::content_as(&state.pool, &viewer, claim_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("claim {} not found", claim_id)))?;

    let sigs = CountersignRepository::list_for_claim(&state.pool, claim_id, &viewer).await?;

    let mut results = Vec::with_capacity(sigs.len());
    for cs in &sigs {
        // Recompute the canonical hash the same way create did
        let canonical = if cs.signature_version == 2 {
            format!(
                "{}|{}|{}|{}",
                cs.claim_id, cs.signer_id, cs.signature_meaning, content
            )
        } else {
            content.clone()
        };
        let expected_hash = ContentHasher::hash(canonical.as_bytes());
        let content_hash_valid = cs.content_hash == expected_hash;

        // The signer's registered SIGNING key (`key_kind = 'ed25519'`), and
        // the same strict check the create path applies.
        let sig_valid =
            match KernelClaimRepository::agent_public_key(&state.pool, cs.signer_id).await? {
                Some(pk) => match (
                    <[u8; 32]>::try_from(pk.as_slice()),
                    <[u8; 64]>::try_from(cs.signature.as_slice()),
                ) {
                    (Ok(pk_arr), Ok(sig_arr)) => crate::auth::tenancy::verify_ed25519_strict(
                        &pk_arr,
                        canonical.as_bytes(),
                        &sig_arr,
                    ),
                    _ => false,
                },
                None => false,
            };

        results.push(VerificationResult {
            countersignature_id: cs.id,
            claim_id: cs.claim_id,
            signer_id: cs.signer_id,
            signature_meaning: cs.signature_meaning.clone(),
            content_hash_valid,
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
