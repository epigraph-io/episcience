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
//! 6. **Reconciles** on startup (`reconcile_stage6_on_startup`) — finds
//!    syntheses where `status='complete'` but provo edges are still pending
//!    (a crash between `stage6_write_edges` and `stage6_mark_complete` can
//!    leave the synthesis "complete" but with unwritten edges if the worker
//!    is later restarted; or if a future code path commits the narrative
//!    before all writes succeed). Replays the writes synchronously, logging
//!    failures so one bad synthesis can't block the whole reconcile.
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
use crate::{SynthesisEmbeddingsRepository, SynthesisProvoEdgesRepository, SynthesisRepository};

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
/// Builds the canonical edge set for this synthesis and makes it the
/// synthesis' planned outbox (`written_at IS NULL`): every unwritten row of an
/// earlier attempt (pending or deferred) is discarded first, so a retry that
/// cites a different claim set never leaves a row naming a claim it dropped;
/// rows already written stay (their kernel edges exist). Repeat invocations
/// are safe: the insert uses `ON CONFLICT DO NOTHING` on the PRIMARY KEY
/// `(synthesis_id, predicate, target_kind, target_id)`, which a written row
/// keeps.
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
/// - optional `REFINES` to `workflow_run_id`
///   (`target_kind = "workflow"`) — correlation edge linking this synthesis
///   to the EpiGraph workflow run that triggered it. `None` for syntheses
///   triggered directly (REST / MCP).
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
    workflow_run_id: Option<Uuid>,
) -> Result<(), SynthesisError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    stage6_plan_edges_conn(
        &mut tx,
        synthesis_id,
        cited_claim_ids,
        parent_synthesis_id,
        prereq_synthesis_ids,
        owner_agent_id,
        workflow_run_id,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(())
}

/// [`stage6_plan_edges`] on the caller's connection (inside the caller's
/// transaction).
///
/// # Errors
/// [`SynthesisError::Db`] on any insert failure.
pub async fn stage6_plan_edges_conn(
    conn: &mut sqlx::PgConnection,
    synthesis_id: Uuid,
    cited_claim_ids: &[Uuid],
    parent_synthesis_id: Option<Uuid>,
    prereq_synthesis_ids: &[Uuid],
    owner_agent_id: Uuid,
    workflow_run_id: Option<Uuid>,
) -> Result<(), SynthesisError> {
    let mut edges = Vec::with_capacity(cited_claim_ids.len() + prereq_synthesis_ids.len() + 3);
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
    if let Some(wf_id) = workflow_run_id {
        edges.push(ProvenanceEdge {
            predicate: "REFINES".into(),
            target_kind: "workflow".into(),
            target_id: wf_id,
        });
    }

    // Replace, never accumulate: an earlier attempt's unwritten rows may name
    // claims this attempt no longer cites.
    SynthesisProvoEdgesRepository::replace_unwritten(&mut *conn, synthesis_id, &edges)
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
    let head_trimmed = narrative_head(narrative);
    let embedding = embedder
        .generate(head_trimmed)
        .await
        .map_err(|e| SynthesisError::Llm(format!("embed: {e}")))?;
    SynthesisEmbeddingsRepository::upsert(pool, synthesis_id, &embedding, model, "narrative_head")
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(())
}

