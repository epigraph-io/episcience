//! EpiScience's own migration ledger.
//!
//! EpiScience's schema sits on top of the kernel's. The kernel's migrator
//! (`epigraph-migrate`) refuses a database whose `public._sqlx_migrations`
//! holds a version it does not embed, so EpiScience must NEVER record a
//! version there. Its ledger lives in the schema [`LEDGER_SCHEMA`]
//! (`episcience_meta`): sqlx's migrator names its table unqualified
//! (`_sqlx_migrations`), and every connection this module opens puts
//! `episcience_meta` first (and alone) on `search_path`, so the ledger resolves
//! there. Every object an EpiScience migration creates is `public.`-qualified
//! for the same reason.
//!
//! Versions start at [`BASELINE_VERSION`] (5032, the consolidated legacy
//! schema). A database that already carries the legacy tables (built by the
//! hand-applied files under `migrations/legacy/`) is ADOPTED: its live
//! tables are fingerprinted, compared with the committed
//! `migrations/5032_legacy_baseline.fingerprint`, and on an exact match the
//! 5032 row is recorded with sqlx's checksum of the embedded file, without
//! running it.

use std::collections::BTreeSet;
use std::str::FromStr;

use sqlx::migrate::{Migrate, Migrator};
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{Connection, Row};

/// The embedded EpiScience migrations: the TOP LEVEL of `migrations/` only.
/// sqlx's resolver does not descend into `migrations/legacy/`, which is
/// history and is run by nothing.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// Schema that holds EpiScience's `_sqlx_migrations`.
pub const LEDGER_SCHEMA: &str = "episcience_meta";

/// The consolidated legacy baseline.
pub const BASELINE_VERSION: i64 = 5032;

/// Every version >= this in the KERNEL ledger would be an EpiScience version
/// in the wrong ledger (the kernel's own versions are far below it).
pub const FOREIGN_VERSION_FLOOR: i64 = 5000;

/// The environment variable the migrator reads. It never falls back to
/// `DATABASE_URL`.
pub const MIGRATION_URL_VAR: &str = "EPISCIENCE_MIGRATION_DATABASE_URL";

/// The 14 tables EpiScience owns (the kernel owns `experiments` and
/// `experiment_results`, which legacy file 001 also created).
pub const EPISCIENCE_TABLES: [&str; 14] = [
    "blobs",
    "countersignatures",
    "episcience_worker_state",
    "protocols",
    "sample_claims",
    "samples",
    "syntheses",
    "synthesis_claim_membership",
    "synthesis_clusters",
    "synthesis_embeddings",
    "synthesis_jobs",
    "synthesis_provo_edges",
    "synthesis_shares",
    "synthesis_staleness_events",
];

/// The committed fingerprint of the 14 tables as 5032 creates them.
pub const BASELINE_FINGERPRINT: &str =
    include_str!("../../../migrations/5032_legacy_baseline.fingerprint");

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migrate: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("{0}")]
    Refused(String),
}

/// Connect options for the migrator: the given URL with
/// `search_path = episcience_meta` forced as a startup option.
pub fn connect_options(url: &str) -> Result<PgConnectOptions, LedgerError> {
    let opts = PgConnectOptions::from_str(url)?;
    Ok(opts.options([("search_path", LEDGER_SCHEMA)]))
}

/// Connect for ledger work. Refuses a session whose effective `search_path`
/// does not START with the ledger schema (an `options` override in the URL
/// could otherwise put `public` first and write the kernel's ledger).
pub async fn connect(url: &str) -> Result<PgConnection, LedgerError> {
    connect_with(PgConnectOptions::from_str(url)?).await
}

/// [`connect`] from parsed options; `search_path = episcience_meta` is forced.
pub async fn connect_with(opts: PgConnectOptions) -> Result<PgConnection, LedgerError> {
    let opts = opts.options([("search_path", LEDGER_SCHEMA)]);
    let mut conn = PgConnection::connect_with(&opts).await?;
    let sp: String = sqlx::query_scalar("SELECT current_setting('search_path')")
        .fetch_one(&mut conn)
        .await?;
    let first = sp.split(',').next().unwrap_or("").trim().trim_matches('"');
    if first != LEDGER_SCHEMA {
        return Err(LedgerError::Refused(format!(
            "refusing: search_path must start with {LEDGER_SCHEMA}, got {sp:?}"
        )));
    }
    Ok(conn)
}

