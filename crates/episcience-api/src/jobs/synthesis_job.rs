//! The synthesis handler: drives the full synthesis pipeline for one job, as
//! `episcience-worker` runs it.
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
//! empty edge list to `stage3_plan`, which means clusters are
//! purely-id-based and every claim becomes its own singleton (still capped
//! at 12). Phase 4 / B-CKL track #43 will revisit.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
use epigraph_embeddings::EmbeddingService;
use epigraph_jobs::{JobError, JobResult, JobResultMetadata};
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType, TraversalConfig};
use episcience_core::synthesis::SynthesisStatus;
use episcience_db::synthesis::{pipeline, publish};
use episcience_db::{SynthesisPipeline, SynthesisRepository};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::jobs::session::{SessionError, StageSession};

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
    /// Workflow correlation key — the EpiGraph workflow run that triggered
    /// this synthesis. Emitted in `synthesis.complete` / `synthesis.failed`
    /// events and used to plan a `REFINES target_kind="workflow"` provo edge.
    /// `None` for syntheses triggered directly (REST / MCP); `Some(id)` when
    /// dispatched from an EpiGraph workflow agent. `#[serde(default)]` keeps
    /// older payloads (without this field) deserializable.
    #[serde(default)]
    pub workflow_run_id: Option<Uuid>,
    /// Theme-anchored seed for wiki articles: Stage 1 seeds from this theme's
    /// members the acting principal can read
    /// ([`SynthesisPipeline::stage1_seed_theme`]). `None` = text recall on
    /// `query` (every non-wiki synthesis). `#[serde(default)]` keeps payloads
    /// enqueued before this field existed deserializable.
    #[serde(default)]
    pub seed_theme_id: Option<Uuid>,
}

// ─── Wrapper newtypes for trait-object generics ──────────────────────────────

/// Adapter that lets an `Arc<dyn LlmClient + Send + Sync>` satisfy the
/// `LlmClient` trait directly. Required because `SynthesisPipeline<L, P>`'s
/// LLM-bound impls take `L: LlmClient` (not `L: Deref<Target = dyn LlmClient>`)
/// and trait objects do not auto-implement their own trait.
pub struct ArcLlm(pub Arc<dyn LlmProvider>);

impl fmt::Debug for ArcLlm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Delegate so we don't lose the model identifier in error logs.
        write!(f, "ArcLlm({})", self.0.model_name())
    }
}

#[async_trait]
impl LlmProvider for ArcLlm {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn is_active(&self) -> bool {
        self.0.is_active()
    }

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

/// Drives a single synthesis through every pipeline stage.
///
/// The `episcience-worker` process calls [`Self::run`] on an owner
/// [`StageSession`] (see [`crate::jobs::session`]): every stage's writes run
/// in their own transaction stamped as the synthesis' acting principal.
///
/// `pool` is the pool the handler's UNSTAMPED reads run on: the kernel
/// engine's recall and belief lookups (stages 1 and 2). It is `ENGINE_POOL`,
/// the worker's unstamped application-role pool (`V1-engine-takes-pool`: the
/// engine takes a plain pool until KE-1, so it reads public claims only). The novelty backends (stage 7)
/// are EpiScience SQL, not the engine: they read on a stage transaction as
/// the acting principal.
///
/// Stage 6 writes the kernel PROV edges and their `edge.added` events IN
/// PROCESS on the stage transaction (no service credential), and
/// `synthesis.*` events likewise, for public, publishable syntheses only.
#[derive(Clone)]
pub struct SynthesisJobHandler {
    pub pool: PgPool,
    pub embedder: Arc<dyn EmbeddingService>,
    pub llm: Arc<dyn LlmProvider>,
    pub edge_provider: Arc<dyn EdgeProvider + Send + Sync>,
    pub cost_budget: u32,
    /// Stored alongside the embedding for audit; written to
    /// `synthesis_embeddings.embedding_model`.
    pub embedding_model: String,
    /// Whether `synthesis.complete` / `synthesis.failed` events are written
    /// (public syntheses only, in process). `false` in tests that do not
    /// need them.
    pub publish_events: bool,
}

/// Why [`SynthesisJobHandler::run`] ended without a result.
#[derive(Debug)]
pub enum RunError {
    /// The acting principal lost write authority on the synthesis (or can no
    /// longer see it). Terminal: the worker finishes the job
    /// `failed: authority: …` and never retries it. Nothing was written for
    /// the refused stage.
    Authority(String),
    /// Any other failure; the synthesis row was marked failed (best effort).
    /// The queue may retry it.
    Failed(JobError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authority(m) => write!(f, "authority: {m}"),
            Self::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// A stage step's failure: an authority refusal (terminal, no write) or a
/// synthesis error (marked failed on the row).
enum StageError {
    Authority(String),
    Synth(SynthesisError),
}

impl From<SessionError> for StageError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Authority(m) => Self::Authority(m),
            SessionError::Db(m) => Self::Synth(SynthesisError::Db(m)),
        }
    }
}

