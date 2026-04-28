//! Tests for `EpiscienceJobQueue` — `JobQueue` impl over `synthesis_jobs`.
//!
//! Runs against the live `epigraph_dev_synthesis` DB. Each test mints a fresh
//! synthesis UUID, so tests are isolated and can run in parallel.
//!
//! Run with:
//!   DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//!     cargo test -p episcience-api --test episcience_job_queue_test

use epigraph_jobs::{Job, JobId, JobQueue, JobState};
use episcience_api::jobs::EpiscienceJobQueue;
use sqlx::PgPool;
use uuid::Uuid;

const DSN: &str = "postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev_synthesis";

async fn connect() -> PgPool {
    PgPool::connect(DSN)
        .await
        .expect("connect to epigraph_dev_synthesis")
}

/// Insert a placeholder `syntheses` row so we can satisfy the
/// `synthesis_jobs.id REFERENCES syntheses(id)` foreign key.
///
/// Returns the newly minted synthesis id, which is also used as the Job id.
async fn insert_test_synthesis(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let agent_id = Uuid::new_v4();
    let content_hash = vec![0u8; 32];

    sqlx::query(
        r"
        INSERT INTO syntheses (
            id, query, agent_id, status, subgraph_snapshot,
            clustering_method, llm_provider, llm_model, content_hash
        )
        VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                'signed_louvain', 'test', 'test-model', $4)
        ",
    )
    .bind(id)
    .bind("test query")
    .bind(agent_id)
    .bind(&content_hash)
    .execute(pool)
    .await
    .expect("insert test synthesis");

    id
}

/// Build a `Job` whose id matches an existing synthesis row.
fn job_for_synthesis(synthesis_id: Uuid) -> Job {
    let mut job = Job::new(
        "synthesis",
        serde_json::json!({"synthesis_id": synthesis_id}),
    );
    job.id = JobId::from_uuid(synthesis_id);
    job
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: Round-trip enqueue → dequeue → update(complete) → get
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn round_trip_enqueue_dequeue_update_get() {
    let pool = connect().await;
    let queue = EpiscienceJobQueue::new(pool.clone());

    let synth_id = insert_test_synthesis(&pool).await;
    let job = job_for_synthesis(synth_id);
    let job_id = job.id;

    // Enqueue
    let returned_id = queue.enqueue(job).await.expect("enqueue");
    assert_eq!(returned_id, job_id);

    // Dequeue — must claim our row (others may exist; loop until we hit ours
    // or run out, since concurrent test runs may produce other 'queued' rows).
    let mut claimed: Option<Job> = None;
    for _ in 0..32 {
        match queue.dequeue().await {
            Some(j) if j.id == job_id => {
                claimed = Some(j);
                break;
            }
            Some(_other) => continue, // claimed someone else's row; keep looking
            None => break,
        }
    }
    let mut dequeued = claimed.expect("our job to be dequeued");
    assert_eq!(dequeued.state, JobState::Running);
    // Spec: dequeue increments attempts. So retry_count should be 1 here
    // (started at 0 in DB, dequeue sets attempts = attempts + 1).
    assert_eq!(dequeued.retry_count, 1);

    // Update -> Completed
    dequeued
        .transition_to(JobState::Completed)
        .expect("transition to completed");
    queue.update(&dequeued).await.expect("update completed");

    // Get
    let fetched = queue.get(job_id).await.expect("get returns the job");
    assert_eq!(fetched.id, job_id);
    assert_eq!(fetched.state, JobState::Completed);
    assert!(fetched.completed_at.is_some());
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: Concurrent dequeue — only one wins, FOR UPDATE SKIP LOCKED
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_dequeue_only_one_wins() {
    let pool = connect().await;
    let queue = EpiscienceJobQueue::new(pool.clone());

    let synth_id = insert_test_synthesis(&pool).await;
    let job = job_for_synthesis(synth_id);
    let job_id = job.id;
    queue.enqueue(job).await.expect("enqueue");

    // Two concurrent dequeues. Both may also pick up unrelated jobs already in
    // the queue from other tests/state; what matters is that *our* row is
    // claimed by at most one of them.
    let q1 = queue.clone();
    let q2 = queue.clone();
    let (a, b) = tokio::join!(
        async move {
            // Try a few times in case another row beats us to the dequeue head.
            for _ in 0..32 {
                match q1.dequeue().await {
                    Some(j) if j.id == job_id => return Some(j),
                    Some(_) => continue,
                    None => return None,
                }
            }
            None
        },
        async move {
            for _ in 0..32 {
                match q2.dequeue().await {
                    Some(j) if j.id == job_id => return Some(j),
                    Some(_) => continue,
                    None => return None,
                }
            }
            None
        },
    );

    let claimed_count = [a.is_some(), b.is_some()].iter().filter(|x| **x).count();
    assert_eq!(
        claimed_count, 1,
        "exactly one concurrent dequeue must claim our row, got {claimed_count}",
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: pending_jobs returns only queued/retry rows (not running, completed)
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pending_jobs_filters_to_queued_and_retry() {
    let pool = connect().await;
    let queue = EpiscienceJobQueue::new(pool.clone());

    // Insert three rows in distinct states using fresh syntheses ids.
    let queued_id = insert_test_synthesis(&pool).await;
    let running_id = insert_test_synthesis(&pool).await;
    let completed_id = insert_test_synthesis(&pool).await;
    let retry_id = insert_test_synthesis(&pool).await;

    // queued
    sqlx::query(
        r"INSERT INTO synthesis_jobs (id, payload, state) VALUES ($1, '{}'::jsonb, 'queued')",
    )
    .bind(queued_id)
    .execute(&pool)
    .await
    .expect("insert queued");

    // running
    sqlx::query(
        r"INSERT INTO synthesis_jobs (id, payload, state, started_at)
          VALUES ($1, '{}'::jsonb, 'running', now())",
    )
    .bind(running_id)
    .execute(&pool)
    .await
    .expect("insert running");

    // complete
    sqlx::query(
        r"INSERT INTO synthesis_jobs (id, payload, state, started_at, completed_at)
          VALUES ($1, '{}'::jsonb, 'complete', now(), now())",
    )
    .bind(completed_id)
    .execute(&pool)
    .await
    .expect("insert complete");

    // retry
    sqlx::query(
        r"INSERT INTO synthesis_jobs (id, payload, state, attempts, last_error)
          VALUES ($1, '{}'::jsonb, 'retry', 1, 'transient blip')",
    )
    .bind(retry_id)
    .execute(&pool)
    .await
    .expect("insert retry");

    let pending = queue.pending_jobs().await;
    let pending_ids: std::collections::HashSet<Uuid> =
        pending.iter().map(|j| j.id.as_uuid()).collect();

    assert!(
        pending_ids.contains(&queued_id),
        "queued job should be pending"
    );
    assert!(
        pending_ids.contains(&retry_id),
        "retry job should be pending (mapped to Pending)"
    );
    assert!(
        !pending_ids.contains(&running_id),
        "running job must NOT appear in pending_jobs"
    );
    assert!(
        !pending_ids.contains(&completed_id),
        "completed job must NOT appear in pending_jobs"
    );
}
