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
//! 3. **Hashes** the canonical (query, snapshot, narrative) tuple
//!    (`compute_content_hash`) — pure BLAKE3 over deterministic JSON. Used
//!    for cache keying and idempotency.
//!
//! 4. **Writes** edges to EpiGraph via [`EdgeWriter`] (`stage6_write_edges`)
//!    — for each pending row, POST `/edges`, mark written on success or
//!    record failure on error.
//!
//! 5. **Marks complete** (`stage6_mark_complete`) — only when zero edges
//!    remain pending; otherwise refuses with [`SynthesisError::EdgeWrite`].
//!    The underlying `save_narrative` call sets narrative + content_hash +
//!    status='complete' + completed_at atomically.
//!
//! The reconciliation entry point lands in a follow-on commit.
//!
//! All substeps are free functions (not methods on `SynthesisPipeline`) so
//! Stage 6 stays decoupled from the `L: LlmClient` / `P: EdgeProvider`
//! generics that earlier stages need. Callers either invoke them directly or
//! the pipeline runner threads them at the end of the synthesis flow.

use sqlx::PgPool;
use uuid::Uuid;

use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::{ProvenanceEdge, SubgraphSnapshot};

use crate::synthesis::edge_writer::{EdgeRequest, EdgeWriter};
use crate::{
    SynthesisEmbeddingsRepository, SynthesisProvoEdgesRepository, SynthesisRepository,
};

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

// ──────────────────────────────────────────────────────────────────────────────
// 2.7c — compute_content_hash
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6c — Compute the canonical content hash.
///
/// BLAKE3 over the concatenation of:
///   - `query` bytes
///   - canonical JSON serialization of `snapshot`
///   - `narrative` bytes
///
/// The same triple always produces the same 32-byte digest; any change to
/// any input changes the digest. Used by the cache layer (Phase 3) to detect
/// whether a previously-computed synthesis is still valid for a re-issued
/// query.
///
/// Pure function — no DB, no async, no panics on well-formed inputs. The
/// `serde_json::to_string` call cannot fail for `SubgraphSnapshot` (all
/// fields serialize cleanly), so we `expect()`; if the type ever grows a
/// non-serializable field, the test suite catches the regression.
pub fn compute_content_hash(
    query: &str,
    snapshot: &SubgraphSnapshot,
    narrative: &str,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(query.as_bytes());
    let canonical = serde_json::to_string(snapshot).expect("SubgraphSnapshot serializes");
    hasher.update(canonical.as_bytes());
    hasher.update(narrative.as_bytes());
    *hasher.finalize().as_bytes()
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7d — stage6_write_edges
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6d — Write planned edges to EpiGraph.
///
/// Drains every `synthesis_provo_edges` row with `written_at IS NULL`, POSTs
/// each to the edges service via [`EdgeWriter`], and marks the row written
/// on success. On the first failure, records the error against the row,
/// surfaces it as [`SynthesisError::EdgeWrite`], and stops — partial
/// progress is preserved (already-written rows stay written) so a retry
/// only re-attempts the failed and remaining rows.
///
/// After successful drain, the function double-checks `count_pending == 0`;
/// any nonzero count is treated as a logic bug and surfaces as
/// [`SynthesisError::EdgeWrite`].
pub async fn stage6_write_edges(
    pool: &PgPool,
    edges_client: &dyn EdgeWriter,
    synthesis_id: Uuid,
) -> Result<(), SynthesisError> {
    let pending = SynthesisProvoEdgesRepository::list_pending(pool, synthesis_id)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    for edge in pending {
        let req = EdgeRequest {
            source_type: "synthesis".into(),
            source_id: synthesis_id,
            target_type: edge.target_kind.clone(),
            target_id: edge.target_id,
            relationship: edge.predicate.clone(),
        };
        match edges_client.create_edge(req).await {
            Ok(edge_id) => {
                SynthesisProvoEdgesRepository::mark_written(
                    pool,
                    synthesis_id,
                    &edge.predicate,
                    &edge.target_kind,
                    edge.target_id,
                    edge_id,
                )
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
            }
            Err(e) => {
                let err_msg = e.to_string();
                SynthesisProvoEdgesRepository::record_failure(
                    pool,
                    synthesis_id,
                    &edge.predicate,
                    &edge.target_kind,
                    edge.target_id,
                    &err_msg,
                )
                .await
                .map_err(|db_e| SynthesisError::Db(db_e.to_string()))?;
                return Err(SynthesisError::EdgeWrite(err_msg));
            }
        }
    }
    let remaining = SynthesisProvoEdgesRepository::count_pending(pool, synthesis_id)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    if remaining > 0 {
        return Err(SynthesisError::EdgeWrite(format!(
            "{remaining} edges still pending after write loop"
        )));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7e — stage6_mark_complete
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6e — Mark the synthesis complete.
///
/// Refuses to mark complete if any provo edges are still pending — a
/// "complete" synthesis must have all its provenance written. On precondition
/// success, delegates to `SynthesisRepository::save_narrative`, which sets
/// `narrative`, `narrative_format='markdown'`, `content_hash`,
/// `status='complete'`, and `completed_at=now()` in a single UPDATE.
pub async fn stage6_mark_complete(
    pool: &PgPool,
    synthesis_id: Uuid,
    narrative: &str,
    content_hash: &[u8; 32],
) -> Result<(), SynthesisError> {
    let pending = SynthesisProvoEdgesRepository::count_pending(pool, synthesis_id)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    if pending > 0 {
        return Err(SynthesisError::EdgeWrite(format!(
            "cannot mark complete: {pending} edges pending"
        )));
    }
    SynthesisRepository::save_narrative(pool, synthesis_id, narrative, content_hash)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(())
}
