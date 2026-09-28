//! Stage 6 — Publish.
//!
//! Stage 6 takes a fully-narrated synthesis and, each step on the caller's
//! connection (the worker's stage transaction, stamped as the synthesis'
//! acting principal):
//!
//! 1. **Plans** PROV-O provenance edges (`stage6_plan_edges_conn`) — one
//!    `WAS_DERIVED_FROM` per cited claim, one `REFINES` for the parent
//!    synthesis (if any), one `COMPOSED_OF` per prerequisite synthesis, and
//!    one `ATTRIBUTED_TO` for the owning agent. Rows go into
//!    `synthesis_provo_edges` with `written_at IS NULL`.
//!
//! 2. **Embeds** the narrative head ([`narrative_head`]) — first paragraph or
//!    first 1000 chars; the handler embeds it with no transaction open and
//!    stores it in `synthesis_embeddings`.
//!
//! 3. **Hashes** the canonical (query, snapshot, narrative) tuple
//!    (`compute_content_hash`) — pure BLAKE3 over deterministic JSON. Used
//!    for cache keying and idempotency.
//!
//! 4. **Writes** the kernel PROV edges IN PROCESS (`stage6_write_edges_conn`)
//!    — public and publishable syntheses only (otherwise the rows are
//!    deferred `private`); one kernel `edges` row and one `edge.added` event
//!    per pending row.
//!
//! 5. **Marks complete** (`stage6_mark_complete_conn`) — only when zero edges
//!    remain pending; otherwise refuses with [`SynthesisError::EdgeWrite`].
//!    The underlying `save_narrative` call sets narrative + content_hash +
//!    status='complete' + completed_at atomically.
//!
//! Pending rows a failed or narrowed-then-widened synthesis leaves behind are
//! written later by the worker's `stage6_pending` worklist, as the
//! synthesis' job principal.
//!
//! All substeps are free functions (not methods on `SynthesisPipeline`) so
//! Stage 6 stays decoupled from the `L: LlmClient` / `P: EdgeProvider`
//! generics that earlier stages need.

use uuid::Uuid;

use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::{ProvenanceEdge, SubgraphSnapshot};

use crate::{SynthesisProvoEdgesRepository, SynthesisRepository};

/// Documented per-call cap on how many embeddings a single Stage 6 invocation
/// is willing to generate. Stage 6 only embeds one head string, so this is
/// effectively documentation for downstream batch flows (Phase 4 staleness
/// re-embedding) — kept here next to the embed step for discoverability.
pub const MAX_EMBEDDING_BATCH: usize = 500;

// ──────────────────────────────────────────────────────────────────────────────
// 6a — plan
// ──────────────────────────────────────────────────────────────────────────────

/// Stage 6a — Plan provenance edges, on the caller's connection (inside the
/// caller's transaction).
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
// 6b — the narrative head
// ──────────────────────────────────────────────────────────────────────────────

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
// 6c — compute_content_hash
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
// Publishability
// ──────────────────────────────────────────────────────────────────────────────

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
// 6d, 6e — kernel PROV edges and events in process, on the caller's
// (owner-stamped) transaction, no service credential; then completion.
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
    /// Unwritten claim-target rows discarded because no cluster of the
    /// synthesis cites their claim
    /// ([`SynthesisProvoEdgesRepository::discard_uncited_unwritten`]).
    pub discarded: u64,
    /// The first failure, if any; the row carries it in `last_error` and one
    /// more `attempt_count`, and the remaining rows were not attempted.
    pub failure: Option<String>,
}

/// Stage 6d, in process: write this synthesis' pending outbox rows as kernel
/// `edges` rows on `conn`, which the caller has stamped as the synthesis'
/// acting principal (the worker). Per edge, an `edge.added` event goes into
/// the kernel `events` table on the same connection, with `actor` as its
/// actor.
///
/// Public only (E1d): a synthesis that is not
/// public AND publishable (asked of the database's own rule on this
/// connection) gets its unwritten rows deferred as `private`, and no edge and
/// no event are written. Each edge is inserted under a SAVEPOINT, so a
/// refused insert rolls back only itself and its failure can be recorded
/// on the row before the call returns.
///
/// First, every unwritten claim-target row whose claim no cluster of the
/// synthesis cites is discarded
/// ([`SynthesisProvoEdgesRepository::discard_uncited_unwritten`]), so no
/// kernel edge ever names a claim the synthesis does not cite, whichever
/// attempt planned the row and whether or not the replan could see it.
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

    // Never name an uncited claim: an unwritten row whose claim no cluster
    // cites (left by an earlier attempt, including one the replan could not
    // see) is discarded before anything is written or deferred.
    let discarded =
        SynthesisProvoEdgesRepository::discard_uncited_unwritten(&mut *conn, synthesis_id)
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
    let mut outcome = EdgeWriteOutcome {
        discarded,
        ..EdgeWriteOutcome::default()
    };
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

/// Stage 6e — Mark the synthesis complete, on the caller's connection:
/// refuses while any outbox row is still pending (rows deferred as `private`
/// are not pending: they wait for the synthesis to become public), then
/// stores the narrative and marks the synthesis complete through
/// `SynthesisRepository::save_narrative` (narrative, `narrative_format`,
/// `content_hash`, `status='complete'`, `completed_at`, in one UPDATE of
/// exactly one row, or an error).
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
