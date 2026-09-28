//! R11 (B-M1): no silent zero-row write.
//!
//! A source scan (no database) of the repositories, the pipeline and the
//! worker: every `UPDATE … SET` / `DELETE FROM` statement's OWN result must
//! reach a check that fails on the wrong count, before the next write
//! statement or function begins: `expect_rows(…)`, `one_row(…)`, an explicit
//! `rows_affected()` comparison, `.fetch_one(…)` (a `RETURNING` read whose
//! absence is an error), or `.fetch_optional(…)` turned into an error with
//! `ok_or…`. Merely reading `rows_affected()`, or checking and then only
//! logging the failure (`if let Err(…) … warn!`), is NOT a check. A write
//! whose 0-row outcome is legitimate is in [`REGISTER`] with its reason; the
//! register is EXACT (an entry with no unchecked write left fails too), so it
//! only shrinks.
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
    (
        "crates/episcience-db/src/repos/synthesis_provo_edges.rs",
        "defer_unwritten",
        "idempotent: rows already deferred or written are left alone; returns the count it deferred",
    ),
    (
        "crates/episcience-db/src/synthesis/pipeline.rs",
        "stage3_persist",
        "the replace clears 0..n clusters of an earlier attempt before inserting the new set",
    ),
    (
        "crates/episcience-api/src/jobs/episcience_job_queue.rs",
        "dequeue",
        "the queue claim: 0 rows means no queued job is due, the normal idle outcome",
    ),
];

/// Whether the code after a write statement (up to the next write or
/// function) checks the statement's row count, given the code just before it
/// (`preceding`, to see an `if let Err(` that swallows the check).
fn is_checked(window: &str, preceding: &str) -> bool {
    let compared = Regex::new(r"rows_affected\(\)\s*(==|!=|>=|<=|<|>)").unwrap();
    let optional_to_error = Regex::new(r"(?s)\.fetch_optional\(.*?\.ok_or").unwrap();
    let checked = window.contains("expect_rows(")
        || window.contains("one_row(")
        || window.contains(".fetch_one(")
        || compared.is_match(window)
        || optional_to_error.is_match(window);
    let swallowed = preceding.contains("if let Err(") && window.contains("warn!(");
    checked && !swallowed
}

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
            // The statement's OWN result: handled before the next write
            // statement or function, whichever comes first.
            let tail = &code[m.end()..];
            let next_fn = func.find(tail).map(|n| n.start()).unwrap_or(tail.len());
            let next_write = write.find(tail).map(|n| n.start()).unwrap_or(tail.len());
            let window = &tail[..next_fn.min(next_write)];
            let head = &code[m.start().saturating_sub(400)..m.start()];
            let preceding = head.rsplit(';').next().unwrap_or(head);
            out.push((rel.clone(), f, is_checked(window, preceding)));
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
/// flagged, an upsert is ignored; reading `rows_affected()` without a
/// comparison, or a check whose failure is only logged, is NOT a check; a
/// check after a LATER write does not cover an earlier one. Kills: a scanner
/// that matches nothing, or the reviewer's mutant (`expect_rows` replaced by
/// `let _n = res.rows_affected(); Ok(())` in `mark_written`) passing.
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

    assert!(is_checked(
        ".execute(pool).await?; expect_rows(res, 1, \"x\", id)",
        ""
    ));
    assert!(is_checked(
        ".execute(pool).await?; if res.rows_affected() != 1 { return Err(e) }",
        ""
    ));
    assert!(is_checked(
        ".fetch_optional(pool).await?.ok_or_else(|| nf())",
        ""
    ));
    assert!(!is_checked(
        ".execute(pool).await?; let _n = res.rows_affected(); Ok(())",
        ""
    ));
    assert!(!is_checked(
        ".execute(pool).await?; Ok(res.rows_affected())",
        ""
    ));
    assert!(!is_checked(".fetch_optional(pool).await?; Ok(row)", ""));
    assert!(!is_checked(
        ".execute(pool).await.and_then(|r| one_row(r.rows_affected(), \"x\", id)) { tracing::warn!(\"x\") }",
        "if let Err(e) = sqlx::query(\""
    ));

    let all = writes();
    assert!(all
        .iter()
        .any(|(f, n, c)| f.ends_with("repos/synthesis.rs") && n == "mark_stale" && !c));
    assert!(all
        .iter()
        .any(|(f, n, c)| f.ends_with("repos/synthesis.rs") && n == "update_status" && *c));
    assert!(all
        .iter()
        .any(|(f, n, c)| f.ends_with("repos/synthesis_provo_edges.rs")
            && n == "mark_written"
            && *c));
}
