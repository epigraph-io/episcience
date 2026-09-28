//! The row-security rollback pair (docs/runbooks/episcience-rls-undo.sql and
//! episcience-rls-redo.sql), run end to end on a migrated clone.
mod support;
use support::{TestDb, APP_LOGIN};

use episcience_db::{catalog, ledger};
use sqlx::postgres::PgPoolOptions;

const UNDO: &str = include_str!("../../../docs/runbooks/episcience-rls-undo.sql");
const REDO: &str = include_str!("../../../docs/runbooks/episcience-rls-redo.sql");
const M5036: &str = include_str!("../../../migrations/5036_row_security.sql");

fn db_err(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(sqlx::Error::Database(d)) => d.message().to_string(),
        Err(e) => e.to_string(),
    }
}

async fn policy_count(db: &TestDb) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid \
          WHERE c.relnamespace = 'public'::regnamespace AND c.relname = ANY($1)",
    )
    .bind(ledger::EPISCIENCE_TABLES.to_vec())
    .fetch_one(&db.admin)
    .await
    .unwrap()
}

/// Undo turns row security off and puts back the pre-5036 privileges
/// (the kernel app role writes every table again; the EpiScience grantee
/// roles hold nothing), leaves the policies in place, and makes `verify`
/// refuse, naming both; it refuses while an EpiScience login is connected to
/// the database. Redo (5036's privilege and flag sections, verbatim) restores
/// a state `verify` accepts. Kills: an undo that leaves row security on or
/// forced, one that forgets a grant, a redo that forgets FORCE or a grant or
/// drifts from the migration's text, the connected-login guard removed.
#[tokio::test]
async fn the_rls_undo_and_redo_round_trip_to_the_migrated_state() {
    let start = M5036.find("-- ─── 1. Privileges").unwrap();
    let end = M5036.find("-- ─── 3. Policies").unwrap();
    assert!(
        REDO.contains(M5036[start..end].trim_end()),
        "the redo script is 5036's privilege and flag sections verbatim"
    );

    let db = TestDb::fresh().await;
    let policies = policy_count(&db).await;
    assert_eq!(policies, 44);

    let app = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(db.login_options(APP_LOGIN))
        .await
        .unwrap();
    let held = app.acquire().await.unwrap();
    let e = db_err(sqlx::raw_sql(UNDO).execute(&db.admin).await);
    assert!(e.contains("an EpiScience login is connected"), "{e:?}");
    drop(held);
    app.close().await;
    // A closed connection leaves pg_stat_activity asynchronously.
    for _ in 0..100 {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND usename = $1",
        )
        .bind(APP_LOGIN.0)
        .fetch_one(&db.admin)
        .await
        .unwrap();
        if n == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert_eq!(db_err(sqlx::raw_sql(UNDO).execute(&db.admin).await), "");
    let flags: Vec<(bool, bool)> = sqlx::query_as(
        "SELECT relrowsecurity, relforcerowsecurity FROM pg_class \
          WHERE relnamespace = 'public'::regnamespace AND relname = ANY($1)",
    )
    .bind(ledger::EPISCIENCE_TABLES.to_vec())
    .fetch_all(&db.admin)
    .await
    .unwrap();
    assert_eq!(flags.len(), 14);
    assert!(flags.iter().all(|f| *f == (false, false)), "{flags:?}");
    for t in ledger::EPISCIENCE_TABLES {
        for p in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
            let (app_has, rw_has): (bool, bool) = sqlx::query_as(
                "SELECT has_table_privilege('epigraph_app', $1, $2), \
                        has_table_privilege('episcience_rw', $1, $2)",
            )
            .bind(format!("public.{t}"))
            .bind(p)
            .fetch_one(&db.admin)
            .await
            .unwrap();
            assert!(app_has && !rw_has, "{t} {p}: app {app_has}, rw {rw_has}");
        }
    }
    assert_eq!(policy_count(&db).await, policies, "the policies stay");
    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("verify refuses while row security is off")
        .to_string();
    assert!(
        e.contains("rls: row security is not enabled on syntheses")
            && e.contains("grants: epigraph_app holds"),
        "{e}"
    );

    assert_eq!(db_err(sqlx::raw_sql(REDO).execute(&db.admin).await), "");
    assert_eq!(
        catalog::findings(&mut conn).await.unwrap(),
        Vec::<String>::new()
    );
    ledger::verify(&mut conn)
        .await
        .expect("verify passes again");
}

/// Redo restores grants and flags only: it refuses when a 5036 policy is
/// missing (the database needs a real repair, not this script). Kills: the
/// policy-count guard removed (redo would report success on a database whose
/// row security admits nothing, or everything).
#[tokio::test]
async fn the_rls_redo_refuses_a_database_missing_a_policy() {
    let db = TestDb::fresh().await;
    sqlx::raw_sql("DROP POLICY samples_delete_owner ON public.samples")
        .execute(&db.admin)
        .await
        .unwrap();
    let e = db_err(sqlx::raw_sql(REDO).execute(&db.admin).await);
    assert!(e.contains("the 5036 policies are not all present"), "{e:?}");
}
