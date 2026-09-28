//! R8: static lint of every EpiScience migration from 5033 on (no database).
//!
//! Rules (each has a negative unit test below that names the mutation it
//! kills):
//!
//! - `first`: the first statement is the contract assertion: 5033 opens with
//!   its inline `DO $contract$` check; every later file opens with
//!   `SELECT public.episcience_assert_kernel_contract(1);`.
//! - `search_path`: no session-level `search_path` change anywhere
//!   (`SET [LOCAL|SESSION] search_path`, `set_config('search_path', …)`,
//!   `RESET search_path`), and every `CREATE FUNCTION` / `PROCEDURE` carries
//!   `SET search_path = public, pg_temp` in its header.
//! - `qualified`: every relation or function a top-level statement (or a DO
//!   block body) creates, alters, drops, writes, grants on or comments on is
//!   `public.`-qualified. Function bodies are exempt: their path is pinned.
//! - `kernel_object`: no DDL, DML or grant on a kernel table, and no function
//!   other than `public.episcience_*` is created, altered, dropped or granted
//!   on (this covers "no `CREATE OR REPLACE FUNCTION public.epigraph_`").
//!   EpiScience tables are the 14 of `ledger::EPISCIENCE_TABLES` plus any
//!   table a top-level migration creates. The only exception is the
//!   allowlisted detach in 5038.
//! - `ledger`: `_sqlx_migrations` appears only inside a marked, read-only
//!   contract-check region (`>>> contract vN checks` … `<<<`), where the
//!   kernel ledger head (C7) is read; no statement there writes it.
//! - `all_objects`: no `ON ALL … IN SCHEMA` and no `ALTER DEFAULT PRIVILEGES`.
//! - `uuid`: no uuid literal (comments included) other than the world and
//!   seed sentinels.
//! - `not_in_contract`: none of the kernel objects the contract excludes is
//!   named in code (the 114+ functions, `epigraph_node_tenancy`,
//!   `epigraph_link_operator`, `epigraph_seed`, `tenancy_exempt`,
//!   `epigraph.allow_declassify`).
//!
//! The canary runs this file against kernel HEAD as well (it reads only the
//! repository, so it is identical there).
use std::collections::BTreeSet;

use episcience_db::ledger::EPISCIENCE_TABLES;
use regex::Regex;

// ─── SQL splitting ───────────────────────────────────────────────────────────

/// One top-level statement: `top` has comments removed and every
/// dollar-quoted body replaced by `$BODY$`; `bodies` are those bodies with
/// their comments removed; `raw` is the original text.
#[derive(Debug)]
struct Stmt {
    top: String,
    bodies: Vec<String>,
    raw: String,
}

/// Split SQL into statements at top-level `;`, honouring `--` and `/* */`
/// comments, single-quoted strings (with `''`), double-quoted identifiers and
/// dollar-quoted strings (`$$` / `$tag$`).
fn split(sql: &str) -> Vec<Stmt> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut top = String::new();
    let mut bodies = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            top.push(' ');
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            top.push(' ');
            continue;
        }
        if c == b'\'' || c == b'"' {
            let q = c;
            let s = i;
            i += 1;
            while i < b.len() {
                if b[i] == q {
                    if b.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            top.push_str(&sql[s..i]);
            continue;
        }
        if c == b'$' {
            if let Some(tag_len) = dollar_tag(&b[i..]) {
                let tag = &sql[i..i + tag_len];
                let body_start = i + tag_len;
                let end = sql[body_start..]
                    .find(tag)
                    .map(|e| body_start + e)
                    .unwrap_or(b.len());
                bodies.push(strip_comments(&sql[body_start..end]));
                top.push_str("$BODY$");
                i = (end + tag_len).min(b.len());
                continue;
            }
        }
        if c == b';' {
            push_stmt(&mut out, &mut top, &mut bodies, &sql[start..i]);
            start = i + 1;
            i += 1;
            continue;
        }
        let ch = sql[i..].chars().next().expect("char boundary");
        top.push(ch);
        i += ch.len_utf8();
    }
    push_stmt(&mut out, &mut top, &mut bodies, &sql[start..]);
    out
}

