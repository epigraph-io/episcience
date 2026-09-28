//! `PaperNoveltyBackend` — composite novelty backend for literature
//! syntheses. Runs the internal backend's scoring and additionally scores
//! against prior `doi`-labeled claims, every read on the caller's connection
//! as the candidate's job principal (see [`crate::synthesis::novelty`]). Final score is
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
//!
//! [`InternalNoveltyBackend`]: crate::synthesis::novelty_backend_internal::InternalNoveltyBackend

use crate::synthesis::novelty::{
    candidate_audience, NoveltyBackend, NoveltyCandidate, NoveltyError, NoveltyScore,
};
use crate::synthesis::novelty_backend_internal::internal_score;
use epigraph_db::Viewer;
use sqlx::PgConnection;
use sqlx::Row;
use uuid::Uuid;

/// Scores against prior syntheses (the internal half) AND prior DOI-labelled
/// claims. Stateless: every read runs on the connection
/// [`NoveltyBackend::score`] is given; the candidate's full-narrative
/// embedding is computed by the caller before the transaction opens.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaperNoveltyBackend;

#[async_trait::async_trait]
impl NoveltyBackend for PaperNoveltyBackend {
    fn name(&self) -> &'static str {
        "paper_novelty"
    }

    /// The DOI half compares the FULL narrative (not the internal backend's
    /// head heuristic) because the DOI claims it is compared against are
    /// themselves full claim contents.
    fn wants_narrative_embedding(&self) -> bool {
        true
    }

    async fn score(
        &self,
        conn: &mut PgConnection,
        reader: &Viewer,
        candidate: &NoveltyCandidate<'_>,
    ) -> Result<NoveltyScore, NoveltyError> {
        // 1. Internal prior-syntheses score: exactly the internal backend's
        //    behaviour, so only the additional DOI signal is new here.
        let internal = internal_score(conn, reader, candidate, "internal_prior_syntheses").await?;

        // 2. Top similarity against prior DOI-labeled claims. Without the
        //    candidate's full-narrative vector there is nothing to score
        //    against, so a missing one is surfaced (Unavailable) rather than
        //    silently returning 0.0 (which would falsely report "no DOI
        //    overlap").
        let cand_emb = candidate.narrative_embedding.ok_or_else(|| {
            NoveltyError::Unavailable("the candidate's narrative embedding was not computed".into())
        })?;
        // The DOI claims are read AS the candidate's job principal and
        // bounded by the candidate's audience: a claim either cannot read
        // never shapes its novelty score.
        let audience = candidate_audience(conn, reader, candidate.id)
            .await?
            .ok_or_else(|| {
                NoveltyError::Db(format!(
                    "candidate synthesis {} has no job principal",
                    candidate.id
                ))
            })?;
        let top_doi = find_top_doi_claim_similarity(conn, reader, audience.group, cand_emb)
            .await
            .map_err(|e| NoveltyError::Db(e.to_string()))?;

        // 3. Combine: take the worse of the two novelty signals. The
        //    `clamp` guards against floating-point drift pushing
        //    cosine slightly above 1.0 (which would yield a negative
        //    `1.0 - top_doi`).
        let combined = internal.score.min((1.0 - top_doi).clamp(0.0, 1.0));

        Ok(NoveltyScore {
            score: combined,
            backend: self.name().to_string(),
            // DOI matches don't fit the `NoveltyNeighbour` shape
            // (which expects a `synthesis_id`); pass through the
            // internal neighbours verbatim and surface the DOI signal
            // via `rationale` so post-hoc inspection still sees both
            // numbers.
            neighbours: internal.neighbours,
            rationale: format!(
                "internal_syntheses {:.3}; top_doi_similarity {:.3}; combined {:.3}",
                internal.score, top_doi, combined
            ),
        })
    }
}

/// Find the maximum cosine similarity between `cand_emb` and the
/// embeddings of the `claims` rows carrying a `doi` label that `viewer` can
/// read (the kernel's `/* {VISIBILITY:c} */` splice, and the kernel's claims
/// row security on a stamped `conn`).
///
/// Returns 0.0 when no DOI claims exist (so the caller's
/// `1.0 - top_doi` correctly yields 1.0 = no DOI signal). Claims
/// without a stored embedding silently drop via `embedding IS NOT
/// NULL`.
///
/// Reads `embedding::text` and parses in-Rust rather than binding the
/// pgvector type. For the current scale (a few thousand DOI claims at
/// most) the in-Rust loop is fine; if the corpus grows large this
/// should move to a `vector <=> $1 ORDER BY 1 LIMIT 1` server-side
/// nearest-neighbour query (the `idx_claims_embedding_hnsw` HNSW
/// index already exists).
async fn find_top_doi_claim_similarity(
    conn: &mut PgConnection,
    viewer: &Viewer,
    audience_group: Option<Uuid>,
    cand_emb: &[f32],
) -> Result<f64, sqlx::Error> {
    let sql = viewer.splice(
        "SELECT c.embedding::text AS emb_text \
         FROM claims c \
         WHERE 'doi' = ANY(c.labels) \
           AND c.embedding IS NOT NULL \
           AND (c.visibility::text = 'public' OR c.owner_group_id = $1) \
           /* {VISIBILITY:c} */",
        2,
    );
    let mut q = sqlx::query(&sql).bind(audience_group);
    if let Some(groups) = viewer.group_bind() {
        q = q.bind(groups);
    }
    let rows = q.fetch_all(&mut *conn).await?;
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

    /// Stable backend identifier — the handler persists
    /// `NoveltyScore.backend` to `syntheses.novelty_backend`, and the
    /// dispatch test (`select_novelty_backend_literature_picks_paper_novelty`)
    /// reads it; if it ever changes both sides must move together. The paper
    /// backend asks for the full-narrative embedding (the DOI half compares
    /// full texts); the internal one does not (it reuses stage 6b's head).
    #[test]
    fn backend_name_is_paper_novelty_and_it_wants_the_full_narrative() {
        assert_eq!(PaperNoveltyBackend.name(), "paper_novelty");
        assert!(PaperNoveltyBackend.wants_narrative_embedding());
        assert!(
            !crate::synthesis::novelty_backend_internal::InternalNoveltyBackend
                .wants_narrative_embedding()
        );
    }
}
