//! `JobHandler` impl that drives the full 6-stage synthesis pipeline.
//!
//! # Why wrappers?
//!
//! [`SynthesisPipeline`](episcience_db::SynthesisPipeline) is generic over its
//! `LlmClient` and `EdgeProvider` parameters. The job handler holds those
//! dependencies as `Arc<dyn Trait>` so it can be constructed once and shared
//! across worker threads. But trait-object generics like `Arc<dyn LlmClient>`
//! do not auto-implement the underlying trait, so we wrap each `Arc` in a
//! tiny newtype that delegates to the inner trait object:
//!
//! - [`ArcLlm`]   wraps `Arc<dyn LlmClient + Send + Sync>` and re-implements
//!   [`epigraph_cli::enrichment::llm_client::LlmClient`] (incl. `Debug`,
//!   which is a supertrait).
//! - [`ArcEdgeProvider`] wraps `Arc<dyn EdgeProvider + Send + Sync>` and
//!   re-implements [`episcience_core::synthesis::traversal::EdgeProvider`].
//!
//! The plan considered refactoring `SynthesisPipeline<L, P>` to take
//! `Arc<dyn ...>` directly. That would touch all five existing pipeline tests
//! and all production callers; the wrapper approach is local to this module
//! and leaves the existing tests untouched.
//!
//! # `EmptyEdgeProvider`
//!
//! Phase 2 v1 ships with [`EmptyEdgeProvider`], a stub that returns no
//! neighbours. With it, Stage 2 traversal degenerates to "seed claims only"
//! and Stage 3 clustering treats every claim as its own cluster (capped at
//! 12 by `cluster_signed`). Phase 4 / B-CKL will replace it with a real
//! provider backed by the upstream `claim_relationships` table or the
//! epigraph HTTP API.
//!
//! # Edge metadata in Stage 3
//!
//! `SubgraphSnapshot.edge_ids` is a `Vec<Uuid>` — bare ids, no `(src, dst,
//! type)` tuples. Stage 3 wants the typed tuples to compute signed weights.
//! Recovering the metadata would require either storing `(src, dst, type)`
//! triples in the snapshot (schema change) or re-querying the edge provider
//! per claim pair (N² calls). Phase 2 v1 takes the simple path: pass an
//! empty edge list to `stage3_cluster`, which means clusters are
//! purely-id-based and every claim becomes its own singleton (still capped
//! at 12). Phase 4 / B-CKL track #43 will revisit.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use epigraph_cli::enrichment::llm_client::{LlmClient, LlmError};
use epigraph_embeddings::EmbeddingService;
use epigraph_jobs::{Job, JobError, JobHandler, JobResult, JobResultMetadata};
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType, TraversalConfig};
use episcience_core::synthesis::SynthesisStatus;
use episcience_db::synthesis::edge_writer::EdgeWriter;
use episcience_db::{SynthesisPipeline, SynthesisRepository};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

// ─── Payload ────────────────────────────────────────────────────────────────

/// Wire-format payload stored in `synthesis_jobs.payload` for `job_type =
/// "synthesis"`. Constructed by the enqueue path (Phase 3) and consumed here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthesisJobPayload {
    pub synthesis_id: Uuid,
    pub query: String,
    /// JSON form of `episcience_core::synthesis::traversal::TraversalConfig`.
    /// `None` means "use defaults"; an unparseable JSON value also falls back
    /// to defaults (we don't fail the job on a malformed knob).
    pub traversal_config: Option<serde_json::Value>,
    pub agent_id: Uuid,
    pub parent_synthesis_id: Option<Uuid>,
    #[serde(default)]
    pub prereq_synthesis_ids: Vec<Uuid>,
}

// ─── Wrapper newtypes for trait-object generics ──────────────────────────────

/// Adapter that lets an `Arc<dyn LlmClient + Send + Sync>` satisfy the
/// `LlmClient` trait directly. Required because `SynthesisPipeline<L, P>`'s
/// LLM-bound impls take `L: LlmClient` (not `L: Deref<Target = dyn LlmClient>`)
/// and trait objects do not auto-implement their own trait.
pub struct ArcLlm(pub Arc<dyn LlmClient + Send + Sync>);

