//! E1g: the request path's database handle (`episcience_db::tenancy`), on the
//! real `episcience_app` login of a fresh template clone.
//!
//! T-A6 (parity refusal), T-A7 (empty writable set), the boot refusal of a
//! privileged DSN, and the stamp itself.
mod support;
use support::{principal, team_group, TestDb, APP_LOGIN};

use std::time::Duration;

use epigraph_db::SessionGucMode;
use episcience_core::{Ownership, Visibility};
use episcience_db::tenancy::{EpiscienceDb, EpiscienceDbOptions, RequestRefusal};
use uuid::Uuid;

fn options(max_connections: u32) -> EpiscienceDbOptions {
    EpiscienceDbOptions {
        application_name: "episcience-test",
        max_connections,
        mode: SessionGucMode::Session,
    }
}

async fn app_db(db: &TestDb, max_connections: u32) -> EpiscienceDb {
    EpiscienceDb::connect(&db.login_url(APP_LOGIN), options(max_connections))
        .await
        .expect("the application login passes every boot refusal")
}

/// An agent with NO group membership at all (no personal group).
async fn bare_agent(pool: &sqlx::PgPool) -> Uuid {
    let agent = Uuid::new_v4();
    let mut pk = [0u8; 32];
    pk[..16].copy_from_slice(agent.as_bytes());
    pk[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    sqlx::query(
        "INSERT INTO public.agents (id, public_key, display_name, agent_type, role, state) \
         VALUES ($1, $2, $3, 'human', 'custom', 'active')",
    )
    .bind(agent)
    .bind(&pk[..])
    .bind(format!("fixture-bare-{agent}"))
    .execute(pool)
    .await
    .expect("insert bare agent");
    agent
}

/// The session a stamped handle runs on: the application login, unprivileged,
/// stamped with the principal. Kills: `read_as` / `write_as` handing out an
/// unstamped connection (the principal GUC would be empty), and a test
/// silently running as the superuser (brief 6.2's first assertion).
async fn assert_stamped_app_session(conn: &mut sqlx::PgConnection, principal: Uuid) {
    let (user, privileged, stamped): (String, bool, String) = sqlx::query_as(
        "SELECT session_user::text, \
                (SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = session_user) \
                  OR pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER'), \
                current_setting('epigraph.principal_id', true)",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("read the session");
    assert_eq!(user, "episcience_app");
    assert!(!privileged, "the application login is unprivileged");
    assert_eq!(stamped, principal.to_string(), "the session is stamped");
}

/// T-A6: a principal with ANY operator-link record is refused, acting or
/// retired (kernel parity: operated agents are stdio-only), and without a
/// token principal the request is `principal_required`. Kills: the
/// `operator_of_author` read dropped, a retired link treated as no link, and
/// a missing principal resolved as an empty viewer.
#[tokio::test]
async fn t_a6_an_operated_principal_is_refused_acting_or_retired() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let app = app_db(&db, 2).await;
    let op = principal(&a, "operator").await;
    let free = principal(&a, "free").await;

    for retired in [false, true] {
        let agent = principal(&a, "operated").await;
        sqlx::query(
            "INSERT INTO operator_links (agent_id, operator_id, operator_group_id, retired) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(agent.agent)
        .bind(op.agent)
        .bind(op.personal_group)
        .bind(retired)
        .execute(&a)
        .await
        .expect("operator link");
        match app.resolve_principal(Some(agent.agent)).await {
            Err(RequestRefusal::Operated(p)) => assert_eq!(p, agent.agent),
            other => panic!("retired={retired}: expected Operated, got {other:?}"),
        }
    }
    let v = app
        .resolve_principal(Some(free.agent))
        .await
        .expect("an unlinked principal resolves");
    assert_eq!(v.principal(), Some(free.agent));
    assert!(v.writable_groups().contains(&free.personal_group));
    assert_eq!(
        app.resolve_principal(None).await.unwrap_err(),
        RequestRefusal::PrincipalRequired
    );
}

/// T-A7: a principal that may write NO group is refused a write before any
/// transaction begins, and reads public rows only. Proven "before BEGIN" by
/// starving the stamped pool: its one connection is held by another read, so
/// a `write_as` that tried to BEGIN would wait for it (the test's 3 s bound
/// fails it) instead of answering at once. Kills: the writable check moved
/// after `begin_as`, or removed (the write would then be attempted).
#[tokio::test]
async fn t_a7_no_writable_group_refuses_writes_before_begin_and_reads_public_only() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let app = app_db(&db, 1).await;
    let owner = principal(&a, "owner").await;
    let public = support::pending_synthesis(&a, &owner, Visibility::Public).await;
    let private = support::pending_synthesis(&a, &owner, Visibility::Group).await;

    let bare = bare_agent(&a).await;
    let viewer = app.resolve_principal(Some(bare)).await.expect("resolves");
    assert!(viewer.writable_groups().is_empty());

    // Reads: public only (row security on the stamped session, no splice).
    {
        let mut read = app.read_as(&viewer).await.expect("read_as");
        assert_stamped_app_session(&mut read, bare).await;
        let seen: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM syntheses WHERE id = ANY($1) ORDER BY id")
                .bind(vec![public, private])
                .fetch_all(&mut *read)
                .await
                .expect("read");
        assert_eq!(seen, vec![public], "public only");

        // The single stamped connection is held right here.
        let refused = tokio::time::timeout(Duration::from_secs(3), app.write_as(&viewer))
            .await
            .expect("write_as answered without waiting for a connection");
        assert!(matches!(refused, Err(RequestRefusal::NoWritableGroup)));
    }

    // A READER of a team (still no writable group) reads that team's rows and
    // still may not write.
    let reader = bare_agent(&a).await;
    let team = team_group(&a, &owner, &[(reader, "reader")]).await;
    sqlx::query("UPDATE syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(private)
        .bind(team)
        .execute(&a)
        .await
        .expect("re-own to the team");
    let rv = app.resolve_principal(Some(reader)).await.expect("resolves");
    let mut read = app.read_as(&rv).await.expect("read_as");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM syntheses WHERE id = $1")
        .bind(private)
        .fetch_one(&mut *read)
        .await
        .expect("read");
    assert_eq!(n, 1, "a team reader reads the team's synthesis");
    drop(read);
    assert!(matches!(
        app.write_as(&rv).await,
        Err(RequestRefusal::NoWritableGroup)
    ));
}

/// `write_as` is a stamped TRANSACTION on the application login: a declared
/// root insert into the caller's writable group commits, the database binds
/// the author, and a row the caller may not write is refused by row security
/// (42501). Kills: `write_as` on an unstamped or privileged connection (the
/// foreign-group insert would pass), or not in a transaction (the rollback
/// below would keep the row).
#[tokio::test]
async fn write_as_is_a_stamped_transaction_under_row_security() {
    let db = TestDb::fresh().await;
    let a = db.admin.clone();
    let app = app_db(&db, 2).await;
    let h1 = principal(&a, "h1").await;
    let h2 = principal(&a, "h2").await;
    let v1 = app
        .resolve_principal(Some(h1.agent))
        .await
        .expect("resolves");

    let mut tx = app.write_as(&v1).await.expect("write_as");
    assert_stamped_app_session(&mut tx, h1.agent).await;
    let own = Uuid::now_v7();
    episcience_db::SynthesisRepository::create_pending(
        &mut *tx,
        own,
        "stamped write",
        h1.agent,
        None,
        &[],
        "anthropic",
        "m",
        Ownership::new(h1.personal_group, Visibility::Group),
    )
    .await
    .expect("a declared insert into the caller's own group");
    tx.commit().await.expect("commit");

    let mut tx = app.write_as(&v1).await.expect("write_as");
    let foreign = episcience_db::SynthesisRepository::create_pending(
        &mut *tx,
        Uuid::now_v7(),
        "into someone else's group",
        h1.agent,
        None,
        &[],
        "anthropic",
        "m",
        Ownership::new(h2.personal_group, Visibility::Group),
    )
    .await
    .expect_err("row security refuses a group the caller may not write");
    assert!(
        foreign.to_string().contains("row-level security"),
        "{foreign}"
    );
    drop(tx);

    let mut tx = app.write_as(&v1).await.expect("write_as");
    let rolled = Uuid::now_v7();
    episcience_db::SynthesisRepository::create_pending(
        &mut *tx,
        rolled,
        "rolled back",
        h1.agent,
        None,
        &[],
        "anthropic",
        "m",
        Ownership::new(h1.personal_group, Visibility::Group),
    )
    .await
    .expect("insert");
    tx.rollback().await.expect("rollback");

    let rows: Vec<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, agent_id FROM syntheses WHERE id = ANY($1)")
            .bind(vec![own, rolled])
            .fetch_all(&a)
            .await
            .expect("admin read");
    assert_eq!(rows, vec![(own, h1.agent)]);
}

/// The boot refusal (T-E1, library half): the superuser DSN is refused before
/// a request can be served, naming why; the application login connects.
/// Kills: `connect` dropping `refuse_privileged_session`.
#[tokio::test]
async fn connect_refuses_a_privileged_dsn() {
    let db = TestDb::fresh().await;
    let err = EpiscienceDb::connect(&db.url(), options(1))
        .await
        .expect_err("the superuser DSN is refused");
    assert!(err.contains("SUPERUSER"), "{err}");
    assert!(
        !err.contains("postgres://"),
        "the refusal never prints the DSN"
    );
    app_db(&db, 1).await;
}
