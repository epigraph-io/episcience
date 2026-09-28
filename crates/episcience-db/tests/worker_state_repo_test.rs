mod support;
use episcience_db::WorkerStateRepository;
use support::TestDb;

#[tokio::test]
async fn get_returns_none_for_unknown() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let state = WorkerStateRepository::get(&pool, "nonexistent-worker")
        .await
        .unwrap();
    assert!(state.is_none());
}
