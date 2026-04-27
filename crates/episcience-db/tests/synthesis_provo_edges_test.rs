#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn provo_edges_pending_partial_index(pool: sqlx::PgPool) {
    let idxs: Vec<(String,String)> = sqlx::query_as(
        "SELECT indexname, indexdef FROM pg_indexes WHERE tablename='synthesis_provo_edges'"
    ).fetch_all(&pool).await.unwrap();
    assert!(idxs.iter().any(|(_, def)| def.contains("WHERE") && def.contains("written_at IS NULL")));
}
