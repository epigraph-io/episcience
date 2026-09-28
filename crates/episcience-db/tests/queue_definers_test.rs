//! The definers of migration 5037, called on the REAL logins they are
//! granted to: the worker login (`episcience_worker`, member of
//! `episcience_queue`), the application login (`episcience_app`, member of
//! `episcience_rw`) and the maintenance login (`episcience_maint`, member of
//! `episcience_maint_ops` only). Each test names the mutation it kills.
//!
//! Cast: H1, H2 (personal groups H1pg, H2pg).
mod support;
use support::{
    principal, team_group, viewer_of, Principal, TestDb, APP_LOGIN, MAINT_LOGIN, WORKER_LOGIN,
};

use epigraph_core::TenancyDecl;
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

async fn login_pool(db: &TestDb, login: (&str, &str)) -> PgPool {
    PgPoolOptions::new()
        .max_connections(3)
        .connect_with(db.login_options(login))
        .await
        .unwrap_or_else(|e| panic!("connect as {}: {e}", login.0))
}

/// The brief's precondition for every login under test.
async fn assert_unprivileged(conn: &mut PgConnection) {
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

fn err<T>(r: &Result<T, sqlx::Error>) -> (String, String) {
    match r {
        Ok(_) => (String::new(), String::new()),
        Err(sqlx::Error::Database(d)) => (
            d.code().map(|c| c.to_string()).unwrap_or_default(),
            d.message().to_string(),
        ),
        Err(e) => ("non-db".into(), e.to_string()),
    }
}

/// A synthesis written on the admin pool (privileged). `complete` fills the
/// columns the completion CHECKs require.
async fn admin_synthesis(
    a: &PgPool,
    author: Uuid,
    status: &str,
    vis: &str,
    owner: Uuid,
    parent: Option<Uuid>,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id, parent_synthesis_id, \
             narrative, completed_at) \
         VALUES ($1, 'definer test', $2, $3, '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), $4, $5, $6, \
                 CASE WHEN $3 = 'complete' THEN 'n' END, CASE WHEN $3 = 'complete' THEN now() END)",
    )
    .bind(id)
    .bind(author)
    .bind(status)
    .bind(vis)
    .bind(owner)
    .bind(parent)
    .execute(a)
    .await
    .expect("admin synthesis");
    id
}

/// The job row of `synthesis`, acting as `principal`, in `state`, due
/// `due_in` from now.
async fn admin_job(a: &PgPool, synthesis: Uuid, principal: Uuid, state: &str, due_in: &str) {
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, payload, state, principal_id, scheduled_at) \
         VALUES ($1, '{}'::jsonb, $2, $3, now() + $4::interval)",
    )
    .bind(synthesis)
    .bind(state)
    .bind(principal)
    .bind(due_in)
    .execute(a)
    .await
    .expect("admin job");
}

