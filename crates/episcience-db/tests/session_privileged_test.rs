//! `public.episcience_session_is_privileged()` (migration 5033): which
//! sessions EpiScience's row guards will exempt. Each arm is isolated so that
//! removing it turns exactly one test red:
//!
//! | arm | isolated by |
//! |---|---|
//! | `current_user` has BYPASSRLS | a throwaway NOLOGIN BYPASSRLS role (not a maintenance member) as the session |
//! | `epigraph_bypass()` (session_user in maintenance) | a DEFINER wrapper owned by `episcience_app`, called with the session authorised as `epigraph_maintenance` |
//! | `epigraph_definer_bypass()` (current_user in maintenance) | a DEFINER wrapper owned by `epigraph_maintenance`, called from the real `episcience_app` login |
//! | none (the default is false) | the real `episcience_app` login, unstamped and stamped through `ScopedPool::begin_as` |
//!
//! The superuser arm is an equivalent mutant: `pg_has_role` counts a
//! superuser as a member of every role, so `epigraph_bypass()` already admits
//! one. The superuser case is still asserted.
//!
//! The wrappers are per-database objects in a throwaway clone. The only
//! cluster-level object a test creates is the uniquely named BYPASSRLS role,
//! which is dropped before the test asserts; no kernel role is altered and no
//! membership in a kernel role is granted.
mod support;
use support::{TestDb, APP_LOGIN};

use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode, Viewer};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection};

const IS_PRIVILEGED: &str = "SELECT public.episcience_session_is_privileged()";

