//! Repository for the `synthesis_jobs` queue table.
//!
//! `synthesis_jobs.id` is `REFERENCES syntheses(id) ON DELETE CASCADE`, so
//! enqueueing reuses the synthesis id as the job id (no separate
//! `synthesis_id` column exists in the table — see migration 5014).
//!
//! The runtime job machinery
//! ([`crate::jobs::EpiscienceJobQueue`](../../episcience-api/src/jobs/episcience_job_queue.rs))
//! consumes these rows; this repo just provides a transaction-aware enqueue
//! helper for the Phase-3 REST handler so the synthesis row and its job row
//! are inserted in one atomic step.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::errors::DbError;

pub struct SynthesisJobsRepository;

impl SynthesisJobsRepository {
    /// Enqueue a synthesis job in the same transaction that creates the
    /// `syntheses` row.
    ///
    /// The `synthesis_id` is reused as the `synthesis_jobs.id` (the FK
    /// constraint forces this — see migration 5014). `ON CONFLICT (id) DO
    /// NOTHING` makes the call idempotent against retries of the same
    /// synthesis id.
    ///
    /// `payload` is taken as a `serde_json::Value` to avoid coupling the db
    /// crate to the API crate's `SynthesisJobPayload` type. The route
    /// serialises before calling.
    pub async fn enqueue_tx(
        tx: &mut Transaction<'_, Postgres>,
        synthesis_id: Uuid,
        payload: &serde_json::Value,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO synthesis_jobs (id, job_type, payload, state, max_attempts)
             VALUES ($1, 'synthesis', $2, 'queued', 3)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(synthesis_id)
        .bind(payload)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