/// The text stage 6 embeds: the first paragraph of `narrative` (split on a
/// blank line), cut to at most 1000 bytes on a character boundary.
pub fn narrative_head(narrative: &str) -> &str {
    // Take the first paragraph (split on blank line) or the whole narrative
    // if it has no paragraph break, then truncate to ≤1000 bytes.
    let head = narrative.split("\n\n").next().unwrap_or(narrative);
    // `head.len().min(1000)` is byte-based; but slicing a string by bytes
    // can split a multi-byte UTF-8 codepoint. Use a char-boundary-safe cut.
    let cut = head.len().min(1000);
    if head.is_char_boundary(cut) {
        &head[..cut]
    } else {
        // Walk back to the previous char boundary. At most 3 bytes for UTF-8.
        let mut c = cut;
        while c > 0 && !head.is_char_boundary(c) {
            c -= 1;
        }
        &head[..c]
    }
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
pub fn compute_content_hash(query: &str, snapshot: &SubgraphSnapshot, narrative: &str) -> [u8; 32] {
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
    // Public-only (E1d): a kernel PROV edge names this synthesis to everyone,
    // so it is written only for a synthesis that is public AND publishable
    // (every member claim, the parent and every prerequisite public). Any
    // other synthesis' unwritten rows are deferred as `private` and nothing
    // is POSTed; widening it to public later clears the deferral, and the
    // next reconcile writes them.
    if !is_publishable(pool, synthesis_id).await? {
        SynthesisProvoEdgesRepository::defer_unwritten(pool, synthesis_id, "private")
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
        return Ok(());
    }
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

/// Whether synthesis `id` may be named in public: it is `public`, every member
/// claim is public, its parent (if any) is public, and every prerequisite
/// exists and is public. The database's publish rule (5035) narrows a public
/// synthesis that fails this at completion; this check lets stage 6 withhold
/// its kernel edges and `synthesis.*` events BEFORE that happens (the edges
/// are written before the status flips).
///
/// Asks the database's own rule (`episcience_synthesis_is_publishable`), so
/// the answer is the guard's: on a row-secured session the member half is
/// counted over ALL membership rows (a claim narrowed out of the caller's
/// reach hides its membership row, and a count over the session's rows alone
/// would call the synthesis publishable), and a hidden parent or
/// prerequisite counts as non-public.
pub async fn is_publishable<'e, E>(executor: E, id: Uuid) -> Result<bool, SynthesisError>
where
    E: sqlx::PgExecutor<'e>,
{
    let ok: Option<bool> = sqlx::query_scalar(
        "SELECT s.visibility = 'public'
            AND public.episcience_synthesis_is_publishable(s.id, s.parent_synthesis_id, s.prereq_synthesis_ids)
           FROM syntheses s WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(ok.unwrap_or(false))
}

// ──────────────────────────────────────────────────────────────────────────────
// 2.7e — stage6_mark_complete
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6e — Mark the synthesis complete.
///
/// Refuses to mark complete if any provo edges are still pending — a
/// "complete" synthesis must have all its provenance written (rows deferred
/// as `private` are not pending: they wait for the synthesis to become
/// public). On precondition
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

// ──────────────────────────────────────────────────────────────────────────────
// 2.7f — reconcile_stage6_on_startup
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6f — Reconcile pending edges on worker startup.
///
/// Finds every synthesis where `status='complete'` but at least one provo
/// edge is still unwritten and not deferred, and replays
/// [`stage6_write_edges`] for each (which re-checks publishability).
/// Failures are logged and the loop continues — one bad synthesis must not
/// block reconciliation of the rest.
///
/// In v1 this runs synchronously. Phase 2.8a will wire up
/// `EpiscienceJobQueue` (B-CKL-13); when that lands, this function should be
/// updated to enqueue retries instead of replaying inline. For now,
/// "reconcile" means "drain on startup".
// TODO(B-CKL-13 / Task 2.8a): take `&dyn JobQueue` and enqueue retries
// instead of synchronous replay, once `EpiscienceJobQueue` exists.
pub async fn reconcile_stage6_on_startup(
    pool: &PgPool,
    edges_client: &dyn EdgeWriter,
) -> Result<(), SynthesisError> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT s.id FROM syntheses s
         WHERE s.status = 'complete'
           AND EXISTS (
             SELECT 1 FROM synthesis_provo_edges pe
             WHERE pe.synthesis_id = s.id AND pe.written_at IS NULL
               AND pe.deferred_reason IS NULL
           )",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| SynthesisError::Db(e.to_string()))?;

    for (synthesis_id,) in rows {
        if let Err(e) = stage6_write_edges(pool, edges_client, synthesis_id).await {
            tracing::warn!(
                synthesis_id = %synthesis_id,
                error = %e,
                "stage 6 reconciliation failed for synthesis; continuing",
            );
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// In-process stage 6 (E1f): kernel PROV edges and events on the caller's
// (owner-stamped) transaction, no service credential.
// ──────────────────────────────────────────────────────────────────────────────

/// The four PROV predicates stage 6 plans, each with the one target kind it
/// may name (`REFINES` also names a workflow run). The kernel's HTTP edge
/// route validated the predicate against its allowlist; the in-process
/// writer calls the repository directly, so it validates against this fixed
/// set instead and never writes anything else under a synthesis' name.
pub const PROVO_EDGE_SHAPES: [(&str, &str); 5] = [
    ("WAS_DERIVED_FROM", "claim"),
    ("REFINES", "synthesis"),
    ("REFINES", "workflow"),
    ("COMPOSED_OF", "synthesis"),
    ("ATTRIBUTED_TO", "agent"),
];

/// Whether `(predicate, target_kind)` is one of [`PROVO_EDGE_SHAPES`].
pub fn is_provo_edge_shape(predicate: &str, target_kind: &str) -> bool {
    PROVO_EDGE_SHAPES
        .iter()
        .any(|(p, k)| *p == predicate && *k == target_kind)
}

/// What an in-process stage-6 write did. The caller COMMITS its transaction
/// in every case (the written edges, their events and a failed row's attempt
/// counter are all meant to persist), then treats `failure` as the stage's
/// error.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EdgeWriteOutcome {
    /// Kernel edge ids written by this call, in outbox order.
    pub written: Vec<Uuid>,
    /// Rows newly deferred as `private` (the synthesis is not publishable).
    pub deferred: u64,
    /// The first failure, if any; the row carries it in `last_error` and one
    /// more `attempt_count`, and the remaining rows were not attempted.
    pub failure: Option<String>,
}

/// Stage 6d, in process: write this synthesis' pending outbox rows as kernel
/// `edges` rows on `conn`, which the caller has stamped as the synthesis'
/// acting principal (the worker) or which is privileged (the legacy
/// in-process runner). Per edge, an `edge.added` event goes into the kernel
/// `events` table on the same connection, with `actor` as its actor.
///
/// Public only, exactly as the HTTP path was (E1d): a synthesis that is not
/// public AND publishable (asked of the database's own rule on this
/// connection) gets its unwritten rows deferred as `private`, and no edge and
/// no event are written. Each edge is inserted under a SAVEPOINT, so a
/// refused insert rolls back only itself and its failure can be recorded
/// on the row before the call returns.
///
/// # Errors
/// [`SynthesisError::Db`] when the outbox itself cannot be read or updated
/// (the caller rolls back). An edge the kernel refuses is NOT an `Err`: it
/// is reported in [`EdgeWriteOutcome::failure`] so the caller can commit the
/// recorded attempt.
pub async fn stage6_write_edges_conn(
    conn: &mut sqlx::PgConnection,
    synthesis_id: Uuid,
    actor: Option<Uuid>,
) -> Result<EdgeWriteOutcome, SynthesisError> {
    use sqlx::Acquire;

    let mut outcome = EdgeWriteOutcome::default();
    if !is_publishable(&mut *conn, synthesis_id).await? {
        outcome.deferred =
            SynthesisProvoEdgesRepository::defer_unwritten(&mut *conn, synthesis_id, "private")
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
        return Ok(outcome);
    }
    let pending = SynthesisProvoEdgesRepository::list_pending(&mut *conn, synthesis_id)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    for edge in pending {
        let written: Result<Uuid, String> =
            if !is_provo_edge_shape(&edge.predicate, &edge.target_kind) {
                Err(format!(
                    "refused: ({}, {}) is not a synthesis PROV edge shape",
                    edge.predicate, edge.target_kind
                ))
            } else {
                let mut sp = conn
                    .begin()
                    .await
                    .map_err(|e| SynthesisError::Db(e.to_string()))?;
                match epigraph_db::EdgeRepository::create(
                    &mut *sp,
                    synthesis_id,
                    "synthesis",
                    edge.target_id,
                    &edge.target_kind,
                    &edge.predicate,
                    None,
                    None,
                    None,
                )
                .await
                {
                    Ok(id) => {
                        sp.commit()
                            .await
                            .map_err(|e| SynthesisError::Db(e.to_string()))?;
                        Ok(id)
                    }
                    Err(e) => {
                        sp.rollback()
                            .await
                            .map_err(|re| SynthesisError::Db(re.to_string()))?;
                        Err(e.to_string())
                    }
                }
            };
        match written {
            Ok(edge_id) => {
                SynthesisProvoEdgesRepository::mark_written(
                    &mut *conn,
                    synthesis_id,
                    &edge.predicate,
                    &edge.target_kind,
                    edge.target_id,
                    edge_id,
                )
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
                epigraph_db::EventRepository::publish_or_log_conn(
                    &mut *conn,
                    "edge.added",
                    actor,
                    &serde_json::json!({
                        "edge_id": edge_id,
                        "source_type": "synthesis",
                        "source_id": synthesis_id,
                        "target_type": edge.target_kind,
                        "target_id": edge.target_id,
                        "relationship": edge.predicate,
                    }),
                )
                .await;
                outcome.written.push(edge_id);
            }
            Err(msg) => {
                SynthesisProvoEdgesRepository::record_failure(
                    &mut *conn,
                    synthesis_id,
                    &edge.predicate,
                    &edge.target_kind,
                    edge.target_id,
                    &msg,
                )
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
                outcome.failure = Some(msg);
                return Ok(outcome);
            }
        }
    }
    let remaining = SynthesisProvoEdgesRepository::count_pending(&mut *conn, synthesis_id)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    if remaining > 0 {
        outcome.failure = Some(format!("{remaining} edges still pending after write loop"));
    }
    Ok(outcome)
}

/// Publish a `synthesis.*` event in process, on `conn`, only when the
/// synthesis is public AND publishable (asked on the same connection): the
/// kernel `events` table is readable without a group check and the payload
/// carries the query text. Best effort, like the HTTP path it replaces
/// (`publish_or_log_conn` runs under a SAVEPOINT and logs a failure).
/// Returns whether an event was written.
pub async fn publish_synthesis_event_conn(
    conn: &mut sqlx::PgConnection,
    synthesis_id: Uuid,
    event_type: &str,
    actor: Option<Uuid>,
    payload: &serde_json::Value,
) -> bool {
    match is_publishable(&mut *conn, synthesis_id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(event_type, %synthesis_id, "event withheld: synthesis is not public");
            return false;
        }
        Err(e) => {
            tracing::warn!(event_type, %synthesis_id, error = %e, "event withheld: publishability check failed");
            return false;
        }
    }
    epigraph_db::EventRepository::publish_or_log_conn(&mut *conn, event_type, actor, payload)
        .await
        .is_some()
}

/// Stage 6f, in process: the startup reconcile of [`reconcile_stage6_on_startup`]
/// on `pool` (the legacy in-process runner's privileged pool), each synthesis
/// in its own transaction through [`stage6_write_edges_conn`]. The worker
/// does not call this: its `stage6_pending` worklist replaces it.
///
/// # Errors
/// [`SynthesisError::Db`] if the candidate list cannot be read; per-synthesis
/// failures are logged and the loop continues.
pub async fn reconcile_stage6_inprocess(pool: &PgPool) -> Result<(), SynthesisError> {
    let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT s.id, j.principal_id FROM syntheses s
           LEFT JOIN synthesis_jobs j ON j.id = s.id
         WHERE s.status = 'complete'
           AND EXISTS (
             SELECT 1 FROM synthesis_provo_edges pe
             WHERE pe.synthesis_id = s.id AND pe.written_at IS NULL
               AND pe.deferred_reason IS NULL
           )",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| SynthesisError::Db(e.to_string()))?;

    for (synthesis_id, principal) in rows {
        // Act as the synthesis' job principal (D-S9); a synthesis with no job
        // principal is skipped, never written with a NULL actor.
        let Some(principal) = principal else {
            tracing::warn!(
                %synthesis_id,
                "stage 6 reconciliation skipped: the synthesis has no job principal",
            );
            continue;
        };
        let result = async {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
            let outcome = stage6_write_edges_conn(&mut tx, synthesis_id, Some(principal)).await?;
            tx.commit()
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
            match outcome.failure {
                Some(f) => Err(SynthesisError::EdgeWrite(f)),
                None => Ok(()),
            }
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(
                synthesis_id = %synthesis_id,
                error = %e,
                "stage 6 reconciliation failed for synthesis; continuing",
            );
        }
    }
    Ok(())
}

/// [`stage6_mark_complete`] on the caller's connection: refuses while any
/// outbox row is still pending, then stores the narrative and marks the
/// synthesis complete (exactly one row, or an error).
///
/// # Errors
/// [`SynthesisError::EdgeWrite`] while edges are pending;
/// [`SynthesisError::Db`] on a write failure.
pub async fn stage6_mark_complete_conn(
    conn: &mut sqlx::PgConnection,
    synthesis_id: Uuid,
    narrative: &str,
    content_hash: &[u8; 32],
) -> Result<(), SynthesisError> {
    let pending = SynthesisProvoEdgesRepository::count_pending(&mut *conn, synthesis_id)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    if pending > 0 {
        return Err(SynthesisError::EdgeWrite(format!(
            "cannot mark complete: {pending} edges pending"
        )));
    }
    SynthesisRepository::save_narrative(&mut *conn, synthesis_id, narrative, content_hash)
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The in-process writer's shape set is exactly the five (predicate,
    /// target kind) pairs stage 6 plans (brief E1f requirement 4: the fixed
    /// predicates pinned by a unit test). Kills: a predicate or target kind
    /// added to, or dropped from, what the worker may write under a
    /// synthesis' name.
    #[test]
    fn the_prov_edge_shapes_are_exactly_the_planned_five() {
        let mut shapes: Vec<(&str, &str)> = PROVO_EDGE_SHAPES.to_vec();
        shapes.sort_unstable();
        assert_eq!(
            shapes,
            vec![
                ("ATTRIBUTED_TO", "agent"),
                ("COMPOSED_OF", "synthesis"),
                ("REFINES", "synthesis"),
                ("REFINES", "workflow"),
                ("WAS_DERIVED_FROM", "claim"),
            ]
        );
        assert!(is_provo_edge_shape("WAS_DERIVED_FROM", "claim"));
        assert!(!is_provo_edge_shape("ATTRIBUTED_TO", "claim"));
        assert!(!is_provo_edge_shape("REFINES", "claim"));
        assert!(!is_provo_edge_shape("supports", "claim"));
    }
}
