//! R8: static lint of every EpiScience migration from 5033 on (no database).
//!
//! Rules (each has a negative unit test below that names the mutation it
//! kills):
//!
//! - `first`: the first statement is the contract assertion: 5033 opens with
//!   its inline `DO $contract$` check; every later file opens with
//!   `SELECT public.episcience_assert_kernel_contract(1);`.
//! - `lock_timeout`: from 5036 on, the second statement is
//!   `SET LOCAL lock_timeout = '<n>s'` (or `ms`): these migrations take ACCESS
//!   EXCLUSIVE locks on tables the running service uses, and without a
//!   timeout a long transaction makes the migration queue and every later
//!   query on that table queue behind it. `LOCAL`: the setting ends with the
//!   migration's own transaction.
//! - `search_path`: no session-level `search_path` change anywhere, dynamic
//!   text included (`SET [LOCAL|SESSION] search_path`,
//!   `set_config('search_path', …)`, `RESET search_path`), and every `CREATE FUNCTION` / `PROCEDURE` carries
//!   exactly `SET search_path = public, pg_temp` in its header (nothing may
//!   follow `pg_temp` in the list).
//! - `qualified`: every relation or function a top-level statement (or a DO
//!   block body) creates, alters, drops, writes, grants on or comments on is
//!   `public.`-qualified. Function bodies are exempt: their path is pinned.
//!   Names are read with whitespace around the qualifying dot removed
//!   (`public . claims` is `public.claims`); `UPDATE` is read with its full
//!   grammar (`ONLY`, the inheritance `*`, a bare or quoted alias, `AS`).
//! - `kernel_object`: no DDL, DML or grant on a kernel table, and no function
//!   other than `public.episcience_*` is created, altered, dropped or granted
//!   on (this covers "no `CREATE OR REPLACE FUNCTION public.epigraph_`").
//!   EpiScience tables are the 14 of `ledger::EPISCIENCE_TABLES` plus any
//!   table a top-level migration creates; an index may be altered or dropped
//!   only if a migration of this repository created it. Comma-separated
//!   object lists (GRANT / REVOKE, TRUNCATE, DROP, LOCK) are read in full, and
//!   MERGE, COPY, CREATE RULE, ATTACH / DETACH PARTITION, INHERIT(S) and
//!   PARTITION OF name their target. Function bodies are checked too, with
//!   unqualified names read as `public.` (their path is pinned) and exactly
//!   one admitted kernel write: `INSERT INTO public.security_events` (the
//!   maintenance definers' audit rows). The only other exception is the
//!   allowlisted detach in 5040 (E1h): its two statements, matched exactly.
//! - `roles`: no role DDL (`CREATE` / `ALTER` / `DROP` `ROLE|USER|GROUP`), no
//!   membership grant or revoke (`GRANT <role> TO`, `REVOKE <role> FROM`), no
//!   `SET ROLE` / `SET SESSION AUTHORIZATION` (nor their `set_config('role'
//!   | 'session_authorization', …)` forms), anywhere (top level, DO and
//!   function bodies, dynamic SQL). `set_config`'s first argument must be a
//!   plain literal (a variable, `%L` or expression would hide which setting
//!   is changed). The one exception is 5033's creation of its NOLOGIN roles,
//!   admitted by exact text and version.
//! - `cluster`: no schema-, database- or cluster-level statement: GRANT /
//!   REVOKE `ON SCHEMA|DATABASE|TABLESPACE|LANGUAGE|FOREIGN …|LARGE OBJECT|
//!   PARAMETER|TYPE|DOMAIN`, `ALTER SYSTEM`, `CREATE|ALTER|DROP` `DATABASE|
//!   SCHEMA|EXTENSION|TABLESPACE|PUBLICATION|SUBSCRIPTION|EVENT TRIGGER|
//!   FOREIGN …|SERVER|LANGUAGE|USER MAPPING`, `LOAD`.
//! - `dynamic`: in every DO and function body, `EXECUTE` runs only a string
//!   literal, or a `format()` whose first argument is a literal that uses
//!   only `%I`, `%L` and `%%`; after that literal or that `format(…)` call
//!   (parentheses balanced) only the end of the statement, `INTO` or `USING`
//!   may follow: no `||`, no second literal, no operator. So every dynamic
//!   statement's text is visible to the rules above.
//! - `continuation`: no string literal is followed, across whitespace only,
//!   by another literal, anywhere (top level and bodies). SQL joins
//!   `'GRANT epigraph'` newline `'_maintenance …'` into ONE string, which no
//!   rule reading literals could see whole. Dynamic text is read unescaped,
//!   so a continuation inside an EXECUTE literal is refused too.
//! - `ledger`: `_sqlx_migrations` appears only inside a marked, read-only
//!   contract-check region (`>>> contract vN checks` … `<<<`), where the
//!   kernel ledger head (C7) is read; no statement there writes it.
//! - `all_objects`: no `ON ALL … IN SCHEMA` and no `ALTER DEFAULT PRIVILEGES`.
//! - `uuid`: no uuid literal (comments included) other than the world and
//!   seed sentinels.
//! - `not_in_contract`: every kernel `epigraph_*` name in code (bodies and
//!   string literals included, comments excluded) is one of the contract-v1
//!   names ([`CONTRACT_NAMES`]) or, in column position only (not called, not
//!   where a role is named), an `epigraph_`-prefixed COLUMN a migration of
//!   this repository declared on an EpiScience table (the outbox's
//!   `epigraph_edge_id`); no kernel `epigraph.*` setting is named; and
//!   none of the other excluded objects ([`NOT_IN_CONTRACT`]: the named 114+
//!   and tenancy objects, and the kernel's unprefixed trigger functions at the
//!   pinned head) is named.
//!
//! The canary runs this file against kernel HEAD as well (it reads only the
//! repository, so it is identical there).
use std::collections::BTreeSet;

use episcience_db::ledger::EPISCIENCE_TABLES;
use regex::Regex;

// ─── SQL splitting ───────────────────────────────────────────────────────────

/// One top-level statement: `top` has comments removed and every
/// dollar-quoted body replaced by `$BODY$`; `bodies` are the comment-free code
/// of those bodies, nested bodies included ([`code_of`]); `raw` is the
/// original text.
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
                bodies.push(code_of(&sql[body_start..end]));
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

/// The comment-free code of `s` at every nesting level: each statement's
/// top, followed by the code of its dollar-quoted bodies. String literals are
/// kept.
fn code_of(s: &str) -> String {
    split(s)
        .into_iter()
        .map(|st| {
            let mut t = st.top;
            for b in st.bodies {
                t.push('\n');
                t.push_str(&b);
            }
            t
        })
        .collect::<Vec<_>>()
        .join(";\n")
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

/// `s` with the contents of every single-quoted literal replaced by spaces of
/// the same byte length, so a match in the result is at the same offset in
/// `s` and never inside a literal.
fn mask_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if in_str && chars.peek() == Some(&'\'') {
                chars.next();
                out.push_str("  ");
                continue;
            }
            in_str = !in_str;
            out.push(c);
        } else if in_str {
            out.push_str(&" ".repeat(c.len_utf8()));
        } else {
            out.push(c);
        }
    }
    out
}

/// The single-quoted literal whose opening quote is at byte `q` of `s`: its
/// text (with `''` unescaped) and the byte offset just past its closing
/// quote.
fn literal_at(s: &str, q: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if b.get(q) != Some(&b'\'') {
        return None;
    }
    let mut i = q + 1;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some((s[q + 1..i].replace("''", "'"), i + 1));
        }
        i += 1;
    }
    None
}

