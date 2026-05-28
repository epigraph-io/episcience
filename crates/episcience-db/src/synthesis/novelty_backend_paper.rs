//! `PaperNoveltyBackend` — composite novelty backend for literature
//! syntheses. Wraps [`InternalNoveltyBackend`] and additionally scores
//! against prior `doi`-labeled claims. Final score is
//! `min(internal, 1.0 - top_doi_similarity)` — both sources must agree
//! the candidate is novel for a high combined score.
//!
//! # Schema dependency
//!
//! Claim embeddings live on the upstream `claims` table directly as
//! the `embedding vector(1536)` column (NOT a separate
//! `claim_embeddings` table). DOI provenance is encoded as a `'doi'`
//! entry in the `claims.labels text[]` column (GIN-indexed via
//! `idx_claims_labels`). The query in
//! [`find_top_doi_claim_similarity`] therefore selects directly off
//! `claims` and uses `'doi' = ANY(c.labels)` for the filter.
//!
//! Embeddings are read via the `::text` cast and parsed in-Rust to
//! avoid wiring pgvector through this read path — same approach
//! [`InternalNoveltyBackend`] uses for `synthesis_embeddings`. The
//! parser is duplicated here (rather than shared) so the two
//! backends can evolve independently if the encode side ever
//! differs; the parser is trivial.
//!
//! # Empty-corpus behaviour
//!
//! When no DOI-labeled claims exist (common today — `doi` labels are
//! seeded by upstream ingestion, not by episcience), the SQL returns
//! zero rows, `top_doi_similarity` stays at 0.0, and the combined
//! score collapses to `min(internal, 1.0) = internal`. The backend
//! is then behaviourally equivalent to `InternalNoveltyBackend`
//! (modulo the `name()` and `rationale` strings).

use crate::synthesis::novelty_backend_internal::InternalNoveltyBackend;
use episcience_core::synthesis::novelty::{NoveltyBackend, NoveltyError, NoveltyScore};
use sqlx::PgPool;
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

pub struct PaperNoveltyBackend {
    pub pool: PgPool,
    pub embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
}

// Manual `Debug` impl: `EmbeddingService` is a trait object without a
// `Debug` supertrait, so `#[derive(Debug)]` doesn't work. Mirror the
// internal backend's placeholder so log lines stay shape-consistent
// across both backends.
impl std::fmt::Debug for PaperNoveltyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaperNoveltyBackend")
            .field("pool", &"<PgPool>")
            .field("embedder", &"<dyn EmbeddingService>")
            .finish()
    }
}

#[async_trait::async_trait]
impl NoveltyBackend for PaperNoveltyBackend {
    fn name(&self) -> &'static str {
        "paper_novelty"
    }

    async fn score(
        &self,
        candidate_id: Uuid,
        candidate_narrative: &str,
        candidate_member_ids: &[Uuid],
    ) -> Result<NoveltyScore, NoveltyError> {
        // 1. Internal prior-syntheses score. Reuses the exact
        //    `InternalNoveltyBackend` implementation so behaviour for
        //    the "internal" half stays identical to non-literature
        //    skills — only the additional DOI signal is new here.
        let internal = InternalNoveltyBackend {
            pool: self.pool.clone(),
            embedder: self.embedder.clone(),
        };
        let internal_score = internal
            .score(candidate_id, candidate_narrative, candidate_member_ids)
            .await?;

        // 2. Top similarity against prior DOI-labeled claims. We embed
        //    the candidate narrative once (full text, NOT the
        //    InternalNoveltyBackend's "head" heuristic) because the
        //    DOI claims being compared against are themselves full
        //    claim contents — not synthesis narrative heads — so
        //    embedding the whole narrative is closer to apples-to-
        //    apples. An embedder failure is fatal here (Unavailable):
        //    without the candidate's vector there's nothing to score
        //    against, so we surface rather than silently returning
        //    0.0 (which would falsely report "no DOI overlap").
        let cand_emb = self
            .embedder
            .generate(candidate_narrative)
            .await
            .map_err(|e| NoveltyError::Unavailable(e.to_string()))?;
        let top_doi = find_top_doi_claim_similarity(&self.pool, &cand_emb)
            .await
            .map_err(|e| NoveltyError::Db(e.to_string()))?;

        // 3. Combine: take the worse of the two novelty signals. The
        //    `clamp` guards against floating-point drift pushing
        //    cosine slightly above 1.0 (which would yield a negative
        //    `1.0 - top_doi`).
        let combined = internal_score.score.min((1.0 - top_doi).clamp(0.0, 1.0));

        Ok(NoveltyScore {
            score: combined,
            backend: self.name().to_string(),
            // DOI matches don't fit the `NoveltyNeighbour` shape
            // (which expects a `synthesis_id`); pass through the
            // internal neighbours verbatim and surface the DOI signal
            // via `rationale` so post-hoc inspection still sees both
            // numbers. A future schema change could extend
            // `NoveltyNeighbour` with a `kind` discriminator if DOI
            // neighbour ids become useful for the UI.
            neighbours: internal_score.neighbours,
            rationale: format!(
                "internal_syntheses {:.3}; top_doi_similarity {:.3}; combined {:.3}",
                internal_score.score, top_doi, combined
            ),
        })
    }
}

