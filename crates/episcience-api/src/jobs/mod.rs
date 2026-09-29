//! The synthesis job machinery `episcience-worker` runs.
//!
//! `synthesis_jobs` is owned by episcience (id FK to `syntheses`, one row per
//! synthesis). The worker is a plain loop over the queue definers
//! ([`worker`]), not `epigraph_jobs::JobRunner`: the application login holds
//! no UPDATE on `synthesis_jobs`, so claims, finishes and retries go through
//! the maintenance-owned queue definers. Every stage's writes run on a
//! [`StageSession`] stamped as the job's acting principal.

pub mod session;
pub mod synthesis_job;
pub mod worker;

pub use session::{OwnerSession, SessionError, StageSession, StageTx};
pub use synthesis_job::{
    resolve_skill_for_row, resolve_traversal_config, select_novelty_backend, ArcEdgeProvider,
    ArcLlm, EmptyEdgeProvider, SynthesisJobHandler, SynthesisJobPayload,
};
