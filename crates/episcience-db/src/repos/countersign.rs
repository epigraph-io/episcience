use chrono::{DateTime, Utc};
use epigraph_crypto::ContentHasher;
use episcience_core::Countersignature;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct CountersignRepository;

impl CountersignRepository {
    pub async fn create(
        pool: &PgPool,
        claim_id: Uuid,
        signer_id: Uuid,
        signature_meaning: &str,
        content_hash: &[u8],
        signature: &[u8],
        signature_version: i16,
    ) -> Result<Countersignature, DbError> {
        let id = Uuid::now_v7();

        // Compute prev_signature_hash: hash of the most recent signature for this claim
        let prev_row = sqlx::query(
            "SELECT signature FROM countersignatures WHERE claim_id = $1 ORDER BY created_at DESC LIMIT 1"
        )
        .bind(claim_id)
        .fetch_optional(pool)
        .await?;

        let prev_signature_hash: Option<Vec<u8>> = prev_row.map(|r| {
            let sig: Vec<u8> = r.get("signature");
            ContentHasher::hash(&sig).to_vec()
        });

        let row = sqlx::query(
            r#"
            INSERT INTO countersignatures (id, claim_id, signer_id, signature_meaning,
                content_hash, signature, prev_signature_hash, signature_version, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW())
            RETURNING id, claim_id, signer_id, signature_meaning,
                content_hash, signature, prev_signature_hash, signature_version, created_at
            "#,
        )
        .bind(id)
        .bind(claim_id)
        .bind(signer_id)
        .bind(signature_meaning)
        .bind(content_hash)
        .bind(signature)
        .bind(prev_signature_hash.as_deref())
        .bind(signature_version)
        .fetch_one(pool)
        .await?;

        Ok(row_to_cs(&row))
    }

    pub async fn list_for_claim(
        pool: &PgPool,
        claim_id: Uuid,
    ) -> Result<Vec<Countersignature>, DbError> {
        let rows = sqlx::query(
            r#"
            SELECT id, claim_id, signer_id, signature_meaning,
                content_hash, signature, prev_signature_hash, signature_version, created_at
            FROM countersignatures
            WHERE claim_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(claim_id)
        .fetch_all(pool)
        .await?;

        Ok(rows.iter().map(row_to_cs).collect())
    }
}

fn row_to_cs(row: &sqlx::postgres::PgRow) -> Countersignature {
    Countersignature {
        id: row.get("id"),
        claim_id: row.get("claim_id"),
        signer_id: row.get("signer_id"),
        signature_meaning: row.get("signature_meaning"),
        content_hash: row.get("content_hash"),
        signature: row.get("signature"),
        prev_signature_hash: row.get("prev_signature_hash"),
        signature_version: row.get("signature_version"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
    }
}
