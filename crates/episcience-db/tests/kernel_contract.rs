//! R10 (tenancy contract v1) and T-S1 (5033 refuses a drifted kernel).
//!
//! Contract v1 is asserted in three copies: the inline DO block that opens
//! migration 5033, `public.episcience_assert_kernel_contract(1)` (which every
//! later migration calls first), and the boot probe
//! `episcience_db::tenancy_contract::probe`. Every negative case below breaks
//! ONE item on a throwaway clone, proves the break took effect, and then
//! requires every copy that covers the item to refuse naming exactly that
//! item. A copy that drifts from the others (a check dropped, renumbered or
//! weakened in one place) turns a case red.
//!
//! Mutations are per-database only (drop/rename/revoke inside the clone).
//! Roles are cluster-scoped and shared with other workflows, so no case drops
//! or alters a role: C1's negative is not exercised here (see the PR body).
mod support;
use support::{TestDb, APP_LOGIN};

use episcience_db::ledger;
use episcience_db::tenancy_contract::{self, ContractError, CONTRACT_VERSION};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};

const MIGRATION_5033: &str = include_str!("../../../migrations/5033_kernel_contract_v1.sql");

/// The migration's first statement: the inline DO block.
fn inline_do() -> &'static str {
    let end = MIGRATION_5033
        .find("\n$contract$;")
        .expect("5033 ends its inline check with $contract$;")
        + "\n$contract$;".len();
    let stmt = &MIGRATION_5033[..end];
    assert!(
        stmt.starts_with("DO $contract$"),
        "5033 must open with the inline DO"
    );
    stmt
}

/// The text between the `>>> contract v1 checks` and `<<< contract v1 checks`
/// markers, for every occurrence.
fn regions(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(a) = rest.find(">>> contract v1 checks") {
        let after = &rest[a..];
        let b = after
            .find("<<< contract v1 checks")
            .expect("every >>> marker is closed");
        out.push(&after[..b]);
        rest = &after[b..];
    }
    out
}

/// The DO block and the function carry the SAME checks. Kills: editing one
/// copy (a new item, a changed threshold, a dropped check) without the other.
#[test]
fn the_inline_check_and_the_function_carry_identical_checks() {
    let r = regions(MIGRATION_5033);
    assert_eq!(r.len(), 2, "5033 must hold exactly two check regions");
    assert_eq!(r[0], r[1], "the inline DO and the function's checks differ");
    assert!(
        regions(inline_do()).len() == 1,
        "the first region must sit inside the opening DO block"
    );
}

fn db_error(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(sqlx::Error::Database(e)) => e.message().to_string(),
        Err(e) => panic!("expected a database error, got {e}"),
    }
}

async fn run_inline(pool: &sqlx::PgPool) -> String {
    db_error(sqlx::raw_sql(inline_do()).execute(pool).await)
}

async fn run_function(pool: &sqlx::PgPool) -> String {
    db_error(
        sqlx::query("SELECT public.episcience_assert_kernel_contract(1)")
            .execute(pool)
            .await,
    )
}

/// Positive: on the kernel the pin builds, all three copies pass, and the
/// probe reports every non-row item checked and exactly C6/C7/C8 skipped.
/// Kills: an item whose check is wrong on the real kernel (a false refusal
/// would stop every deploy).
#[tokio::test]
async fn contract_v1_holds_on_the_pinned_kernel() {
    let db = TestDb::fresh().await;
    assert_eq!(run_inline(&db.admin).await, "", "inline DO must pass");
    assert_eq!(run_function(&db.admin).await, "", "function must pass");
    let report = tenancy_contract::probe(&db.admin)
        .await
        .expect("probe passes");
    assert_eq!(
        report.checked,
        vec!["C1", "C2", "C3", "C4", "C5", "C9", "C10", "C11", "C12", "C13", "C14", "L1", "S1"]
    );
    assert_eq!(report.skipped, vec!["C6", "C7", "C8"]);
    assert_eq!(CONTRACT_VERSION, 1);
}

