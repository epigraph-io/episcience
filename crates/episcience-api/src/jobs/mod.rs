//! Job-queue infrastructure for the synthesis pipeline.
//!
//! `synthesis_jobs` is owned by episcience (id FK to `syntheses`, columns
//! `attempts / max_attempts / last_error`, states `queued / running / complete
//! / failed / retry`). Upstream `epigraph_jobs::PostgresJobQueue` is hardcoded
//! to a `jobs` table with different columns, so we provide our own
//! [`EpiscienceJobQueue`] implementation of [`epigraph_jobs::JobQueue`].

pub mod episcience_job_queue;
pub mod synthesis_job;

pub use episcience_job_queue::EpiscienceJobQueue;
pub use synthesis_job::{
    ArcEdgeProvider, ArcLlm, EmptyEdgeProvider, SynthesisJobHandler, SynthesisJobPayload,
};
