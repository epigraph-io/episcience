//! Stage 7 novelty: the backend contract.
//!
//! A backend scores a freshly accepted synthesis (the CANDIDATE) against what
//! its readers can see. It runs on a connection the caller supplies: the
//! worker's stage transaction stamped as the synthesis' acting principal (so
//! row security applies exactly as to every other stage). Every read of a
//! backend goes through that connection AND the kernel viewer splice of the
//! READER (defence in depth: the same answer on a privileged connection).
//!
//! Every embedding a backend needs is computed by the caller BEFORE the
//! transaction opens ([`NoveltyCandidate`]), so no transaction is held across
//! a network call.
//!
//! The score types stay in `episcience_core` (they are persisted as JSON on
//! the row and their shape and backend names must not change).

use epigraph_db::Viewer;
pub use episcience_core::synthesis::novelty::{NoveltyError, NoveltyNeighbour, NoveltyScore};
use sqlx::PgConnection;
use uuid::Uuid;

/// The synthesis being scored, with its embeddings precomputed.
#[derive(Debug, Clone, Copy)]
pub struct NoveltyCandidate<'a> {
    /// The candidate synthesis.
    pub id: Uuid,
    /// Its cluster member claim ids.
    pub member_ids: &'a [Uuid],
    /// The embedding of the narrative HEAD
    /// ([`crate::synthesis::publish::narrative_head`]): the text stage 6b
    /// embeds and stores, so candidate and priors compare like with like.
    pub head_embedding: &'a [f32],
    /// The embedding of the FULL narrative, when the backend asked for it
    /// ([`NoveltyBackend::wants_narrative_embedding`]).
    pub narrative_embedding: Option<&'a [f32]>,
}

#[allow(clippy::double_must_use)] // async_trait's generated #[must_use] on an already-must-use boxed future
#[async_trait::async_trait]
pub trait NoveltyBackend: Send + Sync + std::fmt::Debug {
    /// Stable identifier, persisted in `syntheses.novelty_backend`.
    fn name(&self) -> &'static str;

    /// Whether [`Self::score`] needs [`NoveltyCandidate::narrative_embedding`].
    fn wants_narrative_embedding(&self) -> bool {
        false
    }

    /// Score `candidate` on `conn`, reading as `reader`: the candidate's
    /// acting principal (its job row's `principal_id`, D-S9). A reader that
    /// is not the job's principal is refused (the caller has the wrong
    /// session); a candidate with no job principal is scored against nothing.
    async fn score(
        &self,
        conn: &mut PgConnection,
        reader: &Viewer,
        candidate: &NoveltyCandidate<'_>,
    ) -> Result<NoveltyScore, NoveltyError>;
}

/// Whose eyes a candidate's inputs are read through.
///
/// The READER is the candidate's JOB principal (D-S9: a synthesis acts as
/// `synthesis_jobs.principal_id`), never its author: for a re-owned legacy
/// synthesis the author is a shared agent that reads nothing of the owner's.
/// The AUDIENCE is the candidate's own: a public candidate is compared only
/// with public inputs; a group candidate with public inputs and those its own
/// owner group holds. Both bounds hold at once, so a stored score never names
/// or reflects an input the candidate's readers cannot see.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CandidateAudience {
    /// `Some(g)` for a group candidate owned by `g`; `None` for a public one.
    pub group: Option<Uuid>,
}

/// The audience of `candidate` read on `conn`, after checking that `reader`
/// is its job principal. `Ok(None)` when the candidate or its job principal
/// is not visible on `conn` (nothing is read without a principal).
///
/// # Errors
/// [`NoveltyError::Db`] when the read fails, or when `reader` is not the
/// candidate's job principal.
pub(crate) async fn candidate_audience(
    conn: &mut PgConnection,
    reader: &Viewer,
    candidate: Uuid,
) -> Result<Option<CandidateAudience>, NoveltyError> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT j.principal_id, s.owner_group_id, s.visibility::text \
           FROM syntheses s LEFT JOIN synthesis_jobs j ON j.id = s.id WHERE s.id = $1",
    )
    .bind(candidate)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| NoveltyError::Db(e.to_string()))?;
    let Some((Some(principal), owner, visibility)) = row else {
        return Ok(None);
    };
    if reader.principal() != Some(principal) {
        return Err(NoveltyError::Db(format!(
            "candidate synthesis {candidate} is scored as its job principal, not as the \
             supplied reader"
        )));
    }
    Ok(Some(CandidateAudience {
        group: match visibility.as_deref() {
            Some("public") => None,
            _ => owner,
        },
    }))
}

/// The backend for a skill: `"literature"` scores against prior syntheses AND
/// prior DOI-labelled claims ([`super::novelty_backend_paper::PaperNoveltyBackend`]);
/// every other skill against prior syntheses only
/// ([`super::novelty_backend_internal::InternalNoveltyBackend`]).
#[must_use]
pub fn select_novelty_backend(skill_name: &str) -> Box<dyn NoveltyBackend> {
    match skill_name {
        "literature" => Box::new(super::novelty_backend_paper::PaperNoveltyBackend),
        _ => Box::new(super::novelty_backend_internal::InternalNoveltyBackend),
    }
}