/// The literal SQL text a body hands to dynamic execution (`EXECUTE '…'`,
/// `format('…', …)`), which the object and role rules must still see. Found
/// on the string-masked text, so a word inside a literal is never taken for
/// a call.
fn dynamic_sql(body: &str) -> Vec<String> {
    let masked = mask_strings(body);
    re(r"\b(?:EXECUTE|format)\s*\(?\s*'")
        .find_iter(&masked)
        .filter_map(|m| literal_at(body, m.end() - 1).map(|(t, _)| t))
        .collect()
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

/// The kernel `epigraph_*` names contract v1 admits (docs/tenancy-contract.md):
/// the C1 roles, the C2 session functions and the C12 functions.
const CONTRACT_NAMES: [&str; 9] = [
    "epigraph_app",
    "epigraph_maintenance",
    "epigraph_bypass",
    "epigraph_definer_bypass",
    "epigraph_session_groups",
    "epigraph_writable_groups",
    "epigraph_principal_id",
    "epigraph_live_memberships",
    "epigraph_operator_of_author",
];

/// Named exclusions (brief 6.5), kept as an explicit list: the
/// `epigraph_*` ones are refused by the allowlist too; the others are not
/// `epigraph_`-prefixed, among them the kernel's trigger functions at the
/// pinned head that carry no prefix (every prefixed one is caught by the
/// allowlist).
const NOT_IN_CONTRACT: [&str; 15] = [
    "epigraph_writer_group",
    "epigraph_attach_writer_owner",
    "epigraph_lock_public_claim_for_attach",
    "epigraph_session_is_privileged_writer",
    "epigraph_node_tenancy",
    "epigraph_link_operator",
    "epigraph_seed",
    "tenancy_exempt",
    "epigraph.allow_declassify",
    "auto_create_factor_from_edge",
    "cascade_delete_edges",
    "deactivate_superseded_factors",
    "raise_immutable_error",
    "trigger_validate_edge_refs",
    "update_updated_at_column",
];

/// `(version, statement)`: the only statements that may touch a kernel
/// object, E1h's detach. Matched EXACTLY against a TOP-LEVEL statement,
/// whitespace-normalised and without its `;`, at its own version only: a
/// different trigger or rule on `public.edges`, a `CASCADE`, another
/// function, the same text in a DO body or in dynamic SQL, or the same text at
/// another version is read by the `kernel_object` rule like any other
/// statement. (A `(version, verb, table)` key would admit any `DROP TRIGGER`
/// or `DROP RULE` on `public.edges` at that version.)
const KERNEL_STATEMENT_ALLOWLIST: [(i64, &str); 2] = [
    (
        5040,
        "DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges",
    ),
    (
        5040,
        "DROP FUNCTION IF EXISTS public.create_shared_evidence_factor()",
    ),
];

/// `(verb, object)` pairs a FUNCTION BODY may use on a kernel table: the
/// audit row every maintenance definer appends.
const FUNCTION_BODY_KERNEL_ALLOWLIST: [(&str, &str); 1] = [("INSERT", "public.security_events")];

/// `(version, dynamic SQL text)` the `roles` rule admits: 5033's creation of
/// its NOLOGIN grantee roles (whitespace-normalised).
const ROLE_ALLOWLIST: [(i64, &str); 1] = [(
    5033,
    "CREATE ROLE %I NOLOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION INHERIT",
)];

fn unquote(name: &str) -> String {
    name.replace('"', "").trim_end_matches('(').to_string()
}

fn is_function_stmt(top: &str) -> bool {
    re(r"^\s*CREATE\s+(OR\s+REPLACE\s+)?(FUNCTION|PROCEDURE)\b").is_match(top)
}

/// `(verb, object, kind)` for every object a piece of SQL touches. `kind` is
/// `table`, `function`, `sequence`, `index` or `other`.
fn touched_objects(sql: &str) -> Vec<(String, String, &'static str)> {
    let args = Regex::new(r"\([^)]*\)").unwrap();
    // `public . claims` is `public.claims`: whitespace around the qualifying
    // dot is legal SQL, and the name class stops at whitespace.
    let dot = Regex::new(r"\s*\.\s*").unwrap();
    let sql = dot.replace_all(sql, ".");
    let mut out = Vec::new();
    for (r, verb, kind, list) in object_patterns() {
        for c in r.captures_iter(&sql) {
            let names: Vec<String> = if *list {
                args.replace_all(&c[1], "")
                    .split(',')
                    .map(|n| unquote(n.trim()))
                    .filter(|n| !n.is_empty())
                    .collect()
            } else {
                vec![unquote(&c[1])]
            };
            // `ON FUNCTION|SEQUENCE|SCHEMA|ALL …` is handled by its own pattern
            // or rule; the bare-`ON` grant pattern sees the keyword.
            if *kind == "grant"
                && names.first().is_some_and(|n| {
                    [
                        "FUNCTION",
                        "PROCEDURE",
                        "ROUTINE",
                        "SEQUENCE",
                        "SCHEMA",
                        "ALL",
                        "DATABASE",
                        "TABLE",
                        "TABLESPACE",
                        "LANGUAGE",
                        "FOREIGN",
                        "LARGE",
                        "PARAMETER",
                        "TYPE",
                        "DOMAIN",
                    ]
                    .contains(&n.to_ascii_uppercase().as_str())
                })
            {
                continue;
            }
            let kind = if *kind == "grant" { "table" } else { kind };
            for obj in names {
                out.push((verb.to_string(), obj, kind));
            }
        }
    }
    out
}

/// `(pattern, verb, kind, captures a comma-separated list)`.
type Pattern = (Regex, &'static str, &'static str, bool);

/// The object patterns, compiled once.
fn object_patterns() -> &'static [Pattern] {
    static PATS: std::sync::OnceLock<Vec<Pattern>> = std::sync::OnceLock::new();
    PATS.get_or_init(build_object_patterns)
}