impl From<SynthesisError> for StageError {
    fn from(e: SynthesisError) -> Self {
        Self::Synth(e)
    }
}

fn db_err(e: impl std::fmt::Display) -> StageError {
    StageError::Synth(SynthesisError::Db(e.to_string()))
}

impl SynthesisJobHandler {
    /// Construct a handler with the given dependencies.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn EmbeddingService>,
        llm: Arc<dyn LlmProvider>,
        edge_provider: Arc<dyn EdgeProvider + Send + Sync>,
        cost_budget: u32,
        embedding_model: impl Into<String>,
        publish_events: bool,
    ) -> Self {
        Self {
            pool,
            embedder,
            llm,
            edge_provider,
            cost_budget,
            embedding_model: embedding_model.into(),
            publish_events,
        }
    }

    /// Mark the synthesis failed and publish `synthesis.failed` (public
    /// syntheses only), in one stage transaction; best effort. An authority
    /// refusal writes nothing (the principal may not write the row).
    async fn fail(
        &self,
        session: &StageSession,
        synthesis_id: Uuid,
        workflow_run_id: Option<Uuid>,
        acting: Uuid,
        e: StageError,
    ) -> RunError {
        let e = match e {
            StageError::Authority(m) => {
                tracing::warn!(%synthesis_id, reason = %m, "synthesis stopped: authority");
                return RunError::Authority(m);
            }
            StageError::Synth(e) => e,
        };
        tracing::error!(%synthesis_id, error = %e, "synthesis stage failed");
        let failure_reason = e.to_string();
        let written = async {
            let mut tx = session.begin().await.map_err(|e| e.to_string())?;
            SynthesisRepository::mark_failed(&mut *tx, synthesis_id, &failure_reason)
                .await
                .map_err(|e| e.to_string())?;
            if self.publish_events {
                publish::publish_synthesis_event_conn(
                    &mut tx,
                    synthesis_id,
                    "synthesis.failed",
                    Some(acting),
                    &serde_json::json!({
                        "synthesis_id": synthesis_id,
                        "workflow_run_id": workflow_run_id,
                        "failure_reason": failure_reason,
                    }),
                )
                .await;
            }
            tx.commit().await
        }
        .await;
        if let Err(db_e) = written {
            tracing::warn!(
                %synthesis_id,
                error = %db_e,
                "failed to mark synthesis failed; original error: {e}"
            );
        }
        RunError::Failed(synth_err_to_job_err(e))
    }
}

/// `Ok` when a write that targets exactly one row affected exactly one; a
/// write that matched nothing is an error, never a silent success (B-M1).
fn one_row(affected: u64, what: &str, synthesis_id: uuid::Uuid) -> Result<(), JobError> {
    if affected == 1 {
        Ok(())
    } else {
        Err(JobError::ProcessingFailed {
            message: format!(
                "{what} (synthesis_id={synthesis_id}): {affected} rows affected, expected 1"
            ),
        })
    }
}

