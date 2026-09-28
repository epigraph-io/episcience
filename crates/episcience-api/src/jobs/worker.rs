//! The `episcience-worker` loop (E1f).
//!
//! A plain loop over the queue definers, not `epigraph_jobs::JobRunner`: the
//! runner re-enqueues retries through `JobQueue::enqueue` (an INSERT … ON
//! CONFLICT DO UPDATE), and after row security the application role has no
//! UPDATE on `synthesis_jobs`. Every queue transition goes through a
//! maintenance-owned definer the worker login may EXECUTE
//! (`episcience_queue_claim` / `_finish` / `_retry`), and no job row is ever
//! created for an existing synthesis: rechecks and stage-6 retries come from
//! `episcience_owner_worklist`, which returns `(synthesis, principal)` pairs
//! only.
//!
//! Per job:
//! 1. claim (the definer marks it `running`, one more attempt);
//! 2. older than [`JOB_MAX_AGE`] → `failed: expired`, never run;
//! 3. authorize the queue row's `principal_id` ([`Worker::authorize`]):
//!    unresolvable → `failed: authority: …`; any operator link →
//!    `failed: principal_operated` (kernel parity: the kernel refuses that
//!    principal's tokens); no writable group → `failed: authority: …`. Nothing
//!    is written for a refused job;
//! 4. run every stage stamped as that principal
//!    ([`SynthesisJobHandler::run`] on a [`StageSession::Owner`]); a stage
//!    refused for authority ends the job `failed: authority: …` and is never
//!    retried; any other failure is retried until the row's `max_attempts`,
//!    then `failed`;
//! 5. `complete`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use epigraph_db::{AgentRepository, ScopedPool, Viewer};
use episcience_db::synthesis::{publish, staleness};
use episcience_db::{SynthesisRepository, SynthesisStalenessRepository};
use sqlx::PgPool;
use uuid::Uuid;

use crate::jobs::session::{OwnerSession, StageSession};
use crate::jobs::synthesis_job::{RunError, SynthesisJobHandler, SynthesisJobPayload};

/// Jobs older than this are failed unrun (`expired`): a principal whose
/// client was suspended keeps its queued jobs only this long.
pub const JOB_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// How often the owner worklist runs.
pub const WORKLIST_PERIOD: Duration = Duration::from_secs(60);

/// Items per worklist kind per period.
pub const WORKLIST_LIMIT: i32 = 50;

/// The failure reason for an expired job.
pub const REASON_EXPIRED: &str = "expired";

/// The failure reason for a principal with an operator link.
pub const REASON_OPERATED: &str = "principal_operated";

/// One job, as the claim definer returns it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedJob {
    pub job_id: Uuid,
    pub synthesis_id: Uuid,
    pub principal_id: Uuid,
    pub job_type: String,
    pub payload: serde_json::Value,
    pub attempts: i32,
    pub created_at: DateTime<Utc>,
}

/// What one [`Worker::run_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// No job was due.
    Idle,
    /// The job ran to its end (`complete`; a rejected synthesis is a
    /// complete job).
    Completed(Uuid),
    /// The job was finished `failed` with this reason.
    Failed { job: Uuid, reason: String },
    /// The job went back to the queue with this error.
    Retried { job: Uuid, reason: String },
}

/// What one worklist period did (counts, for the log and the tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorklistReport {
    /// `stage6_pending` items whose edges were all written.
    pub edges_written: usize,
    /// `staleness_check` items rechecked (`staleness_checked_at` advanced).
    pub rechecked: usize,
    /// Of those, the ones marked stale.
    pub marked_stale: usize,
    /// Items skipped (refused authority, a failure; retried next period).
    pub skipped: usize,
}

/// A refusal to act as a job's principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The principal has an operator link (`principal_operated`).
    Operated,
    /// Anything else, with the reason (`authority: …`).
    Authority(String),
}

