use chrono::DateTime;
use chrono::Utc;
use episcience_core::synthesis::WorkerState;
use sqlx::{PgPool, Row};

use crate::errors::DbError;

pub struct WorkerStateRepository;

impl WorkerStateRepository {
    pub async fn get(pool: &PgPool, worker_id: &str) -> Result<Option<WorkerState>, DbError> {
        let row = sqlx::query(
            "SELECT worker_id, last_event_id, last_event_ts, updated_at
             FROM episcience_worker_state WHERE worker_id = $1",
        )
        .bind(worker_id)
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|r| WorkerState {
            worker_id: r.get("worker_id"),
            last_event_id: r.get("last_event_id"),
            last_event_ts: r.get("last_event_ts"),
            updated_at: r.get("updated_at"),
        }))
    }

    pub async fn upsert(
        pool: &PgPool,
        worker_id: &str,
        last_event_id: Option<&str>,
        last_event_ts: Option<DateTime<Utc>>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO episcience_worker_state
             (worker_id, last_event_id, last_event_ts, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (worker_id) DO UPDATE
             SET last_event_id = EXCLUDED.last_event_id,
                 last_event_ts = EXCLUDED.last_event_ts,
                 updated_at = now()",
        )
        .bind(worker_id)
        .bind(last_event_id)
        .bind(last_event_ts)
        .execute(pool)
        .await?;
        Ok(())
    }
}
