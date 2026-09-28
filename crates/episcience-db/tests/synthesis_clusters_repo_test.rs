mod support;
use episcience_core::synthesis::{Cluster, Visibility};
use episcience_db::{SynthesisClustersRepository, SynthesisRepository};
use sqlx::PgPool;
use support::TestDb;
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

fn make_cluster(synthesis_id: Uuid, index: i32) -> Cluster {
    Cluster {
        id: Uuid::now_v7(),
        synthesis_id,
        cluster_index: index,
        title: format!("Cluster {index}"),
        summary: "test summary".to_string(),
        member_claim_ids: vec![Uuid::now_v7()],
        support_count: 2,
        contradict_count: 1,
    }
}

#[tokio::test]
async fn insert_and_list_round_trip() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = create_synthesis(&pool).await;
    let c1 = make_cluster(synthesis_id, 0);
    let c2 = make_cluster(synthesis_id, 1);

    SynthesisClustersRepository::insert(&pool, &c1)
        .await
        .unwrap();
    SynthesisClustersRepository::insert(&pool, &c2)
        .await
        .unwrap();

    let list = SynthesisClustersRepository::list_by_synthesis(&pool, synthesis_id)
        .await
        .unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().any(|c| c.cluster_index == 0));
    assert!(list.iter().any(|c| c.cluster_index == 1));
}

#[tokio::test]
async fn list_empty_when_no_clusters() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = create_synthesis(&pool).await;
    let list = SynthesisClustersRepository::list_by_synthesis(&pool, synthesis_id)
        .await
        .unwrap();
    assert!(list.is_empty());
}

#[tokio::test]
async fn duplicate_index_fails() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let synthesis_id = create_synthesis(&pool).await;
    let c1 = make_cluster(synthesis_id, 0);
    let c2 = Cluster {
        id: Uuid::now_v7(),
        ..make_cluster(synthesis_id, 0)
    };

    SynthesisClustersRepository::insert(&pool, &c1)
        .await
        .unwrap();
    let result = SynthesisClustersRepository::insert(&pool, &c2).await;
    assert!(
        result.is_err(),
        "duplicate (synthesis_id, cluster_index) should fail"
    );
}

#[tokio::test]
async fn insert_nonexistent_synthesis_fails() {
    let db = TestDb::fresh().await;
    let pool = db.admin.clone();
    let c = make_cluster(Uuid::now_v7(), 0);
    let result = SynthesisClustersRepository::insert(&pool, &c).await;
    assert!(result.is_err(), "should fail FK violation");
}
