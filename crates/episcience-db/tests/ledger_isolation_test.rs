//! T-S2 (ratchet R9): EpiScience's ledger is isolated from the kernel's.
//!
//! `episcience-migrate run` on a database whose kernel schema was built by the
//! kernel's own `epigraph-migrate` must (a) add NO row to
//! `public._sqlx_migrations`, (b) record its versions in
//! `episcience_meta._sqlx_migrations`, and (c) leave a database the kernel's
//! migrator still accepts (it refuses one holding versions it does not embed).
mod support;
use support::TestDb;

use episcience_db::ledger;

async fn kernel_versions(pool: &sqlx::PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM public._sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("kernel ledger")
}

fn epigraph_migrate(url: &str) -> std::process::ExitStatus {
    let bin = std::env::var("E1_EPIGRAPH_MIGRATE_BIN")
        .expect("E1_EPIGRAPH_MIGRATE_BIN (exported by scripts/e1-test-db.sh)");
    std::process::Command::new(bin)
        .env_remove("DATABASE_URL")
        .env("MIGRATION_DATABASE_URL", url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn epigraph-migrate")
}

/// Kills: a ledger connection that loses `search_path = episcience_meta`
/// (sqlx would then create/insert `_sqlx_migrations` in `public`, the
/// kernel's ledger), and any EpiScience migration that writes the kernel
/// ledger itself.
#[tokio::test]
async fn run_records_only_in_episcience_meta_and_the_kernel_migrator_still_accepts() {
    let db = TestDb::fresh_kernel_only().await;
    let before = kernel_versions(&db.admin).await;
    assert!(
        before.iter().all(|v| *v < ledger::FOREIGN_VERSION_FLOOR),
        "a kernel-only template holds kernel versions only"
    );
    assert!(
        !before.is_empty(),
        "the kernel ledger must exist for this test to mean anything"
    );

    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    ledger::run(&mut conn)
        .await
        .expect("episcience-migrate run");

    assert_eq!(
        kernel_versions(&db.admin).await,
        before,
        "episcience-migrate added or removed a row in public._sqlx_migrations"
    );
    let ours: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT version, success FROM episcience_meta._sqlx_migrations ORDER BY version",
    )
    .fetch_all(&db.admin)
    .await
    .expect("episcience ledger");
    assert_eq!(ours, vec![(ledger::BASELINE_VERSION, true)]);

    let public_can_use: bool =
        sqlx::query_scalar("SELECT has_schema_privilege('public', 'episcience_meta', 'USAGE')")
            .fetch_one(&db.admin)
            .await
            .expect("schema acl");
    assert!(
        !public_can_use,
        "PUBLIC must hold nothing on episcience_meta"
    );

    drop(conn);
    assert!(
        epigraph_migrate(&db.url()).success(),
        "the kernel's epigraph-migrate must still accept the database"
    );
}

/// Negative control for the check above: the kernel migrator really does
/// refuse a database whose kernel ledger holds an EpiScience-range version, so
/// "it still exits 0" is evidence, not a tautology.
#[tokio::test]
async fn kernel_migrator_refuses_a_foreign_version_in_its_ledger() {
    let db = TestDb::fresh_kernel_only().await;
    sqlx::query(
        "INSERT INTO public._sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (5032, 'foreign', true, '\\x00'::bytea, 0)",
    )
    .execute(&db.admin)
    .await
    .expect("plant a foreign version");
    assert!(
        !epigraph_migrate(&db.url()).success(),
        "epigraph-migrate must refuse a database holding a version it does not embed"
    );
}
