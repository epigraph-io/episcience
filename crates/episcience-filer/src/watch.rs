//! The watcher: scan a root for run directories and feed the journal.
//! Phase 0 polls; it reads run directories only (orchestrator logs have no
//! agreed format yet).

use std::io;
use std::path::Path;

use crate::contract::{campaign_and_cell_hint, read_run_dir, CONTRACT_FILES};
use crate::journal::{Journal, JournalError};
use crate::plan::{build_plan, PlanRecord, ScanOutcome};

/// Every run directory under `root`, as a `/`-joined path relative to it, in
/// byte order. Symlinked directories are not followed, and a run directory's
/// own subdirectories are not searched.
pub fn find_run_dirs(root: &Path) -> io::Result<Vec<String>> {
    let mut found = Vec::new();
    walk(root, &mut Vec::new(), &mut found)?;
    found.sort();
    Ok(found)
}

fn walk(dir: &Path, rel: &mut Vec<String>, found: &mut Vec<String>) -> io::Result<()> {
    if CONTRACT_FILES.iter().any(|f| dir.join(f).exists()) {
        found.push(if rel.is_empty() {
            ".".to_string()
        } else {
            rel.join("/")
        });
        return Ok(());
    }
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            subdirs.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    subdirs.sort();
    for name in subdirs {
        rel.push(name.clone());
        walk(&dir.join(&name), rel, found)?;
        rel.pop();
    }
    Ok(())
}

/// Read and verify every run directory under `root`.
pub fn scan(root: &Path) -> io::Result<Vec<ScanOutcome>> {
    Ok(find_run_dirs(root)?
        .into_iter()
        .map(|run_dir| {
            let dir = root.join(&run_dir);
            match read_run_dir(&dir) {
                Ok(contract) => ScanOutcome::Valid { run_dir, contract },
                Err(reason) => {
                    let (campaign, cell) = campaign_and_cell_hint(&dir);
                    ScanOutcome::Rejected {
                        run_dir,
                        campaign,
                        cell,
                        reason,
                    }
                }
            }
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
