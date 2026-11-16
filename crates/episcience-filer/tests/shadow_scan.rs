//! Phase 0 (shadow mode) end to end, on tempdir fixtures: the validator's
//! typed rejections, both manifest formats, the watcher's idempotence across
//! scans and restarts, and the journal's crash recovery.

use std::fs;
use std::path::{Path, PathBuf};

use episcience_filer::journal::{read_journal, Journal, JournalLine, Outcome};
use episcience_filer::manifest::{
    verify_manifest, HashAlgorithm, ManifestIssue, TraversalKind, MANIFEST_B3, MANIFEST_SHA256,
};
use episcience_filer::plan::PlannedAction;
use episcience_filer::validate::{
    validate_run_dir, ContractFile, Rejection, Validation, ValidatorOptions,
};
use episcience_filer::watcher::{Watcher, WatcherOptions};
use tempfile::TempDir;

const GATE_STATEMENT: &str =
    "Gate G1 PASS: the measured value 2.50 ± 0.10 is within the tolerance 0.40 of the reference 2.60.";

const RUN_JSON: &str = r#"{
  "campaign": "campaign-a",
  "cell": "cell-01",
  "workflow_id": "00000000-0000-4000-8000-000000000001",
  "canonical_name": "example-workflow",
  "step_id": "step-3",
  "started_at": "2026-01-02T03:04:05Z",
  "data_root_uri": "pvc://example-volume/campaign-a/cell-01",
  "host": {"gpu": "example-gpu", "driver": "1.2.3"},
  "small_files": ["logs/run.log"],
  "an_unknown_field": [1, 2, 3]
}"#;

fn result_json() -> String {
    serde_json::json!({
        "status": "completed",
        "finished_at": "2026-01-02T04:04:05Z",
        "gates": [{"name": "G1", "verdict": "PASS", "statement": GATE_STATEMENT}],
        "another_unknown_field": {"x": 1}
    })
    .to_string()
}

/// Every regular file under `dir` (relative, `/`-separated, sorted), the
/// manifest files left out.
fn data_files(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(base, &path, out);
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.retain(|p| p != MANIFEST_SHA256 && p != MANIFEST_B3);
    out.sort();
    out
}

/// Write `dir`'s manifest over every file currently in it.
fn write_manifest(dir: &Path, algorithm: HashAlgorithm) {
    let mut text = String::new();
    for rel in data_files(dir) {
        let digest = algorithm.hash_bytes(&fs::read(dir.join(&rel)).unwrap());
        text.push_str(&format!("{digest}  {rel}\n"));
    }
    fs::write(dir.join(algorithm.manifest_file()), text).unwrap();
}

/// A run directory with its contract files and data, but no manifest yet.
fn write_run_files(dir: &Path) {
    fs::create_dir_all(dir.join("logs")).unwrap();
    fs::create_dir_all(dir.join("data")).unwrap();
    fs::write(dir.join("RUN.json"), RUN_JSON).unwrap();
    fs::write(dir.join("RESULT.json"), result_json()).unwrap();
    fs::write(dir.join("logs/run.log"), b"step 1 ok\nstep 2 ok\n").unwrap();
    fs::write(dir.join("data/traj.bin"), vec![7u8; 200_000]).unwrap();
}

/// A complete, valid run directory.
fn write_valid_run(dir: &Path, algorithm: HashAlgorithm) {
    write_run_files(dir);
    write_manifest(dir, algorithm);
}

fn rejections(dir: &Path) -> Vec<Rejection> {
    match validate_run_dir(dir, &ValidatorOptions::default()).unwrap() {
        Validation::Rejected(rejections) => rejections,
        Validation::Accepted(_) => panic!("expected a rejection for {}", dir.display()),
    }
}

fn run_lines(journal: &Path) -> Vec<episcience_filer::journal::RunEntry> {
    read_journal(journal)
        .unwrap()
        .into_iter()
        .filter_map(|line| match line {
            JournalLine::Run(entry) => Some(entry),
            JournalLine::Record(_) => None,
        })
        .collect()
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    journal: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("runs");
    fs::create_dir_all(&root).unwrap();
    let journal = tmp.path().join("outbox.jsonl");
    Fixture {
        _tmp: tmp,
        root,
        journal,
    }
}

