mod support;
use episcience_core::synthesis::Visibility;
use episcience_db::SynthesisEmbeddingsRepository;
use sqlx::PgPool;
use support::TestDb;
use uuid::Uuid;

async fn create_synthesis(pool: &PgPool) -> (Uuid, Uuid) {
    let author = support::principal(pool, "author").await;
    let id = support::pending_synthesis(pool, &author, Visibility::Public).await;
    (id, author.agent)
}

fn test_embedding() -> Vec<f32> {
    let mut v = vec![0.0f32; 1536];
    v[0] = 1.0;
    v
}

#[tokio::test]
async fn upsert_and_exists() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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

#[tokio::test]
async fn upsert_is_idempotent() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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

#[tokio::test]
async fn search_finds_similar() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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

    let viewer = support::viewer_of(&pool, agent_id).await;
    let results = SynthesisEmbeddingsRepository::search(&pool, &emb, 10, 0.0, &viewer, true)
        .await
        .unwrap();

    assert!(!results.is_empty());
    assert_eq!(results[0].0, synthesis_id);
}

#[tokio::test]
async fn upsert_nonexistent_synthesis_fails() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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