async fn job_state(a: &PgPool, id: Uuid) -> (String, i32, Option<String>) {
    sqlx::query_as("SELECT state, attempts, last_error FROM synthesis_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(a)
        .await
        .unwrap()
}

async fn public_claim(a: &PgPool, author: &Principal) -> Uuid {
    support::claim(
        a,
        author.agent,
        &format!("definer claim {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(author.personal_group),
    )
    .await
}

// ─── T-J0: the queue definers on the worker login ───────────────────────────

/// T-J0 (R-M2): on the worker login, `episcience_queue_claim` returns the one
/// DUE queued job (with the job's principal, one more attempt) and marks it
/// running; `retry` puts it back (never a second row); `finish` ends it; each
/// refuses a job that is not running, an unknown state, and a retry past the
/// attempt limit. Two workers claiming at once get different jobs without
/// waiting. Kills: `synthesis_jobs_bypass_update` dropped (the claim then
/// updates nothing and returns no row), `SKIP LOCKED` removed (the second
/// claim blocks and hits its lock timeout), the due-time filter removed, the
/// running-state check of finish or retry removed, a retry that inserts.
#[tokio::test]
async fn the_queue_claims_retries_and_finishes_jobs_on_the_worker_login() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let g = h1.personal_group;
    let due = admin_synthesis(a, h1.agent, "pending", "group", g, None).await;
    let later = admin_synthesis(a, h1.agent, "pending", "group", g, None).await;
    let done = admin_synthesis(a, h1.agent, "complete", "group", g, None).await;
    admin_job(a, due, h1.agent, "queued", "-1 minute").await;
    admin_job(a, later, h1.agent, "queued", "1 hour").await;
    admin_job(a, done, h1.agent, "complete", "-1 hour").await;

    let worker = login_pool(&db, WORKER_LOGIN).await;
    let mut w = worker.acquire().await.unwrap();
    assert_unprivileged(&mut w).await;
    let claim = "SELECT job_id, synthesis_id, principal_id, attempts FROM public.episcience_queue_claim($1)";
    let got: Vec<(Uuid, Uuid, Uuid, i32)> = sqlx::query_as(claim)
        .bind("w1")
        .fetch_all(&mut *w)
        .await
        .expect("claim");
    assert_eq!(got, vec![(due, due, h1.agent, 1)], "the one due job");
    assert_eq!(job_state(a, due).await.0, "running");
    let none: Vec<(Uuid, Uuid, Uuid, i32)> = sqlx::query_as(claim)
        .bind("w1")
        .fetch_all(&mut *w)
        .await
        .unwrap();
    assert!(none.is_empty(), "nothing else is due: {none:?}");

    let r = sqlx::query("SELECT public.episcience_queue_claim('')")
        .execute(&mut *w)
        .await;
    assert_eq!(err(&r).0, "22004", "a worker name is required");

    sqlx::query("SELECT public.episcience_queue_retry($1, interval '0', 'transient')")
        .bind(due)
        .execute(&mut *w)
        .await
        .expect("retry a running job");
    assert_eq!(
        job_state(a, due).await,
        ("queued".to_string(), 1, Some("transient".to_string()))
    );
    let again: Vec<(Uuid, Uuid, Uuid, i32)> = sqlx::query_as(claim)
        .bind("w1")
        .fetch_all(&mut *w)
        .await
        .unwrap();
    assert_eq!(again, vec![(due, due, h1.agent, 2)], "claimed again");

    for (job, state, want) in [
        (due, "queued", "22023"),
        (later, "failed", "55000"),
        (done, "complete", "55000"),
    ] {
        let r = sqlx::query("SELECT public.episcience_queue_finish($1, $2, NULL)")
            .bind(job)
            .bind(state)
            .execute(&mut *w)
            .await;
        assert_eq!(err(&r).0, want, "finish({state}) of {job}");
    }
    let r = sqlx::query("SELECT public.episcience_queue_retry($1, interval '0', NULL)")
        .bind(later)
        .execute(&mut *w)
        .await;
    assert_eq!(err(&r).0, "55000", "a queued job cannot be retried");

    sqlx::query("UPDATE synthesis_jobs SET max_attempts = 2 WHERE id = $1")
        .bind(due)
        .execute(a)
        .await
        .unwrap();
    let r = sqlx::query("SELECT public.episcience_queue_retry($1, interval '0', NULL)")
        .bind(due)
        .execute(&mut *w)
        .await;
    let (code, msg) = err(&r);
    assert_eq!(code, "55000");
    assert!(msg.contains("used all its attempts"), "{msg}");
    sqlx::query("SELECT public.episcience_queue_finish($1, 'failed', 'expired')")
        .bind(due)
        .execute(&mut *w)
        .await
        .expect("finish a running job");
    assert_eq!(
        job_state(a, due).await,
        ("failed".to_string(), 2, Some("expired".to_string()))
    );
    let completed: bool =
        sqlx::query_scalar("SELECT completed_at IS NOT NULL FROM synthesis_jobs WHERE id = $1")
            .bind(due)
            .fetch_one(a)
            .await
            .unwrap();
    assert!(completed);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM synthesis_jobs WHERE id = ANY($1)")
        .bind(vec![due, later, done])
        .fetch_one(a)
        .await
        .unwrap();
    assert_eq!(rows, 3, "one job row per synthesis, throughout");
    drop(w);

    // Two claims at once: the first holds its row (transaction open), the
    // second must take the other due job at once, not wait.
    let s1 = admin_synthesis(a, h1.agent, "pending", "group", g, None).await;
    let s2 = admin_synthesis(a, h1.agent, "pending", "group", g, None).await;
    admin_job(a, s1, h1.agent, "queued", "-2 minutes").await;
    admin_job(a, s2, h1.agent, "queued", "-1 minute").await;
    let mut t1 = worker.begin().await.unwrap();
    let first: Uuid = sqlx::query_scalar("SELECT job_id FROM public.episcience_queue_claim('w1')")
        .fetch_one(&mut *t1)
        .await
        .unwrap();
    let mut t2 = worker.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *t2)
        .await
        .unwrap();
    let second: Vec<Uuid> =
        sqlx::query_scalar("SELECT job_id FROM public.episcience_queue_claim('w2')")
            .fetch_all(&mut *t2)
            .await
            .expect("the second claim does not wait for the first");
    assert_eq!(first, s1, "the earliest due job first");
    assert_eq!(second, vec![s2], "the other worker takes the next job");
    t2.rollback().await.unwrap();
    t1.rollback().await.unwrap();
}

