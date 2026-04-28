//! [`JobQueue`] implementation backed by the episcience-local `synthesis_jobs`
//! table.
//!
//! # Schema differences from upstream `PostgresJobQueue`
//!
//! Upstream `epigraph_jobs::PostgresJobQueue` is hardcoded to a `jobs` table
//! with columns `retry_count / max_retries / error_message` and states
//! `pending / running / completed / failed / cancelled`.
//!
//! `synthesis_jobs` (migration `5014_create_synthesis_jobs.sql`) has different
//! column names and states. This impl translates between them:
//!
//! | `Job` field        | `synthesis_jobs` column |
//! |--------------------|-------------------------|
//! | `retry_count`      | `attempts`              |
//! | `max_retries`      | `max_attempts`          |
//! | `error_message`    | `last_error`            |
//!
//! ## State mapping
//!
//! Write (`JobState` → string):
//! - `Pending`   → `'queued'`
//! - `Running`   → `'running'`
//! - `Completed` → `'complete'`
//! - `Failed`    → `'failed'`
//! - `Cancelled` → **rejected** (`synthesis_jobs` has no representation; loud
//!   refusal beats silent lossiness)
//!
//! Read (string → `JobState`):
//! - `'queued'`   → `Pending`
//! - `'retry'`    → `Pending` (also dequeue-eligible; "ready to run again")
//! - `'running'`  → `Running`
//! - `'complete'` → `Completed`
//! - `'failed'`   → `Failed`
//!
//! This impl never *writes* `'retry'`. The synthesis worker, on transient
//! failure, calls `update` with `state = Pending`, which serialises as
//! `'queued'`. If a future feature wants `'retry'` as a distinct DB-only state,
//! we can add it then.
//!
//! # Foreign-key constraint on `id`
//!
//! `synthesis_jobs.id REFERENCES syntheses(id) ON DELETE CASCADE`. Callers
//! **must** construct each `Job` with `id = JobId::from_uuid(synthesis_id)`
//! where the synthesis row already exists. A `Job::new()`-generated random
//! UUID will fail to insert with a foreign-key violation.
//!
//! # Dequeue semantics
//!
//! `dequeue` increments `attempts` (i.e. "times dequeued"), unlike upstream
//! `PostgresJobQueue` which leaves `retry_count` alone in dequeue. So a freshly
//! enqueued job, after one dequeue, returns with `retry_count == 1`.

use chrono::{DateTime, Utc};
use epigraph_jobs::{async_trait, Job, JobError, JobId, JobQueue, JobState};
use sqlx::{PgPool, Row};
use tracing::instrument;
use uuid::Uuid;

/// `JobQueue` impl over the episcience-local `synthesis_jobs` table.
///
/// See module docs for the column / state mapping and the FK invariant on the
/// `id` column.
#[derive(Clone)]
pub struct EpiscienceJobQueue {
    pool: PgPool,
}

impl EpiscienceJobQueue {
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// ─── State <-> string ──────────────────────────────────────────────────────

/// Convert `JobState` to its `synthesis_jobs.state` representation.
///
/// Returns `None` for `Cancelled`: `synthesis_jobs` has no cancellation column,
/// so we refuse to silently coerce. Callers see `JobError::ProcessingFailed`.
const fn job_state_to_db(state: JobState) -> Option<&'static str> {
    match state {
        JobState::Pending => Some("queued"),
        JobState::Running => Some("running"),
        JobState::Completed => Some("complete"),
        JobState::Failed => Some("failed"),
        JobState::Cancelled => None,
    }
}

fn db_state_to_job(s: &str) -> Result<JobState, JobError> {
    match s {
        // Both 'queued' and 'retry' are dequeue-eligible / "ready to run".
        "queued" | "retry" => Ok(JobState::Pending),
        "running" => Ok(JobState::Running),
        "complete" => Ok(JobState::Completed),
        "failed" => Ok(JobState::Failed),
        other => Err(JobError::ProcessingFailed {
            message: format!("invalid synthesis_jobs.state value: {other}"),
        }),
    }
}

// ─── Row → Job ─────────────────────────────────────────────────────────────

