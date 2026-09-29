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
//!    is written for a refused job. A TRANSIENT database failure of these
//!    checks (connection, pool, lock timeout) is not a refusal: the job goes
//!    back to the queue;
//! 4. run every stage stamped as that principal
//!    ([`SynthesisJobHandler::run`] on a [`StageSession::Owner`]); a stage
//!    refused for authority ends the job `failed: authority: …` and is never
//!    retried; any other failure is retried until the row's `max_attempts`,
//!    then `failed`;
//! 5. `complete`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use epigraph_db::{AgentRepository, ScopedPool, Viewer};
use episcience_db::synthesis::{publish, staleness};
use episcience_db::{SynthesisRepository, SynthesisStalenessRepository};
use sqlx::PgPool;
use uuid::Uuid;

use crate::jobs::session::{is_transient_db, is_transient_sqlx, OwnerSession, StageSession};
use crate::jobs::synthesis_job::{RunError, SynthesisJobHandler, SynthesisJobPayload};

/// Jobs older than this are failed unrun (`expired`): a principal whose
/// client was suspended keeps its queued jobs only this long.
pub const JOB_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// How often the owner worklist runs.
pub const WORKLIST_PERIOD: Duration = Duration::from_secs(60);

/// Items per worklist kind per period.
pub const WORKLIST_LIMIT: i32 = 50;

/// The worklist definer's own upper bound on `p_limit` (5037).
const WORKLIST_DEFINER_MAX: usize = 1000;

/// A skipped item is held back for `2^(skips-1)` periods, at most this many.
pub const WORKLIST_MAX_HOLD_PERIODS: u64 = 64;

/// Every this many consecutive skips of one item, the worker logs an ERROR
/// (the item is not advancing; an operator should look at it).
pub const WORKLIST_PERSISTENT_SKIPS: u32 = 5;

/// How many times a queue-definer call is made before a TRANSIENT failure
/// is returned (the first try included).
pub const QUEUE_CALL_TRIES: u32 = 4;

/// The pause before the second try of a queue-definer call (doubled after
/// each further try).
const QUEUE_CALL_FIRST_PAUSE: Duration = Duration::from_millis(500);

/// The arguments of a queue-definer call after the job id.
#[derive(Clone, Copy)]
enum QueueArgs<'a> {
    Finish {
        state: &'a str,
        error: Option<&'a str>,
    },
    Retry {
        delay_secs: f64,
        error: &'a str,
    },
}

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
    /// `stage6_pending` items that advanced: every edge the principal can see
    /// written, or rows deferred or discarded (an item on which nothing
    /// happened is a skip).
    pub edges_written: usize,
    /// `staleness_check` items rechecked (`staleness_checked_at` advanced).
    pub rechecked: usize,
    /// Of those, the ones marked stale.
    pub marked_stale: usize,
    /// Items skipped (refused authority, a failure): each is held back for a
    /// growing number of periods ([`WorklistBackoff`]).
    pub skipped: usize,
    /// Items the definer offered that were still held back from an earlier
    /// skip (not attempted this period).
    pub held_back: usize,
}

/// The two worklist kinds (5037's `episcience_owner_worklist`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorklistKind {
    Stage6Pending,
    StalenessCheck,
}

impl WorklistKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stage6Pending => "stage6_pending",
            Self::StalenessCheck => "staleness_check",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Hold {
    skips: u32,
    /// The first period in which the item is attempted again.
    eligible_at: u64,
}

/// Per-item backoff of the owner worklist, shared by every clone of a
/// [`Worker`].
///
/// The definer orders each kind deterministically (`stage6_pending` by
/// completion time, `staleness_check` oldest check first) and returns at most
/// `limit` items. An item the worker skips never advances its own position:
/// a refused principal writes nothing, and an engine failure leaves
/// `staleness_checked_at` as it was, so without this `limit` such items would
/// be offered first in every period and no other synthesis would ever be
/// processed. A skipped item is held back for `2^(skips-1)` periods (at most
/// [`WORKLIST_MAX_HOLD_PERIODS`]); the worker then asks the definer for
/// `limit` + the held items (at most the definer's bound) and attempts the
/// first `limit` that are not held, so held items never take a slot. A
/// success clears the item's record.
#[derive(Clone, Default)]
pub struct WorklistBackoff {
    inner: Arc<Mutex<BackoffState>>,
}

#[derive(Default)]
struct BackoffState {
    period: u64,
    holds: HashMap<(WorklistKind, Uuid), Hold>,
}

impl WorklistBackoff {
    /// Start a period; returns its number.
    fn begin_period(&self) -> u64 {
        let mut st = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.period += 1;
        let now = st.period;
        // Forget records of items long past their hold that were not skipped
        // again (they left the worklist, or succeeded elsewhere).
        st.holds
            .retain(|_, h| h.eligible_at + 2 * WORKLIST_MAX_HOLD_PERIODS > now);
        now
    }

