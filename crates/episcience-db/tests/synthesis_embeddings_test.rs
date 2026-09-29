mod support;
use support::TestDb;

#[tokio::test]
async fn pgvector_extension_loaded() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pg_extension WHERE extname = 'vector'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count.0, 1);
}

#[tokio::test]
async fn synthesis_embeddings_table_exists_with_vector_column() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let r = sqlx::query("SELECT pg_typeof(embedding)::text FROM synthesis_embeddings LIMIT 0")
        .execute(&pool)
        .await;
    assert!(r.is_ok());
}

#[tokio::test]
async fn synthesis_embeddings_dim_is_1536() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    // Pin the dim to match epigraph's primary embedding dim.
    let dim: (String,) = sqlx::query_as(
        "SELECT format_type(atttypid, atttypmod)
         FROM pg_attribute
         WHERE attrelid = 'synthesis_embeddings'::regclass AND attname = 'embedding'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dim.0, "vector(1536)");
}