// ── Manifest formats ─────────────────────────────────────────────────────

#[test]
fn both_manifest_formats_verify() {
    for algorithm in HashAlgorithm::ALL {
        let fx = fixture();
        let dir = fx.root.join("run");
        write_valid_run(&dir, algorithm);

        let report = verify_manifest(&dir, algorithm).unwrap();
        assert!(report.verifies(), "{algorithm:?}: {:?}", report.issues);
        assert_eq!(report.entries.len(), 4);
        assert_eq!(report.verified, 4);
        assert!(report.unlisted.is_empty());
        assert!(report
            .manifest_hash
            .starts_with(&format!("{}:", algorithm.label())));

        match validate_run_dir(&dir, &ValidatorOptions::default()).unwrap() {
            Validation::Accepted(valid) => {
                assert_eq!(valid.manifest.algorithm, algorithm);
                assert_eq!(valid.run.campaign, "campaign-a");
                assert_eq!(valid.result.gates.len(), 1);
            }
            Validation::Rejected(r) => panic!("{algorithm:?} rejected: {r:?}"),
        }
    }
}

#[test]
fn a_hand_written_sha256sum_manifest_with_a_binary_marker_verifies() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("abc.txt"), b"abc").unwrap();
    // The published SHA-256 of "abc", as `sha256sum -b` prints it.
    fs::write(
        tmp.path().join(MANIFEST_SHA256),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad *abc.txt\n",
    )
    .unwrap();
    let report = verify_manifest(tmp.path(), HashAlgorithm::Sha256).unwrap();
    assert!(report.verifies(), "{:?}", report.issues);
    assert_eq!(report.total_bytes, 3);

    // The published BLAKE3 of "abc", as `b3sum` prints it.
    fs::write(
        tmp.path().join(MANIFEST_B3),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85  abc.txt\n",
    )
    .unwrap();
    let report = verify_manifest(tmp.path(), HashAlgorithm::Blake3).unwrap();
    assert!(report.verifies(), "{:?}", report.issues);
}

#[test]
fn both_manifests_present_are_both_verified_and_sha256_is_primary() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_run_files(&dir);
    write_manifest(&dir, HashAlgorithm::Sha256);
    write_manifest(&dir, HashAlgorithm::Blake3);
    match validate_run_dir(&dir, &ValidatorOptions::default()).unwrap() {
        Validation::Accepted(valid) => {
            assert_eq!(valid.manifest.algorithm, HashAlgorithm::Sha256);
        }
        Validation::Rejected(r) => panic!("rejected: {r:?}"),
    }

    // Corrupt only the BLAKE3 manifest: the run is rejected on it.
    let b3 = fs::read_to_string(dir.join(MANIFEST_B3)).unwrap();
    let flipped = if b3.starts_with('0') { "1" } else { "0" };
    fs::write(dir.join(MANIFEST_B3), format!("{flipped}{}", &b3[1..])).unwrap();
    assert!(rejections(&dir).iter().any(|r| matches!(
        r,
        Rejection::ManifestInvalid {
            manifest: HashAlgorithm::Blake3,
            issue: ManifestIssue::HashMismatch { .. }
        }
    )));
}

// ── Typed rejections ─────────────────────────────────────────────────────

#[test]
fn a_missing_contract_file_is_rejected_by_name() {
    let fx = fixture();

    let no_result = fx.root.join("no-result");
    write_run_files(&no_result);
    fs::remove_file(no_result.join("RESULT.json")).unwrap();
    write_manifest(&no_result, HashAlgorithm::Sha256);
    assert_eq!(
        rejections(&no_result),
        vec![Rejection::MissingContractFile {
            file: ContractFile::ResultJson
        }]
    );

    let no_run = fx.root.join("no-run");
    write_run_files(&no_run);
    fs::remove_file(no_run.join("RUN.json")).unwrap();
    write_manifest(&no_run, HashAlgorithm::Sha256);
    assert_eq!(
        rejections(&no_run),
        vec![Rejection::MissingContractFile {
            file: ContractFile::RunJson
        }]
    );

    let no_manifest = fx.root.join("no-manifest");
    write_run_files(&no_manifest);
    assert_eq!(
        rejections(&no_manifest),
        vec![Rejection::MissingContractFile {
            file: ContractFile::Manifest
        }]
    );
}