/// The principal a job acts as, and so the principal a refinement of
/// synthesis `parent` acts as: that JOB row's `principal_id`. `None` when the parent job has none (a legacy job in the
/// deploy window, before the re-own sets one): the caller then spawns no
/// refinement rather than guessing, because the payload's and the row's
/// author there is a legacy shared agent, which the re-own does not overwrite
/// and which the kernel refuses once it is link-retired. `None` is a
/// deterministic, terminal answer, not a transient error: the caller must not
/// turn it into a retry.
pub async fn refinement_principal(
    conn: &mut sqlx::PgConnection,
    parent: Uuid,
) -> Result<Option<Uuid>, JobError> {
    Ok(sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT principal_id FROM synthesis_jobs WHERE id = $1",
    )
    .bind(parent)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| JobError::ProcessingFailed {
        message: format!("read the acting principal (parent={parent}): {e}"),
    })?
    .flatten())
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
pub async fn resolve_skill_for_row<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    id: Uuid,
) -> Result<Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>, JobError> {
    let row: Option<String> = sqlx::query_scalar("SELECT skill_name FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
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

/// Pick a [`NoveltyBackend`](episcience_db::synthesis::novelty::NoveltyBackend)
/// implementation for the given skill name.
///
/// Dispatch table (Phase 9):
/// - `"literature"` → [`PaperNoveltyBackend`](episcience_db::synthesis::novelty_backend_paper::PaperNoveltyBackend):
///   the internal score plus prior DOI-labeled claims.
/// - everything else (`"baseline"`, `"lab_notebook"`, `"code_review"`,
///   `"registry_diff"`, unknown) → [`InternalNoveltyBackend`](episcience_db::synthesis::novelty_backend_internal::InternalNoveltyBackend).
///
/// The backends are stateless: they read on the stage transaction the
/// handler hands them (see Stage 7 in [`SynthesisJobHandler::run`]).
/// Re-exported here so the dispatch stays testable from this crate (the
/// `select_novelty_backend_*` tests in `synthesis_job_handler_test.rs`).
pub use episcience_db::synthesis::novelty::select_novelty_backend;

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

impl SynthesisJobHandler {
    /// Run every stage of `payload`'s synthesis on `session`, acting as
    /// `acting` (the worker passes the queue row's `principal_id`, never the
    /// payload's `agent_id`).
    ///
    /// Each stage's writes run in their own [`StageSession::begin`]
    /// transaction; no transaction is held across an LLM or embedding call.
    ///
    /// # Errors
    /// [`RunError::Authority`] when a stage transaction is refused for
    /// authority (terminal, nothing written for that stage);
    /// [`RunError::Failed`] otherwise (the row is marked failed, best effort).
    pub async fn run(
        &self,
        session: &StageSession,
        payload: SynthesisJobPayload,
        acting: Uuid,
    ) -> Result<JobResult, RunError> {
        let synthesis_id = payload.synthesis_id;
        let workflow_run_id = payload.workflow_run_id;
        match self.run_stages(session, &payload, acting).await {
            Ok(r) => Ok(r),
            Err(e) => Err(self
                .fail(session, synthesis_id, workflow_run_id, acting, e)
                .await),
        }
    }

    async fn run_stages(
        &self,
        session: &StageSession,
        payload: &SynthesisJobPayload,
        acting: Uuid,
    ) -> Result<JobResult, StageError> {
        let started = std::time::Instant::now();
        let synthesis_id = payload.synthesis_id;
        let workflow_run_id = payload.workflow_run_id;

        // 1. Transition pending → running, and read the skill named on the
        //    row (defaults to baseline; unknown names fall back to baseline).
        let skill = {
            let mut tx = session.begin().await?;
            SynthesisRepository::update_status(&mut *tx, synthesis_id, SynthesisStatus::Running)
                .await
                .map_err(|e| db_err(format!("update_status running: {e}")))?;
            let skill = resolve_skill_for_row(&mut *tx, synthesis_id)
                .await
                .map_err(|e| db_err(format!("{e:?}")))?;
            tx.commit().await.map_err(db_err)?;
            skill
        };

        // 2. Precompute the query embedding for Stage 2 traversal pruning.
        //
        // Soft-fail policy for text recall: an embedder error here does NOT
        // abort the job. Stage 1 `recall::recall` calls `generate_query`
        // independently and falls back to text search on the same failure,
        // so seeds are still produced; Stage 2 then prunes every neighbour
        // (seed-only graphs). A THEME seed (`payload.seed_theme_id`) ranks
        // the theme's members against this embedding, so it fails closed
        // without it (`stage1_seed_theme` refuses an empty embedding).
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

        // 3. Construct the pipeline on the handler's (engine) pool.
        let mut pipeline: SynthesisPipeline<ArcLlm, ArcEdgeProvider> = SynthesisPipeline::new(
            self.pool.clone(),
            self.embedder.clone(),
            ArcLlm(self.llm.clone()),
            ArcEdgeProvider(self.edge_provider.clone()),
            query_embedding,
            self.cost_budget,
        )
        .with_skill(skill);

        // 3b. The acting principal's read authority. Stages 1, 2 and 4 read
        //     kernel claims AS this viewer. An unresolvable principal fails
        //     the job CLOSED before any stage runs (no seed, no snapshot).
        let owner = match session.viewer(acting).await {
            Ok(v) => v,
            Err(e) => {
                return Err(StageError::Synth(SynthesisError::Validation(format!(
                    "synthesis owner could not be resolved; failing closed: {e}"
                ))))
            }
        };

        // 4. Stage 1 — Seed (theme-anchored for a wiki article, text recall
        //    otherwise), then the seed filter on the stamped session: a
        //    public synthesis keeps public claims only, a group synthesis
        //    public claims plus claims its own owner group owns.
        let seeds = match payload.seed_theme_id {
            Some(theme_id) => pipeline.stage1_seed_theme(&owner, theme_id).await?,
            None => {
                pipeline
                    .stage1_seed(&owner, &payload.query, 50, 0.5)
                    .await?
            }
        };
        let seeds = {
            let mut tx = session.begin().await?;
            let kept = seed_filter(&mut tx, synthesis_id, &seeds)
                .await
                .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            kept
        };
        if seeds.is_empty() {
            return Err(StageError::Synth(SynthesisError::EmptyResult));
        }

        // 5. Stage 2 — Traverse (engine reads), then persist the snapshot and
        //    the membership in one stage transaction.
        let cfg =
            resolve_traversal_config(payload.traversal_config.as_ref(), pipeline.skill.as_ref());
        let snapshot = pipeline.stage2_compute(&owner, seeds, &cfg).await?;
        {
            let mut tx = session.begin().await?;
            pipeline::stage2_persist(&mut tx, synthesis_id, &snapshot).await?;
            tx.commit().await.map_err(db_err)?;
        }

        // 6. Stage 3 — Cluster. Phase 2 v1: empty edge tuples (module docs).
        let edges_with_types: Vec<(Uuid, Uuid, EdgeType)> = Vec::new();
        let clusters = pipeline::stage3_plan(synthesis_id, &snapshot, &edges_with_types);
        {
            let mut tx = session.begin().await?;
            pipeline::stage3_persist(&mut tx, synthesis_id, &clusters).await?;
            tx.commit().await.map_err(db_err)?;
        }

        // 7. Stage 4 — Narrate: read every cluster's member text in one
        //    transaction, narrate with no transaction open, store the text in
        //    a second one (authority re-checked).
        let contents = {
            let mut tx = session.begin().await?;
            let mut all = Vec::with_capacity(clusters.len());
            for c in &clusters {
                all.push(
                    pipeline::fetch_claim_contents(&mut *tx, &owner, &c.member_claim_ids).await?,
                );
            }
            tx.commit().await.map_err(db_err)?;
            all
        };
        let mut narrated = Vec::with_capacity(clusters.len());
        for (c, text) in clusters.iter().zip(contents.iter()) {
            narrated.push(pipeline.narrate_cluster(c, text).await?);
        }
        let clusters = narrated;
        {
            let mut tx = session.begin().await?;
            pipeline::stage4_persist(&mut tx, &clusters).await?;
            tx.commit().await.map_err(db_err)?;
        }

        // 8. Stage 5 — Compose (no database).
        let narrative = pipeline
            .stage5_compose(synthesis_id, &payload.query, &clusters)
            .await?;

        // 9. Stage 6 — Verify (pure).
        let cluster_member_ids: Vec<Uuid> = clusters
            .iter()
            .flat_map(|c| c.member_claim_ids.iter().copied())
            .collect();
        let outcome = pipeline
            .stage6_verify(
                synthesis_id,
                &payload.query,
                &narrative,
                &cluster_member_ids,
            )
            .await?;
        let outcome_json = serde_json::to_value(&outcome).map_err(|e| {
            db_err(format!(
                "verifier outcome serialize (synthesis_id={synthesis_id}): {e}"
            ))
        })?;

        // Persist the outcome on the row regardless of accept/reject, and bump
        // the attempt counter so refinement chains have a bound.
        if let episcience_core::synthesis::verifier::VerificationOutcome::Reject {
            rubric, ..
        } = &outcome
        {
            return self
                .reject_and_refine(session, payload, acting, &outcome_json, rubric, started)
                .await;
        }
        {
            let mut tx = session.begin().await?;
            persist_verifier_outcome(&mut tx, synthesis_id, &outcome_json).await?;
            tx.commit().await.map_err(db_err)?;
        }

        // 10. Stage 7 — Publish.
        // 10a. Plan provo edges. The parent and prerequisites come from the
        //      ROW, the same source every publishability check reads, never
        //      the payload.
        let cited: Vec<Uuid> = clusters
            .iter()
            .flat_map(|c| c.member_claim_ids.iter().copied())
            .collect();
        {
            let mut tx = session.begin().await?;
            let (row_parent, row_prereqs) = sqlx::query_as::<_, (Option<Uuid>, Option<Vec<Uuid>>)>(
                "SELECT parent_synthesis_id, prereq_synthesis_ids FROM syntheses WHERE id = $1",
            )
            .bind(synthesis_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            publish::stage6_plan_edges_conn(
                &mut tx,
                synthesis_id,
                &cited,
                row_parent,
                row_prereqs.as_deref().unwrap_or(&[]),
                acting,
                workflow_run_id,
            )
            .await?;
            tx.commit().await.map_err(db_err)?;
        }

        // 10b. Embed the narrative head (no transaction open), then store it.
        let embedding = self
            .embedder
            .generate(publish::narrative_head(&narrative))
            .await
            .map_err(|e| StageError::Synth(SynthesisError::Llm(format!("embed: {e}"))))?;
        {
            let mut tx = session.begin().await?;
            episcience_db::SynthesisEmbeddingsRepository::upsert(
                &mut *tx,
                synthesis_id,
                &embedding,
                &self.embedding_model,
                "narrative_head",
            )
            .await
            .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
        }

        // 10c. Content hash (pure).
        let content_hash = publish::compute_content_hash(&payload.query, &snapshot, &narrative);

        // 10d. Kernel PROV edges, in process, on the stage transaction (public
        //      and publishable only; otherwise deferred `private`). The
        //      transaction commits whatever happened (written edges, a failed
        //      row's attempt), then a failure fails the stage.
        {
            let mut tx = session.begin().await?;
            let written =
                publish::stage6_write_edges_conn(&mut tx, synthesis_id, Some(acting)).await?;
            tx.commit().await.map_err(db_err)?;
            if let Some(f) = written.failure {
                return Err(StageError::Synth(SynthesisError::EdgeWrite(f)));
            }
        }

        // 10e. Mark complete (refuses while an edge is pending), and publish
        //      `synthesis.complete` on the same transaction: the publish rule
        //      may narrow the row at completion, and the event must see that.
        {
            let mut tx = session.begin().await?;
            publish::stage6_mark_complete_conn(&mut tx, synthesis_id, &narrative, &content_hash)
                .await?;
            if self.publish_events {
                publish::publish_synthesis_event_conn(
                    &mut tx,
                    synthesis_id,
                    "synthesis.complete",
                    Some(acting),
                    &serde_json::json!({
                        "synthesis_id": synthesis_id,
                        "workflow_run_id": workflow_run_id,
                        "agent_id": acting,
                        "query": payload.query,
                    }),
                )
                .await;
            }
            tx.commit().await.map_err(db_err)?;
        }

        // Stage 7 — Novelty (non-fatal metadata). The backend reads on a
        // STAGE transaction (on the worker: stamped as the acting principal,
        // so row security applies as to every other stage), reading as that
        // principal, and the score is stored on the same transaction. Every
        // embedding it needs is computed first, with no transaction open:
        // the head embedding is 10b's; the full narrative only for a backend
        // that asks for it.
        {
            let backend = select_novelty_backend(pipeline.skill.name());
            let stored = async {
                let full = if backend.wants_narrative_embedding() {
                    Some(
                        self.embedder
                            .generate(&narrative)
                            .await
                            .map_err(|e| format!("embed the narrative: {e}"))?,
                    )
                } else {
                    None
                };
                let reader = session.viewer(acting).await.map_err(|e| e.to_string())?;
                let candidate = episcience_db::synthesis::novelty::NoveltyCandidate {
                    id: synthesis_id,
                    member_ids: &cluster_member_ids,
                    head_embedding: &embedding,
                    narrative_embedding: full.as_deref(),
                };
                let mut tx = session.begin().await.map_err(|e| e.to_string())?;
                let novelty = pipeline
                    .stage7_novelty(&mut tx, &reader, &candidate, backend.as_ref())
                    .await
                    .map_err(|e| format!("stage7_novelty: {e}"))?;
                let novelty_json =
                    serde_json::to_value(&novelty).unwrap_or(serde_json::Value::Null);
                let r = sqlx::query(
                    "UPDATE syntheses SET novelty_score = $2, novelty_backend = $3 \
                     WHERE id = $1",
                )
                .bind(synthesis_id)
                .bind(&novelty_json)
                .bind(novelty.backend.clone())
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
                one_row(r.rows_affected(), "novelty persist", synthesis_id)
                    .map_err(|e| format!("{e:?}"))?;
                tx.commit().await
            }
            .await;
            if let Err(e) = stored {
                tracing::warn!(
                    synthesis_id = %synthesis_id,
                    error = %e,
                    "stage 7 novelty failed (non-fatal)",
                );
            }
        }

        Ok(JobResult {
            output: serde_json::json!({
                "synthesis_id": synthesis_id,
                "completed_at": Utc::now(),
            }),
            execution_duration: started.elapsed(),
            metadata: JobResultMetadata::default(),
        })
    }

    /// The Reject path (Phase 7, simulated-annealing refinement), in ONE
    /// stage transaction: store the verifier outcome, mark this row
    /// `rejected`, and, unless the temperature is at its ceiling or the
    /// parent job has no principal, insert the refinement child (the
    /// parent's recipe and ownership pair, authored by the acting
    /// principal), its REFINES outbox row and its job.
    async fn reject_and_refine(
        &self,
        session: &StageSession,
        payload: &SynthesisJobPayload,
        acting: Uuid,
        outcome_json: &serde_json::Value,
        rubric: &str,
        started: std::time::Instant,
    ) -> Result<JobResult, StageError> {
        let synthesis_id = payload.synthesis_id;
        let workflow_run_id = payload.workflow_run_id;
        let mut tx = session.begin().await?;
        persist_verifier_outcome(&mut tx, synthesis_id, outcome_json).await?;

        let current_temp_json: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT refinement_temperature FROM syntheses WHERE id = $1")
                .bind(synthesis_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| {
                    db_err(format!(
                        "read refinement_temperature (synthesis_id={synthesis_id}): {e}"
                    ))
                })?;
        let current_temp: episcience_core::synthesis::refinement::RefinementTemperature =
            current_temp_json
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();

        // Mark this row rejected (terminal for this row; any refinement child
        // is a sibling row, not a state transition on this one).
        let r = sqlx::query("UPDATE syntheses SET status = 'rejected' WHERE id = $1")
            .bind(synthesis_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                db_err(format!(
                    "verifier reject status update (synthesis_id={synthesis_id}): {e}"
                ))
            })?;
        one_row(
            r.rows_affected(),
            "verifier reject status update",
            synthesis_id,
        )
        .map_err(|e| db_err(format!("{e:?}")))?;

        if current_temp.at_ceiling() {
            tx.commit().await.map_err(db_err)?;
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

        // The acting principal of the chain: the parent JOB's principal,
        // never the parent row's author nor the payload's. A parent job with
        // no principal (a legacy job in the deploy window) spawns nothing: a
        // terminal result (Ok), never an Err, since a retry would re-run every
        // LLM stage to the same refusal.
        let Some(chain) = refinement_principal(&mut tx, synthesis_id)
            .await
            .map_err(|e| db_err(format!("{e:?}")))?
        else {
            tx.commit().await.map_err(db_err)?;
            tracing::warn!(
                synthesis_id = %synthesis_id,
                "the parent job has no principal; no refinement spawned"
            );
            return Ok(JobResult {
                output: serde_json::json!({
                    "synthesis_id": synthesis_id,
                    "status": "rejected",
                    "rubric": rubric,
                    "refinement_skipped": "no principal",
                }),
                execution_duration: started.elapsed(),
                metadata: JobResultMetadata::default(),
            });
        };
        if chain != acting {
            // The worker acts as the queue row's principal; the parent job's
            // principal is that same row's. A mismatch is a bug, never a
            // reason to act as someone else.
            return Err(StageError::Authority(
                "the refinement chain's principal is not the acting principal".into(),
            ));
        }

        let new_temp = current_temp.anneal();
        let new_temp_json = serde_json::to_value(new_temp).map_err(|e| {
            db_err(format!(
                "serialize refinement_temperature (parent={synthesis_id}): {e}"
            ))
        })?;
        let child_id = uuid::Uuid::now_v7();

        // The child copies the parent's recipe AND its ownership pair (an
        // automatic refinement stays where its parent is), and its
        // prerequisites on the ROW (publishability and stage 6 read them
        // there); it is authored by the chain's principal.
        sqlx::query(
            "INSERT INTO syntheses
             (id, query, agent_id, status, parent_synthesis_id, subgraph_snapshot,
              clustering_method, llm_provider, llm_model, content_hash,
              visibility, owner_group_id, skill_name, refinement_temperature,
              prereq_synthesis_ids)
             SELECT
                $1, query, $5, 'pending', id, '{}'::jsonb,
                clustering_method, llm_provider, llm_model, $2,
                visibility, owner_group_id, skill_name, $3,
                prereq_synthesis_ids
             FROM syntheses
             WHERE id = $4",
        )
        .bind(child_id)
        .bind(&[0u8; 32][..])
        .bind(&new_temp_json)
        .bind(synthesis_id)
        .bind(chain)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            db_err(format!(
                "insert refinement child (parent={synthesis_id}, child={child_id}): {e}"
            ))
        })?;

        // PROV-O REFINES outbox row: child REFINES parent. The child's Stage
        // 6 plans the same row; ON CONFLICT DO NOTHING keeps both paths safe.
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
        .map_err(|e| {
            db_err(format!(
                "insert REFINES edge (parent={synthesis_id}, child={child_id}): {e}"
            ))
        })?;

        let child_payload = SynthesisJobPayload {
            synthesis_id: child_id,
            query: payload.query.clone(),
            traversal_config: payload.traversal_config.clone(),
            agent_id: chain,
            parent_synthesis_id: Some(synthesis_id),
            prereq_synthesis_ids: payload.prereq_synthesis_ids.clone(),
            workflow_run_id,
            // A refined wiki article stays anchored to its theme: falling
            // back to text recall is the off-theme drift the seed exists to stop.
            seed_theme_id: payload.seed_theme_id,
        };
        let child_payload_json = serde_json::to_value(&child_payload).map_err(|e| {
            db_err(format!(
                "serialize refinement payload (parent={synthesis_id}, child={child_id}): {e}"
            ))
        })?;
        // Supplied explicitly: the database refuses a job without a
        // principal on a privileged, unstamped session, and forces it to the
        // stamped principal on the worker's.
        sqlx::query(
            "INSERT INTO synthesis_jobs (id, job_type, payload, state, principal_id)
             VALUES ($1, 'synthesis', $2, 'queued', $3)",
        )
        .bind(child_id)
        .bind(&child_payload_json)
        .bind(chain)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            db_err(format!(
                "enqueue refinement job (parent={synthesis_id}, child={child_id}): {e}"
            ))
        })?;

        tx.commit().await.map_err(|e| {
            db_err(format!(
                "commit refinement tx (parent={synthesis_id}, child={child_id}): {e}"
            ))
        })?;

        tracing::info!(
            parent_synthesis_id = %synthesis_id,
            child_synthesis_id = %child_id,
            depth_delta = new_temp.depth_delta,
            "spawned refinement child"
        );

        Ok(JobResult {
            output: serde_json::json!({
                "synthesis_id": synthesis_id,
                "status": "rejected",
                "rubric": rubric,
                "refinement_child_id": child_id,
                "depth_delta": new_temp.depth_delta,
            }),
            execution_duration: started.elapsed(),
            metadata: JobResultMetadata::default(),
        })
    }
}