fn push_stmt(out: &mut Vec<Stmt>, top: &mut String, bodies: &mut Vec<String>, raw: &str) {
    if !top.trim().is_empty() {
        out.push(Stmt {
            top: std::mem::take(top).trim().to_string(),
            bodies: std::mem::take(bodies),
            raw: raw.to_string(),
        });
    } else {
        top.clear();
        bodies.clear();
    }
}

/// Length of a dollar-quote opening tag at the start of `b` (`$$` or
/// `$ident$`), or `None` (a positional parameter such as `$1`).
fn dollar_tag(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'$') {
        return None;
    }
    let mut j = 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        if j == 1 && b[j].is_ascii_digit() {
            return None;
        }
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

/// `s` with the contents of every single-quoted literal blanked (`''`), so
/// message text such as `'cannot INSERT into public.x'` is not read as SQL.
fn blank_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if in_str && chars.peek() == Some(&'\'') {
                chars.next();
                continue;
            }
            in_str = !in_str;
            out.push(c);
        } else if !in_str {
            out.push(c);
        }
    }
    out
}

/// The literal SQL text a body hands to dynamic execution
/// (`EXECUTE '…'`, `format('…', …)`), which the object rules must still see.
fn dynamic_sql(body: &str) -> Vec<String> {
    let r = re(r"\b(?:EXECUTE|format)\s*\(?\s*'((?:[^']|'')*)'");
    r.captures_iter(body)
        .map(|c| c[1].replace("''", "'"))
        .collect()
}

/// Remove `--` and `/* */` comments (outside quotes) from `s`.
fn strip_comments(s: &str) -> String {
    split(s)
        .into_iter()
        .map(|st| st.top)
        .collect::<Vec<_>>()
        .join(";\n")
}

// ─── Rules ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    rule: &'static str,
    detail: String,
}

fn v(rule: &'static str, detail: impl Into<String>) -> Violation {
    Violation {
        rule,
        detail: detail.into(),
    }
}

fn re(p: &str) -> Regex {
    Regex::new(&format!("(?is){p}")).expect("regex")
}

const SENTINELS: [&str; 2] = [
    "00000000-0000-0000-0000-000000000000",
    "00000000-0000-0000-0000-00000000dead",
];

const NOT_IN_CONTRACT: [&str; 9] = [
    "epigraph_writer_group",
    "epigraph_attach_writer_owner",
    "epigraph_lock_public_claim_for_attach",
    "epigraph_session_is_privileged_writer",
    "epigraph_node_tenancy",
    "epigraph_link_operator",
    "epigraph_seed",
    "tenancy_exempt",
    "epigraph.allow_declassify",
];

/// `(version, verb, object)` triples the `kernel_object` rule admits.
const KERNEL_ALLOWLIST: [(i64, &str, &str); 2] = [
    (5038, "DROP TRIGGER", "public.edges"),
    (
        5038,
        "DROP FUNCTION",
        "public.create_shared_evidence_factor",
    ),
];

fn unquote(name: &str) -> String {
    name.replace('"', "").trim_end_matches('(').to_string()
}

fn is_function_stmt(top: &str) -> bool {
    re(r"^\s*CREATE\s+(OR\s+REPLACE\s+)?(FUNCTION|PROCEDURE)\b").is_match(top)
}