#[test]
fn an_invalid_contract_file_is_rejected() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_run_files(&dir);
    fs::write(dir.join("RUN.json"), r#"{"cell": "cell-01"}"#).unwrap();
    write_manifest(&dir, HashAlgorithm::Sha256);
    let found = rejections(&dir);
    assert!(
        matches!(
            found.as_slice(),
            [Rejection::InvalidContractFile {
                file: ContractFile::RunJson,
                detail
            }] if detail.contains("campaign")
        ),
        "{found:?}"
    );
}

#[test]
fn a_hash_mismatch_is_rejected_with_both_digests() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_valid_run(&dir, HashAlgorithm::Sha256);
    let expected = HashAlgorithm::Sha256.hash_bytes(&fs::read(dir.join("data/traj.bin")).unwrap());
    fs::write(dir.join("data/traj.bin"), b"tampered").unwrap();
    let actual = HashAlgorithm::Sha256.hash_bytes(b"tampered");

    assert_eq!(
        rejections(&dir),
        vec![Rejection::ManifestInvalid {
            manifest: HashAlgorithm::Sha256,
            issue: ManifestIssue::HashMismatch {
                path: "data/traj.bin".into(),
                expected,
                actual,
            }
        }]
    );
}

#[test]
fn a_listed_file_that_is_absent_is_rejected() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_valid_run(&dir, HashAlgorithm::Blake3);
    fs::remove_file(dir.join("data/traj.bin")).unwrap();
    assert_eq!(
        rejections(&dir),
        vec![Rejection::ManifestInvalid {
            manifest: HashAlgorithm::Blake3,
            issue: ManifestIssue::MissingFile {
                path: "data/traj.bin".into()
            }
        }]
    );
}

#[test]
fn an_unlisted_file_is_rejected_unless_allowed() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_valid_run(&dir, HashAlgorithm::Sha256);
    fs::write(dir.join("data/extra.bin"), b"not in the manifest").unwrap();

    let report = verify_manifest(&dir, HashAlgorithm::Sha256).unwrap();
    assert!(report.verifies(), "listed files still match");
    assert_eq!(report.unlisted, vec!["data/extra.bin".to_string()]);

    assert_eq!(
        rejections(&dir),
        vec![Rejection::UnlistedFile {
            path: "data/extra.bin".into()
        }]
    );

    let lenient = ValidatorOptions {
        allow_unlisted: true,
    };
    match validate_run_dir(&dir, &lenient).unwrap() {
        Validation::Accepted(valid) => {
            assert_eq!(valid.warnings, vec!["unlisted file: data/extra.bin"]);
        }
        Validation::Rejected(r) => panic!("rejected under allow_unlisted: {r:?}"),
    }
}

#[test]
fn a_contract_file_the_manifest_does_not_list_is_unlisted_too() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_run_files(&dir);
    // The manifest is written before RESULT.json exists.
    fs::remove_file(dir.join("RESULT.json")).unwrap();
    write_manifest(&dir, HashAlgorithm::Sha256);
    fs::write(dir.join("RESULT.json"), result_json()).unwrap();
    assert_eq!(
        rejections(&dir),
        vec![Rejection::UnlistedFile {
            path: "RESULT.json".into()
        }]
    );
}