/// Store the verifier outcome and bump the attempt counter (exactly one row).
async fn persist_verifier_outcome(
    conn: &mut sqlx::PgConnection,
    synthesis_id: Uuid,
    outcome_json: &serde_json::Value,
) -> Result<(), StageError> {
    let r = sqlx::query(
        "UPDATE syntheses
            SET verifier_outcome = $2,
                verifier_attempts = verifier_attempts + 1
          WHERE id = $1",
    )
    .bind(synthesis_id)
    .bind(outcome_json)
    .execute(&mut *conn)
    .await
    .map_err(|e| {
        db_err(format!(
            "verifier outcome persist (synthesis_id={synthesis_id}): {e}"
        ))
    })?;
    one_row(r.rows_affected(), "verifier outcome persist", synthesis_id)
        .map_err(|e| db_err(format!("{e:?}")))
}

/// The seed filter (brief E1f requirement 3), on the caller's (stamped)
/// connection: a PUBLIC synthesis keeps only public seed claims; a GROUP
/// synthesis keeps public claims plus claims owned by its own owner group.
/// Every other seed is dropped (a claim of another group can never seed this
/// synthesis, whoever can read it). Order is preserved.
///
/// # Errors
/// The read failure.
pub async fn seed_filter(
    conn: &mut sqlx::PgConnection,
    synthesis_id: Uuid,
    seeds: &[Uuid],
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT c.id
           FROM unnest($2::uuid[]) WITH ORDINALITY AS x(id, ord)
           JOIN claims c ON c.id = x.id
           JOIN syntheses s ON s.id = $1
          WHERE c.visibility::text = 'public'
             OR (s.visibility = 'group' AND c.owner_group_id = s.owner_group_id)
          ORDER BY x.ord",
    )
    .bind(synthesis_id)
    .bind(seeds)
    .fetch_all(&mut *conn)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Payload round-trips through `serde_json` without losing fields.
    #[test]
    fn payload_round_trip() {
        let workflow_id = Uuid::new_v4();
        let theme_id = Uuid::new_v4();
        let p = SynthesisJobPayload {
            synthesis_id: Uuid::new_v4(),
            query: "what do we know about origami?".into(),
            traversal_config: Some(serde_json::json!({"max_hops": 1})),
            agent_id: Uuid::new_v4(),
            parent_synthesis_id: Some(Uuid::new_v4()),
            prereq_synthesis_ids: vec![Uuid::new_v4(), Uuid::new_v4()],
            workflow_run_id: Some(workflow_id),
            seed_theme_id: Some(theme_id),
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: SynthesisJobPayload = serde_json::from_value(v).unwrap();
        assert_eq!(back.synthesis_id, p.synthesis_id);
        assert_eq!(back.query, p.query);
        assert_eq!(back.agent_id, p.agent_id);
        assert_eq!(back.parent_synthesis_id, p.parent_synthesis_id);
        assert_eq!(back.prereq_synthesis_ids, p.prereq_synthesis_ids);
        assert_eq!(back.workflow_run_id, Some(workflow_id));
        assert_eq!(back.seed_theme_id, Some(theme_id));
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
        assert!(p.workflow_run_id.is_none());
        assert!(p.seed_theme_id.is_none());
    }

    /// `EmptyEdgeProvider` returns no neighbours.
    #[tokio::test]
    async fn empty_edge_provider_returns_no_neighbours() {
        let p = EmptyEdgeProvider;
        let n = p.neighbors(Uuid::new_v4(), &[EdgeType::Supports]).await;
        assert!(n.is_empty());
    }
}
