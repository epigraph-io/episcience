mod support;
use support::TestDb;

#[tokio::test]
async fn staleness_events_table_exists() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let r = sqlx::query("SELECT trigger FROM synthesis_staleness_events LIMIT 0")
        .execute(&pool)
        .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn worker_state_table_exists() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let r = sqlx::query("SELECT worker_id, last_event_id FROM episcience_worker_state LIMIT 0")
        .execute(&pool)
        .await;
    assert!(r.is_ok());
}
