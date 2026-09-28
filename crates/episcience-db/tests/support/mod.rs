//! Test database harness (shared by `episcience-db` and, via `#[path]`,
//! `episcience-api` integration tests).
//!
//! `scripts/e1-test-db.sh` builds, once per run, a TEMPLATE database whose
//! kernel schema comes from the kernel's own `epigraph-migrate` at the pinned
//! rev and whose EpiScience schema comes from `episcience-migrate run`. Every
//! [`TestDb::fresh`] is a `CREATE DATABASE … TEMPLATE` clone of it: a real,
//! kernel-shaped schema per test, in milliseconds.
//!
//! Environment (exported by the script; never printed):
//! - `E1_TEST_ADMIN_URL` — superuser DSN of the TEST cluster's admin database;
//! - `E1_TEMPLATE_DB` / `E1_KERNEL_TEMPLATE_DB` — the two templates;
//! - `E1_RUN_PREFIX` — this run's unique database-name prefix. Every clone is
//!   named `<prefix>_<8 hex>_test`, so the script's EXIT trap drops it even if
//!   the test process dies before `Drop` runs.
//!
//! Refusals (no override, see [`check_test_url`]): a DSN on port 5432
//! (explicit or defaulted) or whose database name does not end in `_test`.
#![allow(dead_code)]

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

/// The one port a test DSN may never use (the port the harness never uses).
pub const FORBIDDEN_PORT: u16 = 5432;

/// Advisory-lock key serialising clones of the template (a `CREATE DATABASE
/// … TEMPLATE` fails when another session touches the template).
const CLONE_LOCK_KEY: i64 = 0x6570_6973_6369_e1b0;

/// CI-only login passwords (scripts/ci-roles.sql). Test clusters only.
pub const APP_LOGIN: (&str, &str) = ("episcience_app", "episcience_app_ci_only");
pub const WORKER_LOGIN: (&str, &str) = ("episcience_worker", "episcience_worker_ci_only");
pub const MAINT_LOGIN: (&str, &str) = ("episcience_maint", "episcience_maint_ci_only");

/// Refuse a DSN on port 5432 or naming a database that does not end in
/// `_test`. The port is read AFTER libpq-style defaulting, so a URL with no
/// port (which means 5432) is refused too.
pub fn check_test_url(url: &str) -> Result<PgConnectOptions, String> {
    let opts = PgConnectOptions::from_str(url).map_err(|e| format!("unparseable test DSN: {e}"))?;
    check_test_options(&opts)?;
    Ok(opts)
}

/// [`check_test_url`] on already-parsed options.
pub fn check_test_options(opts: &PgConnectOptions) -> Result<(), String> {
    if opts.get_port() == FORBIDDEN_PORT {
        return Err(format!(
            "REFUSED: test DSN on port {FORBIDDEN_PORT} (tests run on the TEST cluster only)"
        ));
    }
    let db = opts.get_database().unwrap_or("");
    check_test_db_name(db)
}

/// A test database name: `[a-z0-9_]`, at most 63 bytes (Postgres truncates
/// longer identifiers, which could cut the suffix off), ending in `_test`.
pub fn check_test_db_name(db: &str) -> Result<(), String> {
    if db.is_empty()
        || !db
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("REFUSED: test database name must be non-empty [a-z0-9_]".into());
    }
    if db.len() > 63 {
        return Err("REFUSED: test database name longer than 63 bytes".into());
    }
    if !db.ends_with("_test") {
        return Err("REFUSED: test database name does not end in _test".into());
    }
    Ok(())
}

fn admin_options() -> PgConnectOptions {
    let url = std::env::var("E1_TEST_ADMIN_URL")
        .expect("E1_TEST_ADMIN_URL must be set (run the suite through scripts/e1-test-db.sh)");
    let opts = PgConnectOptions::from_str(&url).expect("E1_TEST_ADMIN_URL parses");
    assert_ne!(
        opts.get_port(),
        FORBIDDEN_PORT,
        "REFUSED: E1_TEST_ADMIN_URL on port {FORBIDDEN_PORT}"
    );
    opts
}

fn env_db(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_else(|_| {
        panic!("{var} must be set (run the suite through scripts/e1-test-db.sh)")
    });
    check_test_db_name(&v).unwrap_or_else(|e| panic!("{var}: {e}"));
    v
}

/// A shared, script-provided database (`DATABASE_URL` or
/// `EPISCIENCE_DATABASE_URL`), checked by [`check_test_url`]. For the suites
/// that share one clone rather than taking a fresh one per test.
pub async fn shared_pool(var: &str) -> PgPool {
    let url = std::env::var(var).unwrap_or_else(|_| {
        panic!("{var} must name a *_test database on the TEST cluster (no default)")
    });
    let opts = check_test_url(&url).unwrap_or_else(|e| panic!("{var}: {e}"));
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("connect {var}: {e}"))
}

