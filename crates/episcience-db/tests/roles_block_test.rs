//! Migration 5033's roles block: which pre-existing `episcience_*` NOLOGIN
//! roles it adopts and which it refuses.
//!
//! Roles are cluster-scoped and the test cluster is shared, so no case touches
//! a real EpiScience or kernel role. Every case runs the block's exact text
//! with all six names it mentions (the three grantee roles and the three
//! EpiScience logins) substituted by uniquely named throwaways, pre-creates
//! throwaways in the shape under test, runs the block, drops every throwaway
//! (including those the block created), and only then asserts.
//!
//! | arm | case (the mutation it kills is deleting that arm) |
//! |---|---|
//! | (a) LOGIN / elevated attribute | a pre-existing LOGIN role |
//! | (b) member of any role | member of a plain throwaway role; member of `pg_write_all_data` |
//! | (c) a member that is not an EpiScience login | a throwaway member |
//! | (d) an EpiScience login member that is elevated | the substituted login is BYPASSRLS |
//! | (c)'s creator exemption | the block run as a non-superuser CREATEROLE role creates, then adopts, its own roles |
//!
//! Positive cases: absent roles are created NOLOGIN and unprivileged, and a
//! re-run adopts them; a role whose only member is a plain EpiScience login is
//! adopted.
mod support;
use support::TestDb;

use sqlx::{Connection, PgConnection};

const MIGRATION_5033: &str = include_str!("../../../migrations/5033_kernel_contract_v1.sql");

/// The throwaway names for one case.
struct Names {
    rw: String,
    queue: String,
    maint_ops: String,
    app: String,
    worker: String,
    maint: String,
    /// Helper roles the case creates (a parent, a foreign member, a creator).
    helpers: Vec<String>,
}

impl Names {
    fn new() -> Names {
        let tag = &uuid::Uuid::new_v4().simple().to_string()[..10];
        let n = |s: &str| format!("e1c_rb_{tag}_{s}");
        Names {
            rw: n("rw"),
            queue: n("queue"),
            maint_ops: n("mops"),
            app: n("app"),
            worker: n("worker"),
            maint: n("maint"),
            helpers: vec![n("parent"), n("other"), n("creator")],
        }
    }
    fn parent(&self) -> &str {
        &self.helpers[0]
    }
    fn other(&self) -> &str {
        &self.helpers[1]
    }
    fn creator(&self) -> &str {
        &self.helpers[2]
    }
    /// Every throwaway, grantee roles first (a creator must outlive the roles
    /// it granted).
    fn all(&self) -> Vec<&str> {
        let mut v = vec![
            self.rw.as_str(),
            self.queue.as_str(),
            self.maint_ops.as_str(),
            self.app.as_str(),
            self.worker.as_str(),
            self.maint.as_str(),
        ];
        v.extend(self.helpers.iter().map(String::as_str));
        v
    }
}

/// The roles block of 5033 with every role name substituted. Panics unless
/// each name was found and no real `episcience_*` name survives, so a case can
/// never touch a real role.
fn roles_block(n: &Names) -> String {
    let start = MIGRATION_5033
        .find("DO $roles$")
        .expect("5033 has the roles block");
    let mut block = MIGRATION_5033[start..].to_string();
    // Longest first: 'episcience_maint' is a prefix of 'episcience_maint_ops'.
    for (real, fake) in [
        ("'episcience_maint_ops'", &n.maint_ops),
        ("'episcience_rw'", &n.rw),
        ("'episcience_queue'", &n.queue),
        ("'episcience_app'", &n.app),
        ("'episcience_worker'", &n.worker),
        ("'episcience_maint'", &n.maint),
    ] {
        assert!(block.contains(real), "the roles block names {real}");
        block = block.replace(real, &format!("'{fake}'"));
    }
    assert!(
        !block.contains("'episcience_"),
        "a real role name survived the substitution"
    );
    block
}

