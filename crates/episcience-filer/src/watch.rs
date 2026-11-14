//! The watcher: scan a root for run directories and feed the journal.
//! Phase 0 polls; it reads run directories only (orchestrator logs have no
//! agreed format yet).

use std::io;
use std::path::{Path, PathBuf};

use crate::contract::{read_run_dir, ContractError, CONTRACT_FILES};
use crate::journal::{Journal, JournalError};
use crate::plan::{build_plan, PlanRecord, ScanOutcome};

/// A directory the scanner found: a run directory, or one it could not scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundDir {
    /// A run directory, as a `/`-joined path relative to the root.
    Run(String),
    /// A directory below the root that cannot be scanned: its name is not
    /// UTF-8, or listing it failed. Its subtree is skipped and it is filed as
    /// a rejection, so one bad directory never stops the scan.
    Unscannable { dir: String, reason: String },
}

impl FoundDir {
    pub fn path(&self) -> &str {
        match self {
            FoundDir::Run(p) => p,
            FoundDir::Unscannable { dir, .. } => dir,
        }
    }
}

/// Every run directory (and every unscannable directory) under `root`, in
/// byte order of their relative paths. Symlinked directories are not
/// followed, and a run directory's own subdirectories are not searched. Only
/// a failure to list `root` itself is an error.
pub fn find_run_dirs(root: &Path) -> io::Result<Vec<FoundDir>> {
    let mut found = Vec::new();
    if is_run_dir(root) {
        found.push(FoundDir::Run(".".to_string()));
        return Ok(found);
    }
    for (name, path) in list_subdirs(root)? {
        walk(&path, &mut vec![name], &mut found);
    }
    found.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(found)
}

fn is_run_dir(dir: &Path) -> bool {
    CONTRACT_FILES.iter().any(|f| dir.join(f).exists())
}

/// The subdirectories of `dir`, sorted by name. A name that is not UTF-8 is
/// returned as `Err(escaped name)`.
#[allow(clippy::type_complexity)]
fn list_subdirs(dir: &Path) -> io::Result<Vec<(Result<String, String>, PathBuf)>> {
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(s) => Ok(s.to_string()),
                None => Err(escape_non_utf8(name.as_encoded_bytes())),
            };
            subdirs.push((name, entry.path()));
        }
    }
    subdirs.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(subdirs)
}

/// Render a non-UTF-8 name injectively: `\` becomes `\\`, and each byte that
/// is not part of valid UTF-8 becomes `\xNN`.
fn escape_non_utf8(mut bytes: &[u8]) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(&valid.replace('\\', "\\\\"));
                return out;
            }
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                // `valid` is UTF-8 by construction.
                out.push_str(
                    &std::str::from_utf8(valid)
                        .unwrap_or("")
                        .replace('\\', "\\\\"),
                );
                let bad = e.error_len().unwrap_or(rest.len());
                for b in &rest[..bad] {
                    out.push_str(&format!("\\x{b:02x}"));
                }
                bytes = &rest[bad..];
            }
        }
    }
}

fn walk(dir: &Path, rel: &mut Vec<Result<String, String>>, found: &mut Vec<FoundDir>) {
    let rel_path = |rel: &[Result<String, String>]| {
        rel.iter()
            .map(|n| match n {
                Ok(s) | Err(s) => s.as_str(),
            })
            .collect::<Vec<_>>()
            .join("/")
    };
    if rel.last().is_some_and(|n| n.is_err()) {
        found.push(FoundDir::Unscannable {
            dir: rel_path(rel),
            reason: "its name is not UTF-8".to_string(),
        });
        return;
    }
    if is_run_dir(dir) {
        found.push(FoundDir::Run(rel_path(rel)));
        return;
    }
    let subdirs = match list_subdirs(dir) {
        Ok(s) => s,
        Err(e) => {
            found.push(FoundDir::Unscannable {
                dir: rel_path(rel),
                reason: e.to_string(),
            });
            return;
        }
    };
    for (name, path) in subdirs {
        rel.push(name);
        walk(&path, rel, found);
        rel.pop();
    }
}

/// Read and verify every run directory under `root`.
pub fn scan(root: &Path) -> io::Result<Vec<ScanOutcome>> {
    Ok(find_run_dirs(root)?
        .into_iter()
        .map(|found| match found {
            FoundDir::Run(run_dir) => match read_run_dir(&root.join(&run_dir)) {
                Ok(contract) => ScanOutcome::Valid { run_dir, contract },
                Err(reason) => ScanOutcome::Rejected { run_dir, reason },
            },
            FoundDir::Unscannable { dir, reason } => ScanOutcome::Rejected {
                run_dir: dir,
                reason: ContractError::Unscannable(reason),
            },
        })
        .collect())
}

/// The plan for the current state of `root`.
pub fn plan_for_root(root: &Path) -> io::Result<Vec<PlanRecord>> {
    Ok(build_plan(&scan(root)?))
}

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("scan: {0}")]
    Scan(#[from] io::Error),
    #[error(transparent)]
    Journal(#[from] JournalError),
}

/// One watcher pass: scan `root` and journal every record not journaled yet.
/// Returns the newly journaled records, in plan order.
pub fn watch_once(root: &Path, journal: &mut Journal) -> Result<Vec<PlanRecord>, WatchError> {
    let plan = plan_for_root(root)?;
    Ok(journal.append_new(&plan)?)
}
