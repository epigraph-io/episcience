use episcience_core::synthesis::Visibility;
use episcience_db::{SynthesisMembershipRepository, SynthesisRepository};
use sqlx::PgPool;
use uuid::Uuid;

async fn create_synthesis(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    SynthesisRepository::create_pending(
        pool,
        id,
        "test",
        Uuid::now_v7(),
        None,
        &[],
        "anthropic",
        "claude-3-7",
        Visibility::Private,
    )
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn replace_and_list_citing(pool: PgPool) {
    let synthesis_id = create_synthesis(&pool).await;
    let claim1 = Uuid::now_v7();
    let claim2 = Uuid::now_v7();

    let mut tx = pool.begin().await.unwrap();
    SynthesisMembershipRepository::replace_for_synthesis(&mut tx, synthesis_id, &[claim1, claim2])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let citing = SynthesisMembershipRepository::syntheses_citing(&pool, claim1, false)
        .await
        .unwrap();
    assert!(citing.contains(&synthesis_id));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn replace_is_idempotent(pool: PgPool) {
    let synthesis_id = create_synthesis(&pool).await;
    let claim = Uuid::now_v7();

    let mut tx = pool.begin().await.unwrap();
    SynthesisMembershipRepository::replace_for_synthesis(&mut tx, synthesis_id, &[claim])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Replace again with same data
    let mut tx = pool.begin().await.unwrap();
    SynthesisMembershipRepository::replace_for_synthesis(&mut tx, synthesis_id, &[claim])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let citing = SynthesisMembershipRepository::syntheses_citing(&pool, claim, false)
        .await
        .unwrap();
    assert_eq!(citing.len(), 1);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn replace_removes_old_members(pool: PgPool) {
    let synthesis_id = create_synthesis(&pool).await;
    let claim1 = Uuid::now_v7();
    let claim2 = Uuid::now_v7();

    let mut tx = pool.begin().await.unwrap();
    SynthesisMembershipRepository::replace_for_synthesis(&mut tx, synthesis_id, &[claim1, claim2])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Replace with only claim2 — claim1 should be removed
    let mut tx = pool.begin().await.unwrap();
    SynthesisMembershipRepository::replace_for_synthesis(&mut tx, synthesis_id, &[claim2])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let citing1 = SynthesisMembershipRepository::syntheses_citing(&pool, claim1, false)
        .await
        .unwrap();
    assert!(citing1.is_empty(), "claim1 should no longer be cited");
    let citing2 = SynthesisMembershipRepository::syntheses_citing(&pool, claim2, false)
        .await
        .unwrap();
    assert!(citing2.contains(&synthesis_id));
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn syntheses_citing_no_results(pool: PgPool) {
    let citing = SynthesisMembershipRepository::syntheses_citing(&pool, Uuid::now_v7(), false)
        .await
        .unwrap();
    assert!(citing.is_empty());
}