/// The brief's app-role precondition: the session under test is unprivileged.
async fn assert_unprivileged_login(conn: &mut PgConnection) {
    let (sup, bypass, maint): (bool, bool, bool) = sqlx::query_as(
        "SELECT r.rolsuper, r.rolbypassrls, pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER') \
           FROM pg_roles r WHERE r.rolname = session_user",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("role attributes");
    assert!(
        !sup && !bypass && !maint,
        "the login under test must be unprivileged"
    );
}

/// Kills: `RETURN true` / an `OR true` arm, and any arm that admits an
/// ordinary application session.
#[tokio::test]
async fn an_unstamped_application_login_is_not_privileged() {
    let db = TestDb::fresh().await;
    let mut app = PgConnection::connect_with(&db.login_options(APP_LOGIN))
        .await
        .expect("connect as episcience_app");
    assert_unprivileged_login(&mut app).await;
    let p: bool = sqlx::query_scalar(IS_PRIVILEGED)
        .fetch_one(&mut app)
        .await
        .expect("call");
    assert!(!p);
}

/// A session stamped with a real principal and its writable groups (the
/// production path: `Viewer::resolve` then `ScopedPool::begin_as`) is still
/// not privileged. Kills: an arm keyed on the tenancy GUCs (a principal or a
/// non-empty writable set) instead of role membership.
#[tokio::test]
async fn a_stamped_application_session_is_not_privileged() {
    let db = TestDb::fresh().await;
    let p = support::principal(&db.admin, "stamped").await;
    let viewer = Viewer::resolve(&db.admin, p.agent)
        .await
        .expect("resolve the fixture principal");
    let scoped = ScopedPool::connect_with_options(
        &db.login_url(APP_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .expect("ScopedPool on the app login");
    let mut tx = scoped.begin_as(&viewer).await.expect("begin_as");
    assert_unprivileged_login(&mut tx).await;
    let (principal, writable): (Option<uuid::Uuid>, Vec<uuid::Uuid>) =
        sqlx::query_as("SELECT public.epigraph_principal_id(), public.epigraph_writable_groups()")
            .fetch_one(&mut *tx)
            .await
            .expect("stamp");
    assert_eq!(principal, Some(p.agent), "the stamp must have taken effect");
    assert!(
        writable.contains(&p.personal_group),
        "the principal's personal group must be writable in the stamp"
    );
    let priv_: bool = sqlx::query_scalar(IS_PRIVILEGED)
        .fetch_one(&mut *tx)
        .await
        .expect("call");
    assert!(!priv_);
}

#[tokio::test]
async fn a_superuser_session_is_privileged() {
    let db = TestDb::fresh().await;
    let p: bool = sqlx::query_scalar(IS_PRIVILEGED)
        .fetch_one(&db.admin)
        .await
        .expect("call");
    assert!(p);
}

/// Inside a SECURITY DEFINER function owned by `epigraph_maintenance` (the
/// shape of every EpiScience maintenance definer and of the propagation
/// trigger), called from the unprivileged app login: privileged, and the call
/// does not fail on EXECUTE. Kills: dropping the `epigraph_definer_bypass()`
/// arm; and pins that the maintenance owner may execute everything the
/// function calls.
#[tokio::test]
async fn a_maintenance_owned_definer_frame_is_privileged() {
    let db = TestDb::fresh().await;
    sqlx::raw_sql(
        "CREATE FUNCTION public.e1c_privileged_as_maintenance() RETURNS boolean \
           LANGUAGE sql SECURITY DEFINER SET search_path = public, pg_temp \
           AS 'SELECT public.episcience_session_is_privileged()'; \
         ALTER FUNCTION public.e1c_privileged_as_maintenance() OWNER TO epigraph_maintenance;",
    )
    .execute(&db.admin)
    .await
    .expect("wrapper");
    let mut app = PgConnection::connect_with(&db.login_options(APP_LOGIN))
        .await
        .expect("connect as episcience_app");
    assert_unprivileged_login(&mut app).await;
    let (inside, outside): (bool, bool) = sqlx::query_as(
        "SELECT public.e1c_privileged_as_maintenance(), public.episcience_session_is_privileged()",
    )
    .fetch_one(&mut app)
    .await
    .expect("call");
    assert!(
        inside,
        "a maintenance-owned definer frame must be privileged"
    );
    assert!(
        !outside,
        "control: the same session outside the frame is not"
    );
}

/// Session authorised as `epigraph_maintenance`, current_user an ordinary
/// login (a DEFINER wrapper owned by `episcience_app`): privileged through
/// the session_user arm alone. Kills: dropping the `epigraph_bypass()` arm.
#[tokio::test]
async fn a_maintenance_session_is_privileged_through_session_user() {
    let db = TestDb::fresh().await;
    sqlx::raw_sql(
        "CREATE FUNCTION public.e1c_privileged_as_app() RETURNS TABLE(cur text, priv boolean) \
           LANGUAGE sql SECURITY DEFINER SET search_path = public, pg_temp \
           AS 'SELECT current_user::text, public.episcience_session_is_privileged()'; \
         ALTER FUNCTION public.e1c_privileged_as_app() OWNER TO episcience_app;",
    )
    .execute(&db.admin)
    .await
    .expect("wrapper");
    let mut c = PgConnection::connect_with(&db.admin_options())
        .await
        .expect("admin connection");
    sqlx::query("SET SESSION AUTHORIZATION epigraph_maintenance")
        .execute(&mut c)
        .await
        .expect("session authorization");
    let (cur, priv_): (String, bool) =
        sqlx::query_as("SELECT * FROM public.e1c_privileged_as_app()")
            .fetch_one(&mut c)
            .await
            .expect("call");
    assert_eq!(
        cur, "episcience_app",
        "current_user must be the wrapper owner"
    );
    assert!(
        priv_,
        "session_user in epigraph_maintenance must be privileged"
    );
    let _ = c.close().await;
}

/// A NOLOGIN BYPASSRLS role that is not a maintenance member: privileged by
/// the attribute alone (the first arm returns before any kernel function is
/// reached). A NOBYPASSRLS twin reaching the last arm without EXECUTE on
/// `epigraph_definer_bypass` gets an error, never `true`. Kills: dropping the
/// BYPASSRLS arm; and a body that swallows the EXECUTE failure into `true`.
#[tokio::test]
async fn a_bypassrls_role_is_privileged_and_an_unentitled_caller_fails_closed() {
    let db = TestDb::fresh().await;
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let with_attr = format!("episcience_e1c_tmp_bypass_{tag}");
    let without = format!("episcience_e1c_tmp_plain_{tag}");
    sqlx::raw_sql(&format!(
        "CREATE ROLE {with_attr} NOLOGIN NOSUPERUSER BYPASSRLS; \
         CREATE ROLE {without} NOLOGIN NOSUPERUSER NOBYPASSRLS;"
    ))
    .execute(&db.admin)
    .await
    .expect("create the throwaway roles");

    let outcome: Result<(bool, String), String> = async {
        let mut c = PgConnection::connect_with(&db.admin_options())
            .await
            .map_err(|e| e.to_string())?;
        let maint: bool = sqlx::query_scalar(&format!(
            "SELECT pg_has_role('{with_attr}', 'epigraph_maintenance', 'MEMBER')"
        ))
        .fetch_one(&mut c)
        .await
        .map_err(|e| e.to_string())?;
        if maint {
            return Err("the throwaway role must not be a maintenance member".into());
        }
        sqlx::query(&format!("SET SESSION AUTHORIZATION {with_attr}"))
            .execute(&mut c)
            .await
            .map_err(|e| e.to_string())?;
        let p: bool = sqlx::query_scalar(IS_PRIVILEGED)
            .fetch_one(&mut c)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query("RESET SESSION AUTHORIZATION")
            .execute(&mut c)
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query(&format!("SET SESSION AUTHORIZATION {without}"))
            .execute(&mut c)
            .await
            .map_err(|e| e.to_string())?;
        let denied = match sqlx::query_scalar::<_, bool>(IS_PRIVILEGED)
            .fetch_one(&mut c)
            .await
        {
            Ok(v) => format!("returned {v}"),
            Err(sqlx::Error::Database(e)) => e.code().map(|c| c.to_string()).unwrap_or_default(),
            Err(e) => e.to_string(),
        };
        let _ = c.close().await;
        Ok((p, denied))
    }
    .await;

    // Drop the cluster-level roles before any assertion can panic.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(db.admin_options())
        .await
        .expect("admin pool");
    sqlx::raw_sql(&format!(
        "DROP ROLE IF EXISTS {with_attr}; DROP ROLE IF EXISTS {without};"
    ))
    .execute(&pool)
    .await
    .expect("drop the throwaway roles");

    let (p, denied) = outcome.expect("the checks ran");
    assert!(p, "a BYPASSRLS current_user must be privileged");
    assert_eq!(
        denied, "42501",
        "a caller without EXECUTE on the last arm must get insufficient_privilege, got {denied:?}"
    );
}
