use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisMembershipRepository;

impl SynthesisMembershipRepository {
    /// Replaces the full membership set for a synthesis in a single transaction.
    /// Deletes existing rows, then bulk-inserts the new claim_ids.
    pub async fn replace_for_synthesis(
        tx: &mut Transaction<'_, Postgres>,
        synthesis_id: Uuid,
        claim_ids: &[Uuid],
    ) -> Result<(), DbError> {
        // Delete existing membership
        sqlx::query(
            "DELETE FROM synthesis_claim_membership WHERE synthesis_id = $1",
        )
        .bind(synthesis_id)
        .execute(&mut **tx)
        .await?;

        // Insert new members
        for &claim_id in claim_ids {
            sqlx::query(
                "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id)
                 VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(synthesis_id)
            .bind(claim_id)
            .execute(&mut **tx)
            .await?;
        }

        Ok(())
    }

    /// Returns synthesis IDs that cite the given claim.
    /// If `only_complete_non_stale` is true, filters to complete and non-stale syntheses.
    pub async fn syntheses_citing(
        pool: &PgPool,
        claim_id: Uuid,
        only_complete_non_stale: bool,
    ) -> Result<Vec<Uuid>, DbError> {
        let rows = sqlx::query(
            "SELECT m.synthesis_id
             FROM synthesis_claim_membership m
             JOIN syntheses s ON s.id = m.synthesis_id
             WHERE m.claim_id = $1
               AND (NOT $2 OR (s.status = 'complete' AND s.stale_since IS NULL))
             ORDER BY m.synthesis_id",
        )
        .bind(claim_id)
        .bind(only_complete_non_stale)
        .fetch_all(pool)
        .await?;

        Ok(rows.iter().map(|r| r.get("synthesis_id")).collect())
    }
}
