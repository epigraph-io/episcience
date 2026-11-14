//! The outbox journal: an append-only JSONL file of every plan record the
//! filer has observed, one canonical line each, at most once per `key_id`.
//!
//! The journal is never rewritten or truncated. A record whose content
//! changes gets a new key (its content hash changes), so history accumulates
//! as new lines. Opening a journal re-decodes every line strictly: an edited
//! line, a duplicate `key_id` or a torn final line is an error, never skipped.
//! One process writes a journal at a time (documented, not enforced).
//!
//! # Recovering a torn journal
//!
//! A crash or a failed write (a full disk, say) can leave a final line without
//! its newline. Every later open then refuses the journal, on purpose: the
//! filer never guesses which bytes are good. To recover, stop the filer, keep
//! a copy of the journal, and truncate it to just after its last `\n` (for
//! example `head -c <offset of the last newline + 1>`). Every complete line
//! before it was fsynced, and the next watcher pass re-journals whatever the
//! torn line held, because its `key_id` is no longer known.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::plan::PlanRecord;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("journal {path} line {line}: {reason}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        reason: String,
    },
}

/// An open journal and the `key_id`s already in it.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    records: Vec<PlanRecord>,
    known: BTreeSet<String>,
}

impl Journal {
    /// Open the journal at `path`, creating it empty if it does not exist.
    pub fn open(path: &Path) -> Result<Journal, JournalError> {
        let io_err = |source| JournalError::Io {
            path: path.to_path_buf(),
            source,
        };
        let text = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8(bytes).map_err(|_| JournalError::Corrupt {
                path: path.to_path_buf(),
                line: 0,
                reason: "is not UTF-8".to_string(),
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                OpenOptions::new()
                    .create_new(true)
                    .append(true)
                    .open(path)
                    .map_err(io_err)?;
                String::new()
            }
            Err(e) => return Err(io_err(e)),
        };
        let mut journal = Journal {
            path: path.to_path_buf(),
            records: Vec::new(),
            known: BTreeSet::new(),
        };
        if text.is_empty() {
            return Ok(journal);
        }
        let body = text
            .strip_suffix('\n')
            .ok_or_else(|| JournalError::Corrupt {
                path: path.to_path_buf(),
                line: text.split('\n').count(),
                reason: "final line has no newline (torn write?)".to_string(),
            })?;
        for (idx, line) in body.split('\n').enumerate() {
            let corrupt = |reason: String| JournalError::Corrupt {
                path: path.to_path_buf(),
                line: idx + 1,
                reason,
            };
            let record = PlanRecord::from_line(line).map_err(|e| corrupt(e.to_string()))?;
            if !journal.known.insert(record.key.id()) {
                return Err(corrupt(format!("duplicate key_id {}", record.key.id())));
            }
            journal.records.push(record);
        }
        Ok(journal)
    }

    /// Every journaled record, in append order.
    pub fn records(&self) -> &[PlanRecord] {
        &self.records
    }

    /// Append the records whose `key_id` the journal does not hold yet, in
    /// the given order, as one write followed by an fsync. Returns them.
    pub fn append_new(&mut self, records: &[PlanRecord]) -> Result<Vec<PlanRecord>, JournalError> {
        let mut fresh = Vec::new();
        let mut ids = BTreeSet::new();
        for r in records {
            let id = r.key.id();
            if !self.known.contains(&id) && ids.insert(id) {
                fresh.push(r.clone());
            }
        }
        if fresh.is_empty() {
            return Ok(fresh);
        }
        let mut buf = String::new();
        for r in &fresh {
            buf.push_str(&r.to_line());
            buf.push('\n');
        }
        let io_err = |source| JournalError::Io {
            path: self.path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(io_err)?;
        file.write_all(buf.as_bytes()).map_err(io_err)?;
        file.sync_data().map_err(io_err)?;
        self.known.extend(ids);
        self.records.extend(fresh.iter().cloned());
        Ok(fresh)
    }
}