fn build_object_patterns() -> Vec<Pattern> {
    // `%` so that a dynamic `format('… %I …')` target is read (and refused).
    let name = r#"([A-Za-z0-9_."%]+)"#;
    // A comma-separated list of names, each optionally followed by an
    // argument list (DROP FUNCTION a(int), b()).
    let n = r#"[A-Za-z0-9_."%]+(?:\s*\([^)]*\))?"#;
    let list = format!(r"({n}(?:\s*,\s*{n})*)");
    vec![
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:UNLOGGED\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?{name}"
            )),
            "CREATE TABLE",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\s+(?:IF\s+NOT\s+EXISTS\s+)?{name}"
            )),
            "CREATE VIEW",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bCREATE\s+SEQUENCE\s+(?:IF\s+NOT\s+EXISTS\s+)?{name}"
            )),
            "CREATE SEQUENCE",
            "sequence",
            false,
        ),
        (
            re(&format!(r"\bCREATE\s+TYPE\s+{name}")),
            "CREATE TYPE",
            "other",
            false,
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:FUNCTION|PROCEDURE)\s+{name}"
            )),
            "CREATE FUNCTION",
            "function",
            false,
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:CONCURRENTLY\s+)?(?:IF\s+NOT\s+EXISTS\s+)?(?:[A-Za-z0-9_\x22.]+\s+)?ON\s+(?:ONLY\s+)?{name}"
            )),
            "CREATE INDEX",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:CONSTRAINT\s+)?TRIGGER\s+\S+\s+.*?\bON\s+{name}"
            )),
            "CREATE TRIGGER",
            "table",
            false,
        ),
        (
            re(&format!(r"\bCREATE\s+POLICY\s+\S+\s+ON\s+{name}")),
            "CREATE POLICY",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?RULE\s+\S+\s+AS\s+ON\s+\w+\s+TO\s+{name}"
            )),
            "CREATE RULE",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bALTER\s+(?:TABLE|VIEW|MATERIALIZED\s+VIEW)\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?{name}"
            )),
            "ALTER TABLE",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bALTER\s+INDEX\s+(?:ALL\s+IN\s+TABLESPACE\s+)?(?:IF\s+EXISTS\s+)?{name}"
            )),
            "ALTER INDEX",
            "index",
            false,
        ),
        (
            re(&format!(r"\b(?:ATTACH|DETACH)\s+PARTITION\s+{name}")),
            "PARTITION",
            "table",
            false,
        ),
        (
            re(&format!(r"\bPARTITION\s+OF\s+{name}")),
            "PARTITION OF",
            "table",
            false,
        ),
        (
            re(&format!(r"\bINHERITS\s*\(\s*{list}")),
            "INHERITS",
            "table",
            true,
        ),
        (
            re(&format!(r"\bINHERIT\s+{name}")),
            "INHERIT",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bALTER\s+(?:FUNCTION|PROCEDURE|ROUTINE)\s+{name}"
            )),
            "ALTER FUNCTION",
            "function",
            false,
        ),
        (
            re(&format!(r"\bALTER\s+SEQUENCE\s+(?:IF\s+EXISTS\s+)?{name}")),
            "ALTER SEQUENCE",
            "sequence",
            false,
        ),
        (
            re(&format!(
                r"\bALTER\s+(?:POLICY|TRIGGER|RULE)\s+\S+\s+ON\s+{name}"
            )),
            "ALTER POLICY",
            "table",
            false,
        ),
        (
            re(&format!(r"\bINSERT\s+INTO\s+{name}")),
            "INSERT",
            "table",
            false,
        ),
        (
            re(&format!(
                r#"\bUPDATE\s+(?:ONLY\s+)?{name}(?:\s*\*)?\s*(?:(?:AS\s*)?(?:"[^"]*"\s*|[\p{{L}}_][\w$]*\s+))?SET\b"#
            )),
            "UPDATE",
            "table",
            false,
        ),
        (
            re(&format!(r"\bDELETE\s+FROM\s+(?:ONLY\s+)?{name}")),
            "DELETE",
            "table",
            false,
        ),
        (
            re(&format!(r"\bMERGE\s+INTO\s+(?:ONLY\s+)?{name}")),
            "MERGE",
            "table",
            false,
        ),
        (re(&format!(r"\bCOPY\s+{name}")), "COPY", "table", false),
        (
            re(&format!(r"\bTRUNCATE\s+(?:TABLE\s+)?(?:ONLY\s+)?{list}")),
            "TRUNCATE",
            "table",
            true,
        ),
        (
            re(&format!(r"\bLOCK\s+(?:TABLE\s+)?(?:ONLY\s+)?{list}")),
            "LOCK",
            "table",
            true,
        ),
        (
            re(&format!(
                r"\bDROP\s+(?:TABLE|VIEW|MATERIALIZED\s+VIEW)\s+(?:IF\s+EXISTS\s+)?{list}"
            )),
            "DROP TABLE",
            "table",
            true,
        ),
        (
            re(&format!(
                r"\bDROP\s+(?:FUNCTION|PROCEDURE|ROUTINE)\s+(?:IF\s+EXISTS\s+)?{list}"
            )),
            "DROP FUNCTION",
            "function",
            true,
        ),
        (
            re(&format!(r"\bDROP\s+SEQUENCE\s+(?:IF\s+EXISTS\s+)?{list}")),
            "DROP SEQUENCE",
            "sequence",
            true,
        ),
        (
            re(&format!(
                r"\bDROP\s+INDEX\s+(?:CONCURRENTLY\s+)?(?:IF\s+EXISTS\s+)?{list}"
            )),
            "DROP INDEX",
            "index",
            true,
        ),
        (
            re(&format!(
                r"\bDROP\s+(?:TRIGGER|RULE)\s+(?:IF\s+EXISTS\s+)?\S+\s+ON\s+{name}"
            )),
            "DROP TRIGGER",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bDROP\s+POLICY\s+(?:IF\s+EXISTS\s+)?\S+\s+ON\s+{name}"
            )),
            "DROP POLICY",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+(?:TABLE\s+)?{list}"
            )),
            "GRANT",
            "grant",
            true,
        ),
        (
            re(&format!(
                r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+(?:FUNCTION|PROCEDURE|ROUTINE)\s+{list}"
            )),
            "GRANT FUNCTION",
            "function",
            true,
        ),
        (
            re(&format!(
                r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+SEQUENCE\s+{list}"
            )),
            "GRANT SEQUENCE",
            "sequence",
            true,
        ),
        (
            re(&format!(r"\bCOMMENT\s+ON\s+(?:TABLE|VIEW|COLUMN)\s+{name}")),
            "COMMENT",
            "table",
            false,
        ),
        (
            re(&format!(
                r"\bCOMMENT\s+ON\s+(?:FUNCTION|PROCEDURE)\s+{name}"
            )),
            "COMMENT FUNCTION",
            "function",
            false,
        ),
    ]
}

/// What the migrations of this repository create: every table a top-level
/// statement creates (plus the 14 of the baseline) and every index created on
/// one of those tables.
struct Known {
    tables: BTreeSet<String>,
    indexes: BTreeSet<String>,
    /// `epigraph_`-prefixed COLUMNS of EpiScience tables (such as the outbox's
    /// `epigraph_edge_id`): EpiScience's own names, not kernel objects.
    columns: BTreeSet<String>,
}

