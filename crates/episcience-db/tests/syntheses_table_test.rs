use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn syntheses_table_has_expected_columns(pool: PgPool) {
    let cols: Vec<(String,)> = sqlx::query_as(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = 'syntheses' ORDER BY ordinal_position"
    )
    .fetch_all(&pool).await.unwrap();
    let names: Vec<&str> = cols.iter().map(|(s,)| s.as_str()).collect();
    for required in &[
        "id", "query", "agent_id", "status", "parent_synthesis_id", "narrative",
        "narrative_format", "subgraph_snapshot", "clustering_method", "llm_provider",
        "llm_model", "llm_call_count", "prereq_synthesis_ids", "created_at",
        "completed_at", "stale_since", "stale_reason", "content_hash", "visibility",
    ] {
        assert!(names.contains(required), "missing column: {required}");
    }
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn syntheses_check_constraints_enforce_invariants(pool: PgPool) {
    // status='complete' requires non-null narrative
    let r = sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot,
         clustering_method, llm_provider, llm_model, content_hash, visibility)
         VALUES ($1, 'q', $2, 'complete', '{}'::jsonb, 'signed_louvain', 'anthropic', 'claude-3', $3, 'private')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(uuid::Uuid::now_v7())
    .bind(&[0u8; 32][..])
    .execute(&pool).await;
    assert!(r.is_err(), "should reject complete status without narrative");
}
