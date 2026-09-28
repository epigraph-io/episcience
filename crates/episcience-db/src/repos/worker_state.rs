use episcience_core::synthesis::WorkerState;
use sqlx::{PgPool, Row};

use crate::errors::DbError;

/// Read-only since E1f: the table's only writer (the retired `belief.updated`
/// event poll) is deleted, and the table is frozen (bypass-only policy, no
/// application privilege). The repository goes in E1h.
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
}
