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

const E1E_UNDO: &str = include_str!("../../../docs/runbooks/e1e-undo.sql");
const E1F_UNDO: &str = include_str!("../../../docs/runbooks/e1f-undo.sql");
const UNDO_5035: &str = include_str!("../../../docs/runbooks/5035-undo.sql");
const UNDO_5040: &str = include_str!("../../../docs/runbooks/5040-undo.sql");
const UNDO_5041: &str = include_str!("../../../docs/runbooks/5041-undo.sql");
const UNDO_5042: &str = include_str!("../../../docs/runbooks/5042-undo.sql");

async fn ledger_versions(db: &TestDb) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM episcience_meta._sqlx_migrations ORDER BY 1")
        .fetch_all(&db.admin)
        .await
        .unwrap()
}

async fn count(db: &TestDb, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&db.admin)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The documented order past row security, on the data a live E1e database
/// holds (the reviewer's case): H1's public synthesis cites X's claim, the
/// claim is narrowed to X's group, the narrowing sweep runs once; and one
/// countersignature carries its stored link hash.
///
/// - `e1e-undo.sql` reverts to the E1d catalog: no policy, no row security,
///   no principal guard, none of 5037's definers, 5035's own helper bodies,
///   no 5036/5037 ledger row; the stored link hash stays.
/// - `5035-undo.sql` then REFUSES up front (a re-apply of 5035 would refuse
///   the membership row citing another group's narrowed claim), changing
///   nothing: the row guards and the 5035 ledger row are still there.
/// - `episcience-migrate run` re-applies 5036 and 5037, and the catalog is
///   exactly the model again (`verify` exits 0), the stored hash intact.
///
/// Kills: e1e-undo missing a policy, a trigger or a definer (the re-apply
/// fails on "already exists") or a ledger row (run re-applies nothing and
/// verify refuses the disabled row security), e1e-undo dropping the stored
/// hashes, the helpers left pointing at a dropped definer, 5035-undo's
/// re-apply guard removed (it would undo 5035 into a dead end).
#[tokio::test]
async fn the_e1e_undo_reverts_to_e1d_and_run_reapplies_on_narrowed_data() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = support::principal(a, "h1").await;
    let x = support::principal(a, "x").await;
    let claim = support::claim(
        a,
        x.agent,
        &format!("x's claim {}", uuid::Uuid::new_v4()),
        0.8,
        epigraph_core::TenancyDecl::public(x.personal_group),
    )
    .await;
    let s = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id) \
         VALUES ($1, 'rollback', $2, 'pending', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), 'public', $3)",
    )
    .bind(s)
    .bind(h1.agent)
    .bind(h1.personal_group)
    .execute(a)
    .await
    .unwrap();
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(s)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    let cs = episcience_db::CountersignRepository::create(
        &mut a.acquire().await.unwrap(),
        claim,
        h1.agent,
        h1.agent,
        "witnessed",
        &[1u8; 32],
        &[7u8; 64],
        2,
        episcience_core::Ownership::public(h1.personal_group),
    )
    .await
    .expect("a countersignature with its link hash");
    let stored = || async {
        sqlx::query_scalar::<_, Option<Vec<u8>>>(
            "SELECT signature_hash FROM countersignatures WHERE id = $1",
        )
        .bind(cs.id)
        .fetch_one(a)
        .await
        .unwrap()
    };
    let hash = stored().await;
    assert!(hash.is_some());
    sqlx::query("UPDATE claims SET visibility = 'group' WHERE id = $1")
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    let n: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(a)
        .await
        .unwrap();
    assert_eq!(n, 1);

    // 5042's, 5041's, E1h's and E1f's migrations come off first (e1e-undo
    // refuses while they are recorded; pinned below).
    assert_eq!(db_err(sqlx::raw_sql(UNDO_5042).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(UNDO_5041).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(UNDO_5040).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(E1F_UNDO).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(E1E_UNDO).execute(a).await), "");
    assert_eq!(policy_count(&db).await, 0, "no policy");
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) FROM pg_class WHERE relnamespace = 'public'::regnamespace \
                  AND relname IN ('{}') AND (relrowsecurity OR relforcerowsecurity)",
                ledger::EPISCIENCE_TABLES.join("','")
            )
        )
        .await,
        0,
        "no row security"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM pg_proc WHERE pronamespace = 'public'::regnamespace \
              AND proname = ANY(ARRAY['episcience_members_all_public','episcience_queue_claim', \
                'episcience_queue_finish','episcience_queue_retry','episcience_owner_worklist', \
                'episcience_countersign_chain_head','episcience_maint_sweep_narrowed', \
                'episcience_require_principal'])"
        )
        .await,
        0,
        "none of 5036's or 5037's functions"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM pg_trigger WHERE tgname = 'tenancy_05_principal' \
             "
        )
        .await,
        0,
        "no principal guard"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM pg_proc WHERE proname LIKE 'episcience\\_%\\_is\\_publishable' \
              AND prosrc LIKE '%members_all_public%'"
        )
        .await,
        0,
        "the helpers are 5035's own again"
    );
    assert_eq!(ledger_versions(&db).await, vec![5032, 5033, 5034, 5035]);
    assert_eq!(stored().await, hash, "the stored link hash stays");

    let e = db_err(sqlx::raw_sql(UNDO_5035).execute(a).await);
    assert!(
        e.contains("would be one-way") && e.contains("nothing changed"),
        "{e:?}"
    );
    assert_eq!(ledger_versions(&db).await, vec![5032, 5033, 5034, 5035]);
    assert!(
        count(
            &db,
            "SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'tenancy\\_%' AND NOT tgisinternal \
             "
        )
        .await
            > 0,
        "the row guards are untouched"
    );

    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    ledger::run(&mut conn).await.expect("5036 to 5042 re-apply");
    assert_eq!(
        catalog::findings(&mut conn).await.unwrap(),
        Vec::<String>::new()
    );
    ledger::verify(&mut conn).await.expect("verify passes");
    assert_eq!(stored().await, hash, "the stored link hash survived");
}

