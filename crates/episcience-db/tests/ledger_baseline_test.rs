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

/// The fingerprint renders the same whatever the caller's `search_path` is:
/// the adopt path runs with `episcience_meta` alone, a read-only runner with
/// the default path, and both must produce the committed lines. The inlined
/// query that `episcience-migrate fingerprint-sql` prints, run as a plain
/// simple-protocol statement, produces the same lines too. Kills: removing
/// the `public.` strip (the `episcience_meta` session would then render
/// `public.vector(1536)` and `REFERENCES public.syntheses(id)` while a default
/// session renders them bare), or an inlined query that drifts from the bound
/// one.
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
    let b = ledger::fingerprint(&mut plain)
        .await
        .expect("fp default path");
    let mut catalog_only = sqlx::PgConnection::connect_with(
        &db.admin_options().options([("search_path", "pg_catalog")]),
    )
    .await
    .expect("pg_catalog connect");
    let c = ledger::fingerprint(&mut catalog_only)
        .await
        .expect("fp pg_catalog path");
    assert_eq!(a, b, "episcience_meta vs default search_path");
    assert_eq!(a, c, "episcience_meta vs pg_catalog-only search_path");

    let rows = sqlx::raw_sql(&ledger::fingerprint_sql_inline())
        .fetch_all(&mut plain)
        .await
        .expect("inlined fingerprint query");
    let mut d: Vec<String> = rows
        .iter()
        .map(|r| sqlx::Row::get::<String, _>(r, 0))
        .collect();
    d.sort();
    assert_eq!(a, d, "the printed (inlined) query must equal the bound one");

    assert!(
        a.iter().any(|l| l.contains(" vector(1536) ")),
        "vector column"
    );
    assert!(
        a.iter().all(|l| !l.contains("public.")),
        "no line may carry a search_path-dependent qualifier"
    );
}

