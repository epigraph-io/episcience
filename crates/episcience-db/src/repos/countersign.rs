use chrono::{DateTime, Utc};
use epigraph_crypto::ContentHasher;
use epigraph_db::Viewer;
use episcience_core::{Countersignature, Ownership, Visibility};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct CountersignRepository;

const CS_COLS: &str = "cs.id, cs.claim_id, cs.signer_id, cs.signature_meaning, cs.content_hash, \
     cs.signature, cs.prev_signature_hash, cs.signature_version, cs.created_at, \
     cs.countersigned_by, cs.owner_group_id, cs.visibility";

impl CountersignRepository {
    /// Append a countersignature.
    ///
    /// `signer_id` holds the key that signed (the caller verified the
    /// signature against that agent's registered key); `countersigned_by` is
    /// the authenticated principal that recorded it. `owner` is the
    /// attestation's pair (see the countersign route for the rules: a public
    /// claim's attestation belongs to the writer's group; a group claim's is
    /// `('group', <the claim's group>)`).
    ///
    /// The chain: `prev_signature_hash` is the hash of the claim's most
    /// recent signature, WHOEVER signed it. Reading the head and appending run
    /// in one transaction under a per-claim advisory lock, so two concurrent
    /// countersignatures of one claim serialise and neither forks the chain.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &PgPool,
        claim_id: Uuid,
        signer_id: Uuid,
        countersigned_by: Uuid,
        signature_meaning: &str,
        content_hash: &[u8],
        signature: &[u8],
        signature_version: i16,
        owner: Ownership,
    ) -> Result<Countersignature, DbError> {
        let id = Uuid::now_v7();
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(claim_id)
            .execute(&mut *tx)
            .await?;

        let prev_row = sqlx::query(
            "SELECT signature FROM countersignatures WHERE claim_id = $1
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(claim_id)
        .fetch_optional(&mut *tx)
        .await?;

        let prev_signature_hash: Option<Vec<u8>> = prev_row.map(|r| {
            let sig: Vec<u8> = r.get("signature");
            ContentHasher::hash(&sig).to_vec()
        });

        let row = sqlx::query(&format!(
            r#"
            INSERT INTO countersignatures AS cs (id, claim_id, signer_id, signature_meaning,
                content_hash, signature, prev_signature_hash, signature_version, created_at,
                countersigned_by, owner_group_id, visibility)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp(), $9, $10, $11)
            RETURNING {CS_COLS}
            "#
        ))
        .bind(id)
        .bind(claim_id)
        .bind(signer_id)
        .bind(signature_meaning)
        .bind(content_hash)
        .bind(signature)
        .bind(prev_signature_hash.as_deref())
        .bind(signature_version)
        .bind(countersigned_by)
        .bind(owner.owner_group_id)
        .bind(owner.visibility.as_str())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(row_to_cs(&row))
    }

    /// The countersignatures of `claim_id` that `viewer` can read, oldest
    /// first. The caller has already checked that the viewer can read the
    /// claim itself.
    pub async fn list_for_claim(
        pool: &PgPool,
        claim_id: Uuid,
        viewer: &Viewer,
    ) -> Result<Vec<Countersignature>, DbError> {
        let sql = viewer.splice(
            &format!(
                "SELECT {CS_COLS} FROM countersignatures cs
                  WHERE cs.claim_id = $1 /* {{VISIBILITY:cs}} */
                  ORDER BY cs.created_at ASC, cs.id ASC"
            ),
            2,
        );
        let mut q = sqlx::query(&sql).bind(claim_id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(pool).await?;
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
        countersigned_by: row
            .try_get::<Option<Uuid>, _>("countersigned_by")
            .ok()
            .flatten(),
        owner_group_id: row
            .try_get::<Option<Uuid>, _>("owner_group_id")
            .ok()
            .flatten(),
        visibility: row
            .try_get::<Option<String>, _>("visibility")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<Visibility>().ok()),
    }
}
