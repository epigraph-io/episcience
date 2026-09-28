//! Stage 3 (`pipeline::stage3_plan` + `pipeline::stage3_persist`) integration
//! tests.
//!
//! # DB strategy
//!
//! The run's shared clone of the E1 template. We pre-insert a `syntheses`
//! row, then run stage 3 the way the synthesis handler does (the pure plan,
//! then the persist on one transaction) with a synthesised in-memory
//! `SubgraphSnapshot` and a hand-crafted edge list.
//!
//! Stage 3 does not require Stage 2 to have run — it only needs a
//! `SubgraphSnapshot` (read-only input) and an `edges_with_types` slice. So
//! these tests construct the snapshot in memory rather than running a full
//! traversal.
//!
//! # Test graphs
//!
//! 1. `stage3_cluster_two_well_separated_groups_persists_two_clusters`
//!    Six claim ids split into two groups of three with within-group SUPPORTS
//!    edges and no cross-group edges. Mirrors the
//!    `two_well_separated_groups_become_two_clusters` unit test in
//!    `episcience_core::synthesis::clustering`.
//!
//! 2. `stage3_cluster_records_contradict_count`
//!    Four claims fully connected by SUPPORTS plus two strong CONTRADICTS edges
//!    between specific pairs. Mirrors the
//!    `separator_splits_cluster_with_high_contradicts_density` unit test —
//!    after the separator splits the single positive-Louvain cluster, the
//!    CONTRADICTS pairs end up in separate clusters and at least one cluster
//!    must record contradict_count >= 1 for cross-cluster purposes — wait, no:
//!    the implementation only counts edges where BOTH endpoints are in the
//!    same cluster, so we need an intra-cluster CONTRADICTS pair instead.
//!
//!    Construction: three claims A, B, C with positive edges A↔B and a single
//!    CONTRADICTS edge A↔B. Density is 1/1 = 1.0 ≥ 0.2 threshold so the
//!    separator splits them, but we add a fourth claim D with CONTRADICTS to
//!    A but no positive edges. The separator only acts on clusters with ≥ 2
//!    members; an isolated CONTRADICTS to a singleton still keeps the partner
//!    in its own cluster. So we go simpler: build a small triangle of
//!    positive edges with one intra-triangle CONTRADICTS that survives
//!    separation because total positive density inside the cluster
//!    overwhelms the negative-density threshold.
//!
//!    Per `separate_on_contradicts(threshold=0.2)`: density = neg_count /
//!    cluster_size. For a 3-claim cluster with 1 negative edge, density =
//!    1/3 ≈ 0.33 > 0.2 → separator triggers and may split. To avoid that
//!    while still landing CONTRADICTS inside a cluster, we use a 6-claim
//!    cluster with 1 negative edge: density = 1/6 ≈ 0.17 < 0.2 → no split.
//!    Then `contradict_count` for that cluster will be ≥ 1.
mod support;

use chrono::Utc;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::EdgeType;
use episcience_core::synthesis::{Cluster, SubgraphSnapshot};
use episcience_db::synthesis::pipeline;
use sqlx::PgPool;
use uuid::Uuid;

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

async fn connect_epigraph() -> PgPool {
    // The run's shared clone of the E1 template; refuses port 5432 and any
    // database name not ending in `_test` (support::check_test_url).
    support::shared_pool("DATABASE_URL").await
}

fn test_agent_id() -> Uuid {
    "f3951e28-9356-42b6-9c80-27dd9f01b19d".parse().unwrap()
}

async fn insert_synthesis_row(pool: &PgPool, synthesis_id: Uuid, query: &str) {
    sqlx::query(
        "INSERT INTO syntheses
         (id, query, agent_id, status, subgraph_snapshot,
          clustering_method, llm_provider, llm_model,
          content_hash, visibility, owner_group_id)
         VALUES ($1, $2, $3, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock',
                 $4, 'group', (SELECT g.id FROM public.groups g WHERE g.did_key = 'did:epigraph:personal:f3951e28-9356-42b6-9c80-27dd9f01b19d'))",
    )
    .bind(synthesis_id)
    .bind(query)
    .bind(test_agent_id())
    .bind(&[0u8; 32][..])
    .execute(pool)
    .await
    .expect("insert synthesis row");
}

