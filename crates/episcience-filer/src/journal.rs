//! The outbox journal: an append-only, fsynced JSONL file.
//!
//! Two kinds of line:
//!
//! - `{"type":"record", ...}`: one [`PlanRecord`], a write a later phase
//!   would make, under its deterministic key;
//! - `{"type":"run", ...}`: one [`RunEntry`], the validation outcome of one
//!   run directory in one state, naming the records of its plan.
//!
//! A run is appended as ONE write (its new record lines, then its run line)
//! followed by `fsync`. The run line is the commit marker.
//!
//! Idempotence. A record is written once per `key_id`; a run line once per
//! `run_id`, which covers the directory, its manifest hash and the outcome.
//! Scanning an unchanged directory again appends nothing; a directory whose
//! state changed (a file arrived, a manifest was rewritten) gets a new run
//! line and the earlier one stays (the file is never rewritten).
//!
//! Crash recovery. A crash can leave a torn last line (no newline, or not
//! valid JSON). [`Journal::open`] drops it by truncating the file back to the
//! last whole line. Record lines whose run line was lost stay; the next scan
//! re-validates the run, skips the records already present and appends the
//! rest. A malformed line anywhere but the tail is corruption and refuses the
//! open.
//!
//! One writer. The journal takes no lock; run one filer per journal file.
//! No line carries a timestamp or an absolute path, so the same run
//! directories always produce the same bytes.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::FilerError;
use crate::plan::{digest_fields, PlanRecord};
use crate::validate::Rejection;

const RUN_DOMAIN: &str = "episcience-filer/v1/run";

/// The validation outcome of a run directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Rejected,
}

/// One run directory, in one state, validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEntry {
    /// Digest of every other field: the same directory in the same state
    /// always has the same id.
    pub run_id: String,
    /// The run directory relative to the watched root, `/`-separated (`.`
    /// for the root itself).
    pub run_dir: String,
    /// `<algorithm>:<hex>` of the primary manifest, when one was readable.
    pub manifest_hash: Option<String>,
    pub outcome: Outcome,
    /// `RUN.json`'s campaign; set on an accepted run only.
    pub campaign: Option<String>,
    /// `RUN.json`'s cell; set on an accepted run only.
    pub cell: Option<String>,
    /// Why the run was rejected; empty when accepted.
    pub rejections: Vec<Rejection>,
    /// Non-fatal findings on an accepted run.
    pub warnings: Vec<String>,
    /// `key_id` of every record of this run's plan, in plan order (also the
    /// ones an earlier run already journaled, such as the campaign sample).
    pub records: Vec<String>,
}

impl RunEntry {
    /// Build an entry and stamp its `run_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_dir: String,
        manifest_hash: Option<String>,
        outcome: Outcome,
        campaign: Option<String>,
        cell: Option<String>,
        rejections: Vec<Rejection>,
        warnings: Vec<String>,
        records: Vec<String>,
    ) -> Result<Self, FilerError> {
        let mut entry = Self {
            run_id: String::new(),
            run_dir,
            manifest_hash,
            outcome,
            campaign,
            cell,
            rejections,
            warnings,
            records,
        };
        // `run_id` is empty while the entry is digested, so the id covers
        // every other field and nothing else.
        let body = serde_json::to_string(&entry)?;
        entry.run_id = digest_fields(RUN_DOMAIN, &[Some(body.as_str())]);
        Ok(entry)
    }
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalLine {
    Record(PlanRecord),
    Run(RunEntry),
}

/// What one [`Journal::append_run`] call wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Appended {
    /// Record lines written (records already journaled are skipped).
    pub records: usize,
    /// Whether the run line was written (`false`: already journaled).
    pub run: bool,
}

/// The open journal and its in-memory index.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    file: File,
    record_ids: HashSet<String>,
    run_ids: HashSet<String>,
    accepted: HashSet<(String, String)>,
    torn_tail_bytes: u64,
}

