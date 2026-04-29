use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn synthesis_jobs_conforms_to_epigraph_jobs_schema(pool: PgPool) {
    // Required fields per epigraph-jobs::PostgresJobQueue:
    let cols: Vec<(String,)> = sqlx::query_as(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = 'synthesis_jobs' ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<String> = cols.into_iter().map(|(s,)| s).collect();
    for required in &[
        "id",
        "job_type",
        "payload",
        "state",
        "attempts",
        "max_attempts",
        "scheduled_at",
        "started_at",
        "completed_at",
        "last_error",
        "created_at",
        "updated_at",
    ] {
        assert!(names.iter().any(|n| n == required), "missing: {required}");
    }
}