/// The probe passes on the APPLICATION login too: once the runtime moves off
/// the superuser DSN it runs as `episcience_app`, and a probe that needed a
/// privilege that login lacks would refuse every boot. Kills: a probe check
/// that reads a table row or uses a name-based inquiry the login cannot make.
#[tokio::test]
async fn the_probe_passes_on_the_application_login() {
    let db = TestDb::fresh().await;
    let app = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(db.login_options(APP_LOGIN))
        .await
        .expect("connect as the app login");
    let (sup, bypass, maint): (bool, bool, bool) = sqlx::query_as(
        "SELECT r.rolsuper, r.rolbypassrls, pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER') \
           FROM pg_roles r WHERE r.rolname = session_user",
    )
    .fetch_one(&app)
    .await
    .expect("role attributes");
    assert!(
        !sup && !bypass && !maint,
        "the app login must be unprivileged"
    );
    tenancy_contract::probe(&app)
        .await
        .expect("probe passes on episcience_app");
}

/// `episcience_assert_kernel_contract` knows v1 only. Kills: a version
/// argument that is ignored (a v2 migration would then run against a v1
/// database unchecked).
#[tokio::test]
async fn an_unknown_contract_version_is_refused() {
    let db = TestDb::fresh().await;
    for arg in ["2", "0", "NULL"] {
        let msg = db_error(
            sqlx::query(&format!(
                "SELECT public.episcience_assert_kernel_contract({arg})"
            ))
            .execute(&db.admin)
            .await,
        );
        assert!(
            msg.contains("unknown contract version"),
            "arg {arg}: {msg:?}"
        );
    }
}

/// Both 5033 functions are SECURITY INVOKER with `search_path` pinned.
/// Kills: either becoming SECURITY DEFINER (a caller would then run the checks,
/// or the privilege test, as the owner), or losing the pinned path.
#[tokio::test]
async fn the_5033_functions_are_invoker_with_a_pinned_search_path() {
    let db = TestDb::fresh().await;
    let rows: Vec<(String, bool, Vec<String>)> = sqlx::query_as(
        "SELECT p.proname::text, p.prosecdef, coalesce(p.proconfig, ARRAY[]::text[]) \
           FROM pg_proc p WHERE p.pronamespace = 'public'::regnamespace \
            AND p.proname IN ('episcience_assert_kernel_contract', 'episcience_session_is_privileged') \
          ORDER BY 1",
    )
    .fetch_all(&db.admin)
    .await
    .expect("pg_proc");
    assert_eq!(rows.len(), 2, "{rows:?}");
    for (name, secdef, config) in rows {
        assert!(!secdef, "{name} must be SECURITY INVOKER");
        assert_eq!(
            config,
            vec!["search_path=public, pg_temp".to_string()],
            "{name}"
        );
    }
}

/// `(rolname, login, super, bypassrls, createrole, createdb, replication,
/// maintenance member)`.
type RoleRow = (String, bool, bool, bool, bool, bool, bool, bool);

/// 5033's NOLOGIN roles exist with no elevated attribute and outside the
/// maintenance role, and re-running the roles block is a no-op. Kills: a
/// LOGIN or BYPASSRLS role, or a create that fails once the role exists
/// (every second database on a cluster would then fail to migrate).
#[tokio::test]
async fn the_nologin_roles_exist_unprivileged_and_their_creation_is_idempotent() {
    let db = TestDb::fresh().await;
    let rows: Vec<RoleRow> = sqlx::query_as(
        "SELECT rolname::text, rolcanlogin, rolsuper, rolbypassrls, rolcreaterole, rolcreatedb, \
                rolreplication, pg_has_role(rolname, 'epigraph_maintenance', 'MEMBER') \
           FROM pg_roles WHERE rolname IN ('episcience_rw', 'episcience_queue', 'episcience_maint_ops') \
          ORDER BY 1",
    )
    .fetch_all(&db.admin)
    .await
    .expect("pg_roles");
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        names,
        vec!["episcience_maint_ops", "episcience_queue", "episcience_rw"]
    );
    for r in &rows {
        assert!(
            !(r.1 || r.2 || r.3 || r.4 || r.5 || r.6 || r.7),
            "role {} carries an attribute or membership it must not: {r:?}",
            r.0
        );
    }
    let start = MIGRATION_5033
        .find("DO $roles$")
        .expect("5033 has the roles block");
    let roles_block = &MIGRATION_5033[start..];
    assert_eq!(
        db_error(sqlx::raw_sql(roles_block).execute(&db.admin).await),
        "",
        "re-running the roles block must succeed"
    );
}

/// One broken contract item.
struct Case {
    item: &'static str,
    /// SQL that breaks the item inside the clone (per-database objects only).
    breaks: &'static str,
    /// A boolean query that is TRUE once the break took effect.
    took_effect: &'static str,
    /// Whether the boot probe covers the item (C6/C7/C8 are preamble-only).
    probed: bool,
}

