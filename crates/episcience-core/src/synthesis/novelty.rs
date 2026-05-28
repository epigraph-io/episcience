//! Novelty assessment — Stage 7 (Phase 6).
//!
//! After a synthesis is accepted by the verifier, the worker can score
//! how novel the narrative is against prior syntheses (the default
//! backend) or external sources (pluggable backends). The score is a
//! 0.0–1.0 number plus structured neighbour evidence, persisted on the
//! row for post-hoc inspection.

use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NoveltyScore {
    /// 0.0 (fully redundant) to 1.0 (highly novel).
    pub score: f64,
    /// The backend that produced the score (matches `NoveltyBackend::name`).
    pub backend: String,
    /// Top prior syntheses that overlap, sorted descending by similarity.
    pub neighbours: Vec<NoveltyNeighbour>,
    /// Free-form rationale text from the backend.
    pub rationale: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NoveltyNeighbour {
    pub synthesis_id: Uuid,
    /// Aggregate similarity (e.g. `0.5*cosine + 0.5*jaccard`).
    pub similarity: f64,
    /// Fraction of cluster members shared with the candidate (Jaccard).
    pub member_overlap: f64,
}

#[async_trait::async_trait]
pub trait NoveltyBackend: Send + Sync + std::fmt::Debug {
    /// Stable identifier (e.g. `"internal_prior_syntheses"`).
    fn name(&self) -> &'static str;

    async fn score(
        &self,
        candidate_synthesis_id: Uuid,
        candidate_narrative: &str,
        candidate_member_ids: &[Uuid],
    ) -> Result<NoveltyScore, NoveltyError>;
}

#[derive(Debug, thiserror::Error)]
pub enum NoveltyError {
    #[error("db: {0}")]
    Db(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}
