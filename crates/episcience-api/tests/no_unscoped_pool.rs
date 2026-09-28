//! R6 (brief 6.3): no unscoped pool on a path that should be stamped.
//!
//! A source scan (no database) of `episcience-api/src` and
//! `episcience-db/src` for `PgPool`, `.pool` and `execute(&pool`, per file,
//! against an EXACT register: a file whose count moves in EITHER direction,
//! or a new file with any, fails, so the register only changes on purpose
//! (and shrinks as E1g converts the request path). Each entry says why the
//! pool is there and which batch removes it. The final entries (after E1g)
//! are `RESOLVE_POOL` (kernel parity: `Viewer::resolve` on a plain pool),
//! `ENGINE_POOL` (`V1-engine-takes-pool`, until KE-1) and the boot probes.
//!
//! Two narrower rules pin the worker (E1f): every transaction it opens is a
//! stage session's (`session.begin()`, which stamps and re-checks authority),
//! and every statement it runs on `RESOLVE_POOL` calls an EpiScience
//! maintenance-owned definer (the queue and the worklist): the unstamped pool
//! never reads or writes an EpiScience table directly.
use std::path::{Path, PathBuf};

use regex::Regex;

/// `(file, count, why)`.
const REGISTER: &[(&str, usize, &str)] = &[
    ("crates/episcience-api/src/auth/tenancy.rs", 10, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/auth/viewer.rs", 2, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/bin/episcience-mcp-server.rs", 1, "E1g: the MCP server's DSN (switched to episcience_app with the request path)"),
    ("crates/episcience-api/src/bin/episcience-worker.rs", 2, "RESOLVE_POOL + ENGINE_POOL: the worker builds its two unstamped pools (resolve/parity/queue definers; the engine, V1-engine-takes-pool until KE-1)"),
    ("crates/episcience-api/src/bin/server.rs", 1, "E1g: the server's DSN (switched to episcience_app with the request path)"),
    ("crates/episcience-api/src/jobs/episcience_job_queue.rs", 10, "LEGACY_RUNNER: the in-process JobRunner queue (deleted in E1h)"),
    ("crates/episcience-api/src/jobs/session.rs", 3, "RESOLVE_POOL for the per-stage re-resolve; the legacy runner's privileged pool (Privileged session, deleted with the runner in E1h)"),
    ("crates/episcience-api/src/jobs/synthesis_job.rs", 7, "ENGINE_POOL on the worker (engine + novelty reads); the legacy runner's privileged pool otherwise (E1h)"),
    ("crates/episcience-api/src/jobs/worker.rs", 4, "RESOLVE_POOL (Viewer::resolve, the operator-link parity read, the queue and worklist definers; the field and Worker::new take it) + ENGINE_POOL (the belief recheck)"),
    ("crates/episcience-api/src/mcp/blobs.rs", 4, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/countersigns.rs", 5, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/errors.rs", 1, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/list_countersignatures.rs", 4, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/mod.rs", 3, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/observations.rs", 4, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/protocols.rs", 3, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/queries.rs", 6, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/mcp/synthesize.rs", 8, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/blobs.rs", 12, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/countersign.rs", 12, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/export.rs", 2, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/protocols.rs", 5, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/samples.rs", 16, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/search.rs", 2, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/syntheses.rs", 26, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/synthesis_search.rs", 2, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/routes/workflow_runs.rs", 3, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-api/src/state.rs", 2, "E1g: request path on the superuser pool (every handler moves onto read_as / write_as)"),
    ("crates/episcience-db/src/repos/blob.rs", 5, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/countersign.rs", 3, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/kernel_claims.rs", 4, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/notebook.rs", 2, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/protocol.rs", 5, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/sample.rs", 9, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/synthesis.rs", 8, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/synthesis_clusters.rs", 2, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/synthesis_embeddings.rs", 3, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/synthesis_membership.rs", 2, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/synthesis_shares.rs", 4, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/synthesis_staleness.rs", 2, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/repos/worker_state.rs", 2, "E1g: repository takes a pool (E1g moves every repository onto &mut PgConnection)"),
    ("crates/episcience-db/src/synthesis/novelty_backend_internal.rs", 6, "NOVELTY_READ: unstamped novelty reads (public rows only on the worker login; E1g moves them onto the stamped session)"),
    ("crates/episcience-db/src/synthesis/novelty_backend_paper.rs", 8, "NOVELTY_READ: unstamped novelty reads (public rows only on the worker login; E1g moves them onto the stamped session)"),
    ("crates/episcience-db/src/synthesis/pipeline.rs", 12, "ENGINE_POOL in stages 1-2 (V1-engine-takes-pool); the pool forms of stages 2-4 used by the legacy runner and tests (E1h)"),
    ("crates/episcience-db/src/synthesis/publish.rs", 7, "LEGACY_RUNNER: the pool forms of stage 6 and the in-process startup reconcile (E1h)"),
    ("crates/episcience-db/src/synthesis/staleness.rs", 2, "ENGINE_POOL: get_belief takes a plain pool (V1-engine-takes-pool, until KE-1)"),
    ("crates/episcience-db/src/tenancy_contract.rs", 3, "BOOT_PROBE: the contract and schema probes run on a plain pool before serving (the privileged-session check is executor-generic)"),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repository root")
        .to_path_buf()
}