impl Refusal {
    /// The `synthesis_jobs.last_error` text for this refusal.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Operated => REASON_OPERATED.to_string(),
            Self::Authority(m) => format!("authority: {m}"),
        }
    }
}

/// The worker. See the module documentation.
#[derive(Clone)]
pub struct Worker {
    /// The name the claim definer logs.
    pub name: String,
    /// The stamped pool every stage transaction comes from.
    pub scoped: Arc<ScopedPool>,
    /// `RESOLVE_POOL`: unstamped, same login. Viewer resolution, the parity
    /// check and the queue/worklist definers (all definers or kernel-parity
    /// resolve reads; none reads an EpiScience table under row security).
    pub resolve_pool: PgPool,
    /// The handler; its `pool` is `ENGINE_POOL` (unstamped, same login).
    pub handler: SynthesisJobHandler,
    /// A failed attempt's delay before the job is due again (times the
    /// attempts used).
    pub retry_delay: Duration,
}

impl Worker {
    /// Resolve `principal` and refuse to act as it unless it has no operator
    /// link and may write at least one group. Kernel parity: the kernel
    /// refuses the tokens of an agent that has an operator.
    ///
    /// # Errors
    /// The [`Refusal`]. A database failure on the parity read is an
    /// authority refusal too (fail closed); the job is then failed, not run.
    pub async fn authorize(&self, principal: Uuid) -> Result<Viewer, Refusal> {
        let viewer = Viewer::resolve(&self.resolve_pool, principal)
            .await
            .map_err(|e| Refusal::Authority(format!("the principal cannot be resolved: {e}")))?;
        let mut conn = self
            .resolve_pool
            .acquire()
            .await
            .map_err(|e| Refusal::Authority(format!("parity check unavailable: {e}")))?;
        let operated = AgentRepository::operator_of_author(&mut conn, principal)
            .await
            .map_err(|e| Refusal::Authority(format!("parity check failed: {e}")))?;
        if operated.is_some() {
            return Err(Refusal::Operated);
        }
        if viewer.writable_groups().is_empty() {
            return Err(Refusal::Authority(
                "the principal may write no group".into(),
            ));
        }
        Ok(viewer)
    }

    fn session(&self, principal: Uuid, synthesis_id: Uuid) -> StageSession {
        StageSession::Owner(OwnerSession {
            scoped: self.scoped.clone(),
            resolve_pool: self.resolve_pool.clone(),
            principal,
            synthesis_id,
        })
    }