/// `(verb, object, kind)` for every object a piece of SQL touches. `kind` is
/// `table`, `function`, `sequence`, `index` or `other`.
fn touched_objects(sql: &str) -> Vec<(String, String, &'static str)> {
    let mut out = Vec::new();
    for (r, verb, kind) in object_patterns() {
        for c in r.captures_iter(sql) {
            let obj = unquote(&c[1]);
            let upper = obj.to_ascii_uppercase();
            // `ON FUNCTION|SEQUENCE|SCHEMA|ALL …` is handled by its own pattern
            // or rule; the bare-`ON` grant pattern sees the keyword.
            if *kind == "grant"
                && [
                    "FUNCTION",
                    "PROCEDURE",
                    "ROUTINE",
                    "SEQUENCE",
                    "SCHEMA",
                    "ALL",
                    "DATABASE",
                    "TABLE",
                ]
                .contains(&upper.as_str())
            {
                continue;
            }
            let kind = if *kind == "grant" { "table" } else { kind };
            out.push((verb.to_string(), obj, kind));
        }
    }
    out
}

type Pattern = (Regex, &'static str, &'static str);

/// The object patterns, compiled once.
fn object_patterns() -> &'static [Pattern] {
    static PATS: std::sync::OnceLock<Vec<Pattern>> = std::sync::OnceLock::new();
    PATS.get_or_init(build_object_patterns)
}

fn build_object_patterns() -> Vec<Pattern> {
    // `%` so that a dynamic `format('… %I …')` target is read (and refused).
    let name = r#"([A-Za-z0-9_."%]+)"#;
    vec![
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:UNLOGGED\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?{name}"
            )),
            "CREATE TABLE",
            "table",
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\s+(?:IF\s+NOT\s+EXISTS\s+)?{name}"
            )),
            "CREATE VIEW",
            "table",
        ),
        (
            re(&format!(
                r"\bCREATE\s+SEQUENCE\s+(?:IF\s+NOT\s+EXISTS\s+)?{name}"
            )),
            "CREATE SEQUENCE",
            "sequence",
        ),
        (
            re(&format!(r"\bCREATE\s+TYPE\s+{name}")),
            "CREATE TYPE",
            "other",
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:FUNCTION|PROCEDURE)\s+{name}"
            )),
            "CREATE FUNCTION",
            "function",
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:CONCURRENTLY\s+)?(?:IF\s+NOT\s+EXISTS\s+)?(?:[A-Za-z0-9_\x22]+\s+)?ON\s+(?:ONLY\s+)?{name}"
            )),
            "CREATE INDEX",
            "table",
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:CONSTRAINT\s+)?TRIGGER\s+\S+\s+.*?\bON\s+{name}"
            )),
            "CREATE TRIGGER",
            "table",
        ),
        (
            re(&format!(r"\bCREATE\s+POLICY\s+\S+\s+ON\s+{name}")),
            "CREATE POLICY",
            "table",
        ),
        (
            re(&format!(
                r"\bALTER\s+(?:TABLE|VIEW)\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?{name}"
            )),
            "ALTER TABLE",
            "table",
        ),
        (
            re(&format!(
                r"\bALTER\s+(?:FUNCTION|PROCEDURE|ROUTINE)\s+{name}"
            )),
            "ALTER FUNCTION",
            "function",
        ),
        (
            re(&format!(r"\bALTER\s+SEQUENCE\s+(?:IF\s+EXISTS\s+)?{name}")),
            "ALTER SEQUENCE",
            "sequence",
        ),
        (
            re(&format!(
                r"\bALTER\s+(?:POLICY|TRIGGER)\s+\S+\s+ON\s+{name}"
            )),
            "ALTER POLICY",
            "table",
        ),
        (re(&format!(r"\bINSERT\s+INTO\s+{name}")), "INSERT", "table"),
        (
            re(&format!(
                r"\bUPDATE\s+(?:ONLY\s+)?{name}\s+(?:AS\s+\S+\s+|[A-Za-z_]\w*\s+)?SET\b"
            )),
            "UPDATE",
            "table",
        ),
        (
            re(&format!(r"\bDELETE\s+FROM\s+(?:ONLY\s+)?{name}")),
            "DELETE",
            "table",
        ),
        (
            re(&format!(r"\bTRUNCATE\s+(?:TABLE\s+)?(?:ONLY\s+)?{name}")),
            "TRUNCATE",
            "table",
        ),
        (
            re(&format!(
                r"\bDROP\s+(?:TABLE|VIEW|MATERIALIZED\s+VIEW)\s+(?:IF\s+EXISTS\s+)?{name}"
            )),
            "DROP TABLE",
            "table",
        ),
        (
            re(&format!(
                r"\bDROP\s+(?:FUNCTION|PROCEDURE|ROUTINE)\s+(?:IF\s+EXISTS\s+)?{name}"
            )),
            "DROP FUNCTION",
            "function",
        ),
        (
            re(&format!(r"\bDROP\s+SEQUENCE\s+(?:IF\s+EXISTS\s+)?{name}")),
            "DROP SEQUENCE",
            "sequence",
        ),
        (
            re(&format!(
                r"\bDROP\s+INDEX\s+(?:CONCURRENTLY\s+)?(?:IF\s+EXISTS\s+)?{name}"
            )),
            "DROP INDEX",
            "index",
        ),
        (
            re(&format!(
                r"\bDROP\s+TRIGGER\s+(?:IF\s+EXISTS\s+)?\S+\s+ON\s+{name}"
            )),
            "DROP TRIGGER",
            "table",
        ),
        (
            re(&format!(
                r"\bDROP\s+POLICY\s+(?:IF\s+EXISTS\s+)?\S+\s+ON\s+{name}"
            )),
            "DROP POLICY",
            "table",
        ),
        (
            re(&format!(
                r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+(?:TABLE\s+)?{name}"
            )),
            "GRANT",
            "grant",
        ),
        (
            re(&format!(
                r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+(?:FUNCTION|PROCEDURE|ROUTINE)\s+{name}"
            )),
            "GRANT FUNCTION",
            "function",
        ),
        (
            re(&format!(
                r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+SEQUENCE\s+{name}"
            )),
            "GRANT SEQUENCE",
            "sequence",
        ),
        (
            re(&format!(r"\bCOMMENT\s+ON\s+(?:TABLE|VIEW|COLUMN)\s+{name}")),
            "COMMENT",
            "table",
        ),
        (
            re(&format!(
                r"\bCOMMENT\s+ON\s+(?:FUNCTION|PROCEDURE)\s+{name}"
            )),
            "COMMENT FUNCTION",
            "function",
        ),
    ]
}

