//! Synthesis-pipeline orchestration that crosses both core types and db
//! repositories. Lives in `episcience-db` (rather than `episcience-core`) so
//! Stage 2+ can call `crate::SynthesisRepository::save_snapshot_tx` etc.
//! without inducing a `core → db` cycle.

pub mod edge_writer;
pub mod novelty_backend_internal;
pub mod pipeline;
pub mod publish;