/// `(rolname, can log in, any elevated attribute, member of any role)`.
type GranteeRow = (String, bool, bool, bool);

/// The block's error message ("" on success) and the grantee rows.
type CaseOutcome = (String, Vec<GranteeRow>);

async fn admin(db: &TestDb) -> PgConnection {
    PgConnection::connect_with(&db.admin_options())
        .await
        .expect("admin connection")
}

/// Run `setup`, then the substituted block (after `before_block`, e.g. a
/// `SET ROLE`), then drop every throwaway. Returns the block's error message
/// ("" on success) and, when it succeeded, the attribute row of each grantee.
async fn run_case(db: &TestDb, n: &Names, setup: &str, before_block: &str) -> CaseOutcome {
    let mut c = admin(db).await;
    let outcome: Result<CaseOutcome, String> = async {
        sqlx::raw_sql(setup)
            .execute(&mut c)
            .await
            .map_err(|e| format!("setup: {e}"))?;
        sqlx::raw_sql(before_block)
            .execute(&mut c)
            .await
            .map_err(|e| format!("before_block: {e}"))?;
        let msg = match sqlx::raw_sql(&roles_block(n)).execute(&mut c).await {
            Ok(_) => String::new(),
            Err(sqlx::Error::Database(e)) => e.message().to_string(),
            Err(e) => return Err(format!("block: {e}")),
        };
        sqlx::raw_sql("RESET ROLE")
            .execute(&mut c)
            .await
            .map_err(|e| format!("reset: {e}"))?;
        let rows: Vec<GranteeRow> = sqlx::query_as(
            "SELECT rolname::text, rolcanlogin, rolsuper OR rolbypassrls OR rolcreaterole \
                    OR rolcreatedb OR rolreplication, \
                    EXISTS (SELECT 1 FROM pg_auth_members m WHERE m.member = r.oid) \
               FROM pg_roles r WHERE rolname = ANY($1) ORDER BY 1",
        )
        .bind(vec![n.rw.clone(), n.queue.clone(), n.maint_ops.clone()])
        .fetch_all(&mut c)
        .await
        .map_err(|e| format!("pg_roles: {e}"))?;
        Ok((msg, rows))
    }
    .await;
    let _ = c.close().await;

    // Drop every throwaway before any assertion can panic.
    let mut d = admin(db).await;
    for r in n.all() {
        sqlx::raw_sql(&format!("DROP ROLE IF EXISTS {r}"))
            .execute(&mut d)
            .await
            .unwrap_or_else(|e| panic!("drop throwaway role {r}: {e}"));
    }
    let _ = d.close().await;
    outcome.expect("the case ran")
}

