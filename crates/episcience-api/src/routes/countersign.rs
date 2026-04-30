use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use epigraph_crypto::{ContentHasher, SignatureVerifier};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;
use episcience_core::{Countersignature, VerificationResult};
use episcience_db::CountersignRepository;

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
    pub signer_id: Uuid,
    pub signature_meaning: String,
    pub signature_hex: String,
    pub public_key_hex: String,
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
    if auth.agent_id != req.signer_id {
        return Err(ApiError::Forbidden("agent mismatch".into()));
    }

    // 2. Fetch claim content
    let claim_row = sqlx::query("SELECT content FROM claims WHERE id = $1")
        .bind(req.claim_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("claim {} not found", req.claim_id)))?;

    let content: String = claim_row.get("content");

    // 3. Parse hex-encoded signature and public key
    let sig_bytes: [u8; 64] = hex::decode(&req.signature_hex)
        .map_err(|e| ApiError::Validation(format!("Invalid signature hex: {e}")))?
        .try_into()
        .map_err(|_| ApiError::Validation("Signature must be 64 bytes (128 hex chars)".into()))?;

    let pub_bytes: [u8; 32] = hex::decode(&req.public_key_hex)
        .map_err(|e| ApiError::Validation(format!("Invalid public key hex: {e}")))?
        .try_into()
        .map_err(|_| ApiError::Validation("Public key must be 32 bytes (64 hex chars)".into()))?;

    // 4. Version 2: signature binds claim_id + signer_id + meaning + content
    let canonical = format!(
        "{}|{}|{}|{}",
        req.claim_id, req.signer_id, req.signature_meaning, content
    );
    let content_hash = ContentHasher::hash(canonical.as_bytes());
    let valid = SignatureVerifier::verify(&pub_bytes, canonical.as_bytes(), &sig_bytes)
        .map_err(|e| ApiError::Validation(format!("Verification error: {e}")))?;

    // 6. Reject invalid signatures
    if !valid {
        return Err(ApiError::Validation(
            "Ed25519 signature verification failed".into(),
        ));
    }

    // 7. Store via repository
    let cs = CountersignRepository::create(
        &state.pool,
        req.claim_id,
        req.signer_id,
        &req.signature_meaning,
        &content_hash,
        &sig_bytes,
        2i16,
    )
    .await?;

    Ok(Json(cs))
}

async fn list_countersignatures(
    State(state): State<ElnState>,
    Path(claim_id): Path<Uuid>,
) -> Result<Json<Vec<Countersignature>>, ApiError> {
    let sigs = CountersignRepository::list_for_claim(&state.pool, claim_id).await?;
    Ok(Json(sigs))
}

async fn verify_countersignatures(
    State(state): State<ElnState>,
    Path(claim_id): Path<Uuid>,
) -> Result<Json<Vec<VerificationResult>>, ApiError> {
    // Fetch claim content
    let claim_row = sqlx::query("SELECT content FROM claims WHERE id = $1")
        .bind(claim_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("claim {} not found", claim_id)))?;

    let content: String = claim_row.get("content");

    let sigs = CountersignRepository::list_for_claim(&state.pool, claim_id).await?;

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

        // Look up signer's public key from agents table
        let sig_valid = match sqlx::query("SELECT public_key FROM agents WHERE id = $1")
            .bind(cs.signer_id)
            .fetch_optional(&state.pool)
            .await
        {
            Ok(Some(agent_row)) => {
                let pk_bytes_vec: Vec<u8> = agent_row.get("public_key");
                if let Ok(pk_arr) = <[u8; 32]>::try_from(pk_bytes_vec.as_slice()) {
                    if let Ok(sig_arr) = <[u8; 64]>::try_from(cs.signature.as_slice()) {
                        let msg = if cs.signature_version == 2 {
                            format!(
                                "{}|{}|{}|{}",
                                cs.claim_id, cs.signer_id, cs.signature_meaning, content
                            )
                        } else {
                            content.clone()
                        };
                        SignatureVerifier::verify(&pk_arr, msg.as_bytes(), &sig_arr)
                            .unwrap_or(false)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            _ => false,
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