// ─── T-J3: who may execute what ─────────────────────────────────────────────

/// T-J3: each definer is executable by exactly its role's login, and the
/// maintenance login holds no table privilege at all. Checked two ways: the
/// catalog (`has_function_privilege` for every definer x login) and real
/// calls (the application login is refused the queue and the sweep, the
/// worker the sweep, the maintenance login the queue and every table).
/// Kills: `GRANT EXECUTE` of a queue definer to `episcience_rw`, of the
/// sweep to `episcience_queue`, `EXECUTE` left to PUBLIC, a table grant to
/// `episcience_maint_ops`.
#[tokio::test]
async fn each_definer_runs_only_for_its_role_and_the_maintenance_login_holds_no_table() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    // (signature, which of app / worker / maint may execute it)
    let expected: [(&str, [bool; 3]); 9] = [
        (
            "episcience_members_all_public(text,uuid)",
            [true, true, false],
        ),
        ("episcience_queue_claim(text)", [false, true, false]),
        (
            "episcience_queue_finish(uuid,text,text)",
            [false, true, false],
        ),
        (
            "episcience_queue_retry(uuid,interval,text)",
            [false, true, false],
        ),
        (
            "episcience_owner_worklist(text,integer)",
            [false, true, false],
        ),
        (
            "episcience_countersign_chain_head(uuid)",
            [true, true, false],
        ),
        ("episcience_maint_sweep_narrowed()", [false, false, true]),
        (
            "episcience_maint_backfill_owners(uuid,boolean)",
            [false, false, true],
        ),
        (
            "episcience_maint_backfill_reverse(jsonb)",
            [false, false, true],
        ),
    ];
    for (f, want) in expected {
        for (i, login) in [APP_LOGIN.0, WORKER_LOGIN.0, MAINT_LOGIN.0]
            .iter()
            .enumerate()
        {
            let can: bool = sqlx::query_scalar("SELECT has_function_privilege($1, $2, 'EXECUTE')")
                .bind(login)
                .bind(format!("public.{f}"))
                .fetch_one(a)
                .await
                .unwrap();
            assert_eq!(can, want[i], "{login} EXECUTE {f}");
        }
    }

    let app = login_pool(&db, APP_LOGIN).await;
    let worker = login_pool(&db, WORKER_LOGIN).await;
    let maint = login_pool(&db, MAINT_LOGIN).await;
    for (pool, sql, who) in [
        (
            &app,
            "SELECT * FROM public.episcience_queue_claim('x')",
            "app",
        ),
        (
            &app,
            "SELECT public.episcience_maint_sweep_narrowed()",
            "app",
        ),
        (
            &worker,
            "SELECT public.episcience_maint_sweep_narrowed()",
            "worker",
        ),
        (
            &maint,
            "SELECT * FROM public.episcience_queue_claim('x')",
            "maint",
        ),
        (
            &maint,
            "SELECT * FROM public.episcience_owner_worklist('staleness_check', 1)",
            "maint",
        ),
    ] {
        let r = sqlx::query(sql).execute(pool).await;
        let (code, msg) = err(&r);
        assert_eq!(code, "42501", "{who}: {sql}");
        assert!(
            msg.contains("permission denied for function"),
            "{who}: {msg}"
        );
    }
    let n: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(&maint)
        .await
        .expect("the maintenance login runs the sweep");
    assert_eq!(n, 0);

    for table in episcience_db::ledger::EPISCIENCE_TABLES {
        for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
            let has: bool = sqlx::query_scalar("SELECT has_table_privilege($1, $2, $3)")
                .bind(MAINT_LOGIN.0)
                .bind(format!("public.{table}"))
                .bind(privilege)
                .fetch_one(a)
                .await
                .unwrap();
            assert!(!has, "{} holds {privilege} on {table}", MAINT_LOGIN.0);
        }
        let r = sqlx::query(&format!("SELECT 1 FROM public.{table} LIMIT 1"))
            .execute(&maint)
            .await;
        let (code, msg) = err(&r);
        assert_eq!(code, "42501", "maint reads {table}");
        assert!(msg.contains("permission denied for table"), "{msg}");
    }
}