/// One throwaway clone of the template.
pub struct TestDb {
    /// Superuser pool on the clone. Fixtures and catalog reads, and (until the
    /// runtime moves to an application login) the code under test.
    pub admin: PgPool,
    pub name: String,
    admin_opts: PgConnectOptions,
}

impl TestDb {
    /// Clone the EpiScience template (kernel schema + `episcience-migrate run`
    /// + CI roles + seed).
    pub async fn fresh() -> TestDb {
        Self::clone_of(&env_db("E1_TEMPLATE_DB")).await
    }

    /// Clone the KERNEL-ONLY template (no EpiScience schema, no ledger).
    pub async fn fresh_kernel_only() -> TestDb {
        Self::clone_of(&env_db("E1_KERNEL_TEMPLATE_DB")).await
    }

    async fn clone_of(template: &str) -> TestDb {
        let admin_opts = admin_options();
        let prefix = std::env::var("E1_RUN_PREFIX")
            .expect("E1_RUN_PREFIX must be set (run the suite through scripts/e1-test-db.sh)");
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let name = format!("{prefix}_{suffix}_test");
        check_test_db_name(&name).unwrap_or_else(|e| panic!("{e}"));

        let mut c = PgConnection::connect_with(&admin_opts)
            .await
            .expect("connect to the test cluster admin database");
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(CLONE_LOCK_KEY)
            .execute(&mut c)
            .await
            .expect("advisory lock");
        let created = sqlx::query(&format!(
            "CREATE DATABASE \"{name}\" TEMPLATE \"{template}\""
        ))
        .execute(&mut c)
        .await;
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(CLONE_LOCK_KEY)
            .execute(&mut c)
            .await
            .expect("advisory unlock");
        created.unwrap_or_else(|e| panic!("CREATE DATABASE {name} TEMPLATE {template}: {e}"));
        let _ = c.close().await;

        let db_opts = admin_opts.clone().database(&name);
        let admin = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(db_opts)
            .await
            .expect("connect to the fresh test database");
        TestDb {
            admin,
            name,
            admin_opts,
        }
    }

    /// The clone's superuser DSN, for handing to a child process (the kernel's
    /// `epigraph-migrate`). Never print it.
    pub fn url(&self) -> String {
        let admin = std::env::var("E1_TEST_ADMIN_URL").expect("E1_TEST_ADMIN_URL");
        let base = admin
            .rsplit_once('/')
            .expect("admin URL names a database")
            .0;
        format!("{base}/{}", self.name)
    }

    /// Connect options for the clone as the superuser.
    pub fn admin_options(&self) -> PgConnectOptions {
        self.admin_opts.clone().database(&self.name)
    }

    /// A URL for the clone as one of the CI logins, for constructors that take
    /// a URL (the kernel's `ScopedPool`). The CI passwords are `[a-z_]` only,
    /// so nothing needs percent-encoding. Never print it.
    pub fn login_url(&self, login: (&str, &str)) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}",
            login.0,
            login.1,
            self.admin_opts.get_host(),
            self.admin_opts.get_port(),
            self.name
        )
    }

    /// Connect options for the clone as one of the CI logins.
    pub fn login_options(&self, login: (&str, &str)) -> PgConnectOptions {
        self.admin_opts
            .clone()
            .database(&self.name)
            .username(login.0)
            .password(login.1)
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let opts = self.admin_opts.clone();
        let name = self.name.clone();
        // A fresh thread with its own runtime: Drop may run inside a Tokio
        // runtime, which cannot be blocked on from its own thread.
        let h = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            rt.block_on(async move {
                if let Ok(mut c) = PgConnection::connect_with(&opts).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut c)
                            .await;
                    let _ = c.close().await;
                }
            });
        });
        let _ = h.join();
    }
}

// ─── Fixtures ───────────────────────────────────────────────────────────────
//
// Agents are inserted on the admin pool (the kernel has no repo call that
// mints a bare fixture agent without an OAuth client) and get their personal
// group through the kernel's own `epigraph_ensure_personal_group`, exactly as
// the OAuth mint provisions a principal. Claims go through the kernel's
// `ClaimRepository::create` with an explicit `TenancyDecl`: never an
// undeclared `claims` insert (on a superuser session the kernel would stamp
// the seed sentinel, which a declared write never produces).

/// A fixture principal: an agent plus its live personal group (admin).
#[derive(Debug, Clone, Copy)]
pub struct Principal {
    pub agent: uuid::Uuid,
    pub personal_group: uuid::Uuid,
}