    /// The items of `kind` held back in `period`.
    fn held(&self, kind: WorklistKind, period: u64) -> HashSet<Uuid> {
        let st = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.holds
            .iter()
            .filter(|((k, _), h)| *k == kind && h.eligible_at > period)
            .map(|((_, id), _)| *id)
            .collect()
    }

    /// Record a skip of `id` in `period`; returns its consecutive skip count.
    fn skipped(&self, kind: WorklistKind, id: Uuid, period: u64) -> u32 {
        let mut st = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let h = st.holds.entry((kind, id)).or_insert(Hold {
            skips: 0,
            eligible_at: 0,
        });
        h.skips = h.skips.saturating_add(1);
        let hold = (1u64 << (h.skips - 1).min(6)).min(WORKLIST_MAX_HOLD_PERIODS);
        h.eligible_at = period + 1 + hold;
        h.skips
    }

    /// Clear `id`'s record after a success.
    fn succeeded(&self, kind: WorklistKind, id: Uuid) {
        let mut st = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.holds.remove(&(kind, id));
    }
}

/// Whether `skips` consecutive skips of one item call for an ERROR (every
/// [`WORKLIST_PERSISTENT_SKIPS`]-th skip; the others are WARN).
fn is_persistent(skips: u32) -> bool {
    skips > 0 && skips % WORKLIST_PERSISTENT_SKIPS == 0
}

/// A refusal to act as a job's principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The principal has an operator link (`principal_operated`).
    Operated,
    /// Anything else, with the reason (`authority: …`).
    Authority(String),
    /// The checks could not be made NOW (a transient database failure): not
    /// an answer about the principal. A job is retried, never failed for it.
    Transient(String),
}

impl Refusal {
    /// The `synthesis_jobs.last_error` text for this refusal.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Operated => REASON_OPERATED.to_string(),
            Self::Authority(m) => format!("authority: {m}"),
            Self::Transient(m) => format!("transient: {m}"),
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
    /// The worklist's per-item backoff (shared by clones).
    pub backoff: WorklistBackoff,
}

impl Worker {
    /// A worker with an empty worklist backoff.
    #[must_use]
    pub fn new(
        name: String,
        scoped: Arc<ScopedPool>,
        resolve_pool: PgPool,
        handler: SynthesisJobHandler,
        retry_delay: Duration,
    ) -> Self {
        Self {
            name,
            scoped,
            resolve_pool,
            handler,
            retry_delay,
            backoff: WorklistBackoff::default(),
        }
    }

