use episcience_core::synthesis::StalenessEvent;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisStalenessRepository;

impl SynthesisStalenessRepository {
    pub async fn record_event(
        pool: &PgPool,
        synthesis_id: Uuid,
        trigger: &str,
        affected_claims: &[Uuid],
        detail: Option<&serde_json::Value>,
    ) -> Result<(), DbError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO synthesis_staleness_events
             (id, synthesis_id, trigger, affected_claim_ids, detail)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(synthesis_id)
        .bind(trigger)
        .bind(affected_claims)
        .bind(detail)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn list_for_synthesis(
        pool: &PgPool,
        synthesis_id: Uuid,
    ) -> Result<Vec<StalenessEvent>, DbError> {
        let rows = sqlx::query(
            "SELECT id, synthesis_id, detected_at, trigger, affected_claim_ids, detail
             FROM synthesis_staleness_events
             WHERE synthesis_id = $1
             ORDER BY detected_at DESC",
        )
        .bind(synthesis_id)
        .fetch_all(pool)
        .await?;

        rows.iter()
            .map(|r| {
                Ok(StalenessEvent {
                    id: r.get("id"),
                    synthesis_id: r.get("synthesis_id"),
                    detected_at: r.get("detected_at"),
                    trigger: r.get("trigger"),
                    affected_claim_ids: r.get("affected_claim_ids"),
                    detail: r.get("detail"),
                })
            })
            .collect()
    }
}
