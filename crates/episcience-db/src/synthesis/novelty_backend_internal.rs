//! Internal novelty backend — scores against prior `syntheses` rows.
//!
//! Stage 7 (Phase 6). The default [`NoveltyBackend`] implementation: given
//! a freshly-accepted synthesis, find prior `complete` syntheses that
//! share at least one cluster member with the candidate and score each
//! prior as `0.5 * cosine(narrative head embeddings) + 0.5 * jaccard(member
//! ids)`, using the candidate's head embedding stage 6b already computed.
//! Score = `1.0 - top_neighbour.similarity` (clamped to `[0, 1]`). Every read
//! runs on the caller's connection as the candidate's job principal (see
//! [`crate::synthesis::novelty`]).
//!
//! Schema notes (verified against `\d syntheses` / `\d synthesis_embeddings`
//! / `\d synthesis_claim_membership`):
//!
//! - `syntheses` has no `narrative_embedding` column. The narrative head
//!   embedding lives in `synthesis_embeddings.embedding` (`vector(1536)`)
//!   keyed by `synthesis_id`, written by Stage 6b
//!   ([`crate::synthesis::publish::stage6_embed_narrative`]).
//! - Cluster membership lives in `synthesis_claim_membership`
//!   (`synthesis_id`, `claim_id`).
//!
//! Reading `vector(1536)` back into a `Vec<f32>` is done via
//! `embedding::text` cast and parsing the pgvector text literal
//! `"[1.0,2.0,...]"` — the inverse of `vec_to_text` in
//! [`crate::SynthesisEmbeddingsRepository`]. Direct sqlx mapping is
//! avoided because the pgvector crate isn't wired through this read
//! path; the text round-trip is cheap (one float parse per dim) and
//! deterministic.

use crate::synthesis::novelty::{
    candidate_audience, NoveltyBackend, NoveltyCandidate, NoveltyError, NoveltyNeighbour,
    NoveltyScore,
};
use epigraph_db::Viewer;
use sqlx::PgConnection;
use sqlx::Row;
use uuid::Uuid;

/// Scores against prior `complete` syntheses sharing a cluster member with
/// the candidate. Stateless: every read runs on the connection
/// [`NoveltyBackend::score`] is given.
#[derive(Debug, Clone, Copy, Default)]
pub struct InternalNoveltyBackend;

