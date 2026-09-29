mod support;
use episcience_core::synthesis::Visibility;
use episcience_db::SynthesisStalenessRepository;
use sqlx::PgPool;
use support::TestDb;
use uuid::Uuid;

async fn create_synthesis(pool: &PgPool) -> Uuid {
    let author = support::principal(pool, "author").await;
    support::pending_synthesis(pool, &author, Visibility::Group).await
}

#[tokio::test]
async fn record_and_list_event() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = create_synthesis(&pool).await;
    let affected = vec![Uuid::now_v7()];

    SynthesisStalenessRepository::record_event(
        &pool,
        synthesis_id,
        "belief_drift",
        &affected,
        None,
    )
    .await
    .unwrap();

    let events = SynthesisStalenessRepository::list_for_synthesis(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].trigger, "belief_drift");
    assert_eq!(events[0].synthesis_id, synthesis_id);
    assert_eq!(events[0].affected_claim_ids, affected);
}

#[tokio::test]
async fn record_multiple_events() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = create_synthesis(&pool).await;
    let claim = Uuid::now_v7();

    SynthesisStalenessRepository::record_event(&pool, synthesis_id, "belief_drift", &[claim], None)
        .await
        .unwrap();
    SynthesisStalenessRepository::record_event(
        &pool,
        synthesis_id,
        "new_contradiction",
        &[claim],
        None,
    )
    .await
    .unwrap();

    let events = SynthesisStalenessRepository::list_for_synthesis(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn invalid_trigger_fails() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = create_synthesis(&pool).await;
    let result = SynthesisStalenessRepository::record_event(
        &pool,
        synthesis_id,
        "invalid_trigger",
        &[],
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "invalid trigger should fail check constraint"
    );
}

#[tokio::test]
async fn record_event_nonexistent_synthesis_fails() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let result = SynthesisStalenessRepository::record_event(
        &pool,
        Uuid::now_v7(),
        "belief_drift",
        &[],
        None,
    )
    .await;
    assert!(result.is_err(), "should fail FK violation");
}