// ─── The worklist ───────────────────────────────────────────────────────────

/// `episcience_owner_worklist` returns (synthesis, JOB principal) pairs, the
/// principal being the job's and not the synthesis' author:
/// - `stage6_pending`: complete AND public AND an outbox row that is
///   unwritten, not deferred and under the retry cap;
/// - `staleness_check`: complete AND not stale AND not checked in the last
///   15 minutes (public or group alike).
///
/// Kills: the principal read from `syntheses.agent_id`, each filter of
/// either kind dropped (deferred, written, cap, public, complete, stale,
/// window), the limit ignored.
#[tokio::test]
async fn the_worklist_names_each_synthesis_with_its_job_principal() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let author = principal(a, "author").await;
    let acting = principal(a, "acting").await;
    // The acting principal writes the owner group (a principal that cannot
    // is filtered out: see the starvation test below).
    let g = team_group(a, &author, &[(acting.agent, "writer")]).await;
    let mk = |status: &'static str, vis: &'static str| async move {
        let s = admin_synthesis(a, author.agent, status, vis, g, None).await;
        admin_job(a, s, acting.agent, "complete", "0").await;
        s
    };
    let outbox = |s: Uuid, written: bool, deferred: Option<&'static str>, attempts: i32| async move {
        sqlx::query(
            "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id, \
                 written_at, deferred_reason, attempt_count) \
             VALUES ($1, 'ATTRIBUTED_TO', 'agent', gen_random_uuid(), \
                     CASE WHEN $2 THEN now() END, $3, $4)",
        )
        .bind(s)
        .bind(written)
        .bind(deferred)
        .bind(attempts)
        .execute(a)
        .await
        .unwrap();
    };
    let pending = mk("complete", "public").await;
    outbox(pending, false, None, 9).await;
    let deferred = mk("complete", "public").await;
    outbox(deferred, false, Some("private"), 0).await;
    let written = mk("complete", "public").await;
    outbox(written, true, None, 0).await;
    let capped = mk("complete", "public").await;
    outbox(capped, false, None, 10).await;
    let grouped = mk("complete", "group").await;
    outbox(grouped, false, None, 0).await;
    let running = mk("running", "public").await;
    outbox(running, false, None, 0).await;
    let recent = mk("complete", "public").await;
    let old = mk("complete", "group").await;
    let stale = mk("complete", "public").await;
    for (s, set) in [
        (recent, "staleness_checked_at = now() - interval '1 minute'"),
        (old, "staleness_checked_at = now() - interval '20 minutes'"),
        (stale, "stale_since = now(), stale_reason = 'belief_drift'"),
        // everything else in this fixture was checked recently, so the
        // staleness list is exactly the three below
        (deferred, "staleness_checked_at = now()"),
        (written, "staleness_checked_at = now()"),
        (capped, "staleness_checked_at = now()"),
    ] {
        sqlx::query(&format!("UPDATE syntheses SET {set} WHERE id = $1"))
            .bind(s)
            .execute(a)
            .await
            .unwrap();
    }

    let worker = login_pool(&db, WORKER_LOGIN).await;
    let list = |kind: &'static str, limit: i32| {
        let worker = worker.clone();
        async move {
            let mut rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT synthesis_id, principal_id FROM public.episcience_owner_worklist($1, $2)",
            )
            .bind(kind)
            .bind(limit)
            .fetch_all(&worker)
            .await
            .unwrap_or_else(|e| panic!("worklist {kind}: {e}"));
            rows.sort();
            rows
        }
    };
    assert_eq!(
        list("stage6_pending", 50).await,
        vec![(pending, acting.agent)],
        "stage6_pending"
    );
    let mut want = vec![
        (pending, acting.agent),
        (grouped, acting.agent),
        (old, acting.agent),
    ];
    want.sort();
    assert_eq!(list("staleness_check", 50).await, want, "staleness_check");
    assert_eq!(list("staleness_check", 2).await.len(), 2, "the limit");
    for (kind, limit) in [("everything", 5), ("staleness_check", 0)] {
        let r = sqlx::query("SELECT * FROM public.episcience_owner_worklist($1, $2)")
            .bind(kind)
            .bind(limit)
            .execute(&worker)
            .await;
        assert_eq!(err(&r).0, "22023", "{kind} / {limit}");
    }
}

