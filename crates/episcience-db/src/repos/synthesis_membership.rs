use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisMembershipRepository;

impl SynthesisMembershipRepository {
    /// Replaces the full membership set for a synthesis in a single transaction.
    /// Deletes existing rows, then bulk-inserts the new claim_ids.
    ///
    /// The claim-attach rule (5035's `tenancy_20_claim_guard`, applied here
    /// as well so that it holds before that guard exists, in the deploy
    /// window at 5034): a non-public claim is a member only of a synthesis
    /// owned by the claim's own group. A violation refuses the WHOLE set
    /// before anything is written, with the guard's own words, exactly as
    /// the guard fails the statement from 5035 on. (Narrowing a PUBLIC
    /// synthesis that takes its own group's non-public claim is the guard
    /// side only; in the window it waits for 5035's data step.)
    pub async fn replace_for_synthesis(
        tx: &mut Transaction<'_, Postgres>,
        synthesis_id: Uuid,
        claim_ids: &[Uuid],
    ) -> Result<(), DbError> {
        let foreign: Option<Uuid> = sqlx::query_scalar(
            "SELECT c.id
               FROM unnest($2::uuid[]) x(id)
               JOIN claims c ON c.id = x.id
               JOIN syntheses s ON s.id = $1
              WHERE c.visibility::text <> 'public'
                AND c.owner_group_id IS DISTINCT FROM s.owner_group_id
              LIMIT 1",
        )
        .bind(synthesis_id)
        .bind(claim_ids)
        .fetch_optional(&mut **tx)
        .await?;
        if foreign.is_some() {
            return Err(DbError::TenancyRefused(
                "a group claim attaches only to a row owned by the claim's group".into(),
            ));
        }

        // Delete existing membership
        sqlx::query("DELETE FROM synthesis_claim_membership WHERE synthesis_id = $1")
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
