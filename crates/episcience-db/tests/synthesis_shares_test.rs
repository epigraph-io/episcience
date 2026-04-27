use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn shares_pk_is_synthesis_plus_recipient(pool: PgPool) {
    // Insert a synthesis first (synthesis_shares has FK to syntheses)
    let synthesis_id = uuid::Uuid::now_v7();
    let agent_id = uuid::Uuid::now_v7();
    let recipient_id = uuid::Uuid::now_v7();

    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot,
         clustering_method, llm_provider, llm_model, content_hash, visibility)
         VALUES ($1, 'test query', $2, 'pending', '{}'::jsonb, 'signed_louvain', 'anthropic', 'claude-3', $3, 'private')"
    )
    .bind(synthesis_id)
    .bind(agent_id)
    .bind(&[0u8; 32][..])
    .execute(&pool).await.unwrap();

    // Insert a share
    sqlx::query(
        "INSERT INTO synthesis_shares (synthesis_id, shared_with_agent_id, shared_by_agent_id)
         VALUES ($1, $2, $3)"
    )
    .bind(synthesis_id)
    .bind(recipient_id)
    .bind(agent_id)
    .execute(&pool).await.unwrap();

    // Insert duplicate (synthesis_id, shared_with_agent_id) — must fail
    let r = sqlx::query(
        "INSERT INTO synthesis_shares (synthesis_id, shared_with_agent_id, shared_by_agent_id)
         VALUES ($1, $2, $3)"
    )
    .bind(synthesis_id)
    .bind(recipient_id)
    .bind(agent_id)
    .execute(&pool).await;

    assert!(r.is_err(), "duplicate (synthesis_id, shared_with_agent_id) should be rejected");
    // Verify it's a unique violation (SQLSTATE 23505)
    if let Err(sqlx::Error::Database(db_err)) = r {
        assert_eq!(db_err.code().as_deref(), Some("23505"), "expected unique_violation");
    }
}