/// 5035-undo refuses while E1e is recorded (its helpers are re-pointed by
/// 5037, whose sweep calls them), changing nothing. Kills: the E1e guard
/// removed (5035-undo would drop the helpers under 5037's definers).
#[tokio::test]
async fn the_5035_undo_refuses_while_e1e_is_recorded() {
    let db = TestDb::fresh().await;
    let e = db_err(sqlx::raw_sql(UNDO_5035).execute(&db.admin).await);
    assert!(e.contains("run docs/runbooks/e1e-undo.sql first"), "{e:?}");
    assert_eq!(
        ledger_versions(&db).await,
        vec![5032, 5033, 5034, 5035, 5036, 5037, 5038, 5039, 5040, 5041, 5042]
    );
}

/// E1f: `e1e-undo.sql` refuses while 5038/5039 are recorded (changing
/// nothing); `e1f-undo.sql` refuses while E1h's 5040 is recorded, and
/// `5040-undo.sql` while 5041 is; after `5042-undo.sql`, `5041-undo.sql` and
/// `5040-undo.sql`, `e1f-undo.sql` removes exactly 5038's guard and 5039's
/// detector and their ledger rows, after which `verify` refuses (pending) and
/// `episcience-migrate run` re-applies 5038 to 5042 (the 5040 detach
/// removes the legacy trigger the 5040 undo put back) and `verify` passes.
/// Kills: the ordering guard removed from e1e-undo (it would strand 5038/5039
/// over an E1d catalog) or from e1f-undo (it would strand 5040 over an E1e
/// ledger), and e1f-undo missing an object (the re-apply fails on "already
/// exists") or a ledger row (run re-applies nothing).
#[tokio::test]
async fn the_e1f_undo_comes_off_first_and_run_reapplies_it() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let e = db_err(sqlx::raw_sql(E1E_UNDO).execute(a).await);
    assert!(e.contains("run docs/runbooks/e1f-undo.sql first"), "{e:?}");
    assert_eq!(
        ledger_versions(&db).await,
        vec![5032, 5033, 5034, 5035, 5036, 5037, 5038, 5039, 5040, 5041, 5042]
    );
    let e = db_err(sqlx::raw_sql(E1F_UNDO).execute(a).await);
    assert!(
        e.contains("a later EpiScience migration is recorded"),
        "{e:?}"
    );
    assert_eq!(
        ledger_versions(&db).await,
        vec![5032, 5033, 5034, 5035, 5036, 5037, 5038, 5039, 5040, 5041, 5042]
    );

    let e = db_err(sqlx::raw_sql(UNDO_5040).execute(a).await);
    assert!(
        e.contains("a later EpiScience migration is recorded"),
        "{e:?}"
    );
    assert_eq!(db_err(sqlx::raw_sql(UNDO_5042).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(UNDO_5041).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(UNDO_5040).execute(a).await), "");
    assert_eq!(db_err(sqlx::raw_sql(E1F_UNDO).execute(a).await), "");
    assert_eq!(
        ledger_versions(&db).await,
        vec![5032, 5033, 5034, 5035, 5036, 5037]
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM pg_proc WHERE pronamespace = 'public'::regnamespace \
              AND proname IN ('episcience_require_signature_hash', 'episcience_maint_unpublishable_public')"
        )
        .await
            + count(
                &db,
                "SELECT count(*) FROM pg_trigger WHERE tgname = 'tenancy_25_signature_hash'"
            )
            .await,
        0
    );
    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    assert!(
        ledger::verify(&mut conn).await.is_err(),
        "5038 to 5042 pending"
    );
    ledger::run(&mut conn).await.expect("5038 to 5042 re-apply");
    ledger::verify(&mut conn).await.expect("verify passes");
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM pg_trigger WHERE tgname = 'edges_shared_evidence'"
        )
        .await,
        0,
        "the re-applied 5040 detached the trigger the undo put back"
    );
}

