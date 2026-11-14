//! The shadow plan, the outbox journal and the watcher pass.

mod common;

use std::path::Path;

use common::*;
use episcience_filer::contract::Algorithm;
use episcience_filer::journal::{Journal, JournalError};
use episcience_filer::plan::{build_plan, normalize, render, PlanRecord};
use episcience_filer::watch::{plan_for_root, scan, watch_once};

/// One valid run (cell-01) and one with RUN.json only (cell-02).
fn golden_root(root: &Path) {
    write_valid_run(&root.join("demo-campaign/cell-01"), Algorithm::Sha256);
    let bad = root.join("demo-campaign/cell-02");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join("RUN.json"),
        r#"{"campaign":"demo-campaign","cell":"cell-02"}"#,
    )
    .unwrap();
}

#[test]
fn the_plan_matches_an_independently_computed_golden() {
    // tests/fixtures/golden_plan.jsonl was computed with Python's hashlib and
    // json (sort_keys, compact separators), not with this crate.
    let tmp = tempfile::tempdir().unwrap();
    golden_root(tmp.path());
    let plan = render(&plan_for_root(tmp.path()).unwrap());
    assert_eq!(plan, include_str!("fixtures/golden_plan.jsonl"));
}

#[test]
fn the_plan_is_byte_identical_whatever_the_creation_or_scan_order() {
    let cells = ["cell-03", "cell-01", "cell-02", "cell-10"];
    let forward = tempfile::tempdir().unwrap();
    for cell in cells {
        write_cell(&forward.path().join("camp").join(cell), "camp", cell);
    }
    let backward = tempfile::tempdir().unwrap();
    for cell in cells.iter().rev() {
        write_cell(&backward.path().join("camp").join(cell), "camp", cell);
    }
    let a = render(&plan_for_root(forward.path()).unwrap());
    let b = render(&plan_for_root(backward.path()).unwrap());
    assert_eq!(a, b);
    assert_eq!(
        a,
        render(&plan_for_root(forward.path()).unwrap()),
        "a second scan differs"
    );

    let mut outcomes = scan(forward.path()).unwrap();
    outcomes.reverse();
    assert_eq!(
        render(&build_plan(&outcomes)),
        a,
        "outcome order leaks into the plan"
    );
    // 1 campaign + 4 runs + 4 gates.
    assert_eq!(a.lines().count(), 9);
}

#[test]
fn a_tampered_run_files_a_rejection_and_no_run_or_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("camp/cell-01");
    write_valid_run(&dir, Algorithm::Sha256);
    flip_first_byte(&dir.join("data/energy.csv"));
    let plan = plan_for_root(tmp.path()).unwrap();
    let kinds: Vec<&str> = plan.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["rejection"]);
    // The key comes from the directory, never from the unverified RUN.json.
    assert_eq!(plan[0].key.campaign, "");
    assert_eq!(plan[0].key.cell, "");
    assert_eq!(plan[0].payload["run_dir"], "camp/cell-01");
    let reason = plan[0].payload["reason"].as_str().unwrap();
    assert!(
        reason.contains("digest mismatch for data/energy.csv"),
        "{reason}"
    );
}

#[test]
fn a_rerun_cell_with_the_same_verdict_gets_new_keys_but_a_reread_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    write_cell(&tmp.path().join("attempt-1"), "camp", "cell-01");
    // Same campaign, cell and gate verdict; different run content.
    write_run(
        &tmp.path().join("attempt-2"),
        &[
            (
                "RUN.json",
                r#"{"campaign":"camp","cell":"cell-01","attempt":2}"#,
            ),
            ("RESULT.json", r#"{"gates":[{"name":"ok","passed":true}]}"#),
        ],
        Algorithm::Sha256,
    );
    let plan = plan_for_root(tmp.path()).unwrap();
    let gates: Vec<&PlanRecord> = plan.iter().filter(|r| r.kind == "gate").collect();
    assert_eq!(
        gates.len(),
        2,
        "a re-run's gate collapsed into the first run's"
    );
    assert_ne!(gates[0].key.id(), gates[1].key.id());
}

#[test]
fn a_second_watcher_pass_appends_nothing_and_replay_equals_a_fresh_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    golden_root(&root);
    let journal_path = tmp.path().join("outbox.jsonl");

    let mut journal = Journal::open(&journal_path).unwrap();
    let first = watch_once(&root, &mut journal).unwrap();
    assert_eq!(first.len(), 5);
    let after_first = std::fs::read(&journal_path).unwrap();

    let second = watch_once(&root, &mut journal).unwrap();
    assert!(second.is_empty(), "second pass appended {second:?}");
    // A fresh process (reopened journal) appends nothing either.
    let mut reopened = Journal::open(&journal_path).unwrap();
    assert!(watch_once(&root, &mut reopened).unwrap().is_empty());
    assert_eq!(std::fs::read(&journal_path).unwrap(), after_first);

    let replayed = render(&normalize(reopened.records().to_vec()));
    assert_eq!(replayed, render(&plan_for_root(&root).unwrap()));
    assert_eq!(replayed, include_str!("fixtures/golden_plan.jsonl"));
}