fn known_objects(files: &[(i64, String)]) -> Known {
    let mut tables: BTreeSet<String> = EPISCIENCE_TABLES
        .iter()
        .map(|t| format!("public.{t}"))
        .collect();
    for (_, text) in files {
        for st in split(text) {
            for (verb, obj, _) in touched_objects(&st.top) {
                if verb == "CREATE TABLE" {
                    tables.insert(obj);
                }
            }
        }
    }
    let index = re(
        r#"\bCREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:CONCURRENTLY\s+)?(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z0-9_."]+)\s+ON\s+(?:ONLY\s+)?([A-Za-z0-9_."]+)"#,
    );
    let mut indexes = BTreeSet::new();
    for (_, text) in files {
        for st in split(text) {
            for c in index.captures_iter(&st.top) {
                let (idx, table) = (unquote(&c[1]), unquote(&c[2]));
                if tables.contains(&table) {
                    // An index lives in its table's schema.
                    let idx = if idx.contains('.') {
                        idx
                    } else {
                        format!("public.{idx}")
                    };
                    indexes.insert(idx);
                }
            }
        }
    }
    // Columns declared on an EpiScience table: in its CREATE TABLE list, or by
    // ALTER TABLE … ADD / RENAME … TO. Only `epigraph_`-prefixed ones matter
    // (the not_in_contract rule would otherwise read them as kernel names).
    let in_create = re(r#"[(,]\s*"?(epigraph_[a-z0-9_]+)"?\s+[a-z]"#);
    let in_alter = re(
        r#"\b(?:ADD\s+(?:COLUMN\s+)?(?:IF\s+NOT\s+EXISTS\s+)?|RENAME\s+(?:COLUMN\s+)?\S+\s+TO\s+)"?(epigraph_[a-z0-9_]+)"#,
    );
    let mut columns = BTreeSet::new();
    for (_, text) in files {
        for st in split(text) {
            let top = blank_strings(&st.top);
            for (verb, obj, _) in touched_objects(&top) {
                let col_re = match verb.as_str() {
                    "CREATE TABLE" => &in_create,
                    "ALTER TABLE" => &in_alter,
                    _ => continue,
                };
                if tables.contains(&obj) {
                    for c in col_re.captures_iter(&top) {
                        columns.insert(c[1].to_ascii_lowercase());
                    }
                }
            }
        }
    }
    Known {
        tables,
        indexes,
        columns,
    }
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

/// Where a piece of SQL sits: the rules differ for function bodies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    TopOrDo,
    FunctionBody,
}

/// `roles` and `cluster` violations in one piece of string-blanked SQL (or
/// one dynamic literal).
fn role_and_cluster(piece: &str, out: &mut Vec<Violation>) {
    let role_ddl = re(r"\b(?:CREATE|ALTER|DROP)\s+(?:ROLE|USER|GROUP)\b");
    let set_role =
        re(r"\b(?:SET|RESET)\s+(?:(?:LOCAL|SESSION)\s+)?(?:ROLE|SESSION\s+AUTHORIZATION)\b");
    for r in [&role_ddl, &set_role] {
        for m in r.find_iter(piece) {
            out.push(v("roles", head(&piece[m.start()..])));
        }
    }
    // GRANT <role> TO / REVOKE <role> FROM: no ON before the TO / FROM.
    let on = re(r"\bON\b");
    for (kw, to) in [("GRANT", r"\bTO\b"), ("REVOKE", r"\bFROM\b")] {
        let to = re(to);
        for m in re(&format!(r"\b{kw}\b")).find_iter(piece) {
            let stmt = piece[m.start()..].split(';').next().unwrap_or("");
            if let Some(t) = to.find(stmt) {
                if !on.is_match(&stmt[..t.start()]) {
                    out.push(v("roles", format!("membership: {}", head(stmt))));
                }
            }
        }
    }
    let cluster = re(concat!(
        r"\b(?:GRANT|REVOKE)\b[^;]*?\bON\s+(?:SCHEMA|DATABASE|TABLESPACE|LANGUAGE|FOREIGN|LARGE\s+OBJECT|PARAMETER|TYPE|DOMAIN)\b",
        r"|\bALTER\s+SYSTEM\b",
        r"|\b(?:CREATE|ALTER|DROP)\s+(?:OR\s+REPLACE\s+)?(?:TRUSTED\s+)?(?:PROCEDURAL\s+)?(?:DATABASE|SCHEMA|EXTENSION|TABLESPACE|PUBLICATION|SUBSCRIPTION|EVENT\s+TRIGGER|FOREIGN|SERVER|LANGUAGE|USER\s+MAPPING)\b",
        r"|\bLOAD\s+'",
    ));
    for m in cluster.find_iter(piece) {
        out.push(v("cluster", head(&piece[m.start()..])));
    }
}

/// `dynamic` violations in one body: every `EXECUTE` must run a literal, or a
/// `format()` of a literal using only `%I` / `%L` / `%%`, never concatenated.
fn dynamic_execute(body: &str, out: &mut Vec<Violation>) {
    let masked = mask_strings(body);
    let privilege_or_trigger = re(r"^\s+(?:ON|FUNCTION|PROCEDURE)\b");
    let literal = re(r"^\s*'");
    let format = re(r"^\s*(?:pg_catalog\s*\.\s*)?format\s*\(\s*'");
    // What may follow the statement text: the end of the EXECUTE (`;` or the
    // end of the body) or its INTO / USING clause. Anything else (`||`, an
    // adjacent literal, an operator) would change the text the rules read.
    let tail_ok = re(r"^\s*(?:;|$|USING\b|INTO\b)");
    let next_argument = re(r"^\s*[,)]");
    for m in re(r"\bEXECUTE\b").find_iter(&masked) {
        let rest = &masked[m.end()..];
        if privilege_or_trigger.is_match(rest) {
            continue;
        }
        let ok = if let Some(l) = literal.find(rest) {
            literal_at(body, m.end() + l.end() - 1)
                .is_some_and(|(_, after)| tail_ok.is_match(&masked[after..]))
        } else if let Some(f) = format.find(rest) {
            let open = m.end() + rest[..f.end()].rfind('(').expect("format(");
            literal_at(body, m.end() + f.end() - 1).is_some_and(|(text, after)| {
                next_argument.is_match(&masked[after..])
                    && format_string_ok(&text)
                    && closing_paren(&masked, open)
                        .is_some_and(|close| tail_ok.is_match(&masked[close + 1..]))
            })
        } else {
            false
        };
        if !ok {
            out.push(v("dynamic", head(&body[m.start()..])));
        }
    }
}

/// The byte offset of the `)` that closes the `(` at `open`, on
/// string-masked text (so a parenthesis inside a literal never counts).
fn closing_paren(masked: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in masked[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `continuation` violations in one piece of code: SQL joins two string
/// literals separated only by whitespace that contains a newline into ONE
/// string (`'GRANT epigraph'` newline `'_maintenance TO x'`), so every rule
/// that reads a literal would see only the fragments. No migration needs the
/// form; it is refused wherever it appears (same-line adjacency is a syntax
/// error, so the newline is not required for the refusal).
fn continuation(piece: &str, out: &mut Vec<Violation>) {
    let blanked = blank_strings(piece);
    for m in re(r"'\s+'").find_iter(&blanked) {
        out.push(v(
            "continuation",
            format!(
                "adjacent string literals: {}",
                head(&blanked[char_floor(&blanked, m.start().saturating_sub(40))..])
            ),
        ));
    }
}

/// The largest char boundary of `s` at or below `i`.
fn char_floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// A `format()` string whose statement text is fixed: it starts with a
/// keyword and substitutes only identifiers (`%I`) and literals (`%L`).
fn format_string_ok(text: &str) -> bool {
    if !text
        .trim_start()
        .starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return false;
    }
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            match b.get(i + 1) {
                Some(b'I') | Some(b'L') | Some(b'%') => i += 2,
                _ => return false,
            }
        } else {
            i += 1;
        }
    }
    true
}

/// `qualified` and `kernel_object` violations for the objects one piece of
/// SQL touches.
fn object_rules(piece: &str, scope: Scope, known: &Known, out: &mut Vec<Violation>) {
    for (verb, obj, kind) in touched_objects(piece) {
        let obj = if obj.starts_with("public.") {
            obj
        } else if scope == Scope::FunctionBody && !obj.contains('.') {
            // The body's search_path is pinned to public, pg_temp.
            format!("public.{obj}")
        } else {
            out.push(v("qualified", format!("{verb} {obj}")));
            continue;
        };
        if scope == Scope::FunctionBody
            && FUNCTION_BODY_KERNEL_ALLOWLIST
                .iter()
                .any(|(vb, o)| *vb == verb && *o == obj)
        {
            continue;
        }
        match kind {
            "function" if !obj.starts_with("public.episcience_") => {
                out.push(v(
                    "kernel_object",
                    format!("{verb} {obj}: only public.episcience_* functions"),
                ));
            }
            "table" => {
                // COMMENT ON COLUMN names public.table.column.
                let table = obj.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
                if !known.tables.contains(&table) && verb != "CREATE TABLE" && verb != "CREATE VIEW"
                {
                    out.push(v("kernel_object", format!("{verb} {obj}")));
                }
            }
            "sequence" => {
                let owned = obj.starts_with("public.episcience_")
                    || known
                        .tables
                        .iter()
                        .any(|t| obj.starts_with(&format!("{t}_")));
                if !owned {
                    out.push(v("kernel_object", format!("{verb} {obj}")));
                }
            }
            "index" if !known.indexes.contains(&obj) => {
                out.push(v(
                    "kernel_object",
                    format!("{verb} {obj}: not an index a migration of this repository created"),
                ));
            }
            _ => {}
        }
    }
}