impl fmt::Debug for ArcLlm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `LlmClient: Debug`, so the inner trait object also implements it.
        // Delegate so we don't lose the model identifier in error logs.
        write!(f, "ArcLlm({:?})", &*self.0)
    }
}

#[async_trait]
impl LlmClient for ArcLlm {
    async fn complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError> {
        self.0.complete_json(prompt).await
    }

    fn model_name(&self) -> &str {
        self.0.model_name()
    }
}

/// Adapter that lets an `Arc<dyn EdgeProvider + Send + Sync>` satisfy the
/// `EdgeProvider` trait directly. Same rationale as [`ArcLlm`].
pub struct ArcEdgeProvider(pub Arc<dyn EdgeProvider + Send + Sync>);

#[async_trait]
impl EdgeProvider for ArcEdgeProvider {
    async fn neighbors(&self, claim: Uuid, types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
        self.0.neighbors(claim, types).await
    }
}

// ─── Phase-2 stub edge provider ──────────────────────────────────────────────

/// Phase 2 v1 stub: returns no neighbours for any claim.
///
/// With this provider, Stage 2 traversal is identity: the snapshot's
/// `claim_ids` equals `seeds` and `edge_ids` is empty. Phase 4 (B-CKL) will
/// replace this with a real provider that queries either the upstream
/// `claim_relationships` table or the epigraph HTTP API.
// TODO(B-CKL Phase 4): real provider backed by claim_relationships / HTTP.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyEdgeProvider;

#[async_trait]
impl EdgeProvider for EmptyEdgeProvider {
    async fn neighbors(&self, _claim: Uuid, _types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
        Vec::new()
    }
}

// ─── Handler ─────────────────────────────────────────────────────────────────

/// `JobHandler` that drives a single synthesis through all 6 pipeline stages.
///
/// Construction is one-time and via `Arc<dyn …>` for shared dependencies
/// (embedder, LLM, edge writer, edge provider). On each `handle` call the
/// handler:
///
/// 1. Marks the synthesis row `running`.
/// 2. Builds a fresh [`SynthesisPipeline`] with the per-job query embedding.
/// 3. Runs Stages 1-5 sequentially; any error transitions the synthesis to
///    `failed` and surfaces as `JobError::ProcessingFailed`.
/// 4. Runs Stage 6 substeps (plan → embed → hash → write → mark complete).
/// 5. Returns `JobResult` with the synthesis id in the output payload.
///
/// The handler is `Send + Sync + Clone` so the job runner can share one
/// instance across worker tasks.
#[derive(Clone)]
pub struct SynthesisJobHandler {
    pub pool: PgPool,
    pub embedder: Arc<dyn EmbeddingService>,
    pub llm: Arc<dyn LlmClient + Send + Sync>,
    pub edges_writer: Arc<dyn EdgeWriter>,
    pub edge_provider: Arc<dyn EdgeProvider + Send + Sync>,
    pub cost_budget: u32,
    /// Stored alongside the embedding for audit; passed to
    /// [`stage6_embed_narrative`] which writes it to
    /// `synthesis_embeddings.embedding_model`.
    pub embedding_model: String,
}

impl SynthesisJobHandler {
    /// Construct a handler with the given dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn EmbeddingService>,
        llm: Arc<dyn LlmClient + Send + Sync>,
        edges_writer: Arc<dyn EdgeWriter>,
        edge_provider: Arc<dyn EdgeProvider + Send + Sync>,
        cost_budget: u32,
        embedding_model: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            embedder,
            llm,
            edges_writer,
            edge_provider,
            cost_budget,
            embedding_model: embedding_model.into(),
        }
    }
}

/// Convert a [`SynthesisError`] into a `JobError`.
///
/// Most synthesis errors are transient (LLM transport, DB blip, edge-service
/// hiccup) so they map to `ProcessingFailed`, which the runner retries.
/// Validation / hallucination / anchor-violation failures are also reported
/// as `ProcessingFailed` for now — Phase 4 may add `PermanentFailure`
/// classification once we have observed which retries are useful in
/// production.
fn synth_err_to_job_err(e: SynthesisError) -> JobError {
    JobError::ProcessingFailed {
        message: e.to_string(),
    }
}