fn job_from_row(row: &sqlx::postgres::PgRow) -> Result<Job, JobError> {
    let id: Uuid = row.try_get("id").map_err(map_field_err("id"))?;
    let job_type: String = row.try_get("job_type").map_err(map_field_err("job_type"))?;
    let payload: serde_json::Value = row.try_get("payload").map_err(map_field_err("payload"))?;
    let state_str: String = row.try_get("state").map_err(map_field_err("state"))?;
    let state = db_state_to_job(&state_str)?;

    let attempts: i32 = row.try_get("attempts").map_err(map_field_err("attempts"))?;
    let max_attempts: i32 = row
        .try_get("max_attempts")
        .map_err(map_field_err("max_attempts"))?;

    let created_at: DateTime<Utc> = row
        .try_get("created_at")
        .map_err(map_field_err("created_at"))?;
    let updated_at: DateTime<Utc> = row
        .try_get("updated_at")
        .map_err(map_field_err("updated_at"))?;

    let started_at: Option<DateTime<Utc>> = row.try_get("started_at").ok().flatten();
    let completed_at: Option<DateTime<Utc>> = row.try_get("completed_at").ok().flatten();
    let last_error: Option<String> = row.try_get("last_error").ok().flatten();

    Ok(Job {
        id: JobId::from_uuid(id),
        job_type,
        payload,
        state,
        retry_count: attempts.max(0) as u32,
        max_retries: max_attempts.max(0) as u32,
        created_at,
        updated_at,
        started_at,
        completed_at,
        error_message: last_error,
    })
}

fn map_field_err(field: &'static str) -> impl Fn(sqlx::Error) -> JobError {
    move |e| JobError::ProcessingFailed {
        message: format!("failed to read synthesis_jobs.{field}: {e}"),
    }
}

// ─── JobQueue impl ─────────────────────────────────────────────────────────