const REPLICA: &str = "SET LOCAL session_replication_role = replica;";

fn cases() -> Vec<Case> {
    vec![
        Case {
            item: "C2",
            breaks: "DROP FUNCTION public.epigraph_writable_groups() CASCADE;",
            took_effect: "SELECT to_regprocedure('public.epigraph_writable_groups()') IS NULL",
            probed: true,
        },
        Case {
            item: "C2",
            breaks: "REVOKE EXECUTE ON FUNCTION public.epigraph_principal_id() FROM PUBLIC, epigraph_app;",
            took_effect: "SELECT NOT has_function_privilege('epigraph_app', 'public.epigraph_principal_id()', 'EXECUTE')",
            probed: true,
        },
        Case {
            item: "C2",
            breaks: "DROP FUNCTION public.epigraph_principal_id() CASCADE; \
                     CREATE FUNCTION public.epigraph_principal_id() RETURNS text LANGUAGE sql STABLE \
                       AS $f$ SELECT NULLIF(current_setting('epigraph.principal_id', true), '') $f$; \
                     GRANT EXECUTE ON FUNCTION public.epigraph_principal_id() TO epigraph_app;",
            took_effect: "SELECT pg_get_function_result('public.epigraph_principal_id()'::regprocedure) = 'text'",
            probed: true,
        },
        Case {
            item: "C3",
            breaks: "ALTER TABLE public.group_memberships RENAME COLUMN revoked_at TO revoked_at_moved;",
            took_effect: "SELECT NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'public.group_memberships'::regclass AND attname = 'revoked_at')",
            probed: true,
        },
        Case {
            item: "C4",
            breaks: "ALTER TABLE public.claims RENAME COLUMN owner_group_id TO owner_group_id_moved;",
            took_effect: "SELECT NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'public.claims'::regclass AND attname = 'owner_group_id')",
            probed: true,
        },
        Case {
            item: "C5",
            breaks: "REVOKE INSERT ON public.security_events FROM epigraph_maintenance;",
            took_effect: "SELECT NOT has_table_privilege('epigraph_maintenance', 'public.security_events', 'INSERT')",
            probed: true,
        },
        Case {
            item: "C6",
            breaks: "DELETE FROM public.groups WHERE id = '00000000-0000-0000-0000-00000000dead';",
            took_effect: "SELECT NOT EXISTS (SELECT 1 FROM public.groups WHERE id = '00000000-0000-0000-0000-00000000dead')",
            probed: false,
        },
        Case {
            item: "C7",
            breaks: "DELETE FROM public._sqlx_migrations WHERE version >= 110;",
            took_effect: "SELECT max(version) = 109 FROM public._sqlx_migrations WHERE success",
            probed: false,
        },
        Case {
            item: "C8",
            breaks: "DELETE FROM public.entity_types WHERE type_name = 'synthesis';",
            took_effect: "SELECT NOT EXISTS (SELECT 1 FROM public.entity_types WHERE type_name = 'synthesis')",
            probed: false,
        },
        Case {
            item: "C9",
            breaks: "CREATE SCHEMA ext_moved; ALTER EXTENSION vector SET SCHEMA ext_moved;",
            took_effect: "SELECT extnamespace = 'ext_moved'::regnamespace FROM pg_extension WHERE extname = 'vector'",
            probed: true,
        },
        Case {
            item: "C10",
            breaks: "REVOKE SELECT ON public.group_memberships FROM epigraph_maintenance;",
            took_effect: "SELECT NOT has_table_privilege('epigraph_maintenance', 'public.group_memberships', 'SELECT')",
            probed: true,
        },
        Case {
            item: "C11",
            breaks: "REVOKE INSERT ON public.events FROM epigraph_app;",
            took_effect: "SELECT NOT has_table_privilege('epigraph_app', 'public.events', 'INSERT')",
            probed: true,
        },
        Case {
            item: "C11",
            breaks: "ALTER TABLE public.events RENAME TO events_moved;",
            took_effect: "SELECT to_regclass('public.events') IS NULL",
            probed: true,
        },
        Case {
            item: "C12",
            breaks: "REVOKE EXECUTE ON FUNCTION public.epigraph_operator_of_author(uuid) FROM PUBLIC, epigraph_app;",
            took_effect: "SELECT NOT has_function_privilege('epigraph_app', 'public.epigraph_operator_of_author(uuid)', 'EXECUTE')",
            probed: true,
        },
        Case {
            item: "C13",
            breaks: "REVOKE SELECT ON public.agents FROM epigraph_app;",
            took_effect: "SELECT NOT has_column_privilege('epigraph_app', 'public.agents', 'display_name', 'SELECT')",
            probed: true,
        },
        Case {
            item: "C13",
            breaks: "ALTER TABLE public.agents RENAME COLUMN display_name TO display_name_moved;",
            took_effect: "SELECT NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'public.agents'::regclass AND attname = 'display_name')",
            probed: true,
        },
        Case {
            item: "C14",
            breaks: "REVOKE USAGE ON SEQUENCE public.events_graph_version_seq FROM PUBLIC, epigraph_app;",
            took_effect: "SELECT NOT has_sequence_privilege('epigraph_app', 'public.events_graph_version_seq', 'USAGE')",
            probed: true,
        },
        Case {
            item: "L1",
            breaks: "ALTER TABLE public.syntheses DROP COLUMN autonomy_level CASCADE;",
            took_effect: "SELECT NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'public.syntheses'::regclass AND attname = 'autonomy_level' AND NOT attisdropped)",
            probed: true,
        },
    ]
}