/// The internal score of `candidate` on `conn` as `reader` (shared by
/// [`InternalNoveltyBackend`] and the paper backend's internal half).
pub(crate) async fn internal_score(
    conn: &mut PgConnection,
    reader: &Viewer,
    candidate: &NoveltyCandidate<'_>,
    backend: &'static str,
) -> Result<NoveltyScore, NoveltyError> {
    // 1. Find prior `complete` syntheses sharing any member with the
    //    candidate. A prior without a narrative embedding (e.g. mid-flight
    //    Stage 6) silently drops via INNER JOIN — that's the intended
    //    behaviour, those rows aren't comparable anyway.
    let prior = find_priors_with_overlap(conn, reader, candidate.id, candidate.member_ids)
        .await
        .map_err(|e| match e {
            PriorError::Novelty(n) => n,
            PriorError::Db(e) => NoveltyError::Db(e.to_string()),
        })?;

    if prior.is_empty() {
        return Ok(NoveltyScore {
            score: 1.0,
            backend: backend.to_string(),
            neighbours: vec![],
            rationale: "no prior synthesis shares any cluster member".into(),
        });
    }

    // 2. Score each prior against the candidate's head embedding (the same
    //    head heuristic stage 6b embeds the priors with); keep the top 5.
    let mut scored: Vec<NoveltyNeighbour> = prior
        .into_iter()
        .map(|p| {
            let cos = cosine(candidate.head_embedding, &p.narrative_embedding);
            let overlap = jaccard(&p.member_ids, candidate.member_ids);
            NoveltyNeighbour {
                synthesis_id: p.id,
                similarity: 0.5 * cos + 0.5 * overlap,
                member_overlap: overlap,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(5);

    let top = scored.first().map(|n| n.similarity).unwrap_or(0.0);
    Ok(NoveltyScore {
        score: (1.0 - top).clamp(0.0, 1.0),
        backend: backend.to_string(),
        neighbours: scored,
        rationale: format!("top-prior similarity {top:.3}"),
    })
}

#[async_trait::async_trait]
impl NoveltyBackend for InternalNoveltyBackend {
    fn name(&self) -> &'static str {
        "internal_prior_syntheses"
    }

    async fn score(
        &self,
        conn: &mut PgConnection,
        reader: &Viewer,
        candidate: &NoveltyCandidate<'_>,
    ) -> Result<NoveltyScore, NoveltyError> {
        internal_score(conn, reader, candidate, self.name()).await
    }
}

/// Internal: a prior synthesis sharing at least one member with the
/// candidate. Carries enough state for scoring without round-tripping.
struct PriorSynthesis {
    id: Uuid,
    narrative_embedding: Vec<f32>,
    member_ids: Vec<Uuid>,
}

enum PriorError {
    Novelty(NoveltyError),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for PriorError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Find `complete`-status prior syntheses that share at least one cluster
/// member with the candidate, that the READER (the candidate's job
/// principal) can read (the kernel's `Viewer::splice`, and row security on a
/// stamped `conn`) AND that the candidate's audience can read
/// ([`candidate_audience`]). Excludes the candidate itself. A candidate
/// without a job principal yields no priors.
/// Returns the narrative embedding from `synthesis_embeddings.embedding`
/// (read via `::text` cast and parsed) and the flattened member id list (the
/// members `conn` can see).
///
/// Three-table join:
/// - `syntheses` filters status='complete' and excludes the candidate id.
/// - `synthesis_claim_membership` enforces "shares at least one member"
///   via `claim_id = ANY(candidate_member_ids)`.
/// - `synthesis_embeddings` INNER-JOINed — a prior without an embedding
///   row (mid-pipeline state) silently drops, which is the right
///   behaviour (it's not yet comparable).
async fn find_priors_with_overlap(
    conn: &mut PgConnection,
    reader: &Viewer,
    candidate_id: Uuid,
    candidate_member_ids: &[Uuid],
) -> Result<Vec<PriorSynthesis>, PriorError> {
    if candidate_member_ids.is_empty() {
        return Ok(vec![]);
    }
    let Some(audience) = candidate_audience(conn, reader, candidate_id)
        .await
        .map_err(PriorError::Novelty)?
    else {
        return Ok(vec![]);
    };
    let sql = reader.splice(
        "SELECT DISTINCT s.id, se.embedding::text AS embedding_text
         FROM syntheses s
         JOIN synthesis_claim_membership m ON m.synthesis_id = s.id
         JOIN synthesis_embeddings se ON se.synthesis_id = s.id
         WHERE s.id <> $1
           AND s.status = 'complete'
           AND m.claim_id = ANY($2)
           AND (s.visibility = 'public' OR s.owner_group_id = $3)
           /* {VISIBILITY:s} */",
        4,
    );
    let mut q = sqlx::query(&sql)
        .bind(candidate_id)
        .bind(candidate_member_ids)
        .bind(audience.group);
    if let Some(groups) = reader.group_bind() {
        q = q.bind(groups);
    }
    let rows = q.fetch_all(&mut *conn).await?;

    let mut priors = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let embedding_text: String = row.try_get("embedding_text")?;
        let narrative_embedding = parse_vector_text(&embedding_text);
        let member_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT claim_id FROM synthesis_claim_membership WHERE synthesis_id = $1",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;
        priors.push(PriorSynthesis {
            id,
            narrative_embedding,
            member_ids,
        });
    }
    Ok(priors)
}

/// Parse a pgvector text literal `"[1.0,2.0,3.0]"` into `Vec<f32>`.
/// Mirrors the encode side in `SynthesisEmbeddingsRepository::vec_to_text`.
/// Unparseable entries are skipped (defensive — pgvector emits a strict
/// format, so this should not trigger in practice).
fn parse_vector_text(s: &str) -> Vec<f32> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(trimmed);
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .filter_map(|tok| tok.trim().parse::<f32>().ok())
        .collect()
}

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

fn jaccard(a: &[Uuid], b: &[Uuid]) -> f64 {
    use std::collections::HashSet;
    let sa: HashSet<&Uuid> = a.iter().collect();
    let sb: HashSet<&Uuid> = b.iter().collect();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        sa.intersection(&sb).count() as f64 / union as f64
    }
}

#[cfg(test)]
mod helper_tests {
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
    fn cosine_empty_is_zero_not_nan() {
        assert_eq!(cosine(&[], &[]), 0.0);
    }
    #[test]
    fn jaccard_disjoint_is_zero() {
        let a = vec![Uuid::new_v4()];
        let b = vec![Uuid::new_v4()];
        assert_eq!(jaccard(&a, &b), 0.0);
    }
    #[test]
    fn jaccard_identical_is_one() {
        let u = Uuid::new_v4();
        assert_eq!(jaccard(&[u], &[u]), 1.0);
    }
    #[test]
    fn jaccard_half_overlap() {
        let shared = Uuid::new_v4();
        let a = vec![shared, Uuid::new_v4()];
        let b = vec![shared, Uuid::new_v4()];
        assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
    }

    /// Parse round-trip for the pgvector text format we round-trip through.
    /// The encode-side lives in `SynthesisEmbeddingsRepository::vec_to_text`;
    /// this test guards against drift in either direction.
    #[test]
    fn parse_vector_text_round_trip() {
        let parsed = parse_vector_text("[1,2.5,-3.25]");
        assert_eq!(parsed, vec![1.0_f32, 2.5, -3.25]);
    }

    #[test]
    fn parse_vector_text_empty() {
        assert!(parse_vector_text("[]").is_empty());
    }
}
