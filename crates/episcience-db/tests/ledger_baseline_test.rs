//! T-S3: the 5032 baseline, its committed fingerprint, and `adopt-baseline`.
mod support;
use support::TestDb;

use episcience_db::ledger::{self, AdoptOutcome, LedgerError};
use sqlx::Connection;

/// A "legacy" database: kernel schema plus the 14 tables, built WITHOUT the
/// ledger (as the hand-applied files built them).
async fn legacy_db() -> TestDb {
    let db = TestDb::fresh_kernel_only().await;
    let baseline = ledger::MIGRATOR
        .iter()
        .find(|m| m.version == ledger::BASELINE_VERSION)
        .expect("5032 embedded");
    sqlx::raw_sql(&baseline.sql)
        .execute(&db.admin)
        .await
        .expect("apply the 5032 DDL by hand");
    db
}

async fn ledger_table_rows(pool: &sqlx::PgPool) -> Vec<(i64, bool, Vec<u8>)> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('episcience_meta._sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await
            .expect("regclass");
    if !exists {
        return Vec::new();
    }
    sqlx::query_as(
        "SELECT version, success, checksum FROM episcience_meta._sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .expect("ledger rows")
}

/// The committed fingerprint is exactly what 5032 builds on the pinned
/// kernel schema. Kills: editing the baseline DDL without regenerating the
/// fingerprint (or the reverse).
#[tokio::test]
async fn template_matches_the_committed_fingerprint() {
    let db = TestDb::fresh().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    let live = ledger::fingerprint(&mut conn).await.expect("fingerprint");
    let diff = ledger::fingerprint_diff(&ledger::expected_fingerprint(), &live);
    assert!(diff.is_empty(), "fingerprint drift:\n{}", diff.join("\n"));
}

/// The fingerprint renders the same whatever the caller's `search_path` is
/// (the adopt path runs with `episcience_meta`, a psql session with `public`).
/// Kills: dropping the `SET LOCAL search_path TO pg_catalog` pin, which would
/// render `public.vector(1536)` as `vector(1536)` on a `public` session.
#[tokio::test]
async fn fingerprint_is_independent_of_the_session_search_path() {
    let db = TestDb::fresh().await;
    let mut meta = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    let a = ledger::fingerprint(&mut meta).await.expect("fp meta");
    let mut plain = sqlx::PgConnection::connect_with(&db.admin_options())
        .await
        .expect("plain connect");
    let b = ledger::fingerprint(&mut plain).await.expect("fp public");
    assert_eq!(a, b);
    assert!(a.iter().any(|l| l.contains("public.vector(1536)")));
}

/// Adopting a matching legacy database records 5032 with the embedded file's
/// checksum, without running it; a second adopt is a no-op; `run` afterwards
/// applies nothing. Kills: recording a different checksum (sqlx's migrator
/// would then report the applied migration as modified), or re-running DDL.
#[tokio::test]
async fn adopt_baseline_records_5032_on_a_matching_legacy_database() {
    let db = legacy_db().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    assert_eq!(
        ledger::adopt_baseline(&mut conn).await.expect("adopt"),
        AdoptOutcome::Adopted
    );
    let want = ledger::MIGRATOR
        .iter()
        .find(|m| m.version == ledger::BASELINE_VERSION)
        .unwrap()
        .checksum
        .to_vec();
    assert_eq!(
        ledger_table_rows(&db.admin).await,
        vec![(ledger::BASELINE_VERSION, true, want)]
    );
    assert_eq!(
        ledger::adopt_baseline(&mut conn)
            .await
            .expect("adopt again"),
        AdoptOutcome::AlreadyRecorded
    );
    ledger::run(&mut conn)
        .await
        .expect("run after adopt is a no-op");
    assert_eq!(ledger_table_rows(&db.admin).await.len(), 1);
}

/// One mutated column makes adopt-baseline refuse, name the column in its
/// diff, and record nothing. Kills: a comparison that ignores types, or one
/// that records 5032 before comparing.
#[tokio::test]
async fn adopt_baseline_refuses_a_mutated_column_and_records_nothing() {
    let db = legacy_db().await;
    sqlx::query("ALTER TABLE public.samples ALTER COLUMN storage_location TYPE varchar(10)")
        .execute(&db.admin)
        .await
        .expect("mutate one column");
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    match ledger::adopt_baseline(&mut conn).await {
        Err(LedgerError::Refused(msg)) => {
            assert!(
                msg.contains("- column samples.storage_location #9 text nullable default=-"),
                "{msg}"
            );
            assert!(
                msg.contains("+ column samples.storage_location #9 character varying(10)"),
                "{msg}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(ledger_table_rows(&db.admin).await.is_empty());
}

/// `run` refuses a legacy database whose ledger is empty (it must be adopted,
/// never re-created). Kills: a `run` that would try (and, with IF NOT EXISTS
/// DDL, silently succeed) to record 5032 over tables it did not build.
#[tokio::test]
async fn run_refuses_a_legacy_database_without_a_ledger() {
    let db = legacy_db().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    match ledger::run(&mut conn).await {
        Err(LedgerError::Refused(msg)) => assert!(msg.contains("adopt-baseline"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(ledger_table_rows(&db.admin).await.is_empty());
}

/// adopt-baseline on a database with no EpiScience table refuses (there is
/// nothing to adopt; `run` builds it).
#[tokio::test]
async fn adopt_baseline_refuses_an_empty_database() {
    let db = TestDb::fresh_kernel_only().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    match ledger::adopt_baseline(&mut conn).await {
        Err(LedgerError::Refused(msg)) => assert!(msg.contains("nothing to adopt"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