/// Create the ledger schema (idempotent) and take it away from PUBLIC.
pub async fn prepare_ledger_schema(conn: &mut PgConnection) -> Result<(), LedgerError> {
    sqlx::query("CREATE SCHEMA IF NOT EXISTS episcience_meta")
        .execute(&mut *conn)
        .await?;
    sqlx::query("REVOKE ALL ON SCHEMA episcience_meta FROM PUBLIC")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Kernel-ledger versions >= [`FOREIGN_VERSION_FLOOR`] (must always be empty).
pub async fn foreign_versions_in_kernel_ledger(
    conn: &mut PgConnection,
) -> Result<Vec<i64>, LedgerError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(Vec::new());
    }
    let v: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM public._sqlx_migrations WHERE version >= $1 ORDER BY version",
    )
    .bind(FOREIGN_VERSION_FLOOR)
    .fetch_all(&mut *conn)
    .await?;
    Ok(v)
}

/// `(version, success)` rows of EpiScience's ledger, or empty if it does not
/// exist yet.
pub async fn ledger_rows(conn: &mut PgConnection) -> Result<Vec<(i64, bool)>, LedgerError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('episcience_meta._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
    if !exists {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT version, success FROM episcience_meta._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<i64, _>(0), r.get::<bool, _>(1)))
        .collect())
}

/// The canonical fingerprint lines of the 14 tables, sorted bytewise.
///
/// One line per table (relkind and persistence, so an UNLOGGED table differs),
/// column (position among NON-dropped columns, type, nullability, default,
/// and, only when present, a collation other than the type's default, the
/// identity kind and the generated kind), constraint (name, kind and the full
/// `pg_get_constraintdef`, so a CHECK body or a foreign key's referenced
/// table and ON DELETE / ON UPDATE actions are compared), index (valid and
/// ready flags, then the full `pg_get_indexdef`: method, columns, operator
/// classes, predicate) and non-internal trigger (its enabled state, then the
/// full `pg_get_triggerdef`: timing, events, level, function). The flags are
/// there because `pg_get_indexdef` renders an INVALID index (one left by a
/// failed `CREATE INDEX CONCURRENTLY`) exactly like a valid one,
/// `pg_get_triggerdef` omits whether a trigger is disabled, and `format_type`
/// omits the collation. NOT NULL constraints are carried on the column line
/// (Postgres 16 keeps them out of `pg_constraint`).
///
/// The rendering does not depend on the session's `search_path`:
/// [`FINGERPRINT_SQL`] strips every `public.` qualifier, which `format_type`
/// and the `pg_get_*def` functions add exactly when `public` is not on the
/// path. The adopt path (`search_path = episcience_meta`), a default session
/// and a read-only runner given [`fingerprint_sql_inline`] therefore produce
/// the same lines.
pub async fn fingerprint(conn: &mut PgConnection) -> Result<Vec<String>, LedgerError> {
    let tables: Vec<String> = EPISCIENCE_TABLES.iter().map(|s| s.to_string()).collect();
    let mut set: Vec<String> = sqlx::query_scalar(FINGERPRINT_SQL)
        .bind(&tables)
        .fetch_all(&mut *conn)
        .await?;
    set.sort();
    Ok(set)
}

/// [`FINGERPRINT_SQL`] with the 14 table names inlined in place of `$1`: one
/// self-contained SELECT that a read-only SQL runner can execute and whose
/// output lines (sorted bytewise) equal [`fingerprint`]'s. Printed by
/// `episcience-migrate fingerprint-sql`, so a pre-deploy comparison runs the
/// exact query that `adopt-baseline` runs.
#[must_use]
pub fn fingerprint_sql_inline() -> String {
    let names: Vec<String> = EPISCIENCE_TABLES.iter().map(|t| format!("'{t}'")).collect();
    FINGERPRINT_SQL.replace(
        "$1::text[]",
        &format!("ARRAY[{}]::text[]", names.join(", ")),
    )
}

