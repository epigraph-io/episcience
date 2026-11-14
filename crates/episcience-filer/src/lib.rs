//! EpiScience experiment filer, phase 0: shadow mode.
//!
//! The experiment filer files each agent-run experiment from its contract
//! files alone. Phase 0 reads and verifies those files, journals what it saw in
//! an append-only outbox, and emits the filing plan it WOULD execute, one JSON
//! line per record. It writes nothing to EpiGraph or EpiScience: the crate has
//! no database driver, no HTTP client and no kernel dependency, and its source
//! opens no socket and spawns no program. `tests/shadow_only.rs` guards
//! those lines; it is a ratchet against the obvious routes, not a proof that
//! no write is possible.
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
//! A contract file must be a regular file (not a symlink, FIFO or device) of
//! at most 16 MiB.
//!
//! A directory that breaks any rule is *rejected*: the plan carries a
//! `rejection` record naming the directory and the reason, and no run or gate
//! record for it. A rejection is keyed by its directory and reason only (empty
//! campaign and cell), since its `RUN.json` is unverified. A directory below
//! the root that cannot be scanned (its name is not UTF-8, or listing it
//! fails) is rejected the same way and its subtree is skipped; a non-UTF-8
//! name is rendered with `\` doubled and each invalid byte as `\xNN`.
//!
//! Open contract questions for phase 1:
//!
//! - A stray `RUN.json`, `RESULT.json` or manifest at campaign level makes the
//!   campaign directory a run directory, which hides every cell below it
//!   (they are not scanned, and the campaign directory files one rejection).
//! - A run still being written is rejected rather than deferred; phase 1 needs
//!   a completion rule (for example, the manifest is written last).
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
//!
//! Canonical JSON writes non-ASCII text as raw UTF-8, not `\uXXXX` escapes,
//! and object keys in byte order. A writer in another language must match
//! that to reproduce a hash (Python: `json.dumps(v, sort_keys=True,
//! separators=(",", ":"), ensure_ascii=False)`).

pub mod canonical;
pub mod contract;
pub mod journal;
pub mod plan;
pub mod watch;
