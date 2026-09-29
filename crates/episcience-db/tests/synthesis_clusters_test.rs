mod support;
use support::TestDb;

#[tokio::test]
async fn synthesis_clusters_has_expected_columns() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let cols: Vec<(String,)> = sqlx::query_as(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = 'synthesis_clusters' ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<&str> = cols.iter().map(|(s,)| s.as_str()).collect();
    for required in &[
        "id",
        "synthesis_id",
        "cluster_index",
        "title",
        "summary",
        "member_claim_ids",
        "support_count",
        "contradict_count",
        "created_at",
    ] {
        assert!(names.contains(required), "missing: {required}");
    }
}
