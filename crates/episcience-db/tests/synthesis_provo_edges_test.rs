mod support;
use support::TestDb;
#[tokio::test]
async fn provo_edges_pending_partial_index() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let idxs: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes WHERE tablename='synthesis_provo_edges'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(idxs
        .iter()
        .any(|(_, def)| def.contains("WHERE") && def.contains("written_at IS NULL")));
}