/// Source with `//` comment lines blanked (docs mention pools too).
fn code(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("read source")
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("read dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn counts() -> Vec<(String, usize)> {
    let root = repo_root();
    let needle = Regex::new(r"PgPool|\.pool\b|execute\(&pool").unwrap();
    let mut files = Vec::new();
    for d in ["crates/episcience-api/src", "crates/episcience-db/src"] {
        rs_files(&root.join(d), &mut files);
    }
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let n = needle.find_iter(&code(&p)).count();
            let rel = p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
            (n > 0).then_some((rel, n))
        })
        .collect()
}

#[test]
fn unscoped_pool_uses_match_the_register_exactly() {
    let found = counts();
    let mut problems = Vec::new();
    for (file, n) in &found {
        match REGISTER.iter().find(|(f, _, _)| f == file) {
            None => problems.push(format!("{file}: {n} use(s), not registered")),
            Some((_, want, _)) if want != n => {
                problems.push(format!("{file}: {n} use(s), the register says {want}"))
            }
            Some(_) => {}
        }
    }
    for (file, _, _) in REGISTER {
        if !found.iter().any(|(f, _)| f == file) {
            problems.push(format!(
                "{file}: registered but has no use left (remove it)"
            ));
        }
    }
    assert!(problems.is_empty(), "R6:\n{}", problems.join("\n"));
}

/// Every transaction the worker loop opens is a stage session's.
#[test]
fn the_worker_opens_transactions_only_through_its_stage_session() {
    let src = code(&repo_root().join("crates/episcience-api/src/jobs/worker.rs"));
    let begin = Regex::new(r"(\w+)\s*\.begin\(\)").unwrap();
    let opened: Vec<String> = begin
        .captures_iter(&src)
        .map(|c| c[1].to_string())
        .collect();
    assert!(
        !opened.is_empty(),
        "the scan found the worker's transactions"
    );
    assert!(
        opened.iter().all(|r| r == "session"),
        "a transaction not opened by a stage session: {opened:?}"
    );
}

/// Every statement the worker runs on RESOLVE_POOL calls an EpiScience
/// definer.
#[test]
fn the_workers_unstamped_statements_are_definer_calls_only() {
    let src = code(&repo_root().join("crates/episcience-api/src/jobs/worker.rs"));
    let stmt = Regex::new(
        r#"(?s)sqlx::query(?:_as|_scalar)?(?:::<[^>]*>)?\(\s*"([^"]*)"[^;]*?\.(?:execute|fetch_one|fetch_all|fetch_optional)\(&self\.resolve_pool\)"#,
    )
    .unwrap();
    let sqls: Vec<String> = stmt.captures_iter(&src).map(|c| c[1].to_string()).collect();
    assert!(
        sqls.len() >= 4,
        "the scan found the queue and worklist calls: {sqls:?}"
    );
    for s in &sqls {
        assert!(
            s.contains("public.episcience_"),
            "an unstamped worker statement that is not a definer call: {s}"
        );
    }
}

/// The scanner itself: a stamped-transaction read is not an unstamped use,
/// and a raw pool is.
#[test]
fn the_needle_tells_a_pool_from_a_connection() {
    let needle = Regex::new(r"PgPool|\.pool\b|execute\(&pool").unwrap();
    assert!(needle.is_match("fn f(pool: &PgPool)"));
    assert!(needle.is_match("x.execute(&pool).await"));
    assert!(needle.is_match("self.pool.clone()"));
    assert!(!needle.is_match("q.execute(&mut *tx).await"));
    assert!(!needle.is_match("self.pooling"));
}