    /// Resolve `principal` and refuse to act as it unless it has no operator
    /// link and may write at least one group. Kernel parity: the kernel
    /// refuses the tokens of an agent that has an operator.
    ///
    /// # Errors
    /// The [`Refusal`]. A TRANSIENT database failure (a connection, pool or
    /// lock-timeout failure: [`is_transient_sqlx`]) is [`Refusal::Transient`]
    /// and the job is retried; any other database failure is an authority
    /// refusal (fail closed): the job is then failed, not run.
    pub async fn authorize(&self, principal: Uuid) -> Result<Viewer, Refusal> {
        let viewer = Viewer::resolve(&self.resolve_pool, principal)
            .await
            .map_err(|e| {
                if is_transient_db(&e) {
                    Refusal::Transient(format!("the principal could not be resolved now: {e}"))
                } else {
                    Refusal::Authority(format!("the principal cannot be resolved: {e}"))
                }
            })?;
        let operated = AgentRepository::operator_of_author_pool(&self.resolve_pool, principal)
            .await
            .map_err(|e| {
                if is_transient_db(&e) {
                    Refusal::Transient(format!("parity check could not run now: {e}"))
                } else {
                    Refusal::Authority(format!("parity check failed: {e}"))
                }
            })?;
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

    /// Run one queue-definer call, again on a TRANSIENT failure
    /// ([`is_transient_sqlx`]) up to [`QUEUE_CALL_TRIES`] times with a growing
    /// pause: a job whose `finish` or `retry` is lost to a connection blip
    /// would otherwise stay `running`, which the claim definer never picks up
    /// again. A deterministic failure (a RAISE) is returned at once.
    async fn queue_call(
        &self,
        sql: &'static str,
        job: Uuid,
        a: QueueArgs<'_>,
    ) -> Result<(), sqlx::Error> {
        let mut pause = QUEUE_CALL_FIRST_PAUSE;
        let mut tries = 0u32;
        loop {
            tries += 1;
            let q = sqlx::query(sql).bind(job);
            let q = match a {
                QueueArgs::Finish { state, error } => q.bind(state).bind(error),
                QueueArgs::Retry { delay_secs, error } => q.bind(delay_secs).bind(error),
            };
            match q.execute(&self.resolve_pool).await {
                Ok(_) => return Ok(()),
                Err(e) if is_transient_sqlx(&e) && tries < QUEUE_CALL_TRIES => {
                    tracing::warn!(%job, tries, error = %e, "queue call failed transiently; trying again");
                    tokio::time::sleep(pause).await;
                    pause = pause.saturating_mul(2);
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn finish(&self, job: Uuid, state: &str, error: Option<&str>) -> Result<(), sqlx::Error> {
        self.queue_call(
            "SELECT public.episcience_queue_finish($1, $2, $3)",
            job,
            QueueArgs::Finish { state, error },
        )
        .await
    }

    /// Send a job back to the queue (a transient failure), or finish it
    /// `failed` when the retry definer refuses (the job used its attempts).
    /// A retry call that still fails transiently after its tries is returned
    /// as an error (the job stays `running`; logged by the loop).
    async fn retry_or_fail(
        &self,
        job: &ClaimedJob,
        reason: String,
    ) -> Result<JobOutcome, sqlx::Error> {
        let delay = self
            .retry_delay
            .saturating_mul(u32::try_from(job.attempts.max(1)).unwrap_or(1));
        let delay_secs = i64::try_from(delay.as_secs()).unwrap_or(i64::MAX) as f64;
        let retried = self
            .queue_call(
                "SELECT public.episcience_queue_retry($1, make_interval(secs => $2::double precision), $3)",
                job.job_id,
                QueueArgs::Retry {
                    delay_secs,
                    error: &reason,
                },
            )
            .await;
        match retried {
            Ok(()) => {
                tracing::info!(job = %job.job_id, %reason, "synthesis job retried");
                Ok(JobOutcome::Retried {
                    job: job.job_id,
                    reason,
                })
            }
            Err(e) if is_transient_sqlx(&e) => Err(e),
            // The definer refuses a job that has used its attempts: finish
            // it failed.
            Err(_) => self.failed(job.job_id, reason).await,
        }
    }

    async fn failed(&self, job: Uuid, reason: String) -> Result<JobOutcome, sqlx::Error> {
        self.finish(job, "failed", Some(&reason)).await?;
        tracing::warn!(%job, %reason, "synthesis job failed");
        Ok(JobOutcome::Failed { job, reason })
    }

    /// Claim the next due job and run it to one of its ends.
    ///
    /// # Errors
    /// A database failure on a queue definer (the claim, or a finish or retry
    /// that still fails transiently after [`QUEUE_CALL_TRIES`] tries); the
    /// job then stays `running` until an operator acts, which is what the
    /// definers' state machine reports.
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
        match self.authorize(job.principal_id).await {
            Ok(_) => {}
            Err(r @ Refusal::Transient(_)) => return self.retry_or_fail(&job, r.reason()).await,
            Err(r) => return self.failed(job.job_id, r.reason()).await,
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
                self.retry_or_fail(&job, reason).await
            }
        }
    }

    /// One worklist period: `stage6_pending`, then `staleness_check`, each
    /// item stamped as its own principal with the same authority checks as a
    /// job. A refused or failing item is skipped and held back for a growing
    /// number of periods ([`WorklistBackoff`]), so it cannot starve the items
    /// behind it; an item skipped [`WORKLIST_PERSISTENT_SKIPS`] times in a row
    /// is logged as an ERROR (again every that many skips).
    ///
    /// # Errors
    /// A failure of the worklist definer itself.
    pub async fn run_worklist(&self, limit: i32) -> Result<WorklistReport, sqlx::Error> {
        let mut report = WorklistReport::default();
        let period = self.backoff.begin_period();
        for kind in [WorklistKind::Stage6Pending, WorklistKind::StalenessCheck] {
            let held = self.backoff.held(kind, period);
            let want = usize::try_from(limit.max(1)).unwrap_or(1);
            let fetch = (want + held.len()).min(WORKLIST_DEFINER_MAX);
            let offered: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT synthesis_id, principal_id FROM public.episcience_owner_worklist($1, $2)",
            )
            .bind(kind.as_str())
            .bind(i32::try_from(fetch).unwrap_or(limit))
            .fetch_all(&self.resolve_pool)
            .await?;
            let mut attempted = 0usize;
            for (synthesis_id, principal) in offered {
                if held.contains(&synthesis_id) {
                    report.held_back += 1;
                    continue;
                }
                if attempted == want {
                    break;
                }
                attempted += 1;
                let outcome = match kind {
                    WorklistKind::Stage6Pending => self
                        .write_pending_edges(synthesis_id, principal)
                        .await
                        .map(|()| false),
                    WorklistKind::StalenessCheck => {
                        self.recheck_staleness(synthesis_id, principal).await
                    }
                };
                match outcome {
                    Ok(stale) => {
                        self.backoff.succeeded(kind, synthesis_id);
                        match kind {
                            WorklistKind::Stage6Pending => report.edges_written += 1,
                            WorklistKind::StalenessCheck => {
                                report.rechecked += 1;
                                if stale {
                                    report.marked_stale += 1;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        report.skipped += 1;
                        let skips = self.backoff.skipped(kind, synthesis_id, period);
                        if is_persistent(skips) {
                            tracing::error!(
                                kind = kind.as_str(), %synthesis_id, skips, error = %e,
                                "worklist item keeps failing; it is not advancing"
                            );
                        } else {
                            tracing::warn!(
                                kind = kind.as_str(), %synthesis_id, skips, error = %e,
                                "worklist item skipped; held back"
                            );
                        }
                    }
                }
            }
        }
        Ok(report)
    }

    /// A `stage6_pending` item: write the synthesis' pending kernel edges as
    /// its principal (public and publishable only; otherwise deferred).
    ///
    /// An item on which the write does nothing (no edge written, no row
    /// deferred or discarded) is NOT a success: the definer reads the outbox
    /// with the bypass and offers the synthesis because of an unwritten row
    /// the principal's session cannot see (its claim was narrowed out of the
    /// principal's reach). Counting that as progress would give the item a
    /// slot in every period forever; as a skip, the backoff holds it back.
    async fn write_pending_edges(&self, synthesis_id: Uuid, principal: Uuid) -> Result<(), String> {
        self.authorize(principal).await.map_err(|r| r.reason())?;
        let session = self.session(principal, synthesis_id);
        let mut tx = session.begin().await.map_err(|e| e.to_string())?;
        let outcome = publish::stage6_write_edges_conn(&mut tx, synthesis_id, Some(principal))
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await?;
        if let Some(f) = outcome.failure {
            return Err(format!("edge write failed: {f}"));
        }
        if outcome.written.is_empty() && outcome.deferred == 0 && outcome.discarded == 0 {
            return Err(
                "no progress: no unwritten outbox row of this synthesis is visible to its principal"
                    .into(),
            );
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented backoff shape, driven period by period with the item
    /// attempted (and skipped again) whenever it is not held: after its k-th
    /// consecutive skip an item is held for min(2^(k-1), 64) periods, so the
    /// gaps between attempts are 2, 3, 5, 9, 17, 33, 65 and then stay 65.
    /// Kills: a constant hold (a refused item retried every other period), a
    /// hold that grows past the cap, and an off-by-one in the eligibility.
    #[test]
    fn a_skipped_item_is_held_for_doubling_periods_up_to_the_cap() {
        let b = WorklistBackoff::default();
        let id = Uuid::now_v7();
        let kind = WorklistKind::StalenessCheck;
        let mut attempts = Vec::new();
        for _ in 0..265 {
            let p = b.begin_period();
            if !b.held(kind, p).contains(&id) {
                attempts.push(p);
                b.skipped(kind, id, p);
            }
        }
        assert_eq!(attempts, vec![1, 3, 6, 11, 20, 37, 70, 135, 200, 265]);
        // The other kind is independent.
        assert!(b.held(WorklistKind::Stage6Pending, 266).is_empty());
    }

    /// A success clears the record: the next skip is a FIRST skip again (held
    /// one period), not the continuation of the earlier run. Kills:
    /// `succeeded` keeping the count.
    #[test]
    fn a_success_resets_the_consecutive_skip_count() {
        let b = WorklistBackoff::default();
        let id = Uuid::now_v7();
        let kind = WorklistKind::Stage6Pending;
        for _ in 0..4 {
            let mut p = b.begin_period();
            while b.held(kind, p).contains(&id) {
                p = b.begin_period();
            }
            b.skipped(kind, id, p);
        }
        b.succeeded(kind, id);
        let p = b.begin_period();
        assert!(
            !b.held(kind, p).contains(&id),
            "a success releases the item"
        );
        assert_eq!(b.skipped(kind, id, p), 1, "the count starts again");
        assert!(b.held(kind, p + 1).contains(&id), "held one period");
        assert!(!b.held(kind, p + 2).contains(&id), "and only one");
    }

    /// The ERROR fires on every 5th consecutive skip only. Kills: logging
    /// every skip as an ERROR (alert noise), or never.
    #[test]
    fn every_fifth_consecutive_skip_is_an_error() {
        let persistent: Vec<u32> = (0..=16).filter(|&k| is_persistent(k)).collect();
        assert_eq!(persistent, vec![5, 10, 15]);
    }
}