async fn cleanup(pool: &PgPool, synthesis_id: Uuid) {
    let _ = sqlx::query("DELETE FROM synthesis_clusters WHERE synthesis_id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(synthesis_id)
        .execute(pool)
        .await;
}

fn make_snapshot(claim_ids: Vec<Uuid>) -> SubgraphSnapshot {
    SubgraphSnapshot {
        claim_ids,
        edge_ids: vec![],
        belief_intervals: vec![],
        traversal_config: serde_json::json!({}),
        captured_at: Utc::now(),
    }
}

/// Stage 3 as the synthesis handler runs it: the pure plan, then the persist
/// on one transaction (here the fixture pool's).
async fn stage3(
    pool: &PgPool,
    synthesis_id: Uuid,
    snapshot: &SubgraphSnapshot,
    edges: &[(Uuid, Uuid, EdgeType)],
) -> Result<Vec<Cluster>, SynthesisError> {
    let clusters = pipeline::stage3_plan(synthesis_id, snapshot, edges);
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    pipeline::stage3_persist(&mut tx, synthesis_id, &clusters).await?;
    tx.commit()
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(clusters)
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

/// Two well-separated triangles of SUPPORTS edges → two clusters persisted.
#[tokio::test]
async fn stage3_cluster_two_well_separated_groups_persists_two_clusters() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage3 two-groups test").await;

    // Six fresh ids; group A = {a1,a2,a3}, group B = {b1,b2,b3}.
    let a1 = Uuid::now_v7();
    let a2 = Uuid::now_v7();
    let a3 = Uuid::now_v7();
    let b1 = Uuid::now_v7();
    let b2 = Uuid::now_v7();
    let b3 = Uuid::now_v7();
    let claim_ids = vec![a1, a2, a3, b1, b2, b3];
    let snapshot = make_snapshot(claim_ids.clone());

    // Within-group SUPPORTS, no cross-group edges.
    let edges: Vec<(Uuid, Uuid, EdgeType)> = vec![
        (a1, a2, EdgeType::Supports),
        (a2, a3, EdgeType::Supports),
        (a1, a3, EdgeType::Supports),
        (b1, b2, EdgeType::Supports),
        (b2, b3, EdgeType::Supports),
        (b1, b3, EdgeType::Supports),
    ];

    let clusters = stage3(&pool, synthesis_id, &snapshot, &edges)
        .await
        .expect("stage 3 should succeed");

    assert_eq!(
        clusters.len(),
        2,
        "expected exactly two clusters, got {}",
        clusters.len()
    );
    for c in &clusters {
        assert!(
            c.support_count >= 1,
            "every cluster should have at least one within-group SUPPORTS, got {}",
            c.support_count
        );
        assert_eq!(c.contradict_count, 0, "no CONTRADICTS edges in this graph");
        assert_eq!(c.synthesis_id, synthesis_id);
        assert!(
            c.title.is_empty() && c.summary.is_empty(),
            "title/summary should be populated by Stage 4, not Stage 3"
        );
    }

    // Persistence check.
    let row_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM synthesis_clusters WHERE synthesis_id = $1")
            .bind(synthesis_id)
            .fetch_one(&pool)
            .await
            .expect("count clusters");
    assert_eq!(
        row_count, 2,
        "expected two persisted cluster rows, got {row_count}"
    );

    // Cluster_index uniqueness/contiguity.
    let indices: Vec<i32> =
        sqlx::query_scalar(
            "SELECT cluster_index FROM synthesis_clusters WHERE synthesis_id = $1 ORDER BY cluster_index",
        )
        .bind(synthesis_id)
        .fetch_all(&pool)
        .await
        .expect("fetch indices");
    assert_eq!(indices, vec![0, 1]);

    cleanup(&pool, synthesis_id).await;
}

/// At least one cluster should record `contradict_count >= 1` when an
/// intra-cluster CONTRADICTS edge survives the separator threshold.
///
/// We build a 6-claim positive clique (Louvain → 1 cluster) plus a single
/// CONTRADICTS edge inside it. negative-density = 1/6 ≈ 0.17 < 0.2 threshold,
/// so the separator does NOT split. The resulting single cluster contains
/// both endpoints of the CONTRADICTS edge, giving contradict_count = 1.
#[tokio::test]
async fn stage3_cluster_records_contradict_count() {
    let pool = connect_epigraph().await;
    let synthesis_id = Uuid::now_v7();
    insert_synthesis_row(&pool, synthesis_id, "stage3 contradict-count test").await;

    let c1 = Uuid::now_v7();
    let c2 = Uuid::now_v7();
    let c3 = Uuid::now_v7();
    let c4 = Uuid::now_v7();
    let c5 = Uuid::now_v7();
    let c6 = Uuid::now_v7();
    let claim_ids = vec![c1, c2, c3, c4, c5, c6];
    let snapshot = make_snapshot(claim_ids.clone());

    // Full positive clique on all 6 claims (15 edges).
    let pos_pairs = [
        (c1, c2),
        (c1, c3),
        (c1, c4),
        (c1, c5),
        (c1, c6),
        (c2, c3),
        (c2, c4),
        (c2, c5),
        (c2, c6),
        (c3, c4),
        (c3, c5),
        (c3, c6),
        (c4, c5),
        (c4, c6),
        (c5, c6),
    ];
    let mut edges: Vec<(Uuid, Uuid, EdgeType)> = pos_pairs
        .iter()
        .map(|(a, b)| (*a, *b, EdgeType::Supports))
        .collect();
    // One intra-cluster CONTRADICTS edge.
    edges.push((c1, c2, EdgeType::Contradicts));

    let clusters = stage3(&pool, synthesis_id, &snapshot, &edges)
        .await
        .expect("stage 3 should succeed");

    let total_contradict: i32 = clusters.iter().map(|c| c.contradict_count).sum();
    assert!(
        total_contradict >= 1,
        "expected at least one cluster to record contradict_count >= 1; clusters={:?}",
        clusters
            .iter()
            .map(|c| (
                c.cluster_index,
                c.support_count,
                c.contradict_count,
                c.member_claim_ids.len()
            ))
            .collect::<Vec<_>>()
    );

    // Persistence: the recorded contradict_count should be readable from DB.
    let db_total: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(contradict_count), 0)::bigint
         FROM synthesis_clusters WHERE synthesis_id = $1",
    )
    .bind(synthesis_id)
    .fetch_one(&pool)
    .await
    .expect("sum contradict_count");
    assert!(
        db_total.unwrap_or(0) >= 1,
        "DB should record at least one contradict_count, got {:?}",
        db_total
    );

    cleanup(&pool, synthesis_id).await;
}