/// Resolve `syntheses.skill_name` for `id` into a concrete skill. Unknown
/// names fall back to baseline so a typo or stale row never blocks the
/// worker; a `tracing::warn!` records the fallback for ops visibility.
///
/// Note: the `JobError::ProcessingFailed` variant has only a `message`
/// field (no `synthesis_id`); the id is included in the message text for
/// log-grep visibility.
pub async fn resolve_skill_for_row(
    pool: &PgPool,
    id: Uuid,
) -> Result<Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>, JobError> {
    let row: Option<String> = sqlx::query_scalar("SELECT skill_name FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| JobError::ProcessingFailed {
            message: format!("resolve_skill_for_row db error (synthesis_id={id}): {e}"),
        })?;

    let name = match row {
        Some(n) => n,
        None => {
            // Row missing is a legitimate "no skill recorded" case — fall
            // back to baseline rather than failing the job. The actual
            // job failure would surface elsewhere if the row is truly
            // gone (the worker would 404 on its own status writes).
            tracing::warn!(
                synthesis_id = %id,
                "syntheses row not found during skill resolution; using baseline",
            );
            return Ok(episcience_core::synthesis::skills::default_skill());
        }
    };

    match episcience_core::synthesis::skills::load_by_name(&name) {
        Some(s) => Ok(s),
        None => {
            tracing::warn!(
                synthesis_id = %id,
                requested_skill = %name,
                "unknown skill, falling back to baseline",
            );
            Ok(episcience_core::synthesis::skills::default_skill())
        }
    }
}

/// Resolve the effective traversal config for this run.
///
/// Precedence (highest first):
/// 1. Payload's explicit `traversal_config` (if present and parseable as
///    `TraversalConfig`).
/// 2. Skill's `traversal_config()` override (skills with strong domain
///    opinions return `Some(_)`; baseline returns `None`).
/// 3. `TraversalConfig::default()` — the schema default.
///
/// A malformed payload config falls through to step 2, not to step 3 —
/// rejecting an unparseable payload as a "no opinion" lets the skill
/// have a say. This mirrors how the job handler already treats
/// unparseable payloads as defaults (it does not fail the job).
pub fn resolve_traversal_config(
    payload_cfg: Option<&serde_json::Value>,
    skill: &dyn episcience_core::synthesis::skill::SynthesisSkill,
) -> TraversalConfig {
    if let Some(json) = payload_cfg {
        if let Ok(cfg) = serde_json::from_value::<TraversalConfig>(json.clone()) {
            return cfg;
        }
    }
    if let Some(cfg) = skill.traversal_config() {
        return cfg;
    }
    TraversalConfig::default()
}

#[async_trait]
impl JobHandler for SynthesisJobHandler {
    fn job_type(&self) -> &str {
        "synthesis"
    }