async fn break_item(db: &TestDb, case: &Case) {
    let mut c = PgConnection::connect_with(&db.admin_options())
        .await
        .expect("admin connection");
    // Row deletes of kernel rows run with triggers (and so FK checks) off:
    // the point is the missing row, not the kernel's guards on removing it.
    let sql = format!("BEGIN; {REPLICA} {} COMMIT;", case.breaks);
    sqlx::raw_sql(&sql)
        .execute(&mut c)
        .await
        .unwrap_or_else(|e| panic!("{}: break the item: {e}", case.item));
    let took: Option<bool> = sqlx::query_scalar(case.took_effect)
        .fetch_one(&mut c)
        .await
        .unwrap_or_else(|e| panic!("{}: confirm the break: {e}", case.item));
    assert_eq!(
        took,
        Some(true),
        "{}: the break did not take effect",
        case.item
    );
    let _ = c.close().await;
}

/// Every item, broken alone, is refused by the inline DO, by the function and
/// (where it probes the item) by the boot probe, each naming THAT item.
/// Kills: a check removed from any one copy, an item renumbered in one copy,
/// a probe query that returns true on a missing object.
#[tokio::test]
async fn each_broken_item_is_refused_by_every_copy_naming_it() {
    for case in cases() {
        let db = TestDb::fresh().await;
        break_item(&db, &case).await;
        let want = format!("kernel contract v1: {} failed", case.item);

        let inline = run_inline(&db.admin).await;
        assert!(
            inline.contains(&want),
            "{}: inline DO said {inline:?}",
            case.item
        );
        let func = run_function(&db.admin).await;
        assert!(
            func.contains(&want),
            "{}: function said {func:?}",
            case.item
        );

        let probe = tenancy_contract::probe(&db.admin).await;
        if case.probed {
            match probe {
                Err(ContractError::Failed(f)) => {
                    let items: Vec<&str> = f.iter().map(|x| x.item).collect();
                    assert!(
                        !items.is_empty() && items.iter().all(|i| *i == case.item),
                        "{}: probe failed on {items:?}",
                        case.item
                    );
                }
                other => panic!("{}: probe must refuse, got {other:?}", case.item),
            }
        } else {
            assert!(
                probe.is_ok(),
                "{}: preamble-only item must not fail the probe: {probe:?}",
                case.item
            );
        }
    }
}

/// S1: a binary for contract v1 refuses a database 5033 never reached.
/// Kills: dropping the S1 check (a binary deployed before its migration would
/// serve against a schema without the contract function).
#[tokio::test]
async fn the_probe_refuses_a_database_without_5033() {
    let db = TestDb::fresh().await;
    sqlx::query("DROP FUNCTION public.episcience_assert_kernel_contract(integer)")
        .execute(&db.admin)
        .await
        .expect("drop");
    let items = tenancy_contract::probe(&db.admin)
        .await
        .expect_err("probe must refuse")
        .items();
    assert_eq!(items, vec!["S1"]);
}

async fn ledger_versions(pool: &sqlx::PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM episcience_meta._sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("ledger")
}

