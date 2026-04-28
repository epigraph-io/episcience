use episcience_core::synthesis::{ProvenanceEdge, Visibility};
use episcience_db::{SynthesisProvoEdgesRepository, SynthesisRepository};
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

fn edge(target_kind: &str) -> ProvenanceEdge {
    ProvenanceEdge {
        predicate: "WAS_DERIVED_FROM".to_string(),
        target_kind: target_kind.to_string(),
        target_id: Uuid::now_v7(),
    }
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn plan_and_list_pending(pool: PgPool) {
    let synthesis_id = create_synthesis(&pool).await;
    let edges = vec![edge("claim"), edge("claim")];

    let mut tx = pool.begin().await.unwrap();
    SynthesisProvoEdgesRepository::plan(&mut tx, synthesis_id, &edges)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let pending = SynthesisProvoEdgesRepository::list_pending(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);

    let count = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn mark_written_removes_from_pending(pool: PgPool) {
    let synthesis_id = create_synthesis(&pool).await;
    let e = edge("claim");

    let mut tx = pool.begin().await.unwrap();
    SynthesisProvoEdgesRepository::plan(&mut tx, synthesis_id, std::slice::from_ref(&e))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let edge_id = Uuid::now_v7();
    SynthesisProvoEdgesRepository::mark_written(
        &pool,
        synthesis_id,
        &e.predicate,
        &e.target_kind,
        e.target_id,
        edge_id,
    )
    .await
    .unwrap();

    let count = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn record_failure_stores_error(pool: PgPool) {
    let synthesis_id = create_synthesis(&pool).await;
    let e = edge("agent");

    let mut tx = pool.begin().await.unwrap();
    SynthesisProvoEdgesRepository::plan(&mut tx, synthesis_id, std::slice::from_ref(&e))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    SynthesisProvoEdgesRepository::record_failure(
        &pool,
        synthesis_id,
        &e.predicate,
        &e.target_kind,
        e.target_id,
        "connection error",
    )
    .await
    .unwrap();

    // Still pending (not written)
    let count = SynthesisProvoEdgesRepository::count_pending(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn plan_nonexistent_synthesis_fails(pool: PgPool) {
    let e = edge("claim");
    let mut tx = pool.begin().await.unwrap();
    let result = SynthesisProvoEdgesRepository::plan(&mut tx, Uuid::now_v7(), &[e]).await;
    assert!(result.is_err(), "should fail FK violation");
    let _ = tx.rollback().await;
}
