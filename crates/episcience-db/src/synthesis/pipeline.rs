//! Synthesis pipeline — Stages 1 and 2.
//!
//! Wires together the seed → traverse → cluster → compose → validate → narrate
//! pipeline. Phase 2 / Tasks 2.2 + 2.3 implement Stages 1 and 2; subsequent
//! stages land in later tasks.
//!
//! # Stage 1 — Seed
//!
//! Calls [`epigraph_engine::recall::recall`] with the user's natural-language
//! query and returns a `Vec<Uuid>` of seed claim ids. Empty results are
//! surfaced as [`SynthesisError::EmptyResult`] so the caller can short-circuit
//! before traversal.
//!
//! # Stage 2 — Traverse + persist
//!
//! BFS traversal over the trust-edge graph using the configured edge provider,
//! relevance-pruning by cosine similarity against the precomputed query
//! embedding. Persists the resulting [`SubgraphSnapshot`] (incl. per-claim
//! Bel/Pl/BetP intervals) and the `synthesis_claim_membership` rows in a
//! single transaction so the two never diverge.
//!
//! Note: `recall::recall` returns `RecallResult.claim_id: String` (the
//! UUID's string form). We parse back to `Uuid` here so downstream stages get
//! the typed id directly. A parse failure means the upstream library returned
//! a malformed id and is treated as a validation error.

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{self, EdgeProvider, TraversalConfig};
use episcience_core::synthesis::{BeliefIntervalEntry, SubgraphSnapshot};

use crate::{SynthesisMembershipRepository, SynthesisRepository};

/// End-to-end synthesis pipeline.
///
/// Generic over the LLM client and edge provider so tests can inject mocks
/// without depending on the production transport stack. The embedder is held
/// behind a trait object because [`epigraph_embeddings::EmbeddingService`] is
/// already a trait-object-friendly `Send + Sync` interface.
///
/// `query_embedding` is the precomputed embedding of the user's natural-language
/// query, used by Stage 2 traversal to score relevance of candidate neighbours
/// via cosine similarity. Stage 1 doesn't read it; callers that only run
/// Stage 1 can pass `vec![]`.
pub struct SynthesisPipeline<L, P> {
    pub pool: PgPool,
    pub embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
    pub llm_client: L,
    pub edge_provider: P,
    pub query_embedding: Vec<f32>,
}

impl<L, P> SynthesisPipeline<L, P> {
    /// Construct a new pipeline with the supplied dependencies.
    ///
    /// `query_embedding` should be the embedding of the user's query as
    /// produced by [`epigraph_embeddings::EmbeddingService::generate_query`].
    /// Pass `vec![]` if you only intend to call [`Self::stage1_seed`] — Stage
    /// 1 doesn't read it.
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
        llm_client: L,
        edge_provider: P,
        query_embedding: Vec<f32>,
    ) -> Self {
        Self {
            pool,
            embedder,
            llm_client,
            edge_provider,
            query_embedding,
        }
    }
}

// Stage 1 needs no `LlmClient` / `EdgeProvider` bounds — it only touches
// `self.pool` and `self.embedder`. Keeping this impl unbounded lets
// `epigraph-cli` stay an optional dependency (gated by the `test-utils`
// feature). Future stages that DO call `self.llm_client` or
// `self.edge_provider` will live in their own `impl` blocks with the bounds
// they actually need (likely re-exporting `LlmClient` through
// `episcience_core` so the trait is visible without the cli dep).
impl<L, P> SynthesisPipeline<L, P> {
    /// Stage 1 — Seed.
    ///
    /// Calls `epigraph_engine::recall::recall` and returns the parsed seed
    /// claim ids. An empty result set is mapped to
    /// [`SynthesisError::EmptyResult`] (not `Ok(vec![])`) so the runner can
    /// fail fast before traversal.
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::EmptyResult`] — recall returned zero results.
    /// - [`SynthesisError::Db`] — recall's database / fallback path failed.
    /// - [`SynthesisError::Validation`] — recall returned a malformed UUID
    ///   string (should not happen with the upstream contract; defensive).
    pub async fn stage1_seed(
        &self,
        query: &str,
        limit: usize,
        min_truth: f64,
    ) -> Result<Vec<Uuid>, SynthesisError> {
        let results = epigraph_engine::recall::recall(
            &self.pool,
            self.embedder.as_ref(),
            query,
            limit,
            min_truth,
        )
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;

        if results.is_empty() {
            return Err(SynthesisError::EmptyResult);
        }

        results
            .into_iter()
            .map(|r| {
                Uuid::parse_str(&r.claim_id).map_err(|e| {
                    SynthesisError::Validation(format!(
                        "recall returned malformed claim_id {:?}: {}",
                        r.claim_id, e
                    ))
                })
            })
            .collect()
    }
}

impl<L, P> SynthesisPipeline<L, P>
where
    P: EdgeProvider,
{
    /// Stage 2 — Traverse + persist.
    ///
    /// BFS over the trust-edge graph from `seeds`, pruning candidate neighbours
    /// whose stored embedding's cosine similarity against `self.query_embedding`
    /// is below `cfg.relevance_prune`. For each surviving claim we compute (or
    /// look up cached) Bel/Pl/BetP via [`epigraph_engine::belief_query::get_belief`]
    /// in the unframed mode (frame_id = `None`).
    ///
    /// The resulting [`SubgraphSnapshot`] and the corresponding
    /// `synthesis_claim_membership` rows are persisted in a single transaction
    /// so the two stay consistent: either both succeed or neither does.
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::Db`] — any database error during traversal,
    ///   belief lookup, or persistence.
    pub async fn stage2_traverse(
        &self,
        synthesis_id: Uuid,
        seeds: Vec<Uuid>,
        cfg: &TraversalConfig,
    ) -> Result<SubgraphSnapshot, SynthesisError> {
        // 1. BFS via traversal::traverse with relevance closure that fetches
        //    stored claim embedding via EmbeddingService::get and computes
        //    cosine vs self.query_embedding.
        let q_embed = self.query_embedding.clone();
        let embedder = self.embedder.clone();
        let mut snapshot = traversal::traverse(&seeds, cfg, &self.edge_provider, |c| {
            let q = q_embed.clone();
            let e = embedder.clone();
            async move {
                match e.get(c).await {
                    Ok(c_embed) => episcience_core::synthesis::util::cosine(&q, &c_embed),
                    // Claim has no stored embedding → conservative prune.
                    Err(_) => 0.0,
                }
            }
        })
        .await?;

        // 2. Per-claim get_belief (unframed; frame_id = None).
        for cid in &snapshot.claim_ids {
            let bi = epigraph_engine::belief_query::get_belief(&self.pool, *cid, None)
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
            snapshot.belief_intervals.push(BeliefIntervalEntry {
                claim_id: *cid,
                frame_id: None,
                belief: bi.belief,
                plausibility: bi.plausibility,
                pignistic_prob: bi.pignistic_prob,
                framed: bi.framed,
            });
        }

        // 3. Persist snapshot + membership in one transaction.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
        SynthesisRepository::save_snapshot_tx(&mut tx, synthesis_id, &snapshot)
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
        SynthesisMembershipRepository::replace_for_synthesis(
            &mut tx,
            synthesis_id,
            &snapshot.claim_ids,
        )
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;

        Ok(snapshot)
    }
}
