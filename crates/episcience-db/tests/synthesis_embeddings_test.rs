use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn pgvector_extension_loaded(pool: PgPool) {
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM pg_extension WHERE extname = 'vector'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(count.0, 1);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn synthesis_embeddings_table_exists_with_vector_column(pool: PgPool) {
    let r = sqlx::query("SELECT pg_typeof(embedding)::text FROM synthesis_embeddings LIMIT 0")
        .execute(&pool).await;
    assert!(r.is_ok());
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn synthesis_embeddings_dim_is_1536(pool: PgPool) {
    // Pin the dim to match epigraph's primary embedding dim.
    let dim: (String,) = sqlx::query_as(
        "SELECT format_type(atttypid, atttypmod)
         FROM pg_attribute
         WHERE attrelid = 'synthesis_embeddings'::regclass AND attname = 'embedding'"
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(dim.0, "vector(1536)");
}
