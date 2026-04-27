use episcience_db::{SynthesisRepository, SynthesisSharesRepository};
use episcience_core::synthesis::Visibility;
use sqlx::PgPool;
use uuid::Uuid;

async fn create_synthesis(pool: &PgPool, visibility: Visibility) -> (Uuid, Uuid) {
    let id = Uuid::now_v7();
    let owner = Uuid::now_v7();
    SynthesisRepository::create_pending(
        pool, id, "test", owner, None, &[],
        "anthropic", "claude-3-7", visibility,
    ).await.unwrap();
    (id, owner)
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn grant_and_list_round_trip(pool: PgPool) {
    let (synthesis_id, owner) = create_synthesis(&pool, Visibility::Shared).await;
    let recipient = Uuid::now_v7();

    SynthesisSharesRepository::grant(&pool, synthesis_id, recipient, owner).await.unwrap();

    let shares = SynthesisSharesRepository::list(&pool, synthesis_id).await.unwrap();
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].shared_with_agent_id, recipient);
    assert_eq!(shares[0].shared_by_agent_id, owner);
    assert_eq!(shares[0].permission, "read");
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn grant_and_revoke(pool: PgPool) {
    let (synthesis_id, owner) = create_synthesis(&pool, Visibility::Shared).await;
    let recipient = Uuid::now_v7();

    SynthesisSharesRepository::grant(&pool, synthesis_id, recipient, owner).await.unwrap();
    SynthesisSharesRepository::revoke(&pool, synthesis_id, recipient).await.unwrap();

    let shares = SynthesisSharesRepository::list(&pool, synthesis_id).await.unwrap();
    assert!(shares.is_empty());
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn grant_duplicate_is_idempotent(pool: PgPool) {
    let (synthesis_id, owner) = create_synthesis(&pool, Visibility::Shared).await;
    let recipient = Uuid::now_v7();

    SynthesisSharesRepository::grant(&pool, synthesis_id, recipient, owner).await.unwrap();
    // Second grant same recipient → should not error (ON CONFLICT DO NOTHING)
    SynthesisSharesRepository::grant(&pool, synthesis_id, recipient, owner).await.unwrap();

    let shares = SynthesisSharesRepository::list(&pool, synthesis_id).await.unwrap();
    assert_eq!(shares.len(), 1);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn grant_nonexistent_synthesis_fails(pool: PgPool) {
    let result = SynthesisSharesRepository::grant(
        &pool, Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(),
    ).await;
    assert!(result.is_err(), "should fail FK violation");
}