#[test]
fn path_traversal_entries_are_rejected() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_valid_run(&dir, HashAlgorithm::Sha256);
    // A real file one level up, with its real digest: only the path is wrong.
    fs::write(fx.root.join("outside.txt"), b"outside").unwrap();
    let digest = HashAlgorithm::Sha256.hash_bytes(b"outside");
    let outside_abs = fs::canonicalize(fx.root.join("outside.txt")).unwrap();
    let mut manifest = fs::read_to_string(dir.join(MANIFEST_SHA256)).unwrap();
    manifest.push_str(&format!("{digest}  ../outside.txt\n"));
    manifest.push_str(&format!("{digest}  data/../../outside.txt\n"));
    manifest.push_str(&format!("{digest}  {}\n", outside_abs.display()));
    fs::write(dir.join(MANIFEST_SHA256), manifest).unwrap();

    let kinds: Vec<(String, TraversalKind)> = rejections(&dir)
        .into_iter()
        .map(|r| match r {
            Rejection::ManifestInvalid {
                issue: ManifestIssue::PathTraversal { path, kind, .. },
                ..
            } => (path, kind),
            other => panic!("unexpected rejection: {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("../outside.txt".to_string(), TraversalKind::ParentDir),
            (
                "data/../../outside.txt".to_string(),
                TraversalKind::ParentDir
            ),
            (outside_abs.display().to_string(), TraversalKind::Absolute),
        ]
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_leading_out_of_the_run_directory_is_rejected() {
    let fx = fixture();
    let dir = fx.root.join("run");
    write_run_files(&dir);
    fs::write(fx.root.join("outside.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(fx.root.join("outside.txt"), dir.join("data/link.txt")).unwrap();
    // `write_manifest` follows the link, so the listed digest is the real one.
    write_manifest(&dir, HashAlgorithm::Sha256);

    let found = rejections(&dir);
    assert!(
        matches!(
            found.as_slice(),
            [Rejection::ManifestInvalid {
                manifest: HashAlgorithm::Sha256,
                issue: ManifestIssue::PathTraversal {
                    path,
                    kind: TraversalKind::SymlinkEscape,
                    ..
                }
            }] if path == "data/link.txt"
        ),
        "{found:?}"
    );

    // A contract file that is itself a link out of the directory.
    let other = fx.root.join("run2");
    write_run_files(&other);
    fs::remove_file(other.join("RESULT.json")).unwrap();
    fs::write(fx.root.join("elsewhere.json"), result_json()).unwrap();
    std::os::unix::fs::symlink(fx.root.join("elsewhere.json"), other.join("RESULT.json")).unwrap();
    write_manifest(&other, HashAlgorithm::Sha256);
    assert!(
        rejections(&other).contains(&Rejection::ContractFileEscapes {
            file: ContractFile::ResultJson
        })
    );
}

// ── Watcher and journal ──────────────────────────────────────────────────

#[test]
fn a_valid_run_is_journaled_exactly_once_across_scans_and_restarts() {
    let fx = fixture();
    write_valid_run(&fx.root.join("campaign-a/cell-01"), HashAlgorithm::Sha256);
    let watcher = Watcher::new(&fx.root, WatcherOptions::default());

    let mut journal = Journal::open(&fx.journal).unwrap();
    let first = watcher.scan_once(&mut journal).unwrap();
    assert_eq!(first.complete, 1);
    assert_eq!(first.journaled_accepted, 1);
    assert_eq!(first.journaled_rejected, 0);
    // campaign sample, run sample, data pointer, host observation, 1 gate,
    // 1 small file.
    assert_eq!(first.records_written, 6);
    let after_first = fs::read(&fx.journal).unwrap();

    let second = watcher.scan_once(&mut journal).unwrap();
    assert_eq!(second.journaled_accepted, 0);
    assert_eq!(second.records_written, 0);
    assert_eq!(second.already_journaled, 1);
    assert_eq!(fs::read(&fx.journal).unwrap(), after_first);

    // A restart: the index is rebuilt from the file.
    drop(journal);
    let mut reopened = Journal::open(&fx.journal).unwrap();
    assert_eq!(reopened.torn_tail_bytes(), 0);
    let third = watcher.scan_once(&mut reopened).unwrap();
    assert_eq!(third.already_journaled, 1);
    assert_eq!(fs::read(&fx.journal).unwrap(), after_first);

    let runs = run_lines(&fx.journal);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_dir, "campaign-a/cell-01");
    assert_eq!(runs[0].outcome, Outcome::Accepted);
    assert_eq!(runs[0].campaign.as_deref(), Some("campaign-a"));
    assert_eq!(runs[0].records.len(), 6);
}

#[test]
fn the_plan_carries_deterministic_keys_and_the_gate_text_word_for_word() {
    let fx = fixture();
    let dir = fx.root.join("campaign-a/cell-01");
    write_valid_run(&dir, HashAlgorithm::Sha256);
    let manifest_hash = HashAlgorithm::Sha256
        .tagged(&HashAlgorithm::Sha256.hash_bytes(&fs::read(dir.join(MANIFEST_SHA256)).unwrap()));
    let mut journal = Journal::open(&fx.journal).unwrap();
    Watcher::new(&fx.root, WatcherOptions::default())
        .scan_once(&mut journal)
        .unwrap();

    let records: Vec<_> = read_journal(&fx.journal)
        .unwrap()
        .into_iter()
        .filter_map(|line| match line {
            JournalLine::Record(record) => Some(record),
            JournalLine::Run(_) => None,
        })
        .collect();
    let events: Vec<&str> = records.iter().map(|r| r.key.event.as_str()).collect();
    assert_eq!(
        events,
        vec![
            "campaign_sample",
            "run_sample",
            "data_pointer",
            "host_observation",
            "gate/G1",
            "blob/logs/run.log"
        ]
    );
    for record in &records {
        assert_eq!(record.key.campaign, "campaign-a");
        assert_eq!(record.key_id, record.key.id());
    }
    assert_eq!(records[0].key.cell, None);
    assert_eq!(records[0].key.content_hash, None);
    assert_eq!(records[1].key.cell.as_deref(), Some("cell-01"));
    assert_eq!(
        records[1].key.content_hash.as_deref(),
        Some(manifest_hash.as_str())
    );

    match &records[4].action {
        PlannedAction::SubmitGateClaim {
            content,
            verdict,
            methodology,
            evidence_type,
            source_url,
            checksum,
            step_id,
            ..
        } => {
            assert_eq!(content, GATE_STATEMENT);
            assert_eq!(verdict, "PASS");
            assert_eq!(methodology, "direct_observation");
            assert_eq!(evidence_type, "empirical");
            assert_eq!(
                source_url.as_deref(),
                Some("pvc://example-volume/campaign-a/cell-01")
            );
            assert_eq!(checksum, &manifest_hash);
            assert_eq!(step_id.as_deref(), Some("step-3"));
        }
        other => panic!("expected a gate claim, got {other:?}"),
    }
    match &records[2].action {
        PlannedAction::RegisterDataPointer {
            file_count,
            total_bytes,
            manifest_file,
            ..
        } => {
            assert_eq!(*file_count, 4);
            assert_eq!(manifest_file, MANIFEST_SHA256);
            let expected: u64 = data_files(&dir)
                .iter()
                .map(|f| fs::metadata(dir.join(f)).unwrap().len())
                .sum();
            assert_eq!(*total_bytes, expected);
        }
        other => panic!("expected a data pointer, got {other:?}"),
    }
}

#[test]
fn replaying_the_same_runs_yields_a_byte_identical_journal() {
    let fx = fixture();
    write_valid_run(&fx.root.join("campaign-a/cell-01"), HashAlgorithm::Sha256);
    write_valid_run(&fx.root.join("campaign-a/cell-02"), HashAlgorithm::Blake3);
    let bad = fx.root.join("campaign-a/cell-03");
    write_valid_run(&bad, HashAlgorithm::Sha256);
    fs::write(bad.join("data/traj.bin"), b"tampered").unwrap();

    let watcher = Watcher::new(&fx.root, WatcherOptions::default());
    let other_journal = fx.journal.with_file_name("replay.jsonl");
    for path in [&fx.journal, &other_journal] {
        let mut journal = Journal::open(path).unwrap();
        let summary = watcher.scan_once(&mut journal).unwrap();
        assert_eq!(summary.journaled_accepted, 2);
        assert_eq!(summary.journaled_rejected, 1);
    }
    let first = fs::read(&fx.journal).unwrap();
    assert!(!first.is_empty());
    assert_eq!(first, fs::read(&other_journal).unwrap());

    // The second run of the campaign does not journal the campaign sample
    // again: 6 records for the first run, 5 for the second.
    let lines = read_journal(&fx.journal).unwrap();
    let campaign_samples = lines
        .iter()
        .filter(|l| matches!(l, JournalLine::Record(r) if r.key.event == "campaign_sample"))
        .count();
    assert_eq!(campaign_samples, 1);
    // The journal names no absolute path.
    let text = String::from_utf8(first).unwrap();
    assert!(!text.contains(fx.root.to_string_lossy().as_ref()));
}

#[test]
fn a_run_without_a_manifest_is_pending_and_never_journaled() {
    let fx = fixture();
    let dir = fx.root.join("campaign-a/cell-01");
    write_run_files(&dir);
    let watcher = Watcher::new(&fx.root, WatcherOptions::default());
    let mut journal = Journal::open(&fx.journal).unwrap();

    let running = watcher.scan_once(&mut journal).unwrap();
    assert_eq!(running.complete, 0);
    assert_eq!(running.pending, vec!["campaign-a/cell-01".to_string()]);
    assert_eq!(fs::read(&fx.journal).unwrap().len(), 0);

    // The manifest arrives: the run is complete and is journaled.
    write_manifest(&dir, HashAlgorithm::Sha256);
    let done = watcher.scan_once(&mut journal).unwrap();
    assert!(done.pending.is_empty());
    assert_eq!(done.journaled_accepted, 1);
}

#[test]
fn a_rejected_run_is_journaled_once_and_again_only_when_its_state_changes() {
    let fx = fixture();
    let dir = fx.root.join("campaign-a/cell-01");
    write_valid_run(&dir, HashAlgorithm::Sha256);
    // A half-copied run: the manifest is there, one data file is not yet.
    let held_back = fs::read(dir.join("data/traj.bin")).unwrap();
    fs::remove_file(dir.join("data/traj.bin")).unwrap();

    let watcher = Watcher::new(&fx.root, WatcherOptions::default());
    let mut journal = Journal::open(&fx.journal).unwrap();
    assert_eq!(
        watcher.scan_once(&mut journal).unwrap().journaled_rejected,
        1
    );
    let again = watcher.scan_once(&mut journal).unwrap();
    assert_eq!(again.journaled_rejected, 0);
    assert_eq!(again.already_journaled, 1);

    fs::write(dir.join("data/traj.bin"), held_back).unwrap();
    let healed = watcher.scan_once(&mut journal).unwrap();
    assert_eq!(healed.journaled_accepted, 1);
    assert_eq!(healed.records_written, 6);

    let runs = run_lines(&fx.journal);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].outcome, Outcome::Rejected);
    assert_eq!(
        runs[0].rejections,
        vec![Rejection::ManifestInvalid {
            manifest: HashAlgorithm::Sha256,
            issue: ManifestIssue::MissingFile {
                path: "data/traj.bin".into()
            }
        }]
    );
    assert_eq!(runs[1].outcome, Outcome::Accepted);
    assert_ne!(runs[0].run_id, runs[1].run_id);
}

#[test]
fn a_torn_journal_tail_is_dropped_and_nothing_is_journaled_twice() {
    let fx = fixture();
    write_valid_run(&fx.root.join("campaign-a/cell-01"), HashAlgorithm::Sha256);
    let watcher = Watcher::new(&fx.root, WatcherOptions::default());
    let mut journal = Journal::open(&fx.journal).unwrap();
    watcher.scan_once(&mut journal).unwrap();
    drop(journal);
    let whole = fs::read(&fx.journal).unwrap();

    // A crash mid-append: half a line, no newline.
    let mut torn = whole.clone();
    torn.extend_from_slice(br#"{"type":"run","run_id":"0123"#);
    fs::write(&fx.journal, &torn).unwrap();

    // A reader ignores the tail.
    assert_eq!(read_journal(&fx.journal).unwrap().len(), 7);

    let mut reopened = Journal::open(&fx.journal).unwrap();
    assert_eq!(
        reopened.torn_tail_bytes(),
        (torn.len() - whole.len()) as u64
    );
    assert_eq!(
        fs::read(&fx.journal).unwrap(),
        whole,
        "the tail is truncated"
    );
    let summary = watcher.scan_once(&mut reopened).unwrap();
    assert_eq!(summary.already_journaled, 1);
    assert_eq!(summary.records_written, 0);
    assert_eq!(fs::read(&fx.journal).unwrap(), whole);

    // The journal is still appendable after the recovery.
    write_valid_run(&fx.root.join("campaign-a/cell-02"), HashAlgorithm::Blake3);
    assert_eq!(
        watcher.scan_once(&mut reopened).unwrap().journaled_accepted,
        1
    );
    assert_eq!(run_lines(&fx.journal).len(), 2);

    // A terminated but invalid last line is a torn tail too.
    let before = fs::read(&fx.journal).unwrap();
    let mut garbage = before.clone();
    garbage.extend_from_slice(b"\0\0\0\0\n");
    fs::write(&fx.journal, &garbage).unwrap();
    drop(reopened);
    let recovered = Journal::open(&fx.journal).unwrap();
    assert_eq!(recovered.torn_tail_bytes(), 5);
    assert_eq!(fs::read(&fx.journal).unwrap(), before);

    // A journal that is nothing but a torn line opens empty.
    let lone = fx.journal.with_file_name("lone.jsonl");
    fs::write(&lone, br#"{"type":"rec"#).unwrap();
    let empty = Journal::open(&lone).unwrap();
    assert_eq!(empty.torn_tail_bytes(), 12);
    assert_eq!(fs::read(&lone).unwrap().len(), 0);
}

#[test]
fn records_whose_run_line_was_lost_are_not_written_again() {
    let fx = fixture();
    write_valid_run(&fx.root.join("campaign-a/cell-01"), HashAlgorithm::Sha256);
    let watcher = Watcher::new(&fx.root, WatcherOptions::default());
    let mut journal = Journal::open(&fx.journal).unwrap();
    watcher.scan_once(&mut journal).unwrap();
    drop(journal);
    let whole = fs::read_to_string(&fx.journal).unwrap();

    // A crash after four of the six record lines reached the disk.
    let kept: String = whole.lines().take(4).map(|l| format!("{l}\n")).collect();
    fs::write(&fx.journal, &kept).unwrap();

    let mut reopened = Journal::open(&fx.journal).unwrap();
    let summary = watcher.scan_once(&mut reopened).unwrap();
    assert_eq!(summary.records_written, 2);
    assert_eq!(summary.journaled_accepted, 1);
    assert_eq!(fs::read_to_string(&fx.journal).unwrap(), whole);
}

#[test]
fn a_malformed_line_before_the_tail_refuses_the_open() {
    let fx = fixture();
    write_valid_run(&fx.root.join("campaign-a/cell-01"), HashAlgorithm::Sha256);
    let mut journal = Journal::open(&fx.journal).unwrap();
    Watcher::new(&fx.root, WatcherOptions::default())
        .scan_once(&mut journal)
        .unwrap();
    drop(journal);
    let whole = fs::read_to_string(&fx.journal).unwrap();
    let mut lines: Vec<&str> = whole.lines().collect();
    lines[1] = "{not json";
    fs::write(&fx.journal, format!("{}\n", lines.join("\n"))).unwrap();

    let err = Journal::open(&fx.journal).unwrap_err();
    assert!(
        matches!(
            err,
            episcience_filer::FilerError::JournalCorrupt { line: 2, .. }
        ),
        "{err}"
    );
}

#[test]
fn the_scan_writes_nothing_under_the_root() {
    let fx = fixture();
    write_valid_run(&fx.root.join("campaign-a/cell-01"), HashAlgorithm::Sha256);
    let before = data_files(&fx.root);
    let mut journal = Journal::open(&fx.journal).unwrap();
    Watcher::new(&fx.root, WatcherOptions::default())
        .scan_once(&mut journal)
        .unwrap();
    assert_eq!(data_files(&fx.root), before);
}
