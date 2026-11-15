//! The Experiment Filer, phase 0 (shadow mode).
//!
//! The filer files each agent-run experiment from the run's contract files
//! alone: a manifest (`MANIFEST.sha256` or `MANIFEST.b3`), `RUN.json` and
//! `RESULT.json`. Phase 0 is the read side only:
//!
//! - [`contract`]: typed readers for `RUN.json` and `RESULT.json`;
//! - [`manifest`]: manifest parsing and verification (both hash formats);
//! - [`validate`]: the validator, with a typed [`validate::Rejection`] per
//!   failure;
//! - [`plan`]: the shadow filing plan, one record per write a later phase
//!   would make, each under a deterministic key;
//! - [`journal`]: the append-only, fsynced JSONL outbox journal;
//! - [`watcher`]: the polling scan that feeds the journal.
//!
//! Shadow mode sends nothing anywhere. This crate has no network client, no
//! database client and no dependency on `episcience-api`, `episcience-db` or
//! a kernel crate; the only thing it writes is its own journal file. The
//! routes named in a plan record are the ones a later phase calls (see
//! `docs/experiment-filer.md`).

pub mod config;
pub mod contract;
pub mod errors;
pub mod journal;
pub mod manifest;
pub mod plan;
pub mod validate;
pub mod watcher;

pub use errors::FilerError;