/// Lint one migration file.
fn lint_file(version: i64, text: &str, known: &Known) -> Vec<Violation> {
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

    // lock_timeout
    if version >= 5036 {
        let ok = stmts.get(1).is_some_and(|s| {
            let norm = s.top.split_whitespace().collect::<Vec<_>>().join(" ");
            re(r"^SET LOCAL lock_timeout = '[1-9][0-9]*(ms|s)'$").is_match(&norm)
        });
        if !ok {
            out.push(v(
                "lock_timeout",
                "the second statement must be SET LOCAL lock_timeout = '<n>s'",
            ));
        }
    }

    let session_sp = re(
        r"\bSET\s+(LOCAL\s+|SESSION\s+)?search_path\b|\bset_config\s*\(\s*'search_path'|\bRESET\s+search_path\b",
    );
    // Exactly `public, pg_temp`: the next non-blank character after pg_temp
    // may not continue the list.
    let pinned = re(r"\bSET\s+search_path\s*(=|TO)\s*public\s*,\s*pg_temp\b\s*([^,\s]|$)");
    let all_objects = re(
        r"\bON\s+ALL\s+(TABLES|SEQUENCES|FUNCTIONS|ROUTINES|PROCEDURES)\s+IN\s+SCHEMA\b|\bALTER\s+DEFAULT\s+PRIVILEGES\b",
    );

    // The function form of SET ROLE / SET SESSION AUTHORIZATION names the
    // setting in a string literal.
    let set_config_role = re(r"\bset_config\s*\(\s*'\s*(?:role|session_authorization)\s*'");
    let set_config_computed = re(r"\bset_config\s*\(\s*(?:[^'\s]|$)");

    for st in &stmts {
        let func = is_function_stmt(&st.top);
        // search_path
        if func {
            if !pinned.is_match(&st.top) {
                out.push(v(
                    "search_path",
                    format!(
                        "function without exactly SET search_path = public, pg_temp: {}",
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

        // dynamic: every DO and function body
        for b in &st.bodies {
            dynamic_execute(b, &mut out);
        }
        // continuation: the top level and every body
        for piece in std::iter::once(&st.top).chain(st.bodies.iter()) {
            continuation(piece, &mut out);
        }

        // Pieces of SQL the object, role and cluster rules read: the top
        // level, every body (string-blanked) and every dynamic literal.
        let body_scope = if func {
            Scope::FunctionBody
        } else {
            Scope::TopOrDo
        };
        let mut pieces: Vec<(String, Scope, bool)> =
            vec![(blank_strings(&st.top), Scope::TopOrDo, false)];
        for b in &st.bodies {
            pieces.push((blank_strings(b), body_scope, false));
            for d in dynamic_sql(b) {
                pieces.push((d, body_scope, true));
            }
        }
        // The exact allowlisted detach statements skip the object rules (and
        // only those): the top-level piece only, never a body or dynamic SQL.
        let top_norm = st.top.split_whitespace().collect::<Vec<_>>().join(" ");
        let top_admitted = KERNEL_STATEMENT_ALLOWLIST
            .iter()
            .any(|(ver, stmt)| *ver == version && *stmt == top_norm);
        for (i, (piece, scope, is_dynamic)) in pieces.iter().enumerate() {
            if !(i == 0 && top_admitted) {
                object_rules(piece, *scope, known, &mut out);
            }
            let normalised = piece.split_whitespace().collect::<Vec<_>>().join(" ");
            let admitted = *is_dynamic
                && ROLE_ALLOWLIST
                    .iter()
                    .any(|(ver, t)| *ver == version && *t == normalised);
            if !admitted {
                role_and_cluster(piece, &mut out);
            }
            // set_config's first argument names the setting; anything but a
            // plain literal (a variable, a `%L`, an expression) hides which
            // setting is changed. Read on the string-blanked piece, where a
            // literal first argument is exactly `''`.
            let blanked = if *is_dynamic {
                blank_strings(piece)
            } else {
                piece.clone()
            };
            for m in set_config_computed.find_iter(&blanked) {
                out.push(v(
                    "roles",
                    format!(
                        "set_config with a computed setting name: {}",
                        head(&blanked[m.start()..])
                    ),
                ));
            }
            // Dynamic text, unescaped: the literal-reading rules (session
            // search_path, set_config('role' …), continuation) must see it
            // too, because inside the EXECUTE literal its quotes are doubled.
            if *is_dynamic {
                if session_sp.is_match(piece) {
                    out.push(v(
                        "search_path",
                        format!("search_path change in dynamic SQL: {}", head(piece)),
                    ));
                }
                for m in set_config_role.find_iter(piece) {
                    out.push(v("roles", head(&piece[m.start()..])));
                }
                continuation(piece, &mut out);
            }
        }
    }

    // ledger: only inside a contract region, read-only there. Bodies count.
    let code = code_of(text);
    let (outside, regions) = without_contract_regions(text);
    if code_of(&outside).contains("_sqlx_migrations") {
        out.push(v(
            "ledger",
            "_sqlx_migrations named outside a contract-check region",
        ));
    }
    let write = re(
        r"\b(INSERT\s+INTO|UPDATE|DELETE\s+FROM|TRUNCATE|ALTER\s+TABLE|DROP\s+TABLE|CREATE\s+TABLE|MERGE\s+INTO|COPY)\s+[^;]*_sqlx_migrations",
    );
    for r in &regions {
        if write.is_match(&code_of(r)) {
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

    // roles, continued: the function form of SET ROLE / SET SESSION
    // AUTHORIZATION. It names the setting in a string literal, which the
    // blanked pieces above cannot see, so it is read from the code itself
    // (and, above, from every dynamic text).
    for m in set_config_role.find_iter(&code) {
        out.push(v("roles", head(&code[m.start()..])));
    }

    // not_in_contract (code at every nesting level, string literals included)
    let lower = code.to_ascii_lowercase();
    let kernel_name = Regex::new(r"\bepigraph_[a-z0-9_]+").unwrap();
    // An EpiScience column named `epigraph_…` is admitted in column position
    // only: never called (`name(`) and never where a role is named.
    let call_after = Regex::new(r"^\s*\(").unwrap();
    for m in kernel_name.find_iter(&lower) {
        let own_column = known.columns.contains(m.as_str())
            && !call_after.is_match(&lower[m.end()..])
            && !role_position(&lower[..m.start()]);
        if !CONTRACT_NAMES.contains(&m.as_str()) && !own_column {
            out.push(v(
                "not_in_contract",
                format!("{} is not a contract-v1 name", m.as_str()),
            ));
        }
    }
    let kernel_setting = Regex::new(r"\bepigraph\.[a-z_]+").unwrap();
    for m in kernel_setting.find_iter(&lower) {
        out.push(v(
            "not_in_contract",
            format!("kernel setting {}", m.as_str()),
        ));
    }
    for name in NOT_IN_CONTRACT {
        let pat = Regex::new(&format!(r"\b{}\b", regex::escape(name))).unwrap();
        if pat.is_match(&lower) {
            out.push(v("not_in_contract", name));
        }
    }
    out
}

/// Whether a name that follows `prefix` (lowercased code) sits where a role
/// is named: `TO` / `FROM` of a grant or policy, `ROLE`, `AUTHORIZATION`,
/// `GRANTED BY`. A column rename (`RENAME … TO`) and `IS DISTINCT FROM` are
/// column positions.
fn role_position(prefix: &str) -> bool {
    static RES: std::sync::OnceLock<[Regex; 3]> = std::sync::OnceLock::new();
    let [rename, distinct, role] = RES.get_or_init(|| {
        [
            Regex::new(r"\brename\s+(?:column\s+)?\S+\s+to$").unwrap(),
            Regex::new(r"\bdistinct\s+from$").unwrap(),
            Regex::new(r"\b(?:to|from|role|authorization|granted\s+by)$").unwrap(),
        ]
    });
    let p = prefix.trim_end();
    !rename.is_match(p) && !distinct.is_match(p) && role.is_match(p)
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
    let known = known_objects(&files);
    let mut linted = 0;
    for (version, text) in &files {
        if *version < 5033 {
            continue;
        }
        linted += 1;
        let v = lint_file(*version, text, &known);
        assert!(v.is_empty(), "{version}: {v:#?}");
    }
    assert!(linted >= 1, "5033 must be linted");
}

// ─── Negative cases: each rule fires on a planted violation ─────────────────

const PRE: &str =
    "SELECT public.episcience_assert_kernel_contract(1);\nSET LOCAL lock_timeout = '5s';\n";

fn repo_known() -> &'static Known {
    static K: std::sync::OnceLock<Known> = std::sync::OnceLock::new();
    K.get_or_init(|| known_objects(&repo_migrations()))
}

fn rules(version: i64, sql: &str) -> Vec<&'static str> {
    let mut r: Vec<&'static str> = lint_file(version, sql, repo_known())
        .into_iter()
        .map(|x| x.rule)
        .collect();
    r.dedup();
    r
}

fn fires(sql: &str, rule: &str) {
    let r = rules(5099, sql);
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
    assert_eq!(rules(5099, &sql), Vec::<&str>::new());
}

/// Kills: dropping the first-statement rule (a migration could run DDL on a
/// drifted kernel before any assertion).
#[test]
fn first_fires_without_the_assertion_first() {
    fires("ALTER TABLE public.syntheses ADD COLUMN x int;\nSELECT public.episcience_assert_kernel_contract(1);", "first");
    fires("-- only a comment\n", "first");
    assert!(rules(5033, "SELECT 1;").contains(&"first"));
}

/// Kills: dropping the lock-timeout rule (a migration from 5036 on could wait
/// on a long transaction with no bound, queueing every query behind it), or
/// admitting a session-level (non-LOCAL) setting that outlives the
/// migration. 5035 and earlier are not held to it.
#[test]
fn lock_timeout_fires_unless_it_is_the_second_statement() {
    let assertion = "SELECT public.episcience_assert_kernel_contract(1);\n";
    fires(
        &format!("{assertion}ALTER TABLE public.syntheses ADD COLUMN x int;"),
        "lock_timeout",
    );
    fires(
        &format!(
            "{assertion}SET lock_timeout = '5s';\nALTER TABLE public.syntheses ADD COLUMN x int;"
        ),
        "lock_timeout",
    );
    fires(
        &format!(
            "{assertion}ALTER TABLE public.syntheses ADD COLUMN x int;\nSET LOCAL lock_timeout = '5s';"
        ),
        "lock_timeout",
    );
    fires(
        &format!("{assertion}SET LOCAL lock_timeout = '0';"),
        "lock_timeout",
    );
    assert!(!rules(5035, &format!("{assertion}SELECT 1;")).contains(&"lock_timeout"));
    assert!(!rules(5099, &format!("{PRE}SELECT 1;")).contains(&"lock_timeout"));
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
    // The detach's allowlist (5040: E1h's, after E1f's 5038 and 5039) admits
    // exactly its detach, and only at its own version.
    let detach = format!(
        "{PRE}DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges;\n\
         DROP FUNCTION IF EXISTS public.create_shared_evidence_factor();"
    );
    assert_eq!(rules(5040, &detach), Vec::<&str>::new());
    assert!(rules(5038, &detach).contains(&"kernel_object"));
}

/// The 5040 allowlist admits its two exact statements and nothing else at
/// that version: another trigger or a rule on the kernel's `edges`, a
/// `CASCADE` on either statement, another kernel function, or the detach
/// inside a DO body. Kills: an allowlist keyed on `(version, verb, table)`
/// (any `DROP TRIGGER` / `DROP RULE` on `public.edges` at 5040 passed, a
/// kernel trigger such as `edges_auto_factor` included) or on the function
/// NAME (a trailing `CASCADE` passed), and one that reads bodies too.
#[test]
fn the_5040_allowlist_admits_only_its_exact_statements() {
    for stmt in [
        "DROP TRIGGER IF EXISTS edges_auto_factor ON public.edges;",
        "DROP TRIGGER edges_shared_evidence ON public.edges;",
        "DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges CASCADE;",
        "DROP RULE IF EXISTS edges_shared_evidence ON public.edges;",
        "DROP FUNCTION IF EXISTS public.create_shared_evidence_factor() CASCADE;",
        "DROP FUNCTION IF EXISTS public.auto_create_factor_from_edge();",
        "DO $d$ BEGIN DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges; END $d$;",
    ] {
        let sql = format!("{PRE}{stmt}");
        assert!(
            rules(5040, &sql).contains(&"kernel_object"),
            "5040 must refuse: {stmt}\ngot {:?}",
            rules(5040, &sql)
        );
    }
}

/// A RAISE message that names a kernel table is text, not a write. Kills: a
/// lint that reads string literals as SQL (every contract check message would
/// then be refused), while dynamic SQL is still read (see `qualified`).
#[test]
fn message_strings_are_not_read_as_sql() {
    let sql = format!(
        "{PRE}DO $d$ BEGIN RAISE EXCEPTION 'epigraph_app cannot INSERT into public.events'; END $d$;"
    );
    assert_eq!(rules(5099, &sql), Vec::<&str>::new());
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
    assert!(!rules(5099, &read).contains(&"ledger"));
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
    assert!(!rules(5099, &ok).contains(&"uuid"));
}

/// Kills: dropping the explicit exclusion list.
#[test]
fn not_in_contract_fires_on_excluded_kernel_objects() {
    for name in NOT_IN_CONTRACT {
        fires(&format!("{PRE}SELECT {name};"), "not_in_contract");
    }
    // A comment may name them (documentation), code may not.
    let comment = format!("{PRE}-- never calls epigraph_node_tenancy\nSELECT 1;");
    assert!(!rules(5099, &comment).contains(&"not_in_contract"));
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

// ─── Review round: the escalation class and the write forms R8 missed ────────

/// Every statement the review showed passing R8, each with the rule that must
/// now refuse it. Kills: dropping any one of the rules or patterns listed
/// (the corresponding row turns red).
#[test]
fn each_reviewed_escalation_or_write_form_is_refused() {
    let fn_body = |body: &str| {
        format!(
            "{PRE}CREATE FUNCTION public.episcience_evil() RETURNS void LANGUAGE plpgsql \
             SECURITY DEFINER SET search_path = public, pg_temp AS $f$ BEGIN {body} END $f$;"
        )
    };
    let cases: Vec<(String, &str)> = vec![
        // roles: membership, role DDL, role switching
        (format!("{PRE}GRANT epigraph_maintenance TO episcience_rw;"), "roles"),
        (format!("{PRE}GRANT episcience_rw TO episcience_evil WITH ADMIN OPTION;"), "roles"),
        (format!("{PRE}REVOKE episcience_rw FROM episcience_app;"), "roles"),
        (format!("{PRE}ALTER ROLE episcience_rw BYPASSRLS;"), "roles"),
        (format!("{PRE}ALTER USER episcience_app SUPERUSER;"), "roles"),
        (format!("{PRE}CREATE ROLE episcience_evil LOGIN SUPERUSER;"), "roles"),
        (format!("{PRE}DROP ROLE episcience_rw;"), "roles"),
        (format!("{PRE}SET ROLE epigraph_maintenance;"), "roles"),
        (format!("{PRE}SET SESSION AUTHORIZATION epigraph_maintenance;"), "roles"),
        (format!("{PRE}SELECT set_config('role', 'epigraph_maintenance', false);"), "roles"),
        (
            format!("{PRE}DO $d$ BEGIN PERFORM set_config('session_authorization', 'x', true); END $d$;"),
            "roles",
        ),
        (fn_body("PERFORM pg_catalog.set_config('role', 'epigraph_maintenance', true);"), "roles"),
        (
            format!("{PRE}DO $d$ BEGIN GRANT epigraph_maintenance TO episcience_rw; END $d$;"),
            "roles",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'GRANT epigraph_maintenance TO episcience_rw'; END $d$;"),
            "roles",
        ),
        (fn_body("GRANT epigraph_maintenance TO episcience_rw;"), "roles"),
        // cluster: schema / database level
        (format!("{PRE}GRANT CREATE ON SCHEMA public TO episcience_rw;"), "cluster"),
        (format!("{PRE}GRANT CONNECT ON DATABASE x TO episcience_rw;"), "cluster"),
        (format!("{PRE}ALTER SYSTEM SET log_statement = 'none';"), "cluster"),
        (format!("{PRE}ALTER DATABASE x SET work_mem = '1GB';"), "cluster"),
        (format!("{PRE}CREATE EXTENSION IF NOT EXISTS dblink;"), "cluster"),
        (format!("{PRE}CREATE SCHEMA episcience_side;"), "cluster"),
        (format!("{PRE}CREATE PUBLICATION p FOR TABLE public.syntheses;"), "cluster"),
        (format!("{PRE}LOAD 'plugin';"), "cluster"),
        // kernel_object: comma lists
        (
            format!("{PRE}GRANT SELECT, INSERT ON public.syntheses, public.claims TO episcience_rw;"),
            "kernel_object",
        ),
        (format!("{PRE}TRUNCATE public.syntheses, public.claims;"), "kernel_object"),
        (format!("{PRE}DROP TABLE public.syntheses, public.claims;"), "kernel_object"),
        (format!("{PRE}LOCK TABLE public.syntheses, public.claims;"), "kernel_object"),
        (
            format!("{PRE}DROP FUNCTION public.episcience_a(int), public.epigraph_bypass();"),
            "kernel_object",
        ),
        // kernel_object: other write forms
        (
            format!(
                "{PRE}MERGE INTO public.claims c USING public.syntheses s ON c.id = s.id \
                 WHEN MATCHED THEN UPDATE SET visibility = 'public';"
            ),
            "kernel_object",
        ),
        (format!("{PRE}COPY public.claims FROM '/tmp/x';"), "kernel_object"),
        (
            format!("{PRE}CREATE RULE r AS ON INSERT TO public.claims DO INSTEAD NOTHING;"),
            "kernel_object",
        ),
        (
            format!("{PRE}ALTER TABLE public.syntheses ATTACH PARTITION public.claims DEFAULT;"),
            "kernel_object",
        ),
        (
            format!("{PRE}CREATE TABLE public.episcience_x (id int) INHERITS (public.claims);"),
            "kernel_object",
        ),
        (
            format!("{PRE}CREATE TABLE public.episcience_y PARTITION OF public.claims DEFAULT;"),
            "kernel_object",
        ),
        (format!("{PRE}ALTER TABLE public.syntheses INHERIT public.claims;"), "kernel_object"),
        (format!("{PRE}ALTER INDEX public.claims_pkey RENAME TO x;"), "kernel_object"),
        (format!("{PRE}DROP INDEX public.claims_pkey;"), "kernel_object"),
        // dynamic: a statement text the lint cannot see
        (
            format!(
                "{PRE}DO $d$ DECLARE v text; BEGIN v := 'UPDATE public.claims SET x = 1'; EXECUTE v; END $d$;"
            ),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'UPDATE public.' || 'claims SET x = 1'; END $d$;"),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE format('%s', 'UPDATE public.claims SET x = 1'); END $d$;"),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE format('UPDATE public.' || 'claims SET x = %L', 1); END $d$;"),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE $q$UPDATE public.claims SET x = 1$q$; END $d$;"),
            "dynamic",
        ),
        (fn_body("EXECUTE v;"), "dynamic"),
        // set_config inside dynamic text (quotes doubled in the EXECUTE
        // literal), or with a computed setting name
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'SELECT set_config(''role'', ''x'', false)'; END $d$;"),
            "roles",
        ),
        (
            format!(
                "{PRE}DO $d$ BEGIN EXECUTE format('SELECT set_config(%L, %L, false)', 'role', 'x'); END $d$;"
            ),
            "roles",
        ),
        (
            format!("{PRE}DO $d$ DECLARE v text := 'role'; BEGIN PERFORM set_config(v, 'x', false); END $d$;"),
            "roles",
        ),
        (fn_body("PERFORM set_config(E'role', 'x', true);"), "roles"),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'SELECT set_config(''search_path'', ''x'', false)'; END $d$;"),
            "search_path",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'SELECT ''a''\n''b'''; END $d$;"),
            "continuation",
        ),
        // dynamic (delta review): text after the format() call, and SQL's
        // adjacent-literal continuation (two literals separated by a newline
        // are ONE string)
        (
            format!(
                "{PRE}DO $d$ BEGIN EXECUTE format('UPDATE public.syntheses SET status = %L', 'x') \
                 || '; UPDATE public.claims SET visibility = ''public'''; END $d$;"
            ),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'GRANT epigraph'\n '_maintenance TO episcience_rw'; END $d$;"),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'UPDATE public.cl'\n 'aims SET visibility = ''public'''; END $d$;"),
            "dynamic",
        ),
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE format('UPDATE public.cl'\n 'aims SET x = %L', 1); END $d$;"),
            "dynamic",
        ),
        (fn_body("EXECUTE 'SELECT 1' 'x';"), "dynamic"),
        // continuation: wherever a literal is continued, not only after EXECUTE
        (
            format!("{PRE}DO $d$ BEGIN EXECUTE 'GRANT epigraph'\n '_maintenance TO episcience_rw'; END $d$;"),
            "continuation",
        ),
        (
            format!("{PRE}SELECT set_config('ro'\n'le', 'epigraph_maintenance', false);"),
            "continuation",
        ),
        (
            fn_body("PERFORM to_regprocedure('public.epigraph'\n'_ensure_personal_group(uuid)');"),
            "continuation",
        ),
        // kernel_object (delta review): UPDATE with a quoted alias, the
        // inheritance star, whitespace around the schema dot
        (
            format!("{PRE}UPDATE public.claims \"c\" SET visibility = 'public';"),
            "kernel_object",
        ),
        (
            format!("{PRE}UPDATE ONLY public.claims AS\"c\"SET visibility = 'public';"),
            "kernel_object",
        ),
        (
            format!("{PRE}UPDATE public.claims * SET visibility = 'public';"),
            "kernel_object",
        ),
        (
            format!("{PRE}UPDATE public . claims SET visibility = 'public';"),
            "kernel_object",
        ),
        (
            format!("{PRE}INSERT INTO public .edges (id) VALUES (NULL);"),
            "kernel_object",
        ),
        (fn_body("UPDATE claims \"c\" SET visibility = 'public';"), "kernel_object"),
        (fn_body("UPDATE claims* SET visibility = 'public';"), "kernel_object"),
        // not_in_contract: an EpiScience column's name is admitted in column
        // position only, never called or used as a role
        (format!("{PRE}SELECT public.epigraph_edge_id();"), "not_in_contract"),
        (
            format!("{PRE}CREATE POLICY p ON public.syntheses TO epigraph_edge_id USING (true);"),
            "not_in_contract",
        ),
        // kernel_object inside function bodies (qualified and unqualified)
        (
            fn_body("UPDATE public.claims SET visibility = 'public'; DELETE FROM public.group_memberships;"),
            "kernel_object",
        ),
        (fn_body("UPDATE claims SET visibility = 'public';"), "kernel_object"),
        (fn_body("DELETE FROM group_memberships;"), "kernel_object"),
        (fn_body("INSERT INTO public.edges (id) VALUES (NULL);"), "kernel_object"),
        (fn_body("EXECUTE 'UPDATE public.claims SET x = 1';"), "kernel_object"),
        // search_path: nothing may follow pg_temp
        (
            format!(
                "{PRE}CREATE FUNCTION public.episcience_z() RETURNS int LANGUAGE sql \
                 SET search_path = public, pg_temp, other_schema AS $$ SELECT 1 $$;"
            ),
            "search_path",
        ),
        (
            format!(
                "{PRE}CREATE FUNCTION public.episcience_z() RETURNS int LANGUAGE sql \
                 SET search_path = public, pg_temp , other_schema AS $$ SELECT 1 $$;"
            ),
            "search_path",
        ),
    ];
    for (sql, rule) in &cases {
        fires(sql, rule);
    }
}