/// Every table a top-level statement of any migration creates.
fn created_tables(files: &[(i64, String)]) -> BTreeSet<String> {
    let mut s: BTreeSet<String> = EPISCIENCE_TABLES
        .iter()
        .map(|t| format!("public.{t}"))
        .collect();
    for (_, text) in files {
        for st in split(text) {
            for (verb, obj, _) in touched_objects(&st.top) {
                if verb == "CREATE TABLE" {
                    s.insert(obj);
                }
            }
        }
    }
    s
}

/// Text with every `>>> contract vN checks` … `<<< contract vN checks`
/// region removed, and the removed regions.
fn without_contract_regions(s: &str) -> (String, Vec<String>) {
    let open = Regex::new(r">>> contract v\d+ checks").unwrap();
    let close = Regex::new(r"<<< contract v\d+ checks").unwrap();
    let mut rest = s;
    let mut kept = String::new();
    let mut regions = Vec::new();
    while let Some(m) = open.find(rest) {
        kept.push_str(&rest[..m.start()]);
        let after = &rest[m.start()..];
        let end = close.find(after).map(|c| c.end()).unwrap_or(after.len());
        regions.push(after[..end].to_string());
        rest = &after[end..];
    }
    kept.push_str(rest);
    (kept, regions)
}

/// Lint one migration file.
fn lint_file(version: i64, text: &str, episcience_tables: &BTreeSet<String>) -> Vec<Violation> {
    let mut out = Vec::new();
    let stmts = split(text);

    // first
    match stmts.first() {
        None => out.push(v("first", "empty migration")),
        Some(first) => {
            let norm = first.top.split_whitespace().collect::<Vec<_>>().join(" ");
            if version == 5033 {
                let opens = first.raw.trim_start().starts_with("DO $contract$")
                    && first
                        .bodies
                        .first()
                        .is_some_and(|b| b.contains("C7") && b.contains("C11"));
                if !(norm.starts_with("DO $BODY$") && opens) {
                    out.push(v(
                        "first",
                        "5033 must open with its inline DO $contract$ check",
                    ));
                }
            } else if norm != "SELECT public.episcience_assert_kernel_contract(1)" {
                out.push(v(
                    "first",
                    format!("first statement must be the contract assertion, got {norm:?}"),
                ));
            }
        }
    }

    let session_sp = re(
        r"\bSET\s+(LOCAL\s+|SESSION\s+)?search_path\b|\bset_config\s*\(\s*'search_path'|\bRESET\s+search_path\b",
    );
    let pinned = re(r"\bSET\s+search_path\s*(=|TO)\s*public\s*,\s*pg_temp\b");
    let all_objects = re(
        r"\bON\s+ALL\s+(TABLES|SEQUENCES|FUNCTIONS|ROUTINES|PROCEDURES)\s+IN\s+SCHEMA\b|\bALTER\s+DEFAULT\s+PRIVILEGES\b",
    );

    for st in &stmts {
        let func = is_function_stmt(&st.top);
        // search_path
        if func {
            if !pinned.is_match(&st.top) {
                out.push(v(
                    "search_path",
                    format!(
                        "function without SET search_path = public, pg_temp: {}",
                        head(&st.top)
                    ),
                ));
            }
            let header_without_pin = pinned.replace_all(&st.top, "");
            if session_sp.is_match(&header_without_pin) {
                out.push(v(
                    "search_path",
                    format!("extra search_path setting: {}", head(&st.top)),
                ));
            }
        } else if session_sp.is_match(&st.top) {
            out.push(v(
                "search_path",
                format!("session search_path change: {}", head(&st.top)),
            ));
        }
        for body in &st.bodies {
            if session_sp.is_match(body) {
                out.push(v(
                    "search_path",
                    format!("search_path change in a body: {}", head(&st.top)),
                ));
            }
        }

        // all_objects (everywhere, bodies included)
        for piece in std::iter::once(&st.top).chain(st.bodies.iter()) {
            if all_objects.is_match(piece) {
                out.push(v("all_objects", head(piece)));
            }
        }

        // qualified + kernel_object: top level and DO bodies, not function bodies
        let mut scopes = vec![blank_strings(&st.top)];
        if !func {
            for b in &st.bodies {
                scopes.push(blank_strings(b));
                scopes.extend(dynamic_sql(b));
            }
        }
        for scope in &scopes {
            for (verb, obj, kind) in touched_objects(scope) {
                if !obj.starts_with("public.") {
                    out.push(v("qualified", format!("{verb} {obj}")));
                    continue;
                }
                if KERNEL_ALLOWLIST
                    .iter()
                    .any(|(ver, vb, o)| *ver == version && *vb == verb && *o == obj)
                {
                    continue;
                }
                match kind {
                    "function" => {
                        if !obj.starts_with("public.episcience_") {
                            out.push(v(
                                "kernel_object",
                                format!("{verb} {obj}: only public.episcience_* functions"),
                            ));
                        }
                    }
                    "table" => {
                        // COMMENT ON COLUMN names public.table.column.
                        let table = obj.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
                        if !episcience_tables.contains(&table)
                            && verb != "CREATE TABLE"
                            && verb != "CREATE VIEW"
                        {
                            out.push(v("kernel_object", format!("{verb} {obj}")));
                        }
                    }
                    "sequence" => {
                        let owned = obj.starts_with("public.episcience_")
                            || episcience_tables
                                .iter()
                                .any(|t| obj.starts_with(&format!("{t}_")));
                        if !owned {
                            out.push(v("kernel_object", format!("{verb} {obj}")));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // ledger: only inside a contract region, read-only there.
    let code = strip_comments(text);
    let (outside, regions) = without_contract_regions(text);
    if strip_comments(&outside).contains("_sqlx_migrations") {
        out.push(v(
            "ledger",
            "_sqlx_migrations named outside a contract-check region",
        ));
    }
    let write = re(
        r"\b(INSERT\s+INTO|UPDATE|DELETE\s+FROM|TRUNCATE|ALTER\s+TABLE|DROP\s+TABLE|CREATE\s+TABLE)\s+[^;]*_sqlx_migrations",
    );
    for r in &regions {
        if write.is_match(&strip_comments(r)) {
            out.push(v(
                "ledger",
                "a contract-check region writes _sqlx_migrations",
            ));
        }
    }

    // uuid (raw text: comments too)
    let uuid = Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .unwrap();
    for m in uuid.find_iter(text) {
        let lit = m.as_str().to_ascii_lowercase();
        if !SENTINELS.contains(&lit.as_str()) {
            out.push(v("uuid", lit));
        }
    }

    // not_in_contract (code only)
    let lower = code.to_ascii_lowercase();
    for name in NOT_IN_CONTRACT {
        let pat = Regex::new(&format!(r"\b{}\b", regex::escape(name))).unwrap();
        if pat.is_match(&lower) {
            out.push(v("not_in_contract", name));
        }
    }
    out
}

fn head(s: &str) -> String {
    s.split_whitespace().take(8).collect::<Vec<_>>().join(" ")
}

// ─── The repository's migrations ─────────────────────────────────────────────

fn migrations_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations")
}

/// `(version, text)` of every top-level `NNNN_*.sql` file.
fn repo_migrations() -> Vec<(i64, String)> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(migrations_dir()).expect("migrations/") {
        let p = e.expect("entry").path();
        if p.extension().and_then(|x| x.to_str()) != Some("sql") {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        let version: i64 = digits
            .parse()
            .unwrap_or_else(|_| panic!("unnumbered {name}"));
        v.push((version, std::fs::read_to_string(&p).expect("read")));
    }
    v.sort_by_key(|(n, _)| *n);
    v
}

/// Every migration from 5033 on passes every rule. Kills: any R8 violation
/// committed in a migration.
#[test]
fn every_e1_migration_passes_the_lint() {
    let files = repo_migrations();
    let tables = created_tables(&files);
    let mut linted = 0;
    for (version, text) in &files {
        if *version < 5033 {
            continue;
        }
        linted += 1;
        let v = lint_file(*version, text, &tables);
        assert!(v.is_empty(), "{version}: {v:#?}");
    }
    assert!(linted >= 1, "5033 must be linted");
}

// ─── Negative cases: each rule fires on a planted violation ─────────────────

const PRE: &str = "SELECT public.episcience_assert_kernel_contract(1);\n";

fn repo_tables() -> &'static BTreeSet<String> {
    static T: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    T.get_or_init(|| created_tables(&repo_migrations()))
}

fn rules(version: i64, sql: &str) -> Vec<&'static str> {
    let mut r: Vec<&'static str> = lint_file(version, sql, repo_tables())
        .into_iter()
        .map(|x| x.rule)
        .collect();
    r.dedup();
    r
}

fn fires(sql: &str, rule: &str) {
    let r = rules(5040, sql);
    assert!(r.contains(&rule), "{rule} must fire on:\n{sql}\ngot {r:?}");
}

#[test]
fn a_clean_later_migration_passes() {
    let sql = format!(
        "{PRE}ALTER TABLE public.syntheses ADD COLUMN x int;\n\
         CREATE FUNCTION public.episcience_x() RETURNS int LANGUAGE sql \
           SET search_path = public, pg_temp AS $$ SELECT 1 FROM claims $$;\n\
         CREATE INDEX syntheses_x ON public.syntheses (x);\n\
         GRANT SELECT ON public.syntheses TO episcience_rw;\n"
    );
    assert_eq!(rules(5040, &sql), Vec::<&str>::new());
}

/// Kills: dropping the first-statement rule (a migration could run DDL on a
/// drifted kernel before any assertion).
#[test]
fn first_fires_without_the_assertion_first() {
    fires("ALTER TABLE public.syntheses ADD COLUMN x int;\nSELECT public.episcience_assert_kernel_contract(1);", "first");
    fires("-- only a comment\n", "first");
    assert!(rules(5033, "SELECT 1;").contains(&"first"));
}

/// Kills: dropping the session-search_path rule or the required pin.
#[test]
fn search_path_fires_on_a_session_change_or_an_unpinned_function() {
    fires(&format!("{PRE}SET search_path = public;"), "search_path");
    fires(
        &format!("{PRE}SET LOCAL search_path TO public;"),
        "search_path",
    );
    fires(
        &format!("{PRE}SELECT set_config('search_path', 'x', true);"),
        "search_path",
    );
    fires(&format!("{PRE}RESET search_path;"), "search_path");
    fires(
        &format!("{PRE}DO $$ BEGIN PERFORM set_config('search_path', 'x', true); END $$;"),
        "search_path",
    );
    fires(
        &format!("{PRE}CREATE FUNCTION public.episcience_y() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$;"),
        "search_path",
    );
}

/// Kills: dropping the qualification rule (an unqualified name resolves in
/// the migrator's `episcience_meta` path, or not at all).
#[test]
fn qualified_fires_on_unqualified_objects() {
    fires(
        &format!("{PRE}ALTER TABLE syntheses ADD COLUMN x int;"),
        "qualified",
    );
    fires(
        &format!("{PRE}CREATE INDEX i ON syntheses (x);"),
        "qualified",
    );
    fires(
        &format!("{PRE}INSERT INTO samples (id) VALUES (1);"),
        "qualified",
    );
    fires(
        &format!("{PRE}DO $d$ BEGIN UPDATE syntheses SET x = 1; END $d$;"),
        "qualified",
    );
    fires(
        &format!("{PRE}DO $d$ BEGIN EXECUTE format('CREATE TABLE %I (x int)', 'y'); END $d$;"),
        "qualified",
    );
}

/// Kills: dropping the kernel-object rule (EpiScience DDL/DML on a kernel
/// table, or a redefinition of a kernel function).
#[test]
fn kernel_object_fires_on_kernel_tables_and_functions() {
    fires(
        &format!("{PRE}ALTER TABLE public.claims ADD COLUMN x int;"),
        "kernel_object",
    );
    fires(
        &format!("{PRE}CREATE INDEX i ON public.edges (x);"),
        "kernel_object",
    );
    fires(
        &format!("{PRE}UPDATE public.claims SET visibility = 'public';"),
        "kernel_object",
    );
    fires(
        &format!("{PRE}DELETE FROM public.security_events;"),
        "kernel_object",
    );
    fires(
        &format!("{PRE}GRANT INSERT ON public.claims TO episcience_rw;"),
        "kernel_object",
    );
    fires(
        &format!("{PRE}ALTER TABLE public.experiments ADD COLUMN x int;"),
        "kernel_object",
    );
    fires(
        &format!(
            "{PRE}CREATE OR REPLACE FUNCTION public.epigraph_bypass() RETURNS boolean \
             LANGUAGE sql SET search_path = public, pg_temp AS $$ SELECT true $$;"
        ),
        "kernel_object",
    );
    fires(
        &format!(
            "{PRE}GRANT EXECUTE ON FUNCTION public.epigraph_definer_bypass() TO episcience_rw;"
        ),
        "kernel_object",
    );
    fires(
        &format!("{PRE}DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges;"),
        "kernel_object",
    );
    // The 5038 allowlist admits exactly its detach.
    let detach = format!(
        "{PRE}DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges;\n\
         DROP FUNCTION IF EXISTS public.create_shared_evidence_factor();"
    );
    assert_eq!(rules(5038, &detach), Vec::<&str>::new());
}

/// A RAISE message that names a kernel table is text, not a write. Kills: a
/// lint that reads string literals as SQL (every contract check message would
/// then be refused), while dynamic SQL is still read (see `qualified`).
#[test]
fn message_strings_are_not_read_as_sql() {
    let sql = format!(
        "{PRE}DO $d$ BEGIN RAISE EXCEPTION 'epigraph_app cannot INSERT into public.events'; END $d$;"
    );
    assert_eq!(rules(5040, &sql), Vec::<&str>::new());
}

/// Kills: dropping the ledger rule, or exempting writes inside a region.
#[test]
fn ledger_fires_outside_a_region_and_on_a_write_inside_one() {
    fires(
        &format!("{PRE}SELECT max(version) FROM public._sqlx_migrations;"),
        "ledger",
    );
    fires(
        &format!(
            "{PRE}DO $d$ BEGIN\n-- >>> contract v2 checks\nDELETE FROM public._sqlx_migrations;\n-- <<< contract v2 checks\nEND $d$;"
        ),
        "ledger",
    );
    let read = format!(
        "{PRE}DO $d$ DECLARE h bigint; BEGIN\n-- >>> contract v2 checks\nSELECT max(version) INTO h FROM public._sqlx_migrations;\n-- <<< contract v2 checks\nEND $d$;"
    );
    assert!(!rules(5040, &read).contains(&"ledger"));
}

/// Kills: dropping the schema-wide grant rule.
#[test]
fn all_objects_fires_on_schema_wide_grants() {
    fires(
        &format!("{PRE}GRANT SELECT ON ALL TABLES IN SCHEMA public TO episcience_rw;"),
        "all_objects",
    );
    fires(
        &format!("{PRE}ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO episcience_rw;"),
        "all_objects",
    );
}

/// Kills: dropping the uuid rule (a live id in a public migration).
#[test]
fn uuid_fires_on_a_non_sentinel_literal_even_in_a_comment() {
    fires(
        &format!("{PRE}-- owner 12345678-1234-1234-1234-123456789abc\nSELECT 1;"),
        "uuid",
    );
    let ok = format!("{PRE}SELECT '00000000-0000-0000-0000-00000000DEAD'::uuid;");
    assert!(!rules(5040, &ok).contains(&"uuid"));
}

/// Kills: dropping the not-in-contract rule.
#[test]
fn not_in_contract_fires_on_excluded_kernel_objects() {
    for name in NOT_IN_CONTRACT {
        fires(&format!("{PRE}SELECT {name};"), "not_in_contract");
    }
    // A comment may name them (documentation), code may not.
    let comment = format!("{PRE}-- never calls epigraph_node_tenancy\nSELECT 1;");
    assert!(!rules(5040, &comment).contains(&"not_in_contract"));
}

/// The splitter honours dollar quotes, comments and strings. Kills: a
/// splitter that breaks a function body at an inner `;` (every body rule
/// would then read fragments).
#[test]
fn the_splitter_honours_quotes_and_comments() {
    let s = split(
        "CREATE FUNCTION f() AS $x$ SELECT 1; SELECT 2; $x$;\n\
         -- a ; in a comment\n\
         SELECT 'a;b', \"c;d\" /* e; */;\n\
         DO $$ BEGIN PERFORM 1; END $$;",
    );
    assert_eq!(s.len(), 3, "{s:#?}");
    assert_eq!(s[0].bodies.len(), 1);
    assert!(s[0].bodies[0].contains("SELECT 1") && s[0].bodies[0].contains("SELECT 2"));
    assert!(s[1].top.contains("'a;b'"));
    assert!(s[2].top.starts_with("DO $BODY$"));
    assert!(
        split("SELECT $1, $2;").len() == 1,
        "positional params are not tags"
    );
}