#[test]
fn the_journal_only_grows_when_a_run_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let cell = root.join("camp/cell-01");
    write_valid_run(&cell, Algorithm::Sha256);
    let journal_path = tmp.path().join("outbox.jsonl");
    let mut journal = Journal::open(&journal_path).unwrap();
    watch_once(&root, &mut journal).unwrap();
    let before = std::fs::read(&journal_path).unwrap();

    // Corrupt the data: the journal keeps the earlier run record and gains a
    // rejection; nothing earlier is edited.
    flip_first_byte(&cell.join("data/energy.csv"));
    let fresh = watch_once(&root, &mut journal).unwrap();
    assert_eq!(
        fresh.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
        ["rejection"]
    );
    let after = std::fs::read(&journal_path).unwrap();
    assert!(
        after.starts_with(&before),
        "the journal was rewritten, not appended"
    );
    assert!(after.len() > before.len());
}

fn journal_with_one_pass(tmp: &Path) -> std::path::PathBuf {
    let root = tmp.join("root");
    golden_root(&root);
    let path = tmp.join("outbox.jsonl");
    watch_once(&root, &mut Journal::open(&path).unwrap()).unwrap();
    path
}

fn assert_corrupt(path: &Path, expected_line: usize, needle: &str) {
    match Journal::open(path) {
        Err(JournalError::Corrupt { line, reason, .. }) => {
            assert_eq!(line, expected_line, "{reason}");
            assert!(reason.contains(needle), "{reason}");
        }
        other => panic!("expected a corrupt journal, got {other:?}"),
    }
}

#[test]
fn a_torn_final_line_is_refused_not_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let path = journal_with_one_pass(tmp.path());
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
    assert_corrupt(&path, 5, "no newline");
}

#[test]
fn an_edited_journal_line_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = journal_with_one_pass(tmp.path());
    let text = std::fs::read_to_string(&path).unwrap();
    // Flip a gate verdict in place, leaving the JSON valid.
    let edited = text.replacen(r#""passed":false"#, r#""passed":true"#, 1);
    assert_ne!(edited, text);
    let line = text
        .lines()
        .position(|l| l.contains(r#""passed":false"#))
        .unwrap()
        + 1;
    std::fs::write(&path, edited).unwrap();
    // The edited line is still canonical JSON with an intact key, so only the
    // record_hash catches it.
    assert_corrupt(&path, line, "canonical rendering");
}

#[test]
fn a_duplicated_journal_line_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = journal_with_one_pass(tmp.path());
    let text = std::fs::read_to_string(&path).unwrap();
    let first = text.lines().next().unwrap();
    std::fs::write(&path, format!("{text}{first}\n")).unwrap();
    assert_corrupt(&path, 6, "duplicate key_id");
}

#[test]
fn a_rejected_run_json_cannot_file_its_rejection_under_another_cell() {
    let tmp = tempfile::tempdir().unwrap();
    write_cell(&tmp.path().join("camp/cell-01"), "camp", "cell-01");
    // An unverified directory whose RUN.json claims the valid run's cell.
    let bad = tmp.path().join("camp/intruder");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join("RUN.json"),
        r#"{"campaign":"camp","cell":"cell-01"}"#,
    )
    .unwrap();
    let plan = plan_for_root(tmp.path()).unwrap();
    let rejection = plan.iter().find(|r| r.kind == "rejection").unwrap();
    assert_eq!(
        (rejection.key.campaign.as_str(), rejection.key.cell.as_str()),
        ("", "")
    );
    assert_eq!(rejection.payload["run_dir"], "camp/intruder");
    // Every record keyed under (camp, cell-01) comes from the verified run.
    for r in plan.iter().filter(|r| r.key.cell == "cell-01") {
        assert_eq!(r.payload["run_dir"], "camp/cell-01", "{r:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_non_utf8_directory_name_is_rejected_and_the_rest_is_still_planned() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    write_cell(&tmp.path().join("camp/cell-01"), "camp", "cell-01");
    // An empty directory and a run directory, both with non-UTF-8 names.
    std::fs::create_dir_all(tmp.path().join(OsStr::from_bytes(b"junk\xff"))).unwrap();
    write_cell(
        &tmp.path()
            .join("camp")
            .join(OsStr::from_bytes(b"cell\\\xfe")),
        "camp",
        "cell-02",
    );
    let plan = plan_for_root(tmp.path()).expect("one bad name must not stop the scan");
    let kinds: Vec<&str> = plan.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["rejection", "rejection", "campaign", "gate", "run"]);
    let mut rejected: Vec<&str> = plan[..2]
        .iter()
        .map(|r| r.payload["run_dir"].as_str().unwrap())
        .collect();
    rejected.sort();
    // `\` is doubled and the bad byte is `\xNN`, so the rendering is injective.
    assert_eq!(rejected, [r"camp/cell\\\xfe", r"junk\xff"]);
    for r in &plan[..2] {
        assert!(
            r.payload["reason"].as_str().unwrap().contains("not UTF-8"),
            "{r:?}"
        );
    }
    assert_eq!(plan[4].payload["run_dir"], "camp/cell-01");
}

#[cfg(unix)]
#[test]
fn an_unlistable_subdirectory_is_rejected_and_the_rest_is_still_planned() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    write_cell(&tmp.path().join("camp/cell-01"), "camp", "cell-01");
    let locked = tmp.path().join("locked");
    std::fs::create_dir_all(locked.join("inner")).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let listable = std::fs::read_dir(&locked).is_ok(); // true when run as root
    let plan = plan_for_root(tmp.path());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if listable {
        eprintln!("skipped: permissions are not enforced for this user");
        return;
    }
    let plan = plan.expect("one unlistable directory must not stop the scan");
    let kinds: Vec<&str> = plan.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds, ["rejection", "campaign", "gate", "run"]);
    assert_eq!(plan[0].payload["run_dir"], "locked");
}