/// Find the maximum cosine similarity between `cand_emb` and the
/// embeddings of `claims` rows that carry a `doi` label.
///
/// Returns 0.0 when no DOI claims exist (so the caller's
/// `1.0 - top_doi` correctly yields 1.0 = no DOI signal). Claims
/// without a stored embedding silently drop via `embedding IS NOT
/// NULL` — they're not comparable so excluding them is the right
/// behaviour, mirroring how `InternalNoveltyBackend` handles priors
/// without an embedding row.
///
/// Reads `embedding::text` and parses in-Rust rather than binding the
/// pgvector type — same workaround `InternalNoveltyBackend` uses, and
/// avoids depending on `pgvector` crate's sqlx integration in this
/// read path. For the current scale (a few thousand DOI claims at
/// most) the in-Rust loop is fine; if the corpus grows large this
/// should move to a `vector <=> $1 ORDER BY 1 LIMIT 1` server-side
/// nearest-neighbour query (the `idx_claims_embedding_hnsw` HNSW
/// index already exists).
async fn find_top_doi_claim_similarity(
    pool: &PgPool,
    cand_emb: &[f32],
) -> Result<f64, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT embedding::text AS emb_text \
         FROM claims \
         WHERE 'doi' = ANY(labels) \
           AND embedding IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut top: f64 = 0.0;
    for row in rows {
        let text: String = row.try_get("emb_text")?;
        let v = parse_vector_text(&text);
        let sim = cosine(cand_emb, &v);
        if sim > top {
            top = sim;
        }
    }
    Ok(top)
}

/// Cosine similarity between two equal-length `f32` vectors,
/// promoted to `f64` for accumulation. Returns 0.0 for empty or
/// length-mismatched inputs (defensive — length mismatch should
/// never happen in practice since both sides are 1536-dim).
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Parse a pgvector text literal `"[1.0,2.0,3.0]"` into `Vec<f32>`.
/// Tolerates the missing-brackets case (`"1.0,2.0"`) so test fixtures
/// can pass raw csv. Unparseable tokens are skipped — pgvector emits a
/// strict format so this should not trigger on real reads.
fn parse_vector_text(text: &str) -> Vec<f32> {
    text.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_identical_is_one() {
        assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_vector_text_round_trip() {
        let v = parse_vector_text("[1.0, 2.0, 3.0]");
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn parse_vector_text_handles_no_brackets() {
        let v = parse_vector_text("1.0, 2.0");
        assert_eq!(v, vec![1.0, 2.0]);
    }

    /// Stable backend identifier — the job handler dispatches on
    /// `pipeline.skill.name() == "literature"`, then later persists
    /// `NoveltyScore.backend` to the `syntheses.novelty_backend`
    /// column. The integration test for the dispatch path
    /// (`literature_skill_dispatches_to_paper_novelty_backend`) reads
    /// that column and asserts equality with this string; if it ever
    /// changes both sides must move together.
    /// `#[tokio::test]` rather than `#[test]`: sqlx's
    /// `PgPoolOptions::connect_lazy` spawns a background reaper task
    /// during construction, which panics without a Tokio runtime. No
    /// actual DB connection is made.
    #[tokio::test]
    async fn backend_name_is_paper_novelty() {
        // Constructs via `connect_lazy` to avoid a DB roundtrip — the
        // `name()` method takes neither `pool` nor `embedder`, so the
        // lazy pool never actually connects.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://test:test@127.0.0.1:5432/test")
            .expect("lazy pool must construct without a DB roundtrip");
        // Inline stub embedder rather than depending on the test-utils
        // module from `pipeline.rs::tests` — kept self-contained so
        // this single test runs in isolation.
        #[derive(Debug, Default)]
        struct StubEmbedder;
        #[async_trait::async_trait]
        impl epigraph_embeddings::service::EmbeddingService for StubEmbedder {
            async fn generate(
                &self,
                _t: &str,
            ) -> Result<Vec<f32>, epigraph_embeddings::errors::EmbeddingError> {
                Ok(vec![])
            }
            async fn batch_generate(
                &self,
                _t: &[&str],
            ) -> Result<Vec<Vec<f32>>, epigraph_embeddings::errors::EmbeddingError> {
                Ok(vec![])
            }
            async fn store(
                &self,
                _c: Uuid,
                _e: &[f32],
            ) -> Result<(), epigraph_embeddings::errors::EmbeddingError> {
                Ok(())
            }
            async fn get(
                &self,
                claim_id: Uuid,
            ) -> Result<Vec<f32>, epigraph_embeddings::errors::EmbeddingError> {
                Err(epigraph_embeddings::errors::EmbeddingError::NotFound { claim_id })
            }
            async fn similar(
                &self,
                _e: &[f32],
                _k: usize,
                _m: f32,
            ) -> Result<
                Vec<epigraph_embeddings::service::SimilarClaim>,
                epigraph_embeddings::errors::EmbeddingError,
            > {
                Ok(vec![])
            }
            fn dimension(&self) -> usize {
                1536
            }
            fn token_usage(&self) -> epigraph_embeddings::service::TokenUsage {
                epigraph_embeddings::service::TokenUsage::default()
            }
            fn reset_token_usage(&self) {}
            async fn health_check(
                &self,
            ) -> Result<(), epigraph_embeddings::errors::EmbeddingError> {
                Ok(())
            }
        }
        let backend = PaperNoveltyBackend {
            pool,
            embedder: Arc::new(StubEmbedder),
        };
        assert_eq!(backend.name(), "paper_novelty");
    }
}