/// The worklist only names a synthesis whose job principal can still WRITE
/// its owner group (a live `admin` or `writer` membership): 60 complete public
/// syntheses of team T with a pending outbox row, whose job principals lost
/// that (30 acting as a writer whose membership was revoked, 30 as a
/// reader), all sorting BEFORE one valid synthesis acting as T's admin; with
/// the worker's limit of 50, both kinds still return the valid one, and none
/// of the 60. Kills: the membership filter dropped from either kind (the 60
/// fill the limit and the valid synthesis is never returned: the worker
/// writes nothing for them, so their position never advances), a filter that
/// admits a revoked membership or a reader.
#[tokio::test]
async fn the_worklist_never_starves_behind_principals_that_lost_write_authority() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let admin = principal(a, "admin").await;
    let revoked = principal(a, "revoked").await;
    let reader = principal(a, "reader").await;
    let t = team_group(
        a,
        &admin,
        &[(revoked.agent, "writer"), (reader.agent, "reader")],
    )
    .await;
    sqlx::query(
        "UPDATE group_memberships SET revoked_at = now() WHERE group_id = $1 AND agent_id = $2",
    )
    .bind(t)
    .bind(revoked.agent)
    .execute(a)
    .await
    .unwrap();
    let pending = |s: Uuid| async move {
        sqlx::query(
            "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
             VALUES ($1, 'ATTRIBUTED_TO', 'agent', gen_random_uuid())",
        )
        .bind(s)
        .execute(a)
        .await
        .unwrap();
    };
    let mut lost = Vec::new();
    for i in 0..60 {
        let acting = if i % 2 == 0 {
            revoked.agent
        } else {
            reader.agent
        };
        let s = admin_synthesis(a, admin.agent, "complete", "public", t, None).await;
        admin_job(a, s, acting, "complete", "0").await;
        pending(s).await;
        lost.push(s);
    }
    let valid = admin_synthesis(a, admin.agent, "complete", "public", t, None).await;
    admin_job(a, valid, admin.agent, "complete", "0").await;
    pending(valid).await;

    let worker = login_pool(&db, WORKER_LOGIN).await;
    for kind in ["stage6_pending", "staleness_check"] {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT synthesis_id, principal_id FROM public.episcience_owner_worklist($1, 50)",
        )
        .bind(kind)
        .fetch_all(&worker)
        .await
        .unwrap_or_else(|e| panic!("worklist {kind}: {e}"));
        assert_eq!(rows, vec![(valid, admin.agent)], "{kind}");
        assert!(rows.iter().all(|(s, _)| !lost.contains(s)), "{kind}");
    }
}

