//! EpiScience experiment filer, phase 0: shadow mode.
//!
//! The experiment filer files each agent-run experiment from its contract
//! files alone. Phase 0 reads and verifies those files, journals what it saw in
//! an append-only outbox, and emits the filing plan it WOULD execute, one JSON
//! line per record. It writes nothing to EpiGraph or EpiScience: the crate has
//! no database driver, no HTTP client and no kernel dependency
//! (`tests/shadow_only.rs` holds that line).
//!
//! # Run contract (phase 0 proposal)
//!
//! A *run directory* is any directory under the watched root that holds at
//! least one of the contract file names below. The scanner does not descend
//! into a run directory, and does not follow symlinked directories.
//!
//! - `RUN.json`: a JSON object with non-empty string fields `campaign` and
//!   `cell`. Every other field is carried into the plan verbatim.
//! - `RESULT.json`: a JSON object. If it has `gates`, that is an array of
//!   objects, each with a non-empty string `name` (unique within the run) and
//!   a boolean `passed`. Each gate object is carried into the plan verbatim.
//! - `MANIFEST.sha256` (`sha256sum` output) and/or `MANIFEST.b3` (`b3sum`
//!   output). At least one is required; every one present is verified. Each
//!   non-blank line is `<64 lowercase hex digits><sep><path>`, where `<sep>` is
//!   two spaces (text mode) or a space and `*` (binary mode). Paths are
//!   relative to the run directory, use `/`, and may not be absolute, contain
//!   `..`, be listed twice, name a manifest file, or resolve outside the run
//!   directory through a symlink. An empty manifest is rejected. The manifest
//!   must list `RUN.json` and `RESULT.json`, so every filed fact comes from
//!   bytes the manifest attests.
//!
//! A directory that breaks any rule is *rejected*: the plan carries a
//! `rejection` record naming the directory and the reason, and no run or gate
//! record for it.
//!
//! # Plan records
//!
//! Every record has a deterministic key (campaign, cell, event, content hash),
//! a `key_id` (the SHA-256 of the key's canonical JSON) and a `record_hash`
//! (the SHA-256 of the canonical key, kind and payload, an integrity check).
//! Kinds:
//!
//! - `campaign`: once per campaign seen in a valid run (`cell` is empty).
//! - `run`: one per valid run; the content hash is the manifest hash
//!   (`MANIFEST.sha256` when present, else `MANIFEST.b3`).
//! - `gate`: one per gate in `RESULT.json`; event `gate:<name>`; the content
//!   hash covers the gate object and its run's manifest hash.
//! - `rejection`: one per rejected run directory.
//!
//! Plan lines are canonical JSON (object keys sorted, no whitespace), sorted
//! by key, with no wall-clock time and no absolute path, so the same inputs
//! always give a byte-identical plan.

pub mod canonical;
pub mod contract;