/// The forms the later tenancy migrations need stay admitted. Kills: a rule
/// so broad that the RLS and definer migrations could not be written (an
/// owner change to the maintenance role, EXECUTE grants, REVOKE from PUBLIC,
/// table lists of EpiScience tables, a definer's audit row, a literal or
/// `%I`-only dynamic statement).
#[test]
fn the_forms_later_migrations_need_pass() {
    let sql = format!(
        "{PRE}ALTER FUNCTION public.episcience_x() OWNER TO epigraph_maintenance;\n\
         REVOKE ALL ON FUNCTION public.episcience_x() FROM PUBLIC;\n\
         GRANT EXECUTE ON FUNCTION public.episcience_x() TO episcience_queue;\n\
         REVOKE ALL ON public.syntheses, public.samples FROM PUBLIC, epigraph_app;\n\
         GRANT SELECT, INSERT, UPDATE, DELETE ON public.syntheses, public.samples TO episcience_rw;\n\
         GRANT SELECT ON public.syntheses TO epigraph_app WITH GRANT OPTION;\n\
         ALTER TABLE public.syntheses ADD COLUMN owner_group_id uuid \
           REFERENCES public.groups(id) ON DELETE RESTRICT;\n\
         DROP INDEX IF EXISTS public.syntheses_status_idx;\n\
         CREATE TRIGGER tenancy_10_require BEFORE INSERT ON public.syntheses \
           FOR EACH ROW EXECUTE FUNCTION public.episcience_x();\n\
         CREATE FUNCTION public.episcience_x() RETURNS void LANGUAGE plpgsql SECURITY DEFINER \
           SET search_path = public, pg_temp AS $f$ \
           BEGIN \
             UPDATE syntheses SET visibility = 'group' WHERE id = NULL; \
             INSERT INTO public.security_events (event_type, agent_id, success, details) \
               VALUES ('episcience.maint.x', NULL, true, '{{}}'); \
             INSERT INTO security_events (event_type) VALUES ('episcience.maint.y'); \
             PERFORM 1 FROM claims c FOR UPDATE SKIP LOCKED; \
             EXECUTE 'UPDATE public.synthesis_jobs SET state = ''queued'''; \
             EXECUTE format('UPDATE public.synthesis_jobs SET state = %L WHERE id = %L', 'x', NULL) USING 1; \
             EXECUTE 'SELECT count(*) FROM public.synthesis_jobs' INTO n; \
             PERFORM set_config('episcience.allow_widen', 'yes', true); \
             EXECUTE 'SELECT set_config(''episcience.allow_widen'', ''yes'', true)'; \
             PERFORM 1 FROM synthesis_provo_edges p WHERE p.epigraph_edge_id IS NULL; \
             UPDATE synthesis_provo_edges AS \"p\" SET epigraph_edge_id = NULL \
               WHERE p.epigraph_edge_id IS DISTINCT FROM epigraph_edge_id; \
           END $f$;\n\
         CREATE INDEX synthesis_provo_edges_unwritten ON public.synthesis_provo_edges (synthesis_id) \
           WHERE epigraph_edge_id IS NULL;\n\
         COMMENT ON COLUMN public.synthesis_provo_edges.epigraph_edge_id IS 'the kernel edge';\n\
         ALTER TABLE public.synthesis_provo_edges RENAME COLUMN epigraph_edge_id TO kernel_edge_id;\n\
         ALTER TABLE public.synthesis_provo_edges RENAME COLUMN kernel_edge_id TO epigraph_edge_id;\n\
         DO $d$ BEGIN EXECUTE format('ALTER TABLE public.syntheses ADD COLUMN %I int', 'y'); END $d$;\n"
    );
    assert_eq!(rules(5099, &sql), Vec::<&str>::new(), "{sql}");
}