/// Insert an agent and provision its personal group.
pub async fn principal(pool: &PgPool, label: &str) -> Principal {
    let agent = uuid::Uuid::new_v4();
    let mut pk = [0u8; 32];
    pk[..16].copy_from_slice(agent.as_bytes());
    pk[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    sqlx::query(
        "INSERT INTO public.agents (id, public_key, display_name, agent_type, role, state) \
         VALUES ($1, $2, $3, 'human', 'custom', 'active')",
    )
    .bind(agent)
    .bind(&pk[..])
    .bind(format!("fixture-{label}-{agent}"))
    .execute(pool)
    .await
    .expect("insert fixture agent");
    let personal_group: uuid::Uuid =
        sqlx::query_scalar("SELECT public.epigraph_ensure_personal_group($1)")
            .bind(agent)
            .fetch_one(pool)
            .await
            .expect("epigraph_ensure_personal_group");
    Principal {
        agent,
        personal_group,
    }
}

/// A declared claim through the kernel repository. `truth` >= 0.5 keeps it
/// above the synthesis seed floor.
pub async fn claim(
    pool: &PgPool,
    author: uuid::Uuid,
    content: &str,
    truth: f64,
    decl: epigraph_core::TenancyDecl,
) -> uuid::Uuid {
    let c = epigraph_core::Claim::new(
        content.to_string(),
        epigraph_core::AgentId::from_uuid(author),
        [0u8; 32],
        epigraph_core::TruthValue::new(truth).expect("truth in [0,1]"),
    );
    let stored = epigraph_db::ClaimRepository::create(pool, &c, decl)
        .await
        .expect("ClaimRepository::create");
    stored.id.into()
}

/// `(visibility, owner_group_id)` of a claim, read on the admin pool. Tests
/// assert it before relying on a claim being private: a fixture that silently
/// became public would make every "cannot see" assertion vacuous.
pub async fn claim_pair(pool: &PgPool, id: uuid::Uuid) -> (String, uuid::Uuid) {
    sqlx::query_as("SELECT visibility::text, owner_group_id FROM public.claims WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("claim pair")
}

/// A pending synthesis authored by `author`, declared `(author's personal
/// group, visibility)`, through the repository (the production write path).
pub async fn pending_synthesis(
    pool: &PgPool,
    author: &Principal,
    visibility: episcience_core::Visibility,
) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    episcience_db::SynthesisRepository::create_pending(
        pool,
        id,
        "fixture synthesis",
        author.agent,
        None,
        &[],
        "anthropic",
        "claude-3-7",
        episcience_core::Ownership::new(author.personal_group, visibility),
    )
    .await
    .expect("create_pending");
    id
}

/// A team group: `admin` holds the admin role, each `(agent, role)` in
/// `members` the given role (`writer` / `reader`). Written on the admin pool
/// with the full tenancy pair (no kernel repo creates team groups for a
/// fixture).
pub async fn team_group(
    pool: &PgPool,
    admin: &Principal,
    members: &[(uuid::Uuid, &str)],
) -> uuid::Uuid {
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO public.groups (display_name, did_key, public_key, kind, created_by_agent_id) \
         VALUES ('fixture-team', 'did:test:team:' || gen_random_uuid()::text, \
                 decode(repeat('ab', 32), 'hex'), 'team', $1) RETURNING id",
    )
    .bind(admin.agent)
    .fetch_one(pool)
    .await
    .expect("insert team group");
    let mut all: Vec<(uuid::Uuid, &str)> = vec![(admin.agent, "admin")];
    all.extend_from_slice(members);
    for (agent, role) in all {
        sqlx::query(
            "INSERT INTO public.group_memberships (group_id, agent_id, wrapped_key_share, epoch, role) \
             VALUES ($1, $2, '\\x00'::bytea, 0, $3)",
        )
        .bind(id)
        .bind(agent)
        .bind(role)
        .execute(pool)
        .await
        .expect("insert team membership");
    }
    id
}

/// The kernel viewer of a fixture principal (its live group memberships).
pub async fn viewer_of(pool: &PgPool, p: uuid::Uuid) -> epigraph_db::Viewer {
    epigraph_db::Viewer::resolve(pool, p)
        .await
        .expect("Viewer::resolve")
}

/// Put synthesis `synthesis_id` (owned by `owner`) under a fresh team group in
/// which `owner` is admin and `reader` a READER: the group-ownership
/// replacement for the retired per-agent read share. Re-owned on the admin
/// pool (a privileged session), as the one-shot re-own does; the children
/// follow the parent.
pub async fn reown_to_team_with_reader(
    pool: &PgPool,
    synthesis_id: uuid::Uuid,
    owner: &Principal,
    reader: uuid::Uuid,
) -> uuid::Uuid {
    let team = team_group(pool, owner, &[(reader, "reader")]).await;
    sqlx::query("UPDATE public.syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(synthesis_id)
        .bind(team)
        .execute(pool)
        .await
        .expect("re-own the synthesis to the team");
    team
}

/// The personal group of a fixture agent created by [`principal`].
pub async fn personal_group_of(pool: &PgPool, agent: uuid::Uuid) -> uuid::Uuid {
    sqlx::query_scalar(
        "SELECT id FROM public.groups WHERE kind = 'personal' \
          AND did_key = 'did:epigraph:personal:' || $1::text",
    )
    .bind(agent)
    .fetch_one(pool)
    .await
    .expect("the agent's personal group (create it with support::principal)")
}