/// The fingerprint query. `$1` = the table names. Every function is
/// `pg_catalog.`-qualified; `\mpublic\.` is removed from every line (see
/// [`fingerprint`]).
pub const FINGERPRINT_SQL: &str = r#"
WITH rel AS (
    SELECT c.oid, c.relname, c.relkind, c.relpersistence
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND c.relkind IN ('r', 'p')
       AND c.relname = ANY ($1::text[])
), cols AS (
    SELECT r.relname, a.attname,
           pg_catalog.row_number() OVER (PARTITION BY r.oid ORDER BY a.attnum) AS pos,
           pg_catalog.format_type(a.atttypid, a.atttypmod) AS typ,
           a.attnotnull,
           pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS def,
           CASE WHEN a.attcollation <> t.typcollation
                THEN ' collate=' || coalesce(co.collname::text, '?') ELSE '' END
           || CASE WHEN a.attidentity <> '' THEN ' identity=' || a.attidentity::text ELSE '' END
           || CASE WHEN a.attgenerated <> '' THEN ' generated=' || a.attgenerated::text ELSE '' END
              AS extra
      FROM rel r
      JOIN pg_catalog.pg_attribute a ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
      JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
      LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation
      LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
), lines AS (
    SELECT 'table ' || relname || ' kind=' || relkind::text
           || ' persistence=' || relpersistence::text AS line
      FROM rel
    UNION ALL
    SELECT 'column ' || relname || '.' || attname || ' #' || pos::text || ' ' || typ
           || CASE WHEN attnotnull THEN ' not-null' ELSE ' nullable' END
           || ' default=' || coalesce(def, '-') || extra
      FROM cols
    UNION ALL
    SELECT 'constraint ' || r.relname || '.' || con.conname || ' ' || con.contype::text
           || ' ' || pg_catalog.pg_get_constraintdef(con.oid)
      FROM rel r JOIN pg_catalog.pg_constraint con ON con.conrelid = r.oid
    UNION ALL
    SELECT 'index ' || r.relname || '.' || ic.relname
           || CASE WHEN i.indisvalid THEN ' valid' ELSE ' INVALID' END
           || CASE WHEN i.indisready THEN ' ready' ELSE ' NOT-READY' END
           || ' ' || pg_catalog.pg_get_indexdef(i.indexrelid)
      FROM rel r
      JOIN pg_catalog.pg_index i ON i.indrelid = r.oid
      JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
    UNION ALL
    SELECT 'trigger ' || r.relname || '.' || tg.tgname
           || ' enabled=' || tg.tgenabled::text
           || ' ' || pg_catalog.pg_get_triggerdef(tg.oid)
      FROM rel r JOIN pg_catalog.pg_trigger tg ON tg.tgrelid = r.oid AND NOT tg.tgisinternal
)
SELECT pg_catalog.regexp_replace(line, '\mpublic\.', '', 'g') FROM lines
"#;

/// The committed fingerprint, comments (`#`) and blank lines removed, sorted.
#[must_use]
pub fn expected_fingerprint() -> Vec<String> {
    parse_fingerprint(BASELINE_FINGERPRINT)
}

/// Parse a fingerprint file body.
#[must_use]
pub fn parse_fingerprint(body: &str) -> Vec<String> {
    let mut v: Vec<String> = body
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// Lines only in `expected` (prefixed `- `) and only in `live` (`+ `).
/// Empty iff the two are equal as sets.
#[must_use]
pub fn fingerprint_diff(expected: &[String], live: &[String]) -> Vec<String> {
    let e: BTreeSet<&String> = expected.iter().collect();
    let l: BTreeSet<&String> = live.iter().collect();
    let mut out: Vec<String> = e.difference(&l).map(|s| format!("- {s}")).collect();
    out.extend(l.difference(&e).map(|s| format!("+ {s}")));
    out
}

/// Outcome of [`adopt_baseline`].
#[derive(Debug, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// The 5032 row was inserted.
    Adopted,
    /// The ledger already records 5032 with the embedded checksum.
    AlreadyRecorded,
}

/// Adopt a legacy database: fingerprint the live 14 tables and, on an exact
/// match with the committed fingerprint, record [`BASELINE_VERSION`] with
/// sqlx's checksum of the embedded 5032 file. Never runs the file.
///
/// # Errors
/// [`LedgerError::Refused`] with the full diff on a mismatch; when the ledger
/// already holds any version other than a matching 5032; when no EpiScience
/// table exists (use `run`); when the kernel ledger holds a foreign version.
pub async fn adopt_baseline(conn: &mut PgConnection) -> Result<AdoptOutcome, LedgerError> {
    let baseline = MIGRATOR
        .iter()
        .find(|m| m.version == BASELINE_VERSION)
        .ok_or_else(|| LedgerError::Refused("5032 is not embedded in this binary".into()))?;

    let foreign = foreign_versions_in_kernel_ledger(conn).await?;
    if !foreign.is_empty() {
        return Err(LedgerError::Refused(format!(
            "the kernel ledger public._sqlx_migrations holds EpiScience-range versions {foreign:?}; \
             reconcile by hand before adopting"
        )));
    }

    prepare_ledger_schema(conn).await?;
    conn.ensure_migrations_table().await?;
    conn.lock().await?;
    let result = adopt_locked(conn, baseline).await;
    conn.unlock().await?;
    result
}