    async fn handle(&self, job: &Job) -> Result<JobResult, JobError> {
        let started = std::time::Instant::now();

        // 0. Decode payload. Bad payload is a permanent failure — no point
        //    retrying a job whose JSON we can't parse.
        let payload: SynthesisJobPayload =
            serde_json::from_value(job.payload.clone()).map_err(|e| JobError::PayloadError {
                message: format!("invalid synthesis payload: {e}"),
            })?;
        let synthesis_id = payload.synthesis_id;

        // Helper: mark the synthesis row failed and convert the underlying
        // error to a `JobError`. Logs but does not propagate failure to mark
        // failed — the original error is the more useful signal.
        let mark_failed = |e: SynthesisError| async move {
            tracing::error!(
                %synthesis_id,
                error = %e,
                "synthesis stage failed",
            );
            if let Err(db_e) =
                SynthesisRepository::mark_failed(&self.pool, synthesis_id, &e.to_string()).await
            {
                tracing::warn!(
                    %synthesis_id,
                    error = %db_e,
                    "failed to mark synthesis failed; original error: {e}"
                );
            }
            synth_err_to_job_err(e)
        };

        // 1. Transition pending → running.
        SynthesisRepository::update_status(&self.pool, synthesis_id, SynthesisStatus::Running)
            .await
            .map_err(|e| JobError::ProcessingFailed {
                message: format!("update_status running: {e}"),
            })?;

        // 2. Precompute the query embedding for Stage 2 traversal pruning.
        //
        // Soft-fail policy: an embedder error here does NOT abort the job.
        // Rationale:
        // - Stage 1 `recall::recall` calls `generate_query` independently
        //   and falls back to text search on the same failure, so seeds
        //   are still produced.
        // - Stage 2 traversal uses `query_embedding` only to relevance-
        //   prune neighbours via cosine; with an empty vec, every neighbour
        //   scores 0.0 and is pruned. Result: traversal degenerates to
        //   seed-only graphs.
        //
        // This is a degraded mode (Phase 4's real edge provider produces
        // less informative subgraphs when the embedder is down) but it's
        // still better than failing every in-flight synthesis on a
        // transient embedding-API outage. A `tracing::warn!` is emitted so
        // ops can detect persistent failures.
        let query_embedding = match self.embedder.generate_query(&payload.query).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    %synthesis_id,
                    error = %e,
                    "embedder.generate_query failed; Stage 2 will prune all neighbours",
                );
                Vec::new()
            }
        };

        // Resolve the skill named on the row (defaults to baseline; unknown
        // names log a warning and also fall back to baseline). Done before
        // pipeline construction so we can chain `.with_skill(skill)`.
        let skill = resolve_skill_for_row(&self.pool, synthesis_id).await?;

        // 3. Construct the pipeline. Wrappers bridge the `Arc<dyn ...>`
        //    handler fields to the generic `<L, P>` pipeline parameters.
        let mut pipeline: SynthesisPipeline<ArcLlm, ArcEdgeProvider> = SynthesisPipeline::new(
            self.pool.clone(),
            self.embedder.clone(),
            ArcLlm(self.llm.clone()),
            ArcEdgeProvider(self.edge_provider.clone()),
            query_embedding,
            self.cost_budget,
        )
        .with_skill(skill);

        // 4. Stage 1 — Seed.
        let seeds = match pipeline.stage1_seed(&payload.query, 50, 0.5).await {
            Ok(s) => s,
            Err(e) => return Err(mark_failed(e).await),
        };

        // 5. Stage 2 — Traverse.
        let cfg =
            resolve_traversal_config(payload.traversal_config.as_ref(), pipeline.skill.as_ref());
        let snapshot = match pipeline.stage2_traverse(synthesis_id, seeds, &cfg).await {
            Ok(s) => s,
            Err(e) => return Err(mark_failed(e).await),
        };

        // 6. Stage 3 — Cluster.
        //
        // Phase 2 v1: pass empty edge tuples. See module docs for rationale.
        let edges_with_types: Vec<(Uuid, Uuid, EdgeType)> = Vec::new();
        let clusters = match pipeline
            .stage3_cluster(synthesis_id, &snapshot, &edges_with_types)
            .await
        {
            Ok(c) => c,
            Err(e) => return Err(mark_failed(e).await),
        };

        // 7. Stage 4 — Narrate (per cluster).
        let clusters = match pipeline.stage4_narrate(synthesis_id, &clusters).await {
            Ok(c) => c,
            Err(e) => return Err(mark_failed(e).await),
        };

        // 8. Stage 5 — Compose.
        let narrative = match pipeline
            .stage5_compose(synthesis_id, &payload.query, &clusters)
            .await
        {
            Ok(n) => n,
            Err(e) => return Err(mark_failed(e).await),
        };

        // 9. Stage 6 — Verify.
        //
        // Flatten cluster member ids into one slice for the verifier context.
        let cluster_member_ids: Vec<Uuid> = clusters
            .iter()
            .flat_map(|c| c.member_claim_ids.iter().copied())
            .collect();

        let outcome = match pipeline
            .stage6_verify(
                synthesis_id,
                &payload.query,
                &narrative,
                &cluster_member_ids,
            )
            .await
        {
            Ok(o) => o,
            Err(e) => return Err(mark_failed(e).await),
        };

        // Persist the outcome on the row regardless of accept/reject, and bump
        // the attempt counter so refinement chains (Task 7.1) have a bound.
        let outcome_json =
            serde_json::to_value(&outcome).map_err(|e| JobError::ProcessingFailed {
                message: format!("verifier outcome serialize (synthesis_id={synthesis_id}): {e}"),
            })?;
        sqlx::query(
            "UPDATE syntheses
                SET verifier_outcome = $2,
                    verifier_attempts = verifier_attempts + 1
              WHERE id = $1",
        )
        .bind(synthesis_id)
        .bind(&outcome_json)
        .execute(&self.pool)
        .await
        .map_err(|e| JobError::ProcessingFailed {
            message: format!("verifier outcome persist (synthesis_id={synthesis_id}): {e}"),
        })?;

        // Route on the outcome.
        match &outcome {
            episcience_core::synthesis::verifier::VerificationOutcome::Accept { .. } => {
                // Fall through to Stage 7 (publish bundle) below.
            }
            episcience_core::synthesis::verifier::VerificationOutcome::Reject {
                rubric, ..
            } => {
                // Phase 7: simulated-annealing refinement on Reject.
                //
                // Read this row's current temperature (NULL = default cold).
                // If at_ceiling, terminally reject. Otherwise anneal and
                // spawn a refinement child via PROV-O REFINES, with the
                // child carrying the annealed temperature.
                let current_temp_json: Option<serde_json::Value> = sqlx::query_scalar(
                    "SELECT refinement_temperature FROM syntheses WHERE id = $1",
                )
                .bind(synthesis_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| JobError::ProcessingFailed {
                    message: format!(
                        "read refinement_temperature (synthesis_id={synthesis_id}): {e}"
                    ),
                })?;
                let current_temp: episcience_core::synthesis::refinement::RefinementTemperature =
                    current_temp_json
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();

                // Mark this row rejected (terminal for this row; any
                // refinement child is a sibling row, not a state transition
                // on this one).
                sqlx::query("UPDATE syntheses SET status = 'rejected' WHERE id = $1")
                    .bind(synthesis_id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| JobError::ProcessingFailed {
                        message: format!(
                            "verifier reject status update (synthesis_id={synthesis_id}): {e}"
                        ),
                    })?;

                // Ceiling — no child spawned.
                if current_temp.at_ceiling() {
                    tracing::info!(
                        synthesis_id = %synthesis_id,
                        "refinement ceiling reached; no child spawned"
                    );
                    return Ok(JobResult {
                        output: serde_json::json!({
                            "synthesis_id": synthesis_id,
                            "status": "rejected",
                            "rubric": rubric,
                            "refinement_ceiling_reached": true,
                        }),
                        execution_duration: started.elapsed(),
                        metadata: JobResultMetadata::default(),
                    });
                }

                // Otherwise, spawn a refinement child with annealed temperature.
                let new_temp = current_temp.anneal();
                let new_temp_json =
                    serde_json::to_value(new_temp).map_err(|e| JobError::ProcessingFailed {
                        message: format!(
                            "serialize refinement_temperature (parent={synthesis_id}): {e}"
                        ),
                    })?;
                let child_id = uuid::Uuid::now_v7();

                // Insert child syntheses row + PROV-O REFINES edge + enqueue
                // the child synthesis_job, all in one transaction so a crash
                // mid-spawn doesn't leave dangling state.
                let mut tx = self
                    .pool
                    .begin()
                    .await
                    .map_err(|e| JobError::ProcessingFailed {
                        message: format!("tx begin for refinement (parent={synthesis_id}): {e}"),
                    })?;

                // Child inherits the parent's identity columns; status starts
                // as 'pending', subgraph_snapshot starts empty (the worker
                // refills it on Stage 2). content_hash is zeroed on insert
                // (placeholder; Stage 6 mark_complete overwrites it).
                // skill_name and visibility carry over so the child runs the
                // same recipe.
                sqlx::query(
                    "INSERT INTO syntheses
                     (id, query, agent_id, status, parent_synthesis_id, subgraph_snapshot,
                      clustering_method, llm_provider, llm_model, content_hash,
                      visibility, skill_name, refinement_temperature)
                     SELECT
                        $1, query, agent_id, 'pending', id, '{}'::jsonb,
                        clustering_method, llm_provider, llm_model, $2,
                        visibility, skill_name, $3
                     FROM syntheses
                     WHERE id = $4",
                )
                .bind(child_id)
                .bind(&[0u8; 32][..])
                .bind(&new_temp_json)
                .bind(synthesis_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| JobError::ProcessingFailed {
                    message: format!(
                        "insert refinement child (parent={synthesis_id}, child={child_id}): {e}"
                    ),
                })?;

                // PROV-O REFINES edge: child REFINES parent.
                // synthesis_provo_edges has composite PK
                // (synthesis_id, predicate, target_kind, target_id);
                // synthesis_id is the *source* by convention. The child's
                // Stage 6 publish will also try to write this same edge
                // (stage6_plan_edges picks up parent_synthesis_id from the
                // payload). ON CONFLICT DO NOTHING keeps both paths safe.
                sqlx::query(
                    "INSERT INTO synthesis_provo_edges
                     (synthesis_id, predicate, target_kind, target_id)
                     VALUES ($1, 'REFINES', 'synthesis', $2)
                     ON CONFLICT DO NOTHING",
                )
                .bind(child_id)
                .bind(synthesis_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| JobError::ProcessingFailed {
                    message: format!(
                        "insert REFINES edge (parent={synthesis_id}, child={child_id}): {e}"
                    ),
                })?;

                // Enqueue the child job with the SAME query + traversal
                // config + agent. parent_synthesis_id points at this row so
                // Stage 6 emits REFINES when the child eventually publishes.
                let child_payload = SynthesisJobPayload {
                    synthesis_id: child_id,
                    query: payload.query.clone(),
                    traversal_config: payload.traversal_config.clone(),
                    agent_id: payload.agent_id,
                    parent_synthesis_id: Some(synthesis_id),
                    prereq_synthesis_ids: payload.prereq_synthesis_ids.clone(),
                };
                let child_payload_json = serde_json::to_value(&child_payload).map_err(|e| {
                    JobError::ProcessingFailed {
                        message: format!(
                            "serialize refinement payload (parent={synthesis_id}, child={child_id}): {e}"
                        ),
                    }
                })?;
                sqlx::query(
                    "INSERT INTO synthesis_jobs (id, job_type, payload, state)
                     VALUES ($1, 'synthesis', $2, 'queued')",
                )
                .bind(child_id)
                .bind(&child_payload_json)
                .execute(&mut *tx)
                .await
                .map_err(|e| JobError::ProcessingFailed {
                    message: format!(
                        "enqueue refinement job (parent={synthesis_id}, child={child_id}): {e}"
                    ),
                })?;

                tx.commit().await.map_err(|e| JobError::ProcessingFailed {
                    message: format!(
                        "commit refinement tx (parent={synthesis_id}, child={child_id}): {e}"
                    ),
                })?;

                tracing::info!(
                    parent_synthesis_id = %synthesis_id,
                    child_synthesis_id = %child_id,
                    depth_delta = new_temp.depth_delta,
                    "spawned refinement child"
                );

                return Ok(JobResult {
                    output: serde_json::json!({
                        "synthesis_id": synthesis_id,
                        "status": "rejected",
                        "rubric": rubric,
                        "refinement_child_id": child_id,
                        "depth_delta": new_temp.depth_delta,
                    }),
                    execution_duration: started.elapsed(),
                    metadata: JobResultMetadata::default(),
                });
            }
        }

        // 10. Stage 7 — Publish (5 substeps).

        // 9a. Plan provo edges.
        let cited: Vec<Uuid> = clusters
            .iter()
            .flat_map(|c| c.member_claim_ids.iter().copied())
            .collect();
        if let Err(e) = episcience_db::synthesis::publish::stage6_plan_edges(
            &self.pool,
            synthesis_id,
            &cited,
            payload.parent_synthesis_id,
            &payload.prereq_synthesis_ids,
            payload.agent_id,
        )
        .await
        {
            return Err(mark_failed(e).await);
        }

        // 9b. Embed narrative head.
        if let Err(e) = episcience_db::synthesis::publish::stage6_embed_narrative(
            &self.pool,
            self.embedder.as_ref(),
            synthesis_id,
            &narrative,
            &self.embedding_model,
        )
        .await
        {
            return Err(mark_failed(e).await);
        }

        // 9c. Compute content hash (pure).
        let content_hash = episcience_db::synthesis::publish::compute_content_hash(
            &payload.query,
            &snapshot,
            &narrative,
        );

        // 9d. Write edges to EpiGraph.
        if let Err(e) = episcience_db::synthesis::publish::stage6_write_edges(
            &self.pool,
            self.edges_writer.as_ref(),
            synthesis_id,
        )
        .await
        {
            return Err(mark_failed(e).await);
        }

        // 9e. Mark complete (refuses if any provo edge is still pending).
        if let Err(e) = episcience_db::synthesis::publish::stage6_mark_complete(
            &self.pool,
            synthesis_id,
            &narrative,
            &content_hash,
        )
        .await
        {
            return Err(mark_failed(e).await);
        }

        // Stage 7 — Novelty. Only runs on the Accept path (Reject already
        // returned earlier). The publish bundle has completed; the
        // synthesis is `complete`. Novelty failures are non-fatal — they
        // log and continue. Novelty is metadata, not gating.
        {
            let backend =
                episcience_db::synthesis::novelty_backend_internal::InternalNoveltyBackend {
                    pool: self.pool.clone(),
                    embedder: self.embedder.clone(),
                };
            match pipeline
                .stage7_novelty(synthesis_id, &narrative, &cluster_member_ids, &backend)
                .await
            {
                Ok(novelty) => {
                    let novelty_json =
                        serde_json::to_value(&novelty).unwrap_or(serde_json::Value::Null);
                    if let Err(e) = sqlx::query(
                        "UPDATE syntheses SET novelty_score = $2, novelty_backend = $3 \
                         WHERE id = $1",
                    )
                    .bind(synthesis_id)
                    .bind(&novelty_json)
                    .bind(novelty.backend.clone())
                    .execute(&self.pool)
                    .await
                    {
                        tracing::warn!(
                            synthesis_id = %synthesis_id,
                            error = %e,
                            "novelty persist failed (non-fatal)",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        synthesis_id = %synthesis_id,
                        error = %e,
                        "stage7_novelty failed (non-fatal)",
                    );
                }
            }
        }

        // 10. Build a JobResult. `JobResult` is a struct (not enum); the
        //     "success" signal is `Ok(_)`.
        Ok(JobResult {
            output: serde_json::json!({
                "synthesis_id": synthesis_id,
                "completed_at": Utc::now(),
            }),
            execution_duration: started.elapsed(),
            metadata: JobResultMetadata::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Payload round-trips through `serde_json` without losing fields.
    #[test]
    fn payload_round_trip() {
        let p = SynthesisJobPayload {
            synthesis_id: Uuid::new_v4(),
            query: "what do we know about origami?".into(),
            traversal_config: Some(serde_json::json!({"max_hops": 1})),
            agent_id: Uuid::new_v4(),
            parent_synthesis_id: Some(Uuid::new_v4()),
            prereq_synthesis_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: SynthesisJobPayload = serde_json::from_value(v).unwrap();
        assert_eq!(back.synthesis_id, p.synthesis_id);
        assert_eq!(back.query, p.query);
        assert_eq!(back.agent_id, p.agent_id);
        assert_eq!(back.parent_synthesis_id, p.parent_synthesis_id);
        assert_eq!(back.prereq_synthesis_ids, p.prereq_synthesis_ids);
    }

    /// Missing optional fields default cleanly (older payloads forward-compat).
    #[test]
    fn payload_missing_optionals_decode() {
        let v = serde_json::json!({
            "synthesis_id": Uuid::new_v4(),
            "query": "x",
            "agent_id": Uuid::new_v4(),
        });
        let p: SynthesisJobPayload = serde_json::from_value(v).unwrap();
        assert!(p.traversal_config.is_none());
        assert!(p.parent_synthesis_id.is_none());
        assert!(p.prereq_synthesis_ids.is_empty());
    }

    /// `EmptyEdgeProvider` returns no neighbours.
    #[tokio::test]
    async fn empty_edge_provider_returns_no_neighbours() {
        let p = EmptyEdgeProvider;
        let n = p.neighbors(Uuid::new_v4(), &[EdgeType::Supports]).await;
        assert!(n.is_empty());
    }
}