/// Adopting a matching legacy database records 5032 with the embedded file's
/// checksum, without running it; a second adopt is a no-op; `run` afterwards
/// applies exactly the later versions (the deployed path: adopt, then run
/// 5033 on the legacy tables), and a second `run` applies nothing. Kills:
/// recording a different checksum (sqlx's migrator would then report the
/// applied migration as modified), re-running 5032's DDL, or a 5033 that
/// cannot run on an adopted legacy database.
#[tokio::test]
async fn adopt_baseline_records_5032_and_run_then_applies_the_later_versions() {
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
        .expect("run after adopt applies the later versions");
    let expected: Vec<(i64, bool, Vec<u8>)> = ledger::MIGRATOR
        .iter()
        .map(|m| (m.version, true, m.checksum.to_vec()))
        .collect();
    assert!(expected.len() >= 2, "5033 must be embedded");
    assert_eq!(ledger_table_rows(&db.admin).await, expected);
    ledger::run(&mut conn)
        .await
        .expect("a second run is a no-op");
    assert_eq!(ledger_table_rows(&db.admin).await, expected);
    ledger::verify(&mut conn)
        .await
        .expect("verify passes on the adopted-then-migrated database");
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

/// Apply `mutation` to a fresh legacy database, then expect adopt-baseline to
/// refuse with a diff of EXACTLY two lines, the committed (`- `) and the live
/// (`+ `) definition of `object` (so the refusal is caused by that one
/// definition and by nothing else), and record nothing.
async fn assert_adopt_refuses_after(mutation: &str, object: &str) -> (String, String) {
    let db = legacy_db().await;
    sqlx::raw_sql(mutation)
        .execute(&db.admin)
        .await
        .expect("apply the mutation");
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    let lines = match ledger::adopt_baseline(&mut conn).await {
        Err(LedgerError::Refused(msg)) => {
            let diff: Vec<&str> = msg
                .lines()
                .filter(|l| l.starts_with("- ") || l.starts_with("+ "))
                .collect();
            assert_eq!(diff.len(), 2, "{msg}");
            assert!(diff[0].starts_with(&format!("- {object} ")), "{msg}");
            assert!(diff[1].starts_with(&format!("+ {object} ")), "{msg}");
            (diff[0][2..].to_string(), diff[1][2..].to_string())
        }
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(ledger_table_rows(&db.admin).await.is_empty());
    lines
}

/// [`assert_adopt_refuses_after`], and the live line must equal the committed
/// line with `committed` replaced by `live` exactly once, so the refusal is
/// caused by that one rendered flag and by no other part of the definition.
async fn assert_adopt_refuses_on_flag(mutation: &str, object: &str, committed: &str, live: &str) {
    let (expected_line, live_line) = assert_adopt_refuses_after(mutation, object).await;
    assert!(
        expected_line.contains(committed),
        "committed line {expected_line:?} lacks {committed:?}"
    );
    assert_eq!(live_line, expected_line.replacen(committed, live, 1));
}

/// A CHECK constraint with the same name but a different body is refused.
/// Kills: a fingerprint that compares constraint names and kinds only.
#[tokio::test]
async fn adopt_baseline_refuses_a_changed_check_body() {
    assert_adopt_refuses_after(
        "ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_status_check, \
         ADD CONSTRAINT syntheses_status_check CHECK (status IS NOT NULL);",
        "constraint syntheses.syntheses_status_check",
    )
    .await;
}

/// A foreign key with the same name and target but a different ON DELETE
/// action is refused. Kills: a fingerprint that ignores foreign-key actions.
#[tokio::test]
async fn adopt_baseline_refuses_a_changed_foreign_key_action() {
    assert_adopt_refuses_after(
        "ALTER TABLE public.synthesis_jobs DROP CONSTRAINT synthesis_jobs_id_fkey, \
         ADD CONSTRAINT synthesis_jobs_id_fkey FOREIGN KEY (id) REFERENCES public.syntheses(id);",
        "constraint synthesis_jobs.synthesis_jobs_id_fkey",
    )
    .await;
}

/// An index with the same name but a different method and column is refused.
/// Kills: a fingerprint that compares index names only.
#[tokio::test]
async fn adopt_baseline_refuses_a_changed_index_definition() {
    assert_adopt_refuses_after(
        "DROP INDEX public.synthesis_embeddings_hnsw_idx; \
         CREATE INDEX synthesis_embeddings_hnsw_idx ON public.synthesis_embeddings (created_at);",
        "index synthesis_embeddings.synthesis_embeddings_hnsw_idx",
    )
    .await;
}

/// A trigger with the same name but a different timing and event is refused.
/// Kills: a fingerprint that compares trigger names only.
#[tokio::test]
async fn adopt_baseline_refuses_a_changed_trigger_timing() {
    assert_adopt_refuses_after(
        "DROP TRIGGER samples_updated_at ON public.samples; \
         CREATE TRIGGER samples_updated_at AFTER INSERT ON public.samples \
         FOR EACH ROW EXECUTE FUNCTION public.update_updated_at_column();",
        "trigger samples.samples_updated_at",
    )
    .await;
}

/// A DISABLED trigger with an unchanged definition is refused
/// (`pg_get_triggerdef` renders it exactly like an enabled one). Kills: a
/// trigger line without `tgenabled`.
#[tokio::test]
async fn adopt_baseline_refuses_a_disabled_trigger() {
    assert_adopt_refuses_on_flag(
        "ALTER TABLE public.samples DISABLE TRIGGER samples_updated_at;",
        "trigger samples.samples_updated_at",
        " enabled=O ",
        " enabled=D ",
    )
    .await;
}

/// A trigger switched to fire only on replicas (so it never fires in a normal
/// session) is refused. Kills: a trigger line that renders only
/// enabled/disabled.
#[tokio::test]
async fn adopt_baseline_refuses_a_replica_only_trigger() {
    assert_adopt_refuses_on_flag(
        "ALTER TABLE public.samples ENABLE REPLICA TRIGGER samples_updated_at;",
        "trigger samples.samples_updated_at",
        " enabled=O ",
        " enabled=R ",
    )
    .await;
}

/// An INVALID index (the state a failed `CREATE INDEX CONCURRENTLY` leaves)
/// with an unchanged definition is refused. Kills: an index line without
/// `indisvalid`.
#[tokio::test]
async fn adopt_baseline_refuses_an_invalid_index() {
    assert_adopt_refuses_on_flag(
        "UPDATE pg_catalog.pg_index SET indisvalid = false \
         WHERE indexrelid = 'public.synthesis_embeddings_hnsw_idx'::regclass;",
        "index synthesis_embeddings.synthesis_embeddings_hnsw_idx",
        " valid ready ",
        " INVALID ready ",
    )
    .await;
}

/// An index that is not ready for inserts is refused. Kills: an index line
/// without `indisready`.
#[tokio::test]
async fn adopt_baseline_refuses_an_index_that_is_not_ready() {
    assert_adopt_refuses_on_flag(
        "UPDATE pg_catalog.pg_index SET indisready = false \
         WHERE indexrelid = 'public.idx_samples_status'::regclass;",
        "index samples.idx_samples_status",
        " valid ready ",
        " valid NOT-READY ",
    )
    .await;
}

/// A column with a non-default collation (same type) is refused. Kills: a
/// column line without the collation.
#[tokio::test]
async fn adopt_baseline_refuses_a_column_with_another_collation() {
    assert_adopt_refuses_on_flag(
        "ALTER TABLE public.samples ALTER COLUMN name TYPE text COLLATE \"C\";",
        "column samples.name",
        " default=-",
        " default=- collate=C",
    )
    .await;
}

/// An UNLOGGED table is refused (its rows do not survive a crash and are not
/// replicated). Kills: a table line without `relpersistence`.
#[tokio::test]
async fn adopt_baseline_refuses_an_unlogged_table() {
    assert_adopt_refuses_on_flag(
        "ALTER TABLE public.synthesis_staleness_events SET UNLOGGED;",
        "table synthesis_staleness_events",
        " persistence=p",
        " persistence=u",
    )
    .await;
}

/// A plain column turned into an identity column is refused. Kills: a column
/// line without `attidentity` (an identity column has no `pg_attrdef` row, so
/// its default renders as `-`, like the plain column's).
#[tokio::test]
async fn adopt_baseline_refuses_an_identity_column() {
    assert_adopt_refuses_on_flag(
        "ALTER TABLE public.synthesis_clusters ALTER COLUMN cluster_index \
         ADD GENERATED BY DEFAULT AS IDENTITY;",
        "column synthesis_clusters.cluster_index",
        " default=-",
        " default=- identity=d",
    )
    .await;
}

/// A STORED generated column whose expression equals the committed column's
/// DEFAULT is refused (both live in `pg_attrdef` and render the same). The
/// last column is re-created, so every other column keeps its position.
/// Kills: a column line without `attgenerated`.
#[tokio::test]
async fn adopt_baseline_refuses_a_generated_column_that_renders_like_a_default() {
    assert_adopt_refuses_on_flag(
        "ALTER TABLE public.countersignatures DROP COLUMN signature_version; \
         ALTER TABLE public.countersignatures ADD COLUMN signature_version smallint \
         NOT NULL GENERATED ALWAYS AS (1) STORED;",
        "column countersignatures.signature_version",
        " default=1",
        " default=1 generated=s",
    )
    .await;
}

/// An EpiScience-range version in the KERNEL ledger makes adopt refuse before
/// it creates anything: no `episcience_meta` schema, no ledger row. Kills:
/// deleting the foreign-version guard in `adopt_baseline` (the matching
/// legacy tables would then be adopted over a kernel ledger the kernel's own
/// migrator refuses).
#[tokio::test]
async fn adopt_baseline_refuses_a_foreign_version_in_the_kernel_ledger() {
    let db = legacy_db().await;
    sqlx::query(
        "INSERT INTO public._sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (5032, 'planted', TRUE, '\\x00'::bytea, 0)",
    )
    .execute(&db.admin)
    .await
    .expect("plant a foreign version");
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    match ledger::adopt_baseline(&mut conn).await {
        Err(LedgerError::Refused(msg)) => {
            assert!(msg.contains("kernel ledger"), "{msg}");
            assert!(msg.contains("5032"), "{msg}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let schema: bool = sqlx::query_scalar("SELECT to_regnamespace('episcience_meta') IS NOT NULL")
        .fetch_one(&db.admin)
        .await
        .expect("schema check");
    assert!(
        !schema,
        "adopt must refuse before creating the ledger schema"
    );
}

/// A ledger that already holds 5032 with a DIFFERENT checksum is refused, not
/// reported as already recorded, and the row is left as it was. Kills:
/// dropping the checksum comparison in `adopt_locked` (a 5032 row recorded
/// from another file would be accepted, and sqlx would later refuse `run`
/// with a modified-migration error on any database adopted that way).
#[tokio::test]
async fn adopt_baseline_refuses_a_recorded_baseline_with_another_checksum() {
    use sqlx::migrate::Migrate;
    let db = legacy_db().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    ledger::prepare_ledger_schema(&mut conn)
        .await
        .expect("ledger schema");
    conn.ensure_migrations_table()
        .await
        .expect("sqlx ledger table");
    sqlx::query(
        "INSERT INTO episcience_meta._sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (5032, 'legacy baseline', TRUE, '\\x00'::bytea, 0)",
    )
    .execute(&mut conn)
    .await
    .expect("plant a 5032 row with a wrong checksum");
    match ledger::adopt_baseline(&mut conn).await {
        Err(LedgerError::Refused(msg)) => assert!(msg.contains("not empty"), "{msg}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        ledger_table_rows(&db.admin).await,
        vec![(ledger::BASELINE_VERSION, true, vec![0u8])]
    );
}
