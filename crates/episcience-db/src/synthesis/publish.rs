//! Stage 6 — Publish.
//!
//! Stage 6 takes a fully-narrated synthesis and:
//!
//! 1. **Plans** PROV-O provenance edges (`stage6_plan_edges`) — one
//!    `WAS_DERIVED_FROM` per cited claim, one `REFINES` for the parent
//!    synthesis (if any), one `COMPOSED_OF` per prerequisite synthesis, and
//!    one `ATTRIBUTED_TO` for the owning agent. Rows go into
//!    `synthesis_provo_edges` with `written_at IS NULL`.
//!
//! Subsequent substeps (embed, hash, write, mark-complete, reconcile) land
//! in follow-on commits in the same module.
//!
//! All substeps are free functions (not methods on `SynthesisPipeline`) so
//! Stage 6 stays decoupled from the `L: LlmClient` / `P: EdgeProvider`
//! generics that earlier stages need. Callers either invoke them directly or
//! the pipeline runner threads them at the end of the synthesis flow.

use sqlx::PgPool;
use uuid::Uuid;

use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::ProvenanceEdge;

use crate::SynthesisProvoEdgesRepository;

// ──────────────────────────────────────────────────────────────────────────────
// 2.7a — stage6_plan_edges
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6a — Plan provenance edges.
///
/// Builds the canonical edge set for this synthesis and inserts it into
/// `synthesis_provo_edges` with `written_at IS NULL`. Repeat invocations are
/// safe: the underlying repo uses `ON CONFLICT DO NOTHING` and the table's
/// PRIMARY KEY `(synthesis_id, predicate, target_kind, target_id)` ensures
/// duplicates are rejected at the row level.
///
/// Edge layout:
///
/// - one `WAS_DERIVED_FROM` per element of `cited_claim_ids`
///   (`target_kind = "claim"`)
/// - optional `REFINES` to `parent_synthesis_id`
///   (`target_kind = "synthesis"`)
/// - one `COMPOSED_OF` per element of `prereq_synthesis_ids`
///   (`target_kind = "synthesis"`)
/// - one `ATTRIBUTED_TO` to `owner_agent_id`
///   (`target_kind = "agent"`)
///
/// All inserts run inside a single transaction; if any fails, none are
/// persisted.
pub async fn stage6_plan_edges(
    pool: &PgPool,
    synthesis_id: Uuid,
    cited_claim_ids: &[Uuid],
    parent_synthesis_id: Option<Uuid>,
    prereq_synthesis_ids: &[Uuid],
    owner_agent_id: Uuid,
) -> Result<(), SynthesisError> {
    let mut edges = Vec::with_capacity(cited_claim_ids.len() + prereq_synthesis_ids.len() + 2);
    for &claim_id in cited_claim_ids {
        edges.push(ProvenanceEdge {
            predicate: "WAS_DERIVED_FROM".into(),
            target_kind: "claim".into(),
            target_id: claim_id,
        });
    }
    if let Some(parent) = parent_synthesis_id {
        edges.push(ProvenanceEdge {
            predicate: "REFINES".into(),
            target_kind: "synthesis".into(),
            target_id: parent,
        });
    }
    for &prereq in prereq_synthesis_ids {
        edges.push(ProvenanceEdge {
            predicate: "COMPOSED_OF".into(),
            target_kind: "synthesis".into(),
            target_id: prereq,
        });
    }
    edges.push(ProvenanceEdge {
        predicate: "ATTRIBUTED_TO".into(),
        target_kind: "agent".into(),
        target_id: owner_agent_id,
    });

    let mut tx = pool
        .begin()
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    SynthesisProvoEdgesRepository::plan(&mut tx, synthesis_id, &edges)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(())
}