    async fn finish(&self, job: Uuid, state: &str, error: Option<&str>) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT public.episcience_queue_finish($1, $2, $3)")
            .bind(job)
            .bind(state)
            .bind(error)
            .execute(&self.resolve_pool)
            .await
            .map(|_| ())
    }

    async fn failed(&self, job: Uuid, reason: String) -> Result<JobOutcome, sqlx::Error> {
        self.finish(job, "failed", Some(&reason)).await?;
        tracing::warn!(%job, %reason, "synthesis job failed");
        Ok(JobOutcome::Failed { job, reason })
    }

    /// Claim the next due job and run it to one of its ends.
    ///
    /// # Errors
    /// A database failure on a queue definer (the claim, finish or retry
    /// itself); the job then stays `running` until an operator acts, which
    /// is what the definers' state machine reports.
    pub async fn run_once(&self) -> Result<JobOutcome, sqlx::Error> {
        let claimed: Option<ClaimedJob> = sqlx::query_as(
            "SELECT job_id, synthesis_id, principal_id, job_type, payload, attempts, created_at \
               FROM public.episcience_queue_claim($1)",
        )
        .bind(&self.name)
        .fetch_optional(&self.resolve_pool)
        .await?;
        let Some(job) = claimed else {
            return Ok(JobOutcome::Idle);
        };

        if Utc::now() - job.created_at > JOB_MAX_AGE {
            return self.failed(job.job_id, REASON_EXPIRED.into()).await;
        }
        if job.job_type != "synthesis" {
            return self
                .failed(job.job_id, format!("unknown job type {}", job.job_type))
                .await;
        }
        if let Err(r) = self.authorize(job.principal_id).await {
            return self.failed(job.job_id, r.reason()).await;
        }
        let payload: SynthesisJobPayload = match serde_json::from_value(job.payload.clone()) {
            Ok(p) => p,
            Err(e) => {
                return self
                    .failed(job.job_id, format!("invalid synthesis payload: {e}"))
                    .await
            }
        };
        if payload.synthesis_id != job.synthesis_id {
            return self
                .failed(
                    job.job_id,
                    "invalid synthesis payload: it names another synthesis".into(),
                )
                .await;
        }

        let session = self.session(job.principal_id, job.synthesis_id);
        match self.handler.run(&session, payload, job.principal_id).await {
            Ok(_) => {
                self.finish(job.job_id, "complete", None).await?;
                Ok(JobOutcome::Completed(job.job_id))
            }
            Err(RunError::Authority(m)) => self.failed(job.job_id, format!("authority: {m}")).await,
            Err(RunError::Failed(e)) => {
                let reason = e.to_string();
                if matches!(e, epigraph_jobs::JobError::PayloadError { .. }) {
                    return self.failed(job.job_id, reason).await;
                }
                let delay = self
                    .retry_delay
                    .saturating_mul(u32::try_from(job.attempts.max(1)).unwrap_or(1));
                let delay_secs = i64::try_from(delay.as_secs()).unwrap_or(i64::MAX);
                let retried = sqlx::query(
                    "SELECT public.episcience_queue_retry($1, make_interval(secs => $2::double precision), $3)",
                )
                .bind(job.job_id)
                .bind(delay_secs as f64)
                .bind(&reason)
                .execute(&self.resolve_pool)
                .await;
                match retried {
                    Ok(_) => {
                        tracing::info!(job = %job.job_id, %reason, "synthesis job retried");
                        Ok(JobOutcome::Retried {
                            job: job.job_id,
                            reason,
                        })
                    }
                    // The definer refuses a job that has used its attempts:
                    // finish it failed.
                    Err(_) => self.failed(job.job_id, reason).await,
                }
            }
        }
    }

    /// One worklist period: `stage6_pending`, then `staleness_check`, each
    /// item stamped as its own principal with the same authority checks as a
    /// job. A refused or failing item is skipped (it is returned again next
    /// period while it still qualifies).
    ///
    /// # Errors
    /// A failure of the worklist definer itself.
    pub async fn run_worklist(&self, limit: i32) -> Result<WorklistReport, sqlx::Error> {
        let mut report = WorklistReport::default();

        let pending: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT synthesis_id, principal_id FROM public.episcience_owner_worklist('stage6_pending', $1)")
                .bind(limit)
                .fetch_all(&self.resolve_pool)
                .await?;
        for (synthesis_id, principal) in pending {
            match self.write_pending_edges(synthesis_id, principal).await {
                Ok(()) => report.edges_written += 1,
                Err(e) => {
                    report.skipped += 1;
                    tracing::warn!(%synthesis_id, error = %e, "stage-6 retry skipped");
                }
            }
        }

        let due: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT synthesis_id, principal_id FROM public.episcience_owner_worklist('staleness_check', $1)")
                .bind(limit)
                .fetch_all(&self.resolve_pool)
                .await?;
        for (synthesis_id, principal) in due {
            match self.recheck_staleness(synthesis_id, principal).await {
                Ok(stale) => {
                    report.rechecked += 1;
                    if stale {
                        report.marked_stale += 1;
                    }
                }
                Err(e) => {
                    report.skipped += 1;
                    tracing::warn!(%synthesis_id, error = %e, "staleness recheck skipped");
                }
            }
        }
        Ok(report)
    }

    /// A `stage6_pending` item: write the synthesis' pending kernel edges as
    /// its principal (public and publishable only; otherwise deferred).
    async fn write_pending_edges(&self, synthesis_id: Uuid, principal: Uuid) -> Result<(), String> {
        self.authorize(principal).await.map_err(|r| r.reason())?;
        let session = self.session(principal, synthesis_id);
        let mut tx = session.begin().await.map_err(|e| e.to_string())?;
        let outcome = publish::stage6_write_edges_conn(&mut tx, synthesis_id, Some(principal))
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await?;
        match outcome.failure {
            Some(f) => Err(format!("edge write failed: {f}")),
            None => Ok(()),
        }
    }

    /// A `staleness_check` item: compare the recorded belief intervals with
    /// the engine's current ones as the principal, mark stale on drift, and
    /// advance `staleness_checked_at` (exactly one row). An engine failure
    /// other than "claim not found" skips the item WITHOUT advancing it.
    /// Returns whether the synthesis was marked stale.
    async fn recheck_staleness(&self, synthesis_id: Uuid, principal: Uuid) -> Result<bool, String> {
        let viewer = self.authorize(principal).await.map_err(|r| r.reason())?;
        let session = self.session(principal, synthesis_id);
        let snapshot = {
            let mut tx = session.begin().await.map_err(|e| e.to_string())?;
            let s = SynthesisRepository::get_by_id(&mut *tx, synthesis_id)
                .await
                .map_err(|e| e.to_string())?;
            tx.commit().await?;
            s.subgraph_snapshot
        };
        let recheck = staleness::recheck_beliefs(
            &self.handler.pool,
            &viewer,
            &snapshot,
            staleness::DRIFT_EPSILON,
        )
        .await
        .map_err(|e| e.to_string())?;

        let mut tx = session.begin().await.map_err(|e| e.to_string())?;
        if recheck.is_stale() {
            SynthesisStalenessRepository::record_event(
                &mut *tx,
                synthesis_id,
                "belief_drift",
                &recheck.drifted,
                Some(&serde_json::json!({ "source": "worker recheck" })),
            )
            .await
            .map_err(|e| e.to_string())?;
            SynthesisRepository::mark_stale(&mut *tx, synthesis_id, "belief_drift")
                .await
                .map_err(|e| e.to_string())?;
        }
        let r = sqlx::query("UPDATE syntheses SET staleness_checked_at = now() WHERE id = $1")
            .bind(synthesis_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        if r.rows_affected() != 1 {
            return Err(format!(
                "staleness_checked_at: {} rows affected, expected 1",
                r.rows_affected()
            ));
        }
        tx.commit().await?;
        Ok(recheck.is_stale())
    }

    /// Run until `stop` turns true: drain due jobs (sleeping `idle` between
    /// empty polls) and run the worklist every [`WORKLIST_PERIOD`]. `stop` is
    /// checked only BETWEEN jobs, so a job in flight runs to one of its ends
    /// (a job cut off mid-stage would stay `running`: the claim definer never
    /// picks a running job up again). Errors are logged; the loop keeps
    /// running.
    pub async fn run_until(self, idle: Duration, mut stop: tokio::sync::watch::Receiver<bool>) {
        let mut last_worklist: Option<std::time::Instant> = None;
        while !*stop.borrow() {
            if last_worklist.map_or(true, |t| t.elapsed() >= WORKLIST_PERIOD) {
                match self.run_worklist(WORKLIST_LIMIT).await {
                    Ok(r) => tracing::info!(?r, "worklist period"),
                    Err(e) => tracing::error!(error = %e, "worklist period failed"),
                }
                last_worklist = Some(std::time::Instant::now());
            }
            let pause = match self.run_once().await {
                Ok(JobOutcome::Idle) => true,
                Ok(o) => {
                    tracing::info!(?o, "synthesis job");
                    false
                }
                Err(e) => {
                    tracing::error!(error = %e, "queue call failed");
                    true
                }
            };
            if pause {
                tokio::select! {
                    () = tokio::time::sleep(idle) => {}
                    _ = stop.changed() => {}
                }
            }
        }
        tracing::info!("worker stopped between jobs");
    }
}