/// Split journal bytes into whole lines. Returns the parsed lines and the
/// length of the valid prefix; anything after it is a torn tail.
fn parse_lines(path: &Path, bytes: &[u8]) -> Result<(Vec<JournalLine>, usize), FilerError> {
    let mut lines = Vec::new();
    let mut good_len = 0usize;
    let mut offset = 0usize;
    let mut line_no = 0usize;
    while offset < bytes.len() {
        line_no += 1;
        let Some(nl) = bytes[offset..].iter().position(|b| *b == b'\n') else {
            // No newline: the write that produced this line never finished.
            break;
        };
        let end = offset + nl + 1;
        let segment = &bytes[offset..offset + nl];
        if segment.iter().all(u8::is_ascii_whitespace) {
            good_len = end;
            offset = end;
            continue;
        }
        match serde_json::from_slice::<JournalLine>(segment) {
            Ok(line) => {
                lines.push(line);
                good_len = end;
            }
            // The last line, terminated but not valid: also a torn tail (a
            // crash can leave garbage where the line should be).
            Err(_) if end == bytes.len() => break,
            Err(e) => {
                return Err(FilerError::JournalCorrupt {
                    path: path.to_path_buf(),
                    line: line_no,
                    detail: e.to_string(),
                });
            }
        }
        offset = end;
    }
    Ok((lines, good_len))
}

/// Read a journal without opening it for append. A torn tail is ignored
/// (and left in place); a malformed line elsewhere is an error.
pub fn read_journal(path: &Path) -> Result<Vec<JournalLine>, FilerError> {
    let bytes = fs::read(path).map_err(|e| FilerError::io(path, e))?;
    Ok(parse_lines(path, &bytes)?.0)
}

impl Journal {
    /// Open (creating it if absent) the journal at `path`, rebuild the index
    /// from its lines and drop a torn tail.
    pub fn open(path: &Path) -> Result<Self, FilerError> {
        let io = |e| FilerError::io(path, e);
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(io)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        let (lines, good_len) = parse_lines(path, &bytes)?;

        let torn_tail_bytes = (bytes.len() - good_len) as u64;
        if torn_tail_bytes > 0 {
            file.set_len(good_len as u64).map_err(io)?;
        }
        file.sync_all().map_err(io)?;
        // Make the file's directory entry durable too. Best effort: not
        // every platform lets a directory be opened and synced.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        let mut journal = Self {
            path: path.to_path_buf(),
            file,
            record_ids: HashSet::new(),
            run_ids: HashSet::new(),
            accepted: HashSet::new(),
            torn_tail_bytes,
        };
        for line in &lines {
            journal.index(line);
        }
        Ok(journal)
    }

    fn index(&mut self, line: &JournalLine) {
        match line {
            JournalLine::Record(record) => {
                self.record_ids.insert(record.key_id.clone());
            }
            JournalLine::Run(entry) => {
                self.run_ids.insert(entry.run_id.clone());
                if entry.outcome == Outcome::Accepted {
                    if let Some(hash) = &entry.manifest_hash {
                        self.accepted.insert((entry.run_dir.clone(), hash.clone()));
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many bytes of torn tail [`Journal::open`] dropped (0: none).
    #[must_use]
    pub fn torn_tail_bytes(&self) -> u64 {
        self.torn_tail_bytes
    }

    #[must_use]
    pub fn contains_record(&self, key_id: &str) -> bool {
        self.record_ids.contains(key_id)
    }

    #[must_use]
    pub fn contains_run(&self, run_id: &str) -> bool {
        self.run_ids.contains(run_id)
    }

    /// Whether `run_dir` was already accepted with this manifest hash (the
    /// watcher then skips it without re-hashing its data).
    #[must_use]
    pub fn is_accepted(&self, run_dir: &str, manifest_hash: &str) -> bool {
        self.accepted
            .contains(&(run_dir.to_string(), manifest_hash.to_string()))
    }

    /// Append one run: the records of its plan that are not journaled yet,
    /// then its run line, as one write, then `fsync`. Appends nothing when
    /// the run and all its records are already there.
    pub fn append_run(
        &mut self,
        entry: &RunEntry,
        records: &[PlanRecord],
    ) -> Result<Appended, FilerError> {
        let mut buf: Vec<u8> = Vec::new();
        let mut new_lines: Vec<JournalLine> = Vec::new();
        let mut batch_ids: HashSet<&str> = HashSet::new();
        for record in records {
            if self.record_ids.contains(&record.key_id) || !batch_ids.insert(&record.key_id) {
                continue;
            }
            new_lines.push(JournalLine::Record(record.clone()));
        }
        let records_written = new_lines.len();
        let run_written = !self.run_ids.contains(&entry.run_id);
        if run_written {
            new_lines.push(JournalLine::Run(entry.clone()));
        }
        if new_lines.is_empty() {
            return Ok(Appended::default());
        }
        for line in &new_lines {
            serde_json::to_writer(&mut buf, line)?;
            buf.push(b'\n');
        }
        self.file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data())
            .map_err(|e| FilerError::io(&self.path, e))?;
        for line in &new_lines {
            self.index(line);
        }
        Ok(Appended {
            records: records_written,
            run: run_written,
        })
    }
}