/// A synthesis row with `skill_name`, written on the admin pool through the
/// production insert (`create_pending_tx`).
async fn skill_row(
    db: &TestDb,
    p: &support::Principal,
    skill_name: &str,
) -> Result<uuid::Uuid, String> {
    let id = uuid::Uuid::now_v7();
    episcience_db::SynthesisRepository::create_pending_tx(
        &db.admin,
        id,
        "5041 undo fixture",
        p.agent,
        None,
        &[],
        "p",
        "m",
        episcience_core::Ownership::group(p.personal_group),
        skill_name,
        None,
    )
    .await
    .map(|()| id)
    .map_err(|e| e.to_string())
}

/// `5041-undo.sql` refuses while a `wiki_article` synthesis exists (changing
/// nothing); once none does it narrows `syntheses_skill_name_known` back to
/// the five earlier skills exactly (`wiki_article` and an unknown name
/// refused, each of the five accepted) and un-records 5041; it then refuses as "not recorded";
/// `episcience-migrate run` re-applies 5041 (`wiki_article` accepted again)
/// and `verify` passes. Kills: the row guard removed (the narrowed CHECK would
/// fail mid-undo or, if NOT VALID, strand rows it rejects), an undo that drops
/// the CHECK or narrows past the five, and an undo that leaves the 5041 ledger
/// row (run would re-apply nothing and `wiki_article` stay refused).
#[tokio::test]
async fn the_5041_undo_narrows_the_skill_check_and_run_reapplies_it() {
    let db = TestDb::fresh().await;
    let p = support::principal(&db.admin, "wiki-undo").await;
    let fresh = ledger_versions(&db).await;
    assert_eq!(*fresh.last().unwrap(), 5042);
    // 5042 (the wiki columns) comes off first: 5041-undo refuses while it is
    // recorded (pinned in `the_5042_undo_drops_the_wiki_columns_and_run_reapplies_them`).
    assert_eq!(
        db_err(sqlx::raw_sql(UNDO_5042).execute(&db.admin).await),
        ""
    );
    let at_5041 = ledger_versions(&db).await;
    assert_eq!(*at_5041.last().unwrap(), 5041);

    let wiki = skill_row(&db, &p, "wiki_article")
        .await
        .expect("5041 admits wiki_article");
    let e = db_err(sqlx::raw_sql(UNDO_5041).execute(&db.admin).await);
    assert!(e.contains("wiki_article syntheses exist"), "{e:?}");
    assert_eq!(ledger_versions(&db).await, at_5041);
    sqlx::query("DELETE FROM syntheses WHERE id = $1")
        .bind(wiki)
        .execute(&db.admin)
        .await
        .unwrap();

    assert_eq!(
        db_err(sqlx::raw_sql(UNDO_5041).execute(&db.admin).await),
        ""
    );
    assert_eq!(*ledger_versions(&db).await.last().unwrap(), 5040);
    let e = skill_row(&db, &p, "wiki_article")
        .await
        .expect_err("narrowed");
    assert!(e.contains("syntheses_skill_name_known"), "{e:?}");
    // Hard-coded, not `registered_names()`: a later skill's undo must not
    // silently change what this undo is expected to keep.
    for n in [
        "baseline",
        "lab_notebook",
        "literature",
        "code_review",
        "registry_diff",
    ] {
        skill_row(&db, &p, n)
            .await
            .unwrap_or_else(|e| panic!("{n} refused after 5041-undo: {e}"));
    }
    let e = skill_row(&db, &p, "not_a_registered_skill")
        .await
        .expect_err("the narrowed CHECK still refuses unknown skills");
    assert!(e.contains("syntheses_skill_name_known"), "{e:?}");
    let e = db_err(sqlx::raw_sql(UNDO_5041).execute(&db.admin).await);
    assert!(e.contains("5041 is not recorded"), "{e:?}");

    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    ledger::run(&mut conn)
        .await
        .expect("5041 and 5042 re-apply");
    assert_eq!(ledger_versions(&db).await, fresh);
    ledger::verify(&mut conn).await.expect("verify passes");
    skill_row(&db, &p, "wiki_article")
        .await
        .expect("wiki_article admitted again");
}

