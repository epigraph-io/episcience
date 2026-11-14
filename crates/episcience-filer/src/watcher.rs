//! The watcher: a polling scan of a root directory for run directories.
//!
//! What counts as a run directory. Walking down from the root (sorted, not
//! following symlinks, at most `max_depth` levels), a directory that holds a
//! manifest or a `RUN.json` at its top level is a run directory; the walk
//! does not descend into it (what is below is the run's data).
//!
//! The completeness signal is the manifest. A manifest lists the digest of
//! every other file, `RESULT.json` included, so it can only be written
//! after them: a run directory WITH `MANIFEST.sha256` or `MANIFEST.b3` is
//! complete and is validated; one with `RUN.json` and no manifest is still
//! running and is left alone (reported as pending, never journaled).
//!
//! A complete directory can still be caught half-copied (a sync that
//! delivers the manifest first). It is then journaled as rejected, and
//! because a rejected run is re-validated on every scan and a run line is
//! keyed on its outcome, the scan that sees the finished copy journals the
//! acceptance. An accepted run (same directory, same manifest hash) is
//! skipped without re-hashing.
//!
//! Nothing is ever written under the root. The only output is the journal.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::contract::RUN_FILE;
use crate::errors::FilerError;
use crate::journal::{Journal, Outcome, RunEntry};
use crate::manifest::HashAlgorithm;
use crate::plan::build_plan;
use crate::validate::{validate_run_dir, Validation, ValidatorOptions};

/// Watcher settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatcherOptions {
    /// How many directory levels below the root are searched for run
    /// directories (0: only the root itself).
    pub max_depth: usize,
    pub validator: ValidatorOptions,
}

impl Default for WatcherOptions {
    fn default() -> Self {
        Self {
            max_depth: 8,
            validator: ValidatorOptions::default(),
        }
    }
}

/// What one scan found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScanSummary {
    /// Complete run directories seen.
    pub complete: usize,
    /// Run lines appended with outcome accepted.
    pub journaled_accepted: usize,
    /// Run lines appended with outcome rejected.
    pub journaled_rejected: usize,
    /// Complete run directories whose current state was already journaled.
    pub already_journaled: usize,
    /// Record lines appended.
    pub records_written: usize,
    /// Run directories with `RUN.json` and no manifest yet.
    pub pending: Vec<String>,
    /// Directories that could not be read or resolved (not journaled).
    pub errors: Vec<String>,
}

/// Run directories under a root, as one walk found them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    /// Complete run directories: (relative name, path), sorted.
    pub complete: Vec<(String, PathBuf)>,
    /// Run directories still running (relative names), sorted.
    pub pending: Vec<String>,
    pub errors: Vec<String>,
}

/// The polling watcher over one root.
#[derive(Debug, Clone)]
pub struct Watcher {
    root: PathBuf,
    options: WatcherOptions,
}

fn is_present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

impl Watcher {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, options: WatcherOptions) -> Self {
        Self {
            root: root.into(),
            options,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walk the root for run directories. Reads only.
    #[must_use]
    pub fn discover(&self) -> Discovery {
        let mut found = Discovery::default();
        self.walk(&self.root, ".", 0, &mut found);
        found
    }

    fn walk(&self, dir: &Path, rel: &str, depth: usize, found: &mut Discovery) {
        let has_manifest = HashAlgorithm::ALL
            .iter()
            .any(|a| is_present(&dir.join(a.manifest_file())));
        if has_manifest {
            found.complete.push((rel.to_string(), dir.to_path_buf()));
            return;
        }
        if is_present(&dir.join(RUN_FILE)) {
            found.pending.push(rel.to_string());
            return;
        }
        if depth >= self.options.max_depth {
            return;
        }
        let reader = match fs::read_dir(dir) {
            Ok(reader) => reader,
            Err(e) => {
                found.errors.push(format!("{rel}: {e}"));
                return;
            }
        };
        let mut children: Vec<String> = Vec::new();
        for entry in reader {
            match entry.and_then(|e| e.file_type().map(|t| (e.file_name(), t.is_dir()))) {
                // A symlink is not a directory here: the walk never follows
                // one out of (or around) the root.
                Ok((name, true)) => children.push(name.to_string_lossy().into_owned()),
                Ok(_) => {}
                Err(e) => found.errors.push(format!("{rel}: {e}")),
            }
        }
        children.sort();
        for name in children {
            let child_rel = if rel == "." {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            self.walk(&dir.join(&name), &child_rel, depth + 1, found);
        }
    }

    /// One scan: validate every complete run directory whose current state
    /// is not journaled yet and append the outcome (and, when accepted, the
    /// shadow plan) to `journal`. Sends nothing anywhere.
    pub fn scan_once(&self, journal: &mut Journal) -> Result<ScanSummary, FilerError> {
        let found = self.discover();
        let mut summary = ScanSummary {
            complete: found.complete.len(),
            pending: found.pending,
            errors: found.errors,
            ..ScanSummary::default()
        };

        for (rel, dir) in found.complete {
            let manifest_hash = HashAlgorithm::ALL.iter().find_map(|algorithm| {
                fs::read(dir.join(algorithm.manifest_file()))
                    .ok()
                    .map(|bytes| algorithm.tagged(&algorithm.hash_bytes(&bytes)))
            });
            if let Some(hash) = &manifest_hash {
                if journal.is_accepted(&rel, hash) {
                    summary.already_journaled += 1;
                    continue;
                }
            }

            let validation = match validate_run_dir(&dir, &self.options.validator) {
                Ok(validation) => validation,
                Err(e) => {
                    summary.errors.push(format!("{rel}: {e}"));
                    continue;
                }
            };
            let (entry, records) = match validation {
                Validation::Accepted(valid) => {
                    let records = build_plan(&valid);
                    let entry = RunEntry::new(
                        rel,
                        Some(valid.manifest.manifest_hash.clone()),
                        Outcome::Accepted,
                        Some(valid.run.campaign.clone()),
                        Some(valid.run.cell.clone()),
                        Vec::new(),
                        valid.warnings.clone(),
                        records.iter().map(|r| r.key_id.clone()).collect(),
                    )?;
                    (entry, records)
                }
                Validation::Rejected(rejections) => {
                    let entry = RunEntry::new(
                        rel,
                        manifest_hash,
                        Outcome::Rejected,
                        None,
                        None,
                        rejections,
                        Vec::new(),
                        Vec::new(),
                    )?;
                    (entry, Vec::new())
                }
            };

            let appended = journal.append_run(&entry, &records)?;
            summary.records_written += appended.records;
            if appended.run {
                match entry.outcome {
                    Outcome::Accepted => summary.journaled_accepted += 1,
                    Outcome::Rejected => summary.journaled_rejected += 1,
                }
            } else {
                summary.already_journaled += 1;
            }
        }
        Ok(summary)
    }

    /// Scan every `interval` until `stop` is set, handing each summary to
    /// `on_scan`. The sleep is sliced so a stop is honored within a second.
    pub fn run(
        &self,
        journal: &mut Journal,
        interval: Duration,
        stop: &AtomicBool,
        mut on_scan: impl FnMut(&ScanSummary),
    ) -> Result<(), FilerError> {
        let slice = Duration::from_millis(250);
        while !stop.load(Ordering::Relaxed) {
            let summary = self.scan_once(journal)?;
            on_scan(&summary);
            let mut slept = Duration::ZERO;
            while slept < interval && !stop.load(Ordering::Relaxed) {
                let step = slice.min(interval - slept);
                thread::sleep(step);
                slept += step;
            }
        }
        Ok(())
    }
}
