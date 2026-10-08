//! T-H2 (batch E1h): migration 5040 detaches the legacy
//! `edges_shared_evidence` trigger from the kernel's `edges` table, and its
//! compensating undo (`docs/runbooks/5040-undo.sql`) puts it back.
//!
//! A database built from the 5032 baseline never had the trigger, so "an
//! application-role edge insert succeeds after 5040" alone would pass with an
//! EMPTY 5040. The test therefore first gives a template clone the legacy
//! trigger (through the undo script, which recreates the last legacy
//! definition and un-records 5040), proves the trigger acts on an
//! application-role edge insert (it derives a `shared_evidence` factor),
//! re-applies 5040 with `episcience-migrate`'s own `ledger::run`, and proves
//! it no longer does.
//!
//! Later migrations (5041 on) are recorded on every fresh database, and the
//! 5040 undo refuses while any is (asserted first). The test peels them off
//! first with their own runbooks, newest first, as an operator would (see
//! [`undo_later_versions`]); `ledger::run` then re-applies 5040 and them.
//!
//! Run through `scripts/e1-test-db.sh <batch> -- cargo test --test shared_evidence_detach_test`.

mod support;

use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use episcience_db::ledger;
use support::{principal, viewer_of, Principal, TestDb, WORKER_LOGIN};
use uuid::Uuid;

const UNDO_5040: &str = include_str!("../../../docs/runbooks/5040-undo.sql");
const UNDO_5041: &str = include_str!("../../../docs/runbooks/5041-undo.sql");
const UNDO_5042: &str = include_str!("../../../docs/runbooks/5042-undo.sql");

fn db_err(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(sqlx::Error::Database(d)) => d.message().to_string(),
        Err(e) => e.to_string(),
    }
}

/// `(trigger on edges, function)` presence, read on the admin pool.
async fn legacy_objects(db: &TestDb) -> (bool, bool) {
    sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pg_trigger WHERE tgrelid = 'public.edges'::regclass \
                          AND tgname = 'edges_shared_evidence'),
                to_regprocedure('public.create_shared_evidence_factor()') IS NOT NULL",
    )
    .fetch_one(&db.admin)
    .await
    .expect("catalog read")
}

async fn ledger_versions(db: &TestDb) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM episcience_meta._sqlx_migrations ORDER BY version")
        .fetch_all(&db.admin)
        .await
        .expect("ledger")
}

/// The highest version this binary embeds (what a fresh database records).
fn head_version() -> i64 {
    ledger::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("embedded migrations")
}

/// Undoes every EpiScience migration above 5040 with its own runbook, newest
/// first: the documented order, since the 5040 undo refuses while any later
/// version is recorded. A migration added after 5042 puts its runbook first
/// here; the final assertion fails until it does.
async fn undo_later_versions(db: &TestDb) {
    assert_eq!(
        db_err(sqlx::raw_sql(UNDO_5042).execute(&db.admin).await),
        "",
        "5042-undo"
    );
    assert_eq!(
        db_err(sqlx::raw_sql(UNDO_5041).execute(&db.admin).await),
        "",
        "5041-undo"
    );
    assert_eq!(
        *ledger_versions(db).await.last().unwrap(),
        5040,
        "a version above 5040 is still recorded: undo it here first, with its runbook"
    );
}

/// `shared_evidence` factors naming claim `c` (the only factor type the
/// legacy trigger writes; the kernel's own `edges_auto_factor` trigger writes
/// others).
async fn shared_evidence_factors(db: &TestDb, c: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM public.factors \
          WHERE factor_type = 'shared_evidence' AND $1 = ANY (variable_ids)",
    )
    .bind(c)
    .fetch_one(&db.admin)
    .await
    .expect("factor count")
}

/// A kernel `analyses` row authored by `p` (fixture, admin pool: the kernel
/// table has no repository EpiScience may use and no row security).
async fn analysis(db: &TestDb, p: &Principal) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO public.analyses (analysis_type, method_description, agent_id) \
         VALUES ('fixture', 'shared-evidence detach fixture', $1) RETURNING id",
    )
    .bind(p.agent)
    .fetch_one(&db.admin)
    .await
    .expect("insert analysis")
}

