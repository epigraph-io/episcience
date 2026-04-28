//! Synthesis pipeline — Stage 1 (Seed via library `recall`).
//!
//! Wires together the seed → traverse → cluster → compose → validate → narrate
//! pipeline. Phase 2 / Task 2.2 implements Stage 1 only; subsequent stages land
//! in later tasks.
//!
//! # Stage 1 — Seed
//!
//! Calls [`epigraph_engine::recall::recall`] with the user's natural-language
//! query and returns a `Vec<Uuid>` of seed claim ids. Empty results are
//! surfaced as [`SynthesisError::EmptyResult`] so the caller can short-circuit
//! before traversal.
//!
//! Note: `recall::recall` returns `RecallResult.claim_id: String` (the
//! UUID's string form). We parse back to `Uuid` here so downstream stages get
//! the typed id directly. A parse failure means the upstream library returned
//! a malformed id and is treated as a validation error.

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::synthesis::errors::SynthesisError;

/// End-to-end synthesis pipeline.
///
/// Generic over the LLM client and edge provider so tests can inject mocks
/// without depending on the production transport stack. The embedder is held
/// behind a trait object because [`epigraph_embeddings::EmbeddingService`] is
/// already a trait-object-friendly `Send + Sync` interface.
pub struct SynthesisPipeline<L, P> {
    pub pool: PgPool,
    pub embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
    pub llm_client: L,
    pub edge_provider: P,
}

impl<L, P> SynthesisPipeline<L, P> {
    /// Construct a new pipeline with the supplied dependencies.
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
        llm_client: L,
        edge_provider: P,
    ) -> Self {
        Self {
            pool,
            embedder,
            llm_client,
            edge_provider,
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
