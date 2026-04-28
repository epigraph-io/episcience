use episcience_core::synthesis::Visibility;
use episcience_db::{SynthesisEmbeddingsRepository, SynthesisRepository};
use sqlx::PgPool;
use uuid::Uuid;

async fn create_synthesis(pool: &PgPool) -> (Uuid, Uuid) {
    let id = Uuid::now_v7();
    let agent_id = Uuid::now_v7();
    SynthesisRepository::create_pending(
        pool,
        id,
        "test",
        agent_id,
        None,
        &[],
        "anthropic",
        "claude-3-7",
        Visibility::Public,
    )
    .await
    .unwrap();
    (id, agent_id)
}

fn test_embedding() -> Vec<f32> {
    let mut v = vec![0.0f32; 1536];
    v[0] = 1.0;
    v
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn upsert_and_exists(pool: PgPool) {
    let (synthesis_id, _) = create_synthesis(&pool).await;
    let emb = test_embedding();

    assert!(!SynthesisEmbeddingsRepository::exists(&pool, synthesis_id)
        .await
        .unwrap());

    SynthesisEmbeddingsRepository::upsert(
        &pool,
        synthesis_id,
        &emb,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .unwrap();

    assert!(SynthesisEmbeddingsRepository::exists(&pool, synthesis_id)
        .await
        .unwrap());
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn upsert_is_idempotent(pool: PgPool) {
    let (synthesis_id, _) = create_synthesis(&pool).await;
    let emb = test_embedding();

    SynthesisEmbeddingsRepository::upsert(
        &pool,
        synthesis_id,
        &emb,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .unwrap();
    // Second upsert should succeed (ON CONFLICT DO UPDATE)
    SynthesisEmbeddingsRepository::upsert(
        &pool,
        synthesis_id,
        &emb,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn search_finds_similar(pool: PgPool) {
    let (synthesis_id, agent_id) = create_synthesis(&pool).await;
    let emb = test_embedding();

    SynthesisEmbeddingsRepository::upsert(
        &pool,
        synthesis_id,
        &emb,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await
    .unwrap();

    let results = SynthesisEmbeddingsRepository::search(&pool, &emb, 10, 0.0, agent_id, true)
        .await
        .unwrap();

    assert!(!results.is_empty());
    assert_eq!(results[0].0, synthesis_id);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn upsert_nonexistent_synthesis_fails(pool: PgPool) {
    let emb = test_embedding();
    let result = SynthesisEmbeddingsRepository::upsert(
        &pool,
        Uuid::now_v7(),
        &emb,
        "text-embedding-3-small",
        "narrative_head",
    )
    .await;
    assert!(result.is_err(), "should fail FK violation");
}
