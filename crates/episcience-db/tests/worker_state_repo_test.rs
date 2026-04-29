use episcience_db::WorkerStateRepository;
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn upsert_and_get_round_trip(pool: PgPool) {
    let worker_id = "synthesis-worker-1";

    let initial = WorkerStateRepository::get(&pool, worker_id).await.unwrap();
    assert!(initial.is_none(), "should not exist yet");

    WorkerStateRepository::upsert(&pool, worker_id, Some("evt-001"), None)
        .await
        .unwrap();

    let state = WorkerStateRepository::get(&pool, worker_id).await.unwrap();
    assert!(state.is_some());
    let state = state.unwrap();
    assert_eq!(state.worker_id, worker_id);
    assert_eq!(state.last_event_id.as_deref(), Some("evt-001"));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn upsert_updates_existing(pool: PgPool) {
    let worker_id = "synthesis-worker-2";

    WorkerStateRepository::upsert(&pool, worker_id, Some("evt-001"), None)
        .await
        .unwrap();
    WorkerStateRepository::upsert(&pool, worker_id, Some("evt-042"), None)
        .await
        .unwrap();

    let state = WorkerStateRepository::get(&pool, worker_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.last_event_id.as_deref(), Some("evt-042"));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn get_returns_none_for_unknown(pool: PgPool) {
    let state = WorkerStateRepository::get(&pool, "nonexistent-worker")
        .await
        .unwrap();
    assert!(state.is_none());
}