/// 5033's creation of its NOLOGIN roles is admitted at 5033 only. Kills: an
/// allowlist keyed on the text alone (any later migration could create roles
/// with the same statement).
#[test]
fn the_5033_role_creation_is_admitted_at_5033_only() {
    let start = MIGRATION_5033_TEXT.find("DO $roles$").expect("roles block");
    let block = &MIGRATION_5033_TEXT[start..];
    let at_later = format!("{PRE}{block}");
    assert!(
        rules(5099, &at_later).contains(&"roles"),
        "the roles block must be refused outside 5033"
    );
    assert!(!rules(5033, MIGRATION_5033_TEXT).contains(&"roles"));
}

const MIGRATION_5033_TEXT: &str = include_str!("../../../migrations/5033_kernel_contract_v1.sql");

/// Every kernel `epigraph_*` name must be a contract-v1 name, wherever it
/// appears in code (top level, DO and function bodies, string literals), and
/// no kernel setting may be read. Kills: going back to a denylist (the kernel
/// functions below are on no list) or scanning the top level only.
#[test]
fn not_in_contract_is_an_allowlist_over_all_code() {
    for sql in [
        format!("{PRE}SELECT public.epigraph_ensure_personal_group(NULL);"),
        format!("{PRE}SELECT public.epigraph_link_retired_agent(NULL, NULL);"),
        format!("{PRE}DO $d$ BEGIN PERFORM public.epigraph_operates_agents(NULL); END $d$;"),
        format!(
            "{PRE}CREATE FUNCTION public.episcience_q() RETURNS void LANGUAGE plpgsql \
             SET search_path = public, pg_temp AS $f$ BEGIN PERFORM epigraph_root_require_tenancy(); END $f$;"
        ),
        format!("{PRE}SELECT to_regprocedure('public.epigraph_propagate_tenancy()');"),
        format!("{PRE}SELECT current_setting('epigraph.principal_id', true);"),
    ] {
        fires(&sql, "not_in_contract");
    }
    for name in CONTRACT_NAMES {
        let ok = format!("{PRE}SELECT to_regprocedure('public.{name}()');");
        assert!(!rules(5099, &ok).contains(&"not_in_contract"), "{name}");
    }
}

/// `mask_strings` keeps byte offsets and blanks only literal contents; the
/// dynamic-SQL reader ignores words inside literals. Kills: a masker that
/// shifts offsets (EXECUTE targets would be read from the wrong place).
#[test]
fn masking_keeps_offsets_and_ignores_words_in_literals() {
    let s = "PERFORM has_function_privilege('a', 'EXECUTE'); EXECUTE 'SELECT ''é''' ;";
    let m = mask_strings(s);
    assert_eq!(m.len(), s.len());
    assert!(!m.contains("'EXECUTE'"));
    assert_eq!(dynamic_sql(s), vec!["SELECT 'é'".to_string()]);
}
