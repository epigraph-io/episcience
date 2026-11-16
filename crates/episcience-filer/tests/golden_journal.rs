//! The journal for a fixed set of run directories, compared byte for byte with
//! `tests/fixtures/golden_journal.jsonl`, which `tests/fixtures/golden_journal.py`
//! computed WITHOUT this crate (Python hashlib and the `blake3` package, from
//! the documented format). The run directories below must stay identical to
//! the script's `RUNS`.
//!
//! Ported from #72's independently computed golden plan (backlog 28dde255),
//! re-derived for #71's journal format. Two of #72's golden semantics are
//! deliberately NOT carried over: a run with `RUN.json` and no manifest is
//! pending here (`campaign-a/cell-04` is never journaled), not a rejection.

use std::fs;
use std::path::Path;

use episcience_filer::journal::Journal;
use episcience_filer::manifest::HashAlgorithm;
use episcience_filer::watcher::{Watcher, WatcherOptions};

const GOLDEN: &str = include_str!("fixtures/golden_journal.jsonl");

const STATEMENT_1: &str =
    "Gate G1 PASS: the measured value 2.50 \u{b1} 0.10 is within the tolerance 0.40 of the reference 2.60.";

/// (run dir, manifest, files as listed, files whose bytes on disk differ).
type RunSpec = (
    &'static str,
    Option<HashAlgorithm>,
    Vec<(&'static str, String)>,
    Vec<(&'static str, &'static str)>,
);

fn runs() -> Vec<RunSpec> {
    vec![
        (
            "campaign-a/cell-01",
            Some(HashAlgorithm::Sha256),
            vec![
                (
                    "RUN.json",
                    concat!(
                        r#"{"campaign":"campaign-a","cell":"cell-01","#,
                        r#""workflow_id":"00000000-0000-4000-8000-000000000001","#,
                        r#""canonical_name":"example-workflow","step_id":"step-3","#,
                        r#""started_at":"2026-01-02T03:04:05Z","#,
                        r#""data_root_uri":"pvc://example-volume/campaign-a/cell-01","#,
                        r#""host":{"driver":"1.2.3","gpu":"example-gpu"},"#,
                        r#""small_files":["logs/run.log"]}"#
                    )
                    .to_string(),
                ),
                (
                    "RESULT.json",
                    format!(
                        r#"{{"status":"completed","finished_at":"2026-01-02T04:04:05Z","gates":[{{"name":"G1","verdict":"PASS","statement":"{STATEMENT_1}"}}]}}"#
                    ),
                ),
                ("logs/run.log", "step 1 ok\nstep 2 ok\n".to_string()),
                ("data/traj.csv", "t,x\n0,1.5\n1,0.5\n".to_string()),
            ],
            vec![],
        ),
        (
            "campaign-a/cell-02",
            Some(HashAlgorithm::Blake3),
            vec![
                (
                    "RUN.json",
                    r#"{"campaign":"campaign-a","cell":"cell-02","step_id":"step-3"}"#.to_string(),
                ),
                (
                    "RESULT.json",
                    concat!(
                        r#"{"status":"failed","gates":["#,
                        r#"{"name":"G1","verdict":"FAIL","statement":"Gate G1 FAIL: the run did not converge.","step_id":"step-9"},"#,
                        r#"{"name":"G2","verdict":"PASS","statement":"Gate G2 PASS: the trajectory is complete."}]}"#
                    )
                    .to_string(),
                ),
                ("data/traj.csv", "t,x\n0,9.0\n".to_string()),
            ],
            vec![],
        ),
        (
            "campaign-a/cell-03",
            Some(HashAlgorithm::Sha256),
            vec![
                (
                    "RUN.json",
                    r#"{"campaign":"campaign-a","cell":"cell-03"}"#.to_string(),
                ),
                ("RESULT.json", r#"{"status":"completed"}"#.to_string()),
                ("data/traj.csv", "t,x\n0,1.0\n".to_string()),
            ],
            vec![("data/traj.csv", "t,x\n0,2.0\n")],
        ),
        (
            "campaign-a/cell-04",
            None,
            vec![(
                "RUN.json",
                r#"{"campaign":"campaign-a","cell":"cell-04"}"#.to_string(),
            )],
            vec![],
        ),
        (
            "campaign-b/cell-01",
            Some(HashAlgorithm::Sha256),
            vec![
                (
                    "RUN.json",
                    r#"{"campaign":"campaign-b","cell":"cell-01"}"#.to_string(),
                ),
                (
                    "RESULT.json",
                    r#"{"status":"completed","gates":[]}"#.to_string(),
                ),
            ],
            vec![],
        ),
    ]
}

fn write_root(root: &Path, specs: &[RunSpec]) {
    for (run_dir, algorithm, listed, on_disk) in specs {
        let dir = root.join(run_dir);
        for (path, body) in listed {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, body).unwrap();
        }
        if let Some(algorithm) = algorithm {
            let mut sorted: Vec<_> = listed.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let manifest: String = sorted
                .iter()
                .map(|(path, body)| format!("{}  {path}\n", algorithm.hash_bytes(body.as_bytes())))
                .collect();
            fs::write(dir.join(algorithm.manifest_file()), manifest).unwrap();
        }
        for (path, body) in on_disk {
            fs::write(dir.join(path), body).unwrap();
        }
    }
}

#[test]
fn the_journal_matches_an_independently_computed_golden() {
    let mut forward = runs();
    let mut backward = runs();
    backward.reverse();
    // Creation order must not reach the journal.
    for specs in [&mut forward, &mut backward] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("runs");
        write_root(&root, specs);
        let journal_path = tmp.path().join("outbox.jsonl");
        let watcher = Watcher::new(&root, WatcherOptions::default());

        let mut journal = Journal::open(&journal_path).unwrap();
        let summary = watcher.scan_once(&mut journal).unwrap();
        assert_eq!(summary.complete, 4);
        assert_eq!(summary.journaled_accepted, 3);
        assert_eq!(summary.journaled_rejected, 1);
        assert_eq!(summary.pending, vec!["campaign-a/cell-04".to_string()]);
        let written = fs::read_to_string(&journal_path).unwrap();
        assert_eq!(written, GOLDEN);

        // A second scan, and a restart, append nothing.
        assert_eq!(watcher.scan_once(&mut journal).unwrap().records_written, 0);
        drop(journal);
        let mut reopened = Journal::open(&journal_path).unwrap();
        watcher.scan_once(&mut reopened).unwrap();
        assert_eq!(fs::read_to_string(&journal_path).unwrap(), GOLDEN);
    }
}

#[test]
fn the_golden_carries_the_gate_text_as_raw_utf8() {
    // serde_json and the script both write non-ASCII as raw UTF-8: the ±
    // appears as its two bytes, never as a `±` escape.
    assert!(GOLDEN.contains(STATEMENT_1));
    assert!(!GOLDEN.contains("\\u00b1"));
}
