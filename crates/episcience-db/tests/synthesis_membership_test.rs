mod support;
use support::TestDb;
#[tokio::test]
async fn membership_table_with_indexes() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let r = sqlx::query("SELECT synthesis_id, claim_id FROM synthesis_claim_membership LIMIT 0")
        .execute(&pool)
        .await;
    assert!(r.is_ok());
    let idxs: Vec<(String,)> = sqlx::query_as(
        "SELECT indexname FROM pg_indexes WHERE tablename='synthesis_claim_membership'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(idxs.iter().any(|(s,)| s.contains("claim")));
}
