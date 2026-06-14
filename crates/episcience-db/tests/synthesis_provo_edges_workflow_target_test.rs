//! Item 2 enablement: `synthesis_provo_edges.target_kind` must accept
//! `'workflow'` so a REFINES edge can link a synthesis-refinement chain to a
//! workflow-generation chain.
//!
//! These tests are a *contract* on the `target_kind` CHECK constraint, exercised
//! through the real repository round-trip (`plan` → `list_pending`). They guard
//! two distinct failure modes of the widening migration (5031):
//!   - the positive case proves `'workflow'` is now accepted; and
//!   - the negative case proves the CHECK was *widened*, not *removed* —
//!     a bogus `target_kind` must still be rejected. Without the negative
//!     assertion, a migration that dropped the CHECK entirely (or fat-fingered
//!     the re-ADD) would pass silently.

use episcience_core::synthesis::ProvenanceEdge;
use sqlx::PgPool;
use uuid::Uuid;

use episcience_db::SynthesisProvoEdgesRepository;

/// Inserts a minimal `pending` synthesis row so the `synthesis_provo_edges`
/// FK to `syntheses(id)` is satisfied.
async fn insert_synthesis(pool: &PgPool, synthesis_id: Uuid) {
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot,
         clustering_method, llm_provider, llm_model, content_hash, visibility)
         VALUES ($1, 'workflow-target test', $2, 'pending', '{}'::jsonb,
                 'signed_louvain', 'mock', 'mock', $3, 'private')",
    )
    .bind(synthesis_id)
    .bind(Uuid::now_v7())
    .bind(&[0u8; 32][..])
    .execute(pool)
    .await
    .expect("insert synthesis row");
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn plan_accepts_refines_workflow_and_list_pending_returns_it(pool: PgPool) {
    let synthesis_id = Uuid::now_v7();
    let workflow_id = Uuid::now_v7();
    insert_synthesis(&pool, synthesis_id).await;

    let edge = ProvenanceEdge {
        predicate: "REFINES".into(),
        target_kind: "workflow".into(),
        target_id: workflow_id,
    };

    let mut tx = pool.begin().await.expect("begin tx");
    SynthesisProvoEdgesRepository::plan(&mut tx, synthesis_id, std::slice::from_ref(&edge))
        .await
        .expect("plan must accept target_kind='workflow' after migration 5031");
    tx.commit().await.expect("commit tx");

    let pending = SynthesisProvoEdgesRepository::list_pending(&pool, synthesis_id)
        .await
        .expect("list_pending");

    assert_eq!(
        pending.len(),
        1,
        "the planned workflow edge must be pending"
    );
    let got = &pending[0];
    assert_eq!(got.predicate, "REFINES");
    assert_eq!(got.target_kind, "workflow");
    assert_eq!(got.target_id, workflow_id);
}

#[sqlx::test(migrations = "../../migrations/synthesis")]
async fn plan_rejects_bogus_target_kind(pool: PgPool) {
    // Guards against the migration dropping the CHECK instead of widening it:
    // a target_kind outside the allowed set must still violate the constraint.
    let synthesis_id = Uuid::now_v7();
    insert_synthesis(&pool, synthesis_id).await;

    let edge = ProvenanceEdge {
        predicate: "REFINES".into(),
        target_kind: "garbage".into(),
        target_id: Uuid::now_v7(),
    };

    let mut tx = pool.begin().await.expect("begin tx");
    let result =
        SynthesisProvoEdgesRepository::plan(&mut tx, synthesis_id, std::slice::from_ref(&edge))
            .await;

    assert!(
        result.is_err(),
        "an unknown target_kind must be rejected by the CHECK constraint; \
         the migration must widen the allowed set, not remove the check"
    );
}