/// Absent roles are created NOLOGIN, unprivileged, members of nothing; a
/// second run adopts them. Kills: a create that grants an attribute, and a
/// refusal that fires on the block's own roles.
#[tokio::test]
async fn absent_roles_are_created_unprivileged_and_a_rerun_adopts_them() {
    let db = TestDb::fresh().await;
    let n = Names::new();
    let block = roles_block(&n);
    let (msg, rows) = run_case(&db, &n, "SELECT 1", &block).await;
    assert_eq!(
        msg, "",
        "the second run must adopt the roles the first created"
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    for (name, login, elevated, member_of) in rows {
        assert!(!login && !elevated && !member_of, "{name}");
    }
}

/// (a) Kills: deleting the attribute refusal.
#[tokio::test]
async fn a_pre_existing_login_role_is_refused() {
    let db = TestDb::fresh().await;
    let n = Names::new();
    let setup = format!("CREATE ROLE {} LOGIN", n.rw);
    let (msg, _) = run_case(&db, &n, &setup, "SELECT 1").await;
    assert!(
        msg.contains("LOGIN or an elevated attribute"),
        "got {msg:?}"
    );
}

/// (b) Kills: deleting the member-of refusal (a role that already inherits
/// privileges would pass them to every EpiScience login).
#[tokio::test]
async fn a_pre_existing_role_that_is_a_member_of_any_role_is_refused() {
    let db = TestDb::fresh().await;
    for parent in ["throwaway", "pg_write_all_data"] {
        let n = Names::new();
        let parent_name = if parent == "throwaway" {
            n.parent().to_string()
        } else {
            parent.to_string()
        };
        let setup = format!(
            "CREATE ROLE {p} NOLOGIN; CREATE ROLE {rw} NOLOGIN; GRANT {parent_name} TO {rw};",
            p = n.parent(),
            rw = n.rw
        );
        let (msg, _) = run_case(&db, &n, &setup, "SELECT 1").await;
        assert!(
            msg.contains("as a member of role") && msg.contains(&parent_name),
            "{parent}: got {msg:?}"
        );
    }
}

/// (c) Kills: deleting the foreign-member refusal (every later grant to the
/// role would reach that member).
#[tokio::test]
async fn a_pre_existing_role_with_a_foreign_member_is_refused() {
    let db = TestDb::fresh().await;
    let n = Names::new();
    let setup = format!(
        "CREATE ROLE {rw} NOLOGIN; CREATE ROLE {o} NOLOGIN; GRANT {rw} TO {o};",
        rw = n.rw,
        o = n.other()
    );
    let (msg, _) = run_case(&db, &n, &setup, "SELECT 1").await;
    assert!(
        msg.contains("which is not an EpiScience login") && msg.contains(n.other()),
        "got {msg:?}"
    );
}

/// (d) Kills: deleting the elevated-login refusal.
#[tokio::test]
async fn a_pre_existing_role_with_an_elevated_episcience_login_member_is_refused() {
    let db = TestDb::fresh().await;
    let n = Names::new();
    let setup = format!(
        "CREATE ROLE {rw} NOLOGIN; CREATE ROLE {app} NOLOGIN BYPASSRLS; GRANT {rw} TO {app};",
        rw = n.rw,
        app = n.app
    );
    let (msg, _) = run_case(&db, &n, &setup, "SELECT 1").await;
    assert!(
        msg.contains("with an elevated attribute or kernel maintenance membership")
            && msg.contains(&n.app),
        "got {msg:?}"
    );
}

/// A role whose only member is a plain EpiScience login (the shape every
/// re-run meets once the logins exist) is adopted. Kills: (c) or (d)
/// refusing the EpiScience logins themselves.
#[tokio::test]
async fn a_pre_existing_role_with_a_plain_episcience_login_member_is_adopted() {
    let db = TestDb::fresh().await;
    let n = Names::new();
    let setup = format!(
        "CREATE ROLE {rw} NOLOGIN; CREATE ROLE {app} NOLOGIN; GRANT {rw} TO {app};",
        rw = n.rw,
        app = n.app
    );
    let (msg, rows) = run_case(&db, &n, &setup, "SELECT 1").await;
    assert_eq!(msg, "");
    assert_eq!(rows.len(), 3, "{rows:?}");
}

/// Run by a non-superuser CREATEROLE role, PostgreSQL 16 makes that role an
/// admin-only member (no INHERIT, no SET) of each role it creates; the block
/// must create and then adopt them, twice. Kills: deleting (c)'s creator
/// exemption (a non-superuser migrator could never apply 5033).
#[tokio::test]
async fn a_non_superuser_creator_can_create_and_rerun() {
    let db = TestDb::fresh().await;
    let n = Names::new();
    let block = roles_block(&n);
    let setup = format!("CREATE ROLE {c} NOLOGIN CREATEROLE", c = n.creator());
    let before = format!("SET ROLE {c}; {block} ", c = n.creator());
    let (msg, rows) = run_case(&db, &n, &setup, &before).await;
    assert_eq!(msg, "", "the creator's re-run must adopt its roles");
    assert_eq!(rows.len(), 3, "{rows:?}");
}
