//! R11 (B-M1): no silent zero-row write.
//!
//! A source scan (no database) of the repositories, the pipeline and the
//! worker: every `UPDATE … SET` / `DELETE FROM` statement's result must be
//! checked (a rows-affected comparison, `expect_rows`, or a `RETURNING` read
//! whose absence is an error), unless its function is in [`REGISTER`] with
//! the reason 0 rows is a legitimate outcome. The register is EXACT: an
//! entry whose function no longer holds an unchecked write fails too, so the
//! register only shrinks.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regex::Regex;

/// `(file relative to the repository root, function)`: writes whose 0-row
/// outcome is legitimate.
const REGISTER: &[(&str, &str, &str)] = &[
    (
        "crates/episcience-db/src/repos/synthesis.rs",
        "set_visibility_as",
        "the outbox release after a widening touches 0..n deferred rows (the synthesis row itself is checked)",
    ),
    (
        "crates/episcience-db/src/repos/synthesis.rs",
        "mark_failed",
        "conditional on a non-terminal status: a late failure never overwrites complete/deleted",
    ),
    (
        "crates/episcience-db/src/repos/synthesis.rs",
        "mark_stale",
        "idempotent (`WHERE stale_since IS NULL`): an already-stale row keeps its first reason",
    ),
    (
        "crates/episcience-db/src/repos/synthesis_membership.rs",
        "replace_for_synthesis",
        "the replace clears 0..n previous members before inserting the new set",
    ),
    (
        "crates/episcience-db/src/repos/synthesis_shares.rs",
        "revoke",
        "the frozen share table: revoking an absent share is a no-op (the routes are 410)",
    ),
];

/// What counts as checking the result of the statement.
const CHECKS: [&str; 5] = [
    "expect_rows(",
    "rows_affected()",
    "one_row(",
    ".fetch_optional(",
    ".fetch_one(",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repository root")
        .to_path_buf()
}

fn scanned_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = Vec::new();
    for dir in [
        "crates/episcience-db/src/repos",
        "crates/episcience-db/src/synthesis",
        "crates/episcience-api/src/jobs",
    ] {
        for e in std::fs::read_dir(root.join(dir)).expect("read dir") {
            let p = e.expect("entry").path();
            if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Each write statement: `(file, enclosing fn, checked?)`.
fn writes() -> Vec<(String, String, bool)> {
    let write =
        Regex::new(r"(?s)\b(?:UPDATE\s+(?:ONLY\s+)?[a-z_]+(?:\s+[a-z]+)?\s+SET|DELETE\s+FROM)\b")
            .unwrap();
    let func = Regex::new(r"\bfn\s+([a-z_0-9]+)\s*[<(]").unwrap();
    let root = repo_root();
    let mut out = Vec::new();
    for path in scanned_files() {
        let text = std::fs::read_to_string(&path).expect("read source");
        // Comments carry SQL too (docs); drop comment lines before scanning.
        let code: String = text
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        for m in write.find_iter(&code) {
            // `INSERT … ON CONFLICT … DO UPDATE SET` is an upsert, not an UPDATE.
            let before = code[..m.start()].trim_end();
            if before.ends_with("DO") {
                continue;
            }
            let f = func
                .captures_iter(&code[..m.start()])
                .last()
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            // The statement's result is handled before the next function.
            let tail = &code[m.end()..];
            let next_fn = func.find(tail).map(|n| n.start()).unwrap_or(tail.len());
            let window = &tail[..next_fn];
            let checked = CHECKS.iter().any(|c| window.contains(c));
            out.push((rel.clone(), f, checked));
        }
    }
    out
}

/// Kills: a new UPDATE/DELETE whose result is ignored (the B-M1 silent
/// zero-row write), and a registered exemption that is no longer needed.
#[test]
fn every_update_and_delete_checks_its_row_count_or_is_registered() {
    let all = writes();
    assert!(
        all.len() >= 15,
        "the scan found only {} writes; is it reading the right files?",
        all.len()
    );
    let unchecked: BTreeSet<(String, String)> = all
        .iter()
        .filter(|(_, _, checked)| !checked)
        .map(|(f, n, _)| (f.clone(), n.clone()))
        .collect();
    let register: BTreeSet<(String, String)> = REGISTER
        .iter()
        .map(|(f, n, _)| (f.to_string(), n.to_string()))
        .collect();
    let unregistered: Vec<_> = unchecked.difference(&register).collect();
    assert!(
        unregistered.is_empty(),
        "UPDATE/DELETE with an unchecked row count (check rows_affected / expect_rows, or register it with a reason): {unregistered:?}"
    );
    let stale: Vec<_> = register.difference(&unchecked).collect();
    assert!(
        stale.is_empty(),
        "registered exemptions with no unchecked write left (remove them): {stale:?}"
    );
}

/// The scanner itself: an unchecked write is found, a checked one is not
/// flagged, an upsert is ignored. Kills: a scanner that matches nothing.
#[test]
fn the_scanner_tells_checked_from_unchecked() {
    let write =
        Regex::new(r"(?s)\b(?:UPDATE\s+(?:ONLY\s+)?[a-z_]+(?:\s+[a-z]+)?\s+SET|DELETE\s+FROM)\b")
            .unwrap();
    assert!(write.is_match("UPDATE syntheses SET status = $2"));
    assert!(write.is_match("UPDATE syntheses s SET status = $2"));
    assert!(write.is_match("UPDATE synthesis_jobs\n            SET state = $2"));
    assert!(write.is_match("DELETE FROM synthesis_claim_membership WHERE"));
    assert!(!write.is_match("SELECT 1 FROM syntheses"));
    let all = writes();
    assert!(all
        .iter()
        .any(|(f, n, c)| f.ends_with("repos/synthesis.rs") && n == "mark_stale" && !c));
    assert!(all
        .iter()
        .any(|(f, n, c)| f.ends_with("repos/synthesis.rs") && n == "update_status" && *c));
}