// ─── The countersignature chain head ────────────────────────────────────────

/// One countersignature of `claim`, recorded by `by` into `(vis, owner)`,
/// with `signature` and, when `hashed`, the stored hash of its signature
/// (rows written before 5037, or by an older binary, carry none).
#[allow(clippy::too_many_arguments)]
async fn admin_countersignature(
    a: &PgPool,
    claim: Uuid,
    by: Uuid,
    meaning: &str,
    signature: &[u8],
    hashed: bool,
    vis: &str,
    owner: Uuid,
) {
    sqlx::query(
        "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
             signature, countersigned_by, owner_group_id, visibility, created_at, signature_hash) \
         VALUES ($1, $2, $3, decode(repeat('01', 32), 'hex'), $4, $2, $5, $6, clock_timestamp(), \
                 CASE WHEN $7 THEN sha256($4) END)",
    )
    .bind(claim)
    .bind(by)
    .bind(meaning)
    .bind(signature)
    .bind(owner)
    .bind(vis)
    .bind(hashed)
    .execute(a)
    .await
    .unwrap();
}

/// The chain head spans writers and returns the head's stored LINK HASH,
/// never its signature: H2, on the application login, gets the stored hash
/// of the latest countersignature of H1's PUBLIC claim even though H1's
/// attestation is `group` (invisible to H2), and never the signature bytes
/// (with the claim and content known and the signer and meaning
/// enumerable, those would identify who attested and how). A claim with no
/// countersignature yet gives no head; a `group` claim of H1 and a claim that
/// does not exist are refused (42501, never a NULL head). H1 reads its own
/// group claim's head.
///
/// An older head (no stored hash): the raw signature comes back only to a
/// caller that may read that row (H1 for its own attestation, anyone for a
/// public one); H2 asking about H1's hidden older head is refused (55000).
///
/// Kills: the definer made INVOKER (H2 would get no head for the public
/// claim), the claim visibility check removed (H2 would get H1's group-claim
/// head), the raw signature returned for a hashed head, the head-row check
/// removed from the older-head arm (H2 gets H1's signature), a NULL instead
/// of the 55000 refusal.
#[tokio::test]
async fn the_chain_head_spans_writers_and_never_hands_out_a_hidden_signature() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let h2 = principal(a, "h2").await;
    let g1 = h1.personal_group;
    let public = public_claim(a, &h1).await;
    let private = support::claim(
        a,
        h1.agent,
        &format!("group claim {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(g1),
    )
    .await;
    let bare = public_claim(a, &h1).await;
    let older_hidden = public_claim(a, &h1).await;
    let older_public = public_claim(a, &h1).await;
    for (claim, sig, meaning, hashed, vis) in [
        (public, 0x0a_u8, "witnessed", true, "group"),
        (public, 0x0b, "approved", true, "group"),
        (private, 0x0c, "witnessed", true, "group"),
        (older_hidden, 0x0d, "witnessed", false, "group"),
        (older_public, 0x0e, "witnessed", false, "public"),
    ] {
        admin_countersignature(a, claim, h1.agent, meaning, &[sig; 64], hashed, vis, g1).await;
    }
    // The fixture's stand-in link hash (any value the writer stored).
    let mut sha = std::collections::HashMap::new();
    for b in [0x0b_u8, 0x0c] {
        let h: Vec<u8> = sqlx::query_scalar("SELECT sha256($1)")
            .bind(vec![b; 64])
            .fetch_one(a)
            .await
            .unwrap();
        sha.insert(b, h);
    }
    let sha = |b: u8| sha[&b].clone();
    let app = ScopedPool::connect_with_options(
        &db.login_url(APP_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .unwrap();
    let head = "SELECT head_hash, head_signature FROM public.episcience_countersign_chain_head($1)";
    type Head = (Option<Vec<u8>>, Option<Vec<u8>>);

    let v2 = viewer_of(a, h2.agent).await;
    let mut tx = app.begin_as(&v2).await.unwrap();
    assert_unprivileged(&mut tx).await;
    let own_view: i64 =
        sqlx::query_scalar("SELECT count(*) FROM countersignatures WHERE claim_id = $1")
            .bind(public)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(own_view, 0, "H1's group attestations are invisible to H2");
    let h: Head = sqlx::query_as(head)
        .bind(public)
        .fetch_one(&mut *tx)
        .await
        .expect("head of a public claim");
    assert_eq!(
        h,
        (Some(sha(0x0b)), None),
        "the latest link hash, whoever wrote it, and no signature"
    );
    let h: Head = sqlx::query_as(head)
        .bind(older_public)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(h, (None, Some(vec![0x0e; 64])), "an older PUBLIC head");
    let h: Head = sqlx::query_as(head)
        .bind(bare)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(h, (None, None), "no attestation yet");
    drop(tx);
    for (claim, want_code, want_msg) in [
        (private, "42501", "is not visible"),
        (Uuid::new_v4(), "42501", "is not visible"),
        (older_hidden, "55000", "predates chain hashes"),
    ] {
        let mut tx = app.begin_as(&v2).await.unwrap();
        let r = sqlx::query(head).bind(claim).execute(&mut *tx).await;
        let (code, msg) = err(&r);
        assert_eq!(code, want_code, "{claim}: {msg}");
        assert!(msg.contains(want_msg), "{msg}");
    }

    let v1 = viewer_of(a, h1.agent).await;
    let mut tx = app.begin_as(&v1).await.unwrap();
    let h: Head = sqlx::query_as(head)
        .bind(private)
        .fetch_one(&mut *tx)
        .await
        .expect("H1 reads its own group claim's head");
    assert_eq!(h, (Some(sha(0x0c)), None));
    let h: Head = sqlx::query_as(head)
        .bind(older_hidden)
        .fetch_one(&mut *tx)
        .await
        .expect("H1 reads its own older head");
    assert_eq!(h, (None, Some(vec![0x0d; 64])));
}

// ─── The narrowing sweep ────────────────────────────────────────────────────

/// T-M1 (database half): after H1's member claim is narrowed out of band,
/// the sweep, run on the maintenance login, narrows the public synthesis
/// citing it AND (to a fixpoint) its public refinement, whose parent is no
/// longer public; each is marked stale `input_narrowed`, gets one staleness
/// event (naming the non-public member for the first, none for the second)
/// and one audit row; the children follow. An unrelated public synthesis is
/// untouched, and a second run changes nothing. Kills: the fixpoint loop
/// removed, the staleness event or the audit row removed, the stale mark
/// removed, a sweep that is not idempotent.
#[tokio::test]
async fn the_sweep_narrows_what_stopped_being_publishable_exactly_once() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let g = h1.personal_group;
    let member = public_claim(a, &h1).await;
    let other = public_claim(a, &h1).await;
    let s = admin_synthesis(a, h1.agent, "complete", "public", g, None).await;
    let child = admin_synthesis(a, h1.agent, "complete", "public", g, Some(s)).await;
    let untouched = admin_synthesis(a, h1.agent, "complete", "public", g, None).await;
    for (syn, claim) in [(s, member), (child, other), (untouched, other)] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(syn)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
             member_claim_ids, support_count, contradict_count) \
         VALUES (gen_random_uuid(), $1, 0, 't', 's', ARRAY[gen_random_uuid()], 0, 0)",
    )
    .bind(s)
    .execute(a)
    .await
    .unwrap();
    sqlx::query("UPDATE claims SET visibility = 'group' WHERE id = $1")
        .bind(member)
        .execute(a)
        .await
        .expect("narrow the member claim out of band");

    let maint = login_pool(&db, MAINT_LOGIN).await;
    let mut m = maint.acquire().await.unwrap();
    assert_unprivileged(&mut m).await;
    let n: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(&mut *m)
        .await
        .expect("sweep");
    assert_eq!(n, 2, "the synthesis and its public refinement");

    for (syn, want_vis, want_reason) in [
        (s, "group", Some("input_narrowed")),
        (child, "group", Some("input_narrowed")),
        (untouched, "public", None),
    ] {
        let (vis, reason): (String, Option<String>) =
            sqlx::query_as("SELECT visibility::text, stale_reason FROM syntheses WHERE id = $1")
                .bind(syn)
                .fetch_one(a)
                .await
                .unwrap();
        assert_eq!(
            (vis.as_str(), reason.as_deref()),
            (want_vis, want_reason),
            "{syn}"
        );
    }
    let cluster_vis: String = sqlx::query_scalar(
        "SELECT visibility::text FROM synthesis_clusters WHERE synthesis_id = $1",
    )
    .bind(s)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(cluster_vis, "group", "the children follow");
    let events: Vec<(Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT synthesis_id, affected_claim_ids FROM synthesis_staleness_events \
          WHERE trigger = 'input_narrowed' ORDER BY synthesis_id",
    )
    .fetch_all(a)
    .await
    .unwrap();
    let mut want = vec![(s, vec![member]), (child, vec![])];
    want.sort();
    assert_eq!(events, want, "one staleness event each");
    let audits = || async {
        let rows = sqlx::query(
            "SELECT details->>'synthesis_id' AS s, agent_id FROM security_events \
              WHERE event_type = 'episcience.maint.sweep_narrowed' ORDER BY 1",
        )
        .fetch_all(a)
        .await
        .unwrap();
        rows.iter()
            .map(|r| {
                (
                    r.get::<String, _>("s"),
                    r.get::<Option<Uuid>, _>("agent_id"),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut want = vec![(s.to_string(), None), (child.to_string(), None)];
    want.sort();
    assert_eq!(audits().await, want, "one audit row each, no agent");

    let again: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(&mut *m)
        .await
        .unwrap();
    assert_eq!(again, 0, "idempotent");
    assert_eq!(audits().await.len(), 2, "no new audit rows");
    let n_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM synthesis_staleness_events WHERE trigger = 'input_narrowed'",
    )
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(n_events, 2, "no new staleness events");
}

/// The sweep's staleness event names only the non-public member claims the
/// synthesis' OWNER GROUP owns: H1's public synthesis cites H1's claim and
/// X's claim; both are narrowed out of band, H1's to H1's group and X's to
/// X's group. After the sweep, the event (read by H1, stamped, on the
/// application login) names H1's claim and not X's, whose membership row row
/// security already hides from H1. Kills: the owner-group filter removed
/// from the event's claim list (X's id comes back to H1).
#[tokio::test]
async fn the_sweep_event_never_names_a_claim_hidden_from_the_synthesis_readers() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let x = principal(a, "x").await;
    let g = h1.personal_group;
    let own = public_claim(a, &h1).await;
    let foreign = public_claim(a, &x).await;
    let s = admin_synthesis(a, h1.agent, "complete", "public", g, None).await;
    for claim in [own, foreign] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(s)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
        sqlx::query("UPDATE claims SET visibility = 'group' WHERE id = $1")
            .bind(claim)
            .execute(a)
            .await
            .unwrap();
    }
    assert_eq!(
        support::claim_pair(a, foreign).await,
        ("group".to_string(), x.personal_group)
    );
    let maint = login_pool(&db, MAINT_LOGIN).await;
    let n: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(&maint)
        .await
        .expect("sweep");
    assert_eq!(n, 1);

    let app = ScopedPool::connect_with_options(
        &db.login_url(APP_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .unwrap();
    let v1 = viewer_of(a, h1.agent).await;
    let mut tx = app.begin_as(&v1).await.unwrap();
    assert_unprivileged(&mut tx).await;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT unnest(affected_claim_ids) FROM synthesis_staleness_events \
          WHERE synthesis_id = $1 AND trigger = 'input_narrowed'",
    )
    .bind(s)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(ids, vec![own], "only the claim H1's group owns");
}