async fn adopt_locked(
    conn: &mut PgConnection,
    baseline: &sqlx::migrate::Migration,
) -> Result<AdoptOutcome, LedgerError> {
    let rows = sqlx::query(
        "SELECT version, success, checksum FROM episcience_meta._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut *conn)
    .await?;
    if let Some(r) = rows.first() {
        let version: i64 = r.get(0);
        let success: bool = r.get(1);
        let checksum: Vec<u8> = r.get(2);
        if rows.len() == 1
            && version == BASELINE_VERSION
            && success
            && checksum.as_slice() == &*baseline.checksum
        {
            return Ok(AdoptOutcome::AlreadyRecorded);
        }
        let versions: Vec<i64> = rows.iter().map(|r| r.get::<i64, _>(0)).collect();
        return Err(LedgerError::Refused(format!(
            "the EpiScience ledger is not empty ({versions:?}); adopt-baseline only records 5032 \
             on a database that has never been migrated by episcience-migrate"
        )));
    }

    let live = fingerprint(conn).await?;
    if !live.iter().any(|l| l.starts_with("table ")) {
        return Err(LedgerError::Refused(
            "no EpiScience table exists here; nothing to adopt (use `episcience-migrate run`)"
                .into(),
        ));
    }
    let diff = fingerprint_diff(&expected_fingerprint(), &live);
    if !diff.is_empty() {
        return Err(LedgerError::Refused(format!(
            "live tables do not match migrations/5032_legacy_baseline.fingerprint \
             ('-' = expected only, '+' = live only); fix the baseline DDL, never the \
             fingerprint:\n{}",
            diff.join("\n")
        )));
    }

    sqlx::query(
        "INSERT INTO episcience_meta._sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES ($1, $2, TRUE, $3, 0)",
    )
    .bind(baseline.version)
    .bind(&*baseline.description)
    .bind(&*baseline.checksum)
    .execute(&mut *conn)
    .await?;
    Ok(AdoptOutcome::Adopted)
}

/// Apply every pending embedded migration into `episcience_meta`, then check
/// that the kernel ledger holds no EpiScience-range version.
///
/// # Errors
/// Refuses before migrating when the 14 tables already exist but the ledger
/// is empty (a legacy database: adopt it instead).
pub async fn run(conn: &mut PgConnection) -> Result<(), LedgerError> {
    prepare_ledger_schema(conn).await?;
    let recorded = ledger_rows(conn).await?;
    if recorded.is_empty() {
        let live = fingerprint(conn).await?;
        if live.iter().any(|l| l.starts_with("table ")) {
            return Err(LedgerError::Refused(
                "EpiScience tables exist but the ledger is empty: this is a legacy database; \
                 run `episcience-migrate adopt-baseline` first"
                    .into(),
            ));
        }
    }
    MIGRATOR.run_direct(conn).await?;
    let foreign = foreign_versions_in_kernel_ledger(conn).await?;
    if !foreign.is_empty() {
        return Err(LedgerError::Refused(format!(
            "after run, the kernel ledger holds EpiScience-range versions {foreign:?}"
        )));
    }
    Ok(())
}

/// The first E1 migration: tenancy contract v1.
pub const CONTRACT_V1_VERSION: i64 = 5033;

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded set is exactly the baseline plus the E1 series so far.
    /// Kills: a migrator pointed at a directory that recurses into
    /// `migrations/legacy/` (which has its own `5032_*.sql`), or a stray
    /// top-level file.
    #[test]
    fn embedded_versions_are_exactly_the_baseline_and_the_contract() {
        let v: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(v, vec![BASELINE_VERSION, CONTRACT_V1_VERSION]);
    }

    /// The committed fingerprint names all 14 tables and nothing else.
    /// Kills: a fingerprint regenerated from a database missing a table.
    #[test]
    fn committed_fingerprint_covers_exactly_the_fourteen_tables() {
        let tables: BTreeSet<String> = expected_fingerprint()
            .into_iter()
            .filter_map(|l| {
                l.strip_prefix("table ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_string)
            })
            .collect();
        let want: BTreeSet<String> = EPISCIENCE_TABLES.iter().map(|s| s.to_string()).collect();
        assert_eq!(tables, want);
    }

    #[test]
    fn diff_reports_both_sides_and_is_empty_on_equal_sets() {
        let e = vec!["a".to_string(), "b".to_string()];
        let l = vec!["b".to_string(), "c".to_string()];
        assert_eq!(fingerprint_diff(&e, &l), vec!["- a", "+ c"]);
        assert!(fingerprint_diff(&e, &e).is_empty());
    }

    #[test]
    fn connect_options_force_the_ledger_search_path() {
        let o = connect_options("postgres://u:p@127.0.0.1:5433/x_test").expect("parse");
        let opts = o.get_options().unwrap_or_default().to_string();
        assert!(opts.contains("search_path=episcience_meta"), "{opts}");
    }
}
