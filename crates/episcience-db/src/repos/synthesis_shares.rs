use chrono::DateTime;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::errors::DbError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub synthesis_id: Uuid,
    pub shared_with_agent_id: Uuid,
    pub shared_by_agent_id: Uuid,
    pub granted_at: DateTime<Utc>,
    pub permission: String,
}

pub struct SynthesisSharesRepository;

impl SynthesisSharesRepository {
    pub async fn grant(
        pool: &PgPool,
        synthesis_id: Uuid,
        recipient: Uuid,
        granted_by: Uuid,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO synthesis_shares
             (synthesis_id, shared_with_agent_id, shared_by_agent_id, permission)
             VALUES ($1, $2, $3, 'read')
             ON CONFLICT (synthesis_id, shared_with_agent_id) DO NOTHING",
        )
        .bind(synthesis_id)
        .bind(recipient)
        .bind(granted_by)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn revoke(pool: &PgPool, synthesis_id: Uuid, recipient: Uuid) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM synthesis_shares
             WHERE synthesis_id = $1 AND shared_with_agent_id = $2",
        )
        .bind(synthesis_id)
        .bind(recipient)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn list(pool: &PgPool, synthesis_id: Uuid) -> Result<Vec<Share>, DbError> {
        let rows = sqlx::query(
            "SELECT synthesis_id, shared_with_agent_id, shared_by_agent_id, granted_at, permission
             FROM synthesis_shares WHERE synthesis_id = $1
             ORDER BY granted_at",
        )
        .bind(synthesis_id)
        .fetch_all(pool)
        .await?;

        rows.iter()
            .map(|r| {
                Ok(Share {
                    synthesis_id: r.get("synthesis_id"),
                    shared_with_agent_id: r.get("shared_with_agent_id"),
                    shared_by_agent_id: r.get("shared_by_agent_id"),
                    granted_at: r.get("granted_at"),
                    permission: r.get("permission"),
                })
            })
            .collect()
    }
}