#[async_trait]
impl JobQueue for EpiscienceJobQueue {
    /// Enqueue a synthesis job.
    ///
    /// **Precondition:** `job.id.as_uuid()` must reference an existing
    /// `syntheses.id` row (FK constraint). Otherwise the insert fails with a
    /// foreign-key violation surfaced as `JobError::ProcessingFailed`.
    #[instrument(skip(self, job), fields(job_id = %job.id, job_type = %job.job_type))]
    async fn enqueue(&self, job: Job) -> Result<JobId, JobError> {
        let id: Uuid = job.id.into();
        let state_str = job_state_to_db(job.state).ok_or_else(|| JobError::ProcessingFailed {
            message: "synthesis_jobs has no representation for JobState::Cancelled".into(),
        })?;

        sqlx::query(
            r"
            INSERT INTO synthesis_jobs (
                id, job_type, payload, state,
                attempts, max_attempts,
                scheduled_at, started_at, completed_at, last_error,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (id) DO UPDATE SET
                state         = EXCLUDED.state,
                attempts      = EXCLUDED.attempts,
                max_attempts  = EXCLUDED.max_attempts,
                started_at    = EXCLUDED.started_at,
                completed_at  = EXCLUDED.completed_at,
                last_error    = EXCLUDED.last_error,
                updated_at    = EXCLUDED.updated_at
            ",
        )
        .bind(id)
        .bind(&job.job_type)
        .bind(&job.payload)
        .bind(state_str)
        .bind(job.retry_count as i32)
        .bind(job.max_retries as i32)
        .bind(job.created_at) // scheduled_at — run-at-or-after; default to created_at
        .bind(job.started_at)
        .bind(job.completed_at)
        .bind(&job.error_message)
        .bind(job.created_at)
        .bind(job.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| JobError::ProcessingFailed {
            message: format!("failed to enqueue synthesis job: {e}"),
        })?;

        tracing::debug!(job_id = %job.id, "synthesis job enqueued");
        Ok(job.id)
    }

    /// Atomically claim the next runnable synthesis job.
    ///
    /// Selects the earliest-scheduled `'queued'` or `'retry'` row whose
    /// `scheduled_at <= now()`, locks it via `FOR UPDATE SKIP LOCKED`, and
    /// flips it to `'running'` while incrementing `attempts` in a single CTE.
    #[instrument(skip(self))]
    async fn dequeue(&self) -> Option<Job> {
        let result = sqlx::query(
            r"
            WITH next_job AS (
                SELECT id
                FROM synthesis_jobs
                WHERE state IN ('queued', 'retry')
                  AND scheduled_at <= now()
                ORDER BY scheduled_at ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE synthesis_jobs
            SET state       = 'running',
                started_at  = now(),
                attempts    = synthesis_jobs.attempts + 1,
                updated_at  = now()
            FROM next_job
            WHERE synthesis_jobs.id = next_job.id
            RETURNING synthesis_jobs.id,
                      synthesis_jobs.job_type,
                      synthesis_jobs.payload,
                      synthesis_jobs.state,
                      synthesis_jobs.attempts,
                      synthesis_jobs.max_attempts,
                      synthesis_jobs.created_at,
                      synthesis_jobs.updated_at,
                      synthesis_jobs.started_at,
                      synthesis_jobs.completed_at,
                      synthesis_jobs.last_error
            ",
        )
        .fetch_optional(&self.pool)
        .await;

        match result {
            Ok(Some(row)) => match job_from_row(&row) {
                Ok(job) => {
                    tracing::debug!(job_id = %job.id, job_type = %job.job_type, "synthesis job dequeued");
                    Some(job)
                }
                Err(e) => {
                    tracing::error!("failed to parse synthesis_jobs row: {e}");
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::error!("failed to dequeue synthesis job: {e}");
                None
            }
        }
    }

    /// Update mutable fields of a synthesis job: state, attempts, last_error,
    /// started_at, completed_at, updated_at.
    #[instrument(skip(self, job), fields(job_id = %job.id, new_state = %job.state))]
    async fn update(&self, job: &Job) -> Result<(), JobError> {
        let id: Uuid = job.id.into();
        let state_str = job_state_to_db(job.state).ok_or_else(|| JobError::ProcessingFailed {
            message: "synthesis_jobs has no representation for JobState::Cancelled".into(),
        })?;

        let result = sqlx::query(
            r"
            UPDATE synthesis_jobs
            SET state        = $2,
                attempts     = $3,
                last_error   = $4,
                started_at   = $5,
                completed_at = $6,
                updated_at   = $7
            WHERE id = $1
            ",
        )
        .bind(id)
        .bind(state_str)
        .bind(job.retry_count as i32)
        .bind(&job.error_message)
        .bind(job.started_at)
        .bind(job.completed_at)
        .bind(job.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| JobError::ProcessingFailed {
            message: format!("failed to update synthesis job: {e}"),
        })?;

        if result.rows_affected() == 0 {
            tracing::warn!(job_id = %job.id, "synthesis job not found for update");
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get(&self, id: JobId) -> Option<Job> {
        let uuid: Uuid = id.into();
        let result = sqlx::query(
            r"
            SELECT id, job_type, payload, state, attempts, max_attempts,
                   created_at, updated_at, started_at, completed_at, last_error
            FROM synthesis_jobs
            WHERE id = $1
            ",
        )
        .bind(uuid)
        .fetch_optional(&self.pool)
        .await;

        match result {
            Ok(Some(row)) => match job_from_row(&row) {
                Ok(job) => Some(job),
                Err(e) => {
                    tracing::error!("failed to parse synthesis_jobs row: {e}");
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::error!("failed to get synthesis job {id}: {e}");
                None
            }
        }
    }

    /// All `'queued'` and `'retry'` synthesis jobs in FIFO order by
    /// `scheduled_at`.
    #[instrument(skip(self))]
    async fn pending_jobs(&self) -> Vec<Job> {
        let result = sqlx::query(
            r"
            SELECT id, job_type, payload, state, attempts, max_attempts,
                   created_at, updated_at, started_at, completed_at, last_error
            FROM synthesis_jobs
            WHERE state IN ('queued', 'retry')
            ORDER BY scheduled_at ASC
            ",
        )
        .fetch_all(&self.pool)
        .await;

        match result {
            Ok(rows) => {
                let mut jobs = Vec::with_capacity(rows.len());
                for row in rows {
                    match job_from_row(&row) {
                        Ok(j) => jobs.push(j),
                        Err(e) => tracing::error!("failed to parse synthesis_jobs row: {e}"),
                    }
                }
                jobs
            }
            Err(e) => {
                tracing::error!("failed to list pending synthesis jobs: {e}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip_through_db_strings() {
        for state in [
            JobState::Pending,
            JobState::Running,
            JobState::Completed,
            JobState::Failed,
        ] {
            let s = job_state_to_db(state).expect("non-cancelled state has db repr");
            let parsed = db_state_to_job(s).expect("db string parses");
            assert_eq!(parsed, state, "round-trip failed for {state:?}");
        }
    }

    #[test]
    fn cancelled_has_no_db_representation() {
        assert!(job_state_to_db(JobState::Cancelled).is_none());
    }

    #[test]
    fn retry_db_string_parses_as_pending() {
        assert_eq!(db_state_to_job("retry").unwrap(), JobState::Pending);
    }

    #[test]
    fn unknown_db_string_errors() {
        let err = db_state_to_job("bogus").unwrap_err();
        match err {
            JobError::ProcessingFailed { message } => {
                assert!(message.contains("bogus"), "msg = {message}");
            }
            _ => panic!("unexpected error variant"),
        }
    }
}
