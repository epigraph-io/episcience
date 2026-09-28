//! Job-queue infrastructure for the synthesis pipeline.
//!
//! `synthesis_jobs` is owned by episcience (id FK to `syntheses`, columns
//! `attempts / max_attempts / last_error`, states `queued / running / complete
//! / failed / retry`). Upstream `epigraph_jobs::PostgresJobQueue` is hardcoded
//! to a `jobs` table with different columns, so we provide our own
//! [`EpiscienceJobQueue`] implementation of [`epigraph_jobs::JobQueue`].

pub mod episcience_job_queue;
pub mod session;
pub mod synthesis_job;
pub mod worker;

pub use episcience_job_queue::EpiscienceJobQueue;
pub use session::{OwnerSession, SessionError, StageSession, StageTx};
pub use synthesis_job::{
    resolve_skill_for_row, resolve_traversal_config, select_novelty_backend, ArcEdgeProvider,
    ArcLlm, EmptyEdgeProvider, SynthesisJobHandler, SynthesisJobPayload,
};
