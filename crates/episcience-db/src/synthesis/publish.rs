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
//! 2. **Embeds** the narrative head (`stage6_embed_narrative`) — first
//!    paragraph or first 1000 chars, embedded via the supplied
//!    [`EmbeddingService`] and upserted into `synthesis_embeddings`.
//!
//! Subsequent substeps (hash, write, mark-complete, reconcile) land in
//! follow-on commits in the same module.
//!
//! All substeps are free functions (not methods on `SynthesisPipeline`) so
//! Stage 6 stays decoupled from the `L: LlmClient` / `P: EdgeProvider`
//! generics that earlier stages need. Callers either invoke them directly or
//! the pipeline runner threads them at the end of the synthesis flow.

use sqlx::PgPool;
use uuid::Uuid;

use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::ProvenanceEdge;

use crate::{SynthesisEmbeddingsRepository, SynthesisProvoEdgesRepository};

/// Documented per-call cap on how many embeddings a single Stage 6 invocation
/// is willing to generate. Stage 6 only embeds one head string, so this is
/// effectively documentation for downstream batch flows (Phase 4 staleness
/// re-embedding) — kept here next to the embed step for discoverability.
pub const MAX_EMBEDDING_BATCH: usize = 500;

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

// ──────────────────────────────────────────────────────────────────────────────
// 2.7b — stage6_embed_narrative
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6b — Embed the narrative head.
///
/// Takes the first paragraph of `narrative` (split on blank line) or the
/// first 1000 chars, whichever is smaller, and embeds it via the supplied
/// [`epigraph_embeddings::EmbeddingService`]. The result is upserted into
/// `synthesis_embeddings` with `embedding_input = 'narrative_head'` and
/// `embedding_model = model`.
///
/// The model name is taken as a parameter rather than read from the embedder
/// — the upstream `EmbeddingService` trait does not expose a model accessor,
/// and the `synthesis_embeddings` table requires a non-NULL string for audit.
/// Callers are expected to pass the same model identifier they configured
/// the embedder with.
///
/// 1000 chars is a soft heuristic to keep the embedding focused on the
/// thesis sentence and avoid pulling in the entire claim citation tail —
/// the head paragraph is a much better representation of "what this
/// synthesis is about" than the whole document.
pub async fn stage6_embed_narrative(
    pool: &PgPool,
    embedder: &dyn epigraph_embeddings::EmbeddingService,
    synthesis_id: Uuid,
    narrative: &str,
    model: &str,
) -> Result<(), SynthesisError> {
    // Take the first paragraph (split on blank line) or the whole narrative
    // if it has no paragraph break, then truncate to ≤1000 bytes.
    let head = narrative.split("\n\n").next().unwrap_or(narrative);
    // `head.len().min(1000)` is byte-based; but slicing a string by bytes
    // can split a multi-byte UTF-8 codepoint. Use a char-boundary-safe cut.
    let cut = head.len().min(1000);
    let head_trimmed = if head.is_char_boundary(cut) {
        &head[..cut]
    } else {
        // Walk back to the previous char boundary. At most 3 bytes for UTF-8.
        let mut c = cut;
        while c > 0 && !head.is_char_boundary(c) {
            c -= 1;
        }
        &head[..c]
    };

    let embedding = embedder
        .generate(head_trimmed)
        .await
        .map_err(|e| SynthesisError::Llm(format!("embed: {e}")))?;
    SynthesisEmbeddingsRepository::upsert(pool, synthesis_id, &embedding, model, "narrative_head")
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(())
}
