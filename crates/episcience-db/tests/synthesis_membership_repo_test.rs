mod support;
use episcience_core::synthesis::Visibility;
use episcience_db::SynthesisMembershipRepository;
use sqlx::PgPool;
use support::TestDb;
use uuid::Uuid;

async fn create_synthesis(pool: &PgPool) -> Uuid {
    let author = support::principal(pool, "author").await;
    support::pending_synthesis(pool, &author, Visibility::Group).await
}

#[tokio::test]
async fn replace_and_list_citing() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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

#[tokio::test]
async fn replace_is_idempotent() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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

#[tokio::test]
async fn replace_removes_old_members() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
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

#[tokio::test]
async fn syntheses_citing_no_results() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let citing = SynthesisMembershipRepository::syntheses_citing(&pool, Uuid::now_v7(), false)
        .await
        .unwrap();
    assert!(citing.is_empty());
}