/// Two `analysis --provides_evidence--> claim` edges from ONE fresh analysis
/// to two fresh public claims of `p`, each written through the kernel's
/// `EdgeRepository::create` on a transaction of the `episcience_worker`
/// application login stamped as `p` (the production path of every kernel
/// edge EpiScience writes). Returns the second claim, which the legacy
/// trigger pairs with the first; `Err` carries the refusal text.
async fn two_evidence_edges(
    db: &TestDb,
    scoped: &ScopedPool,
    p: &Principal,
    label: &str,
) -> Result<Uuid, String> {
    let a = analysis(db, p).await;
    let mut claims = Vec::new();
    for i in 0..2 {
        claims.push(
            support::claim(
                &db.admin,
                p.agent,
                &format!("shared-evidence {label} claim {i} {}", Uuid::new_v4()),
                0.8,
                epigraph_core::TenancyDecl::public(p.personal_group),
            )
            .await,
        );
    }
    let viewer = viewer_of(&db.admin, p.agent).await;
    let mut tx = scoped.begin_as(&viewer).await.map_err(|e| e.to_string())?;
    for c in &claims {
        epigraph_db::EdgeRepository::create(
            &mut *tx,
            a,
            "analysis",
            *c,
            "claim",
            "provides_evidence",
            None,
            None,
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(claims[1])
}

/// T-H2. With the legacy trigger back in place (the undo script), an
/// application-role pair of evidence edges from one analysis derives a
/// `shared_evidence` factor; after `ledger::run` re-applies 5040 the trigger
/// and its function are gone, the same application-role insert succeeds, and
/// no factor is derived. The undo refuses while 5040 is not recorded and
/// while either object exists, changing nothing. Kills: 5040 without its DROP
/// TRIGGER (the factor is still derived after the re-apply), 5040 without its
/// DROP FUNCTION (the function survives), and an undo that does not un-record
/// 5040 (the re-apply would do nothing).
#[tokio::test]
async fn t_h2_5040_detaches_the_legacy_trigger_from_kernel_edges() {
    let db = TestDb::fresh().await;
    // A fresh database: 5040 recorded, and the baseline never created them.
    assert_eq!(legacy_objects(&db).await, (false, false));
    let fresh = ledger_versions(&db).await;
    assert!(fresh.contains(&5040), "{fresh:?}");
    assert_eq!(*fresh.last().unwrap(), head_version());

    // The undo refuses while a later migration is recorded, and changes
    // nothing.
    let e = db_err(sqlx::raw_sql(UNDO_5040).execute(&db.admin).await);
    assert!(
        e.contains("a later EpiScience migration is recorded"),
        "{e:?}"
    );
    assert_eq!(legacy_objects(&db).await, (false, false));
    assert_eq!(ledger_versions(&db).await, fresh);

    let scoped = ScopedPool::connect_with_options(
        &db.login_url(WORKER_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions {
            max_connections: 2,
            ..ScopedPoolOptions::default()
        },
    )
    .await
    .expect("stamped pool on the worker login");
    // Not vacuous: the insert below is an UNPRIVILEGED session's.
    let plain = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(db.login_options(WORKER_LOGIN))
        .await
        .expect("plain worker pool");
    episcience_db::tenancy_contract::refuse_privileged_session(&plain)
        .await
        .expect("the worker login is unprivileged");
    plain.close().await;
    let p = principal(&db.admin, "shared-evidence").await;

    // The legacy state, as a legacy database has it.
    undo_later_versions(&db).await;
    assert_eq!(
        db_err(sqlx::raw_sql(UNDO_5040).execute(&db.admin).await),
        ""
    );
    assert_eq!(legacy_objects(&db).await, (true, true));
    assert!(
        !ledger_versions(&db).await.contains(&5040),
        "5040 un-recorded"
    );
    let e = db_err(sqlx::raw_sql(UNDO_5040).execute(&db.admin).await);
    assert!(e.contains("5040 is not recorded"), "{e:?}");

    let before = two_evidence_edges(&db, &scoped, &p, "legacy")
        .await
        .expect("the application-role insert with the legacy trigger");
    assert_eq!(
        shared_evidence_factors(&db, before).await,
        1,
        "the legacy trigger acts on an application-role edge insert"
    );

    // Re-apply 5040 (and the versions after it) exactly as
    // `episcience-migrate run` does.
    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    ledger::run(&mut conn)
        .await
        .expect("5040 and later re-apply");
    assert_eq!(legacy_objects(&db).await, (false, false));
    assert_eq!(ledger_versions(&db).await, fresh);
    ledger::verify(&mut conn).await.expect("verify passes");

    let after = two_evidence_edges(&db, &scoped, &p, "detached")
        .await
        .expect("the application-role insert after 5040");
    assert_eq!(
        shared_evidence_factors(&db, after).await,
        0,
        "no factor is derived once the trigger is detached"
    );

    // The undo refuses while either object exists (here: a stray function of
    // that name), and records nothing.
    sqlx::raw_sql(
        "CREATE FUNCTION public.create_shared_evidence_factor() RETURNS trigger \
         LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;",
    )
    .execute(&db.admin)
    .await
    .expect("a stray function");
    undo_later_versions(&db).await;
    let e = db_err(sqlx::raw_sql(UNDO_5040).execute(&db.admin).await);
    assert!(e.contains("already exists; nothing changed"), "{e:?}");
    assert_eq!(*ledger_versions(&db).await.last().unwrap(), 5040);
}