/// The objects 5042 creates on `syntheses` that still exist: its two
/// columns, its two CHECKs and its page index (5 when applied, 0 when undone).
async fn wiki_objects(db: &TestDb) -> i64 {
    count(
        db,
        "SELECT (SELECT count(*) FROM pg_attribute \
                  WHERE attrelid = 'public.syntheses'::regclass AND NOT attisdropped \
                    AND attname IN ('seed_theme_id', 'wiki_key')) \
              + (SELECT count(*) FROM pg_constraint \
                  WHERE conrelid = 'public.syntheses'::regclass \
                    AND conname IN ('syntheses_wiki_key_shape', 'syntheses_wiki_pair')) \
              + (SELECT count(*) FROM pg_class \
                  WHERE relnamespace = 'public'::regnamespace \
                    AND relname = 'syntheses_wiki_page_idx')",
    )
    .await
}

/// `5041-undo.sql` refuses while 5042 is recorded (changing nothing);
/// `5042-undo.sql` drops exactly 5042's objects (both columns, both CHECKs,
/// the page index) and un-records 5042, keeping every synthesis row (only the
/// page keys are lost); it then refuses as "not recorded"; `episcience-migrate
/// run` re-applies 5042 (the columns and their CHECKs are back, a malformed
/// key refused again) and `verify` passes. Kills: an undo that leaves a
/// column or a CHECK standing (the index and the CHECKs also go with the
/// columns they name, so the count is what is pinned), an undo that leaves
/// the 5042 ledger row (run re-applies nothing and the columns stay gone), and
/// the 5041 undo's later-version guard removed (it would narrow the skill
/// CHECK beneath 5042's rows).
#[tokio::test]
async fn the_5042_undo_drops_the_wiki_columns_and_run_reapplies_them() {
    let db = TestDb::fresh().await;
    let p = support::principal(&db.admin, "wiki-cols-undo").await;
    let fresh = ledger_versions(&db).await;
    assert_eq!(*fresh.last().unwrap(), 5042);
    assert_eq!(wiki_objects(&db).await, 5);

    let page = skill_row(&db, &p, "wiki_article")
        .await
        .expect("a wiki synthesis");
    let key = episcience_core::wiki::WikiKey {
        run_id: uuid::Uuid::now_v7(),
        cluster_id: 7,
        split_part: None,
    }
    .as_slug();
    episcience_db::SynthesisRepository::set_wiki_seed_tx(
        &db.admin,
        page,
        uuid::Uuid::new_v4(),
        &key,
    )
    .await
    .expect("5042 admits a page key");

    let e = db_err(sqlx::raw_sql(UNDO_5041).execute(&db.admin).await);
    assert!(
        e.contains("a later EpiScience migration is recorded"),
        "{e:?}"
    );
    assert_eq!(ledger_versions(&db).await, fresh);

    assert_eq!(
        db_err(sqlx::raw_sql(UNDO_5042).execute(&db.admin).await),
        ""
    );
    assert_eq!(*ledger_versions(&db).await.last().unwrap(), 5041);
    assert_eq!(wiki_objects(&db).await, 0, "every 5042 object dropped");
    assert_eq!(
        count(
            &db,
            &format!("SELECT count(*) FROM syntheses WHERE id = '{page}'")
        )
        .await,
        1,
        "the undo deletes no synthesis"
    );
    let e = db_err(sqlx::raw_sql(UNDO_5042).execute(&db.admin).await);
    assert!(e.contains("5042 is not recorded"), "{e:?}");

    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    ledger::run(&mut conn).await.expect("5042 re-applies");
    assert_eq!(ledger_versions(&db).await, fresh);
    ledger::verify(&mut conn).await.expect("verify passes");
    assert_eq!(wiki_objects(&db).await, 5);
    let e = episcience_db::SynthesisRepository::set_wiki_seed_tx(
        &db.admin,
        page,
        uuid::Uuid::new_v4(),
        "not-a-page-key",
    )
    .await
    .expect_err("the shape CHECK is back")
    .to_string();
    assert!(e.contains("syntheses_wiki_key_shape"), "{e:?}");
}