/// T-S1, end to end through `episcience-migrate run`: on a kernel whose
/// ledger stops at 109, and on one without `epigraph_writable_groups`, 5033
/// refuses naming the item, records nothing, and leaves none of its objects
/// behind (5032, which asserts nothing, is applied). Kills: the preamble
/// dropped or moved after the first DDL (5033's functions would then exist on
/// a drifted kernel).
#[tokio::test]
async fn t_s1_5033_refuses_a_kernel_below_head_110_or_missing_a_function() {
    for (item, breaks) in [
        (
            "C7",
            "DELETE FROM public._sqlx_migrations WHERE version >= 110",
        ),
        (
            "C2",
            "DROP FUNCTION public.epigraph_writable_groups() CASCADE",
        ),
    ] {
        let db = TestDb::fresh_kernel_only().await;
        sqlx::raw_sql(breaks)
            .execute(&db.admin)
            .await
            .expect("break the kernel");
        let mut conn = ledger::connect_with(db.admin_options())
            .await
            .expect("connect");
        let err = ledger::run(&mut conn)
            .await
            .expect_err("run must refuse at 5033")
            .to_string();
        assert!(
            err.contains(&format!("kernel contract v1: {item} failed")),
            "{item}: {err}"
        );
        assert_eq!(
            ledger_versions(&db.admin).await,
            vec![ledger::BASELINE_VERSION],
            "{item}: only 5032 may be recorded"
        );
        let leftovers: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_proc WHERE pronamespace = 'public'::regnamespace \
               AND proname IN ('episcience_assert_kernel_contract', 'episcience_session_is_privileged')",
        )
        .fetch_one(&db.admin)
        .await
        .expect("pg_proc");
        assert_eq!(leftovers, 0, "{item}: 5033 left objects behind");
    }
}

/// `episcience-migrate verify` passes on the template and refuses a database
/// with a broken item, a pending, failed, re-checksummed or unknown ledger
/// version, or a foreign kernel-ledger row.
/// Kills: a verify that only prints (the deploy guard would always pass).
#[tokio::test]
async fn verify_passes_on_the_template_and_refuses_each_failure_class() {
    let db = TestDb::fresh().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    ledger::verify(&mut conn).await.expect("verify passes");

    sqlx::query("REVOKE INSERT ON public.events FROM epigraph_app")
        .execute(&db.admin)
        .await
        .expect("revoke");
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("C11 broken")
        .to_string();
    assert!(e.contains("kernel contract v1: C11 failed"), "{e}");

    let db = TestDb::fresh().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    sqlx::query("DELETE FROM episcience_meta._sqlx_migrations WHERE version = 5033")
        .execute(&db.admin)
        .await
        .expect("delete");
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("pending")
        .to_string();
    assert!(e.contains("5033") && e.contains("not recorded"), "{e}");

    let db = TestDb::fresh().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    sqlx::query(
        "UPDATE episcience_meta._sqlx_migrations SET checksum = '\\x00'::bytea WHERE version = 5033",
    )
    .execute(&db.admin)
    .await
    .expect("corrupt checksum");
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("checksum")
        .to_string();
    assert!(e.contains("5033") && e.contains("checksum"), "{e}");

    for (tamper, want) in [
        (
            "UPDATE episcience_meta._sqlx_migrations SET success = FALSE WHERE version = 5033",
            "recorded as failed",
        ),
        (
            "INSERT INTO episcience_meta._sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (5098, 'planted', TRUE, '\\x00'::bytea, 0)",
            "not embedded",
        ),
    ] {
        let db = TestDb::fresh().await;
        let mut conn = ledger::connect_with(db.admin_options())
            .await
            .expect("connect");
        sqlx::query(tamper)
            .execute(&db.admin)
            .await
            .expect("tamper with the ledger");
        let e = ledger::verify(&mut conn).await.expect_err(want).to_string();
        assert!(e.contains(want), "{e}");
    }

    let db = TestDb::fresh().await;
    let mut conn = ledger::connect_with(db.admin_options())
        .await
        .expect("connect");
    sqlx::query(
        "INSERT INTO public._sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (5099, 'planted', TRUE, '\\x00'::bytea, 0)",
    )
    .execute(&db.admin)
    .await
    .expect("plant");
    let e = ledger::verify(&mut conn)
        .await
        .expect_err("foreign")
        .to_string();
    assert!(e.contains("5099"), "{e}");
}
