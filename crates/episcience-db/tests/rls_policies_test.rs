//! Row security of migration 5036, exercised on the REAL logins: the
//! EpiScience application login (`episcience_app`, stamped through the
//! kernel's production path `Viewer::resolve` + `ScopedPool::begin_as`, or
//! deliberately unstamped), and a throwaway login that is a member of the
//! kernel application role only. Each test names the mutation it kills.
//!
//! Row security, a missing privilege and the 5035 row guards all answer
//! 42501, so every refusal below is asserted by SQLSTATE AND message class,
//! and every case is built so that only the layer under test can refuse.
//!
//! Cast: H1, H2 (personal groups H1pg, H2pg); team T (H1 admin, H2 writer, R
//! reader).
mod support;
use support::{principal, team_group, viewer_of, Principal, TestDb, APP_LOGIN};

use epigraph_core::TenancyDecl;
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode, Viewer};
use sqlx::postgres::{PgPoolOptions, PgQueryResult};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

struct Cast {
    db: TestDb,
    app: ScopedPool,
    h1: Principal,
    h2: Principal,
    r: Principal,
    t: Uuid,
}

async fn cast() -> Cast {
    let db = TestDb::fresh().await;
    let h1 = principal(&db.admin, "h1").await;
    let h2 = principal(&db.admin, "h2").await;
    let r = principal(&db.admin, "reader").await;
    let t = team_group(&db.admin, &h1, &[(h2.agent, "writer"), (r.agent, "reader")]).await;
    let app = ScopedPool::connect_with_options(
        &db.login_url(APP_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .expect("ScopedPool on the app login");
    Cast {
        db,
        app,
        h1,
        h2,
        r,
        t,
    }
}

/// The brief's precondition for every application-login test.
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

/// `(SQLSTATE, message)` of a statement ("", "" when it succeeded).
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

/// A refusal BY ROW SECURITY (a WITH CHECK), not by a guard or a grant.
fn is_rls_refusal<T>(r: &Result<T, sqlx::Error>) -> bool {
    let (code, msg) = err(r);
    code == "42501" && msg.contains("violates row-level security policy")
}

/// A refusal by the principal guard (5036 `tenancy_05_principal`): a write by
/// a non-privileged session that carries no principal.
fn is_principal_refusal<T>(r: &Result<T, sqlx::Error>) -> bool {
    let (code, msg) = err(r);
    code == "42501" && msg.contains("needs a principal")
}

/// A refusal by a missing TABLE privilege.
fn is_privilege_refusal<T>(r: &Result<T, sqlx::Error>) -> bool {
    let (code, msg) = err(r);
    code == "42501" && msg.contains("permission denied for table")
}

// ─── Fixtures (admin pool: privileged, row security does not filter it) ──────

const INSERT_SYNTHESIS: &str =
    "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, \
     clustering_method, llm_provider, llm_model, content_hash, visibility, owner_group_id) \
     VALUES ($1, 'rls test', $2, 'pending', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
             decode(repeat('00', 32), 'hex'), $3, $4)";

async fn admin_synthesis(a: &PgPool, author: Uuid, vis: &str, owner: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(INSERT_SYNTHESIS)
        .bind(id)
        .bind(author)
        .bind(vis)
        .bind(owner)
        .execute(a)
        .await
        .expect("admin synthesis");
    id
}

async fn admin_sample(a: &PgPool, author: Uuid, vis: &str, owner: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(repeat('01', 32), 'hex'), $3, $4)",
    )
    .bind(id)
    .bind(author)
    .bind(owner)
    .bind(vis)
    .execute(a)
    .await
    .expect("admin sample");
    id
}

async fn admin_protocol(a: &PgPool, author: Uuid, vis: &str, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO protocols (title, authored_by, content_hash, owner_group_id, visibility) \
         VALUES ('p', $1, decode(repeat('02', 32), 'hex'), $2, $3) RETURNING id",
    )
    .bind(author)
    .bind(owner)
    .bind(vis)
    .fetch_one(a)
    .await
    .expect("admin protocol")
}

async fn admin_blob(a: &PgPool, author: Uuid, vis: &str, owner: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO blobs (filename, mime_type, size_bytes, content_hash, uploader_id, owner_group_id, visibility) \
         VALUES ('f', 'text/plain', 1, decode(repeat('03', 32), 'hex'), $1, $2, $3) RETURNING id",
    )
    .bind(author)
    .bind(owner)
    .bind(vis)
    .fetch_one(a)
    .await
    .expect("admin blob")
}

async fn public_claim(a: &PgPool, author: &Principal) -> Uuid {
    support::claim(
        a,
        author.agent,
        &format!("rls claim {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(author.personal_group),
    )
    .await
}

/// The rows a session inserts in the write matrix. `owner` is the declared
/// owner of the roots and the attestation; the derived rows name `synthesis`
/// or `sample` as their parent and take its pair.
struct Targets {
    owner: Uuid,
    synthesis: Uuid,
    sample: Uuid,
    claim: Uuid,
}

/// Every EpiScience write a session can attempt, by table: roots (declared),
/// the attestation (declared), and the derived rows (their pair comes from
/// the parent). The job row acts as the session principal (forced).
const WRITE_CASES: [&str; 13] = [
    "syntheses",
    "samples",
    "protocols",
    "blobs",
    "countersignatures",
    "synthesis_clusters",
    "synthesis_embeddings",
    "synthesis_staleness_events",
    "synthesis_provo_edges",
    "synthesis_claim_membership",
    "synthesis_jobs",
    "sample_claims",
    "blobs on a sample",
];

async fn try_insert(
    conn: &mut PgConnection,
    case: &str,
    who: Uuid,
    t: &Targets,
) -> Result<PgQueryResult, sqlx::Error> {
    let q = match case {
        "syntheses" => sqlx::query(INSERT_SYNTHESIS)
            .bind(Uuid::now_v7())
            .bind(who)
            .bind("group")
            .bind(t.owner),
        "samples" => sqlx::query(
            "INSERT INTO samples (name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
             VALUES ('s', 'chemical', $1, decode(repeat('04', 32), 'hex'), $2, 'group')",
        )
        .bind(who)
        .bind(t.owner),
        "protocols" => sqlx::query(
            "INSERT INTO protocols (title, authored_by, content_hash, owner_group_id, visibility) \
             VALUES ('p', $1, decode(repeat('05', 32), 'hex'), $2, 'group')",
        )
        .bind(who)
        .bind(t.owner),
        "blobs" => sqlx::query(
            "INSERT INTO blobs (filename, mime_type, size_bytes, content_hash, uploader_id, owner_group_id, visibility) \
             VALUES ('f', 'text/plain', 1, decode(repeat('06', 32), 'hex'), $1, $2, 'group')",
        )
        .bind(who)
        .bind(t.owner),
        "countersignatures" => sqlx::query(
            "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
                 signature, countersigned_by, owner_group_id, visibility, signature_hash) \
             VALUES ($1, $2, 'witnessed', decode(repeat('01', 32), 'hex'), decode(repeat('02', 64), 'hex'), $2, $3, 'group', \
                     decode(repeat('0a', 32), 'hex'))",
        )
        .bind(t.claim)
        .bind(who)
        .bind(t.owner),
        "synthesis_clusters" => sqlx::query(
            "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
                 member_claim_ids, support_count, contradict_count) \
             VALUES (gen_random_uuid(), $1, 7, 't', 's', ARRAY[gen_random_uuid()], 0, 0)",
        )
        .bind(t.synthesis),
        "synthesis_embeddings" => sqlx::query(
            "INSERT INTO synthesis_embeddings (synthesis_id, embedding, embedding_model, embedding_input) \
             VALUES ($1, array_fill(0.1::real, ARRAY[1536])::public.vector, 'm', 'narrative_head')",
        )
        .bind(t.synthesis),
        "synthesis_staleness_events" => sqlx::query(
            "INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids) \
             VALUES (gen_random_uuid(), $1, 'belief_drift', ARRAY[]::uuid[])",
        )
        .bind(t.synthesis),
        "synthesis_provo_edges" => sqlx::query(
            "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
             VALUES ($1, 'WAS_DERIVED_FROM', 'claim', $2)",
        )
        .bind(t.synthesis)
        .bind(t.claim),
        "synthesis_claim_membership" => sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(t.synthesis)
        .bind(t.claim),
        "synthesis_jobs" => {
            sqlx::query("INSERT INTO synthesis_jobs (id, payload) VALUES ($1, '{}'::jsonb)")
                .bind(t.synthesis)
        }
        "sample_claims" => {
            sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
                .bind(t.sample)
                .bind(t.claim)
        }
        "blobs on a sample" => sqlx::query(
            "INSERT INTO blobs (filename, mime_type, size_bytes, content_hash, uploader_id, sample_id) \
             VALUES ('f', 'text/plain', 1, decode(repeat('07', 32), 'hex'), $1, $2)",
        )
        .bind(who)
        .bind(t.sample),
        other => panic!("unknown case {other}"),
    };
    q.execute(&mut *conn).await
}

// ─── T-W1: WITH CHECK uses the WRITABLE set ─────────────────────────────────

/// T-W1, on every EpiScience write: R, a READER of team T (T is in R's
/// session groups, so every parent is visible and every guard passes), is
/// refused BY ROW SECURITY when the row would be owned by T: the roots and
/// the attestation it declares in T, and the derived rows that take T from
/// their parent. H2, a WRITER of T, makes each of the same inserts. Kills:
/// any table's WITH CHECK using the read set (`epigraph_session_groups`)
/// instead of the writable set; any table's tenancy policy dropped (H2's
/// control insert would then be refused too).
#[tokio::test]
async fn a_reader_cannot_write_into_a_group_it_can_read() {
    let c = cast().await;
    let a = &c.db.admin;
    let targets = Targets {
        owner: c.t,
        synthesis: admin_synthesis(a, c.h1.agent, "group", c.t).await,
        sample: admin_sample(a, c.h1.agent, "group", c.t).await,
        claim: public_claim(a, &c.h1).await,
    };
    let vr = viewer_of(a, c.r.agent).await;
    let v2 = viewer_of(a, c.h2.agent).await;
    assert!(vr.writable_groups().iter().all(|g| *g != c.t));
    for case in WRITE_CASES {
        let mut tx = c.app.begin_as(&vr).await.unwrap();
        assert_unprivileged(&mut tx).await;
        let r = try_insert(&mut tx, case, c.r.agent, &targets).await;
        assert!(
            is_rls_refusal(&r),
            "{case}: a reader of T must be refused by row security, got {:?}",
            err(&r)
        );
        drop(tx);

        let mut tx = c.app.begin_as(&v2).await.unwrap();
        let r = try_insert(&mut tx, case, c.h2.agent, &targets).await;
        assert_eq!(
            err(&r),
            (String::new(), String::new()),
            "{case}: a writer of T writes it"
        );
        drop(tx); // rolled back: every case starts from the same state
    }
}

// ─── T-R3: an unstamped application session ─────────────────────────────────

/// One public and one group row per tenancy table (H1's), written on the
/// admin pool. Returns `(table, public ids, group ids)` for the read check.
async fn public_and_group_rows(a: &PgPool, h1: &Principal) -> Vec<(&'static str, Uuid, Uuid)> {
    let g = h1.personal_group;
    let claim = public_claim(a, h1).await;
    let sp = admin_synthesis(a, h1.agent, "public", g).await;
    let sg = admin_synthesis(a, h1.agent, "group", g).await;
    let smp = admin_sample(a, h1.agent, "public", g).await;
    let smg = admin_sample(a, h1.agent, "group", g).await;
    let mut out = vec![
        ("syntheses", sp, sg),
        ("samples", smp, smg),
        (
            "protocols",
            admin_protocol(a, h1.agent, "public", g).await,
            admin_protocol(a, h1.agent, "group", g).await,
        ),
        (
            "blobs",
            admin_blob(a, h1.agent, "public", g).await,
            admin_blob(a, h1.agent, "group", g).await,
        ),
    ];
    let mut ids = Vec::new();
    for s in [sp, sg] {
        let cl: Uuid = sqlx::query_scalar(
            "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
                 member_claim_ids, support_count, contradict_count) \
             VALUES (gen_random_uuid(), $1, 0, 't', 's', ARRAY[gen_random_uuid()], 0, 0) RETURNING id",
        )
        .bind(s)
        .fetch_one(a)
        .await
        .unwrap();
        let ev: Uuid = sqlx::query_scalar(
            "INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids) \
             VALUES (gen_random_uuid(), $1, 'belief_drift', ARRAY[]::uuid[]) RETURNING id",
        )
        .bind(s)
        .fetch_one(a)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO synthesis_jobs (id, payload, principal_id) VALUES ($1, '{}'::jsonb, $2)",
        )
        .bind(s)
        .bind(h1.agent)
        .execute(a)
        .await
        .unwrap();
        ids.push((cl, ev));
    }
    out.push(("synthesis_clusters", ids[0].0, ids[1].0));
    out.push(("synthesis_staleness_events", ids[0].1, ids[1].1));
    out.push(("synthesis_jobs", sp, sg));
    for (s, cid) in [(sp, claim), (sg, claim)] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(s)
        .bind(cid)
        .execute(a)
        .await
        .unwrap();
    }
    for sm in [smp, smg] {
        sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
            .bind(sm)
            .bind(claim)
            .execute(a)
            .await
            .unwrap();
    }
    for (vis, meaning) in [("public", "witnessed"), ("group", "approved")] {
        sqlx::query(
            "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
                 signature, countersigned_by, owner_group_id, visibility) \
             VALUES ($1, $2, $5, decode(repeat('01', 32), 'hex'), decode(repeat('02', 64), 'hex'), $2, $3, $4)",
        )
        .bind(claim)
        .bind(h1.agent)
        .bind(g)
        .bind(vis)
        .bind(meaning)
        .execute(a)
        .await
        .unwrap();
    }
    out
}

/// T-R3: an application session with NO stamp (no principal, no groups):
/// - reads: exactly the public rows of every table, and no job at all (the
///   queue has no public arm);
/// - writes: EVERY write is refused 42501 by the principal guard (brief:
///   "every write is refused 42501"), including an UPDATE or DELETE of a
///   public row, which row security alone would turn into a silent 0-row
///   write; the rows are unchanged;
/// - a stamped principal that holds NO group: a derived insert under a PUBLIC
///   parent and a sample link (which every row guard admits: the parent is
///   visible and the row takes its pair) are refused by row security; its
///   UPDATE of a public row matches nothing.
///
/// Kills: a world arm (`OR true`) in any read policy, a public arm in the
/// queue's read policy, a WITH CHECK admitting an empty group set, dropping
/// ENABLE on a table, the principal guard dropped (the unstamped UPDATE and
/// DELETE become 0-row successes).
#[tokio::test]
async fn an_unstamped_application_session_reads_public_rows_and_writes_nothing() {
    let c = cast().await;
    let a = &c.db.admin;
    let rows = public_and_group_rows(a, &c.h1).await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(c.db.login_options(APP_LOGIN))
        .await
        .expect("unstamped app pool");
    let mut conn = pool.acquire().await.unwrap();
    assert_unprivileged(&mut conn).await;
    let principal: Option<String> =
        sqlx::query_scalar("SELECT nullif(current_setting('epigraph.principal_id', true), '')")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(principal, None, "the session carries no principal");

    for (table, public, group) in &rows {
        let seen: Vec<Uuid> = sqlx::query_scalar(&format!(
            "SELECT id FROM {table} WHERE id = ANY($1) ORDER BY 1"
        ))
        .bind(vec![*public, *group])
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("read {table}: {e}"));
        let want = if *table == "synthesis_jobs" {
            vec![]
        } else {
            vec![*public]
        };
        assert_eq!(
            seen, want,
            "{table}: an unstamped session sees public rows only"
        );
    }
    for (table, key) in [
        ("synthesis_claim_membership", "synthesis_id"),
        ("sample_claims", "sample_id"),
    ] {
        let (public, group) = if table == "sample_claims" {
            (rows[1].1, rows[1].2)
        } else {
            (rows[0].1, rows[0].2)
        };
        let seen: Vec<Uuid> =
            sqlx::query_scalar(&format!("SELECT {key} FROM {table} WHERE {key} = ANY($1)"))
                .bind(vec![public, group])
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert_eq!(seen, vec![public], "{table}: public rows only");
    }
    let vis: Vec<String> = sqlx::query_scalar(
        "SELECT visibility::text FROM countersignatures WHERE countersigned_by = $1",
    )
    .bind(c.h1.agent)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(vis, vec!["public".to_string()], "countersignatures");

    let (sp, smp) = (rows[0].1, rows[1].1);
    let cluster_under = |s: Uuid| {
        sqlx::query(
            "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
                 member_claim_ids, support_count, contradict_count) \
             VALUES (gen_random_uuid(), $1, 9, 't', 's', ARRAY[gen_random_uuid()], 0, 0)",
        )
        .bind(s)
    };
    let r = cluster_under(sp).execute(&mut *conn).await;
    assert!(
        is_principal_refusal(&r),
        "a derived row under a public parent: {:?}",
        err(&r)
    );
    let link_claim = public_claim(a, &c.h2).await;
    let r = sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(smp)
        .bind(link_claim)
        .execute(&mut *conn)
        .await;
    assert!(is_principal_refusal(&r), "a sample link: {:?}", err(&r));
    let r = sqlx::query(INSERT_SYNTHESIS)
        .bind(Uuid::now_v7())
        .bind(c.h1.agent)
        .bind("public")
        .bind(c.h1.personal_group)
        .execute(&mut *conn)
        .await;
    assert!(is_principal_refusal(&r), "a root: {:?}", err(&r));
    let fresh = admin_synthesis(a, c.h1.agent, "public", c.h1.personal_group).await;
    let r = sqlx::query("INSERT INTO synthesis_jobs (id, payload) VALUES ($1, '{}'::jsonb)")
        .bind(fresh)
        .execute(&mut *conn)
        .await;
    assert!(is_principal_refusal(&r), "a job: {:?}", err(&r));
    for (table, public, _) in &rows {
        if *table == "synthesis_jobs" {
            continue;
        }
        let set = match *table {
            "syntheses" => "query = 'changed'",
            "samples" => "name = 'changed'",
            "protocols" => "title = 'changed'",
            "blobs" => "filename = 'changed'",
            "synthesis_clusters" => "title = 'changed'",
            _ => "detail = '{}'::jsonb",
        };
        let u = sqlx::query(&format!("UPDATE {table} SET {set} WHERE id = $1"))
            .bind(*public)
            .execute(&mut *conn)
            .await;
        assert!(
            is_principal_refusal(&u),
            "{table}: unstamped UPDATE is refused, never a silent 0-row write: {:?}",
            err(&u)
        );
        let d = sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
            .bind(*public)
            .execute(&mut *conn)
            .await;
        assert!(
            is_principal_refusal(&d),
            "{table}: unstamped DELETE: {:?}",
            err(&d)
        );
        let still: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE id = $1 AND {}",
            set.replace('=', "IS DISTINCT FROM")
        ))
        .bind(*public)
        .fetch_one(a)
        .await
        .unwrap();
        assert_eq!(still, 1, "{table}: the row is unchanged and present");
    }
    drop(conn);

    // A principal that holds no group at all (its personal membership
    // revoked): past the principal guard, the WITH CHECK is the layer that
    // refuses, and the owner policies filter its UPDATE and DELETE.
    let lone = support::principal(a, "lone").await;
    sqlx::query("UPDATE group_memberships SET revoked_at = now() WHERE agent_id = $1")
        .bind(lone.agent)
        .execute(a)
        .await
        .unwrap();
    let vl = viewer_of(a, lone.agent).await;
    assert!(vl.writable_groups().is_empty(), "no writable group");
    let mut tx = c.app.begin_as(&vl).await.unwrap();
    let r = cluster_under(sp).execute(&mut *tx).await;
    assert!(
        is_rls_refusal(&r),
        "a derived row under a public parent, no group: {:?}",
        err(&r)
    );
    drop(tx);
    let mut tx = c.app.begin_as(&vl).await.unwrap();
    let r = sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(smp)
        .bind(link_claim)
        .execute(&mut *tx)
        .await;
    assert!(is_rls_refusal(&r), "a sample link, no group: {:?}", err(&r));
    drop(tx);
    let mut tx = c.app.begin_as(&vl).await.unwrap();
    let u = sqlx::query("UPDATE syntheses SET query = 'changed' WHERE id = $1")
        .bind(sp)
        .execute(&mut *tx)
        .await
        .expect("a stamped UPDATE the owner policies filter");
    assert_eq!(u.rows_affected(), 0, "a public row it does not own");
}

// ─── The principal guard: groups without a principal ───────────────────────

/// A session that carries groups but NO principal (only a forged or broken
/// stamp can produce it: `ScopedPool::begin_as` always sets the principal,
/// so the stamp here is set by hand, as an attacker would) is refused
/// (42501, the principal guard) on every write shape the row guards would
/// otherwise admit by group alone: a derived INSERT under its own synthesis,
/// a root UPDATE, a root DELETE, an attachment. The same statements stamped
/// through `begin_as` succeed (control), and a privileged cascade (H1
/// deleting its own synthesis, whose children go by foreign-key action; H1
/// deleting a sample whose blob is detached by `ON DELETE SET NULL`) passes
/// the guard. Kills: the guard dropped from any of these tables, a guard
/// that exempts a session holding groups, a guard that also refuses
/// foreign-key actions.
#[tokio::test]
async fn a_session_with_groups_but_no_principal_writes_nothing() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let s = admin_synthesis(a, c.h1.agent, "group", g).await;
    let sm = admin_sample(a, c.h1.agent, "group", g).await;
    let blob = admin_blob(a, c.h1.agent, "group", g).await;
    sqlx::query("UPDATE blobs SET sample_id = $2 WHERE id = $1")
        .bind(blob)
        .bind(sm)
        .execute(a)
        .await
        .unwrap();
    let claim = public_claim(a, &c.h1).await;
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(c.db.login_options(APP_LOGIN))
        .await
        .unwrap();
    let writes: [(&str, String); 6] = [
        (
            "synthesis_staleness_events",
            format!(
                "INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids) \
                 VALUES (gen_random_uuid(), '{s}', 'belief_drift', ARRAY[]::uuid[])"
            ),
        ),
        (
            "synthesis_clusters",
            format!(
                "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
                     member_claim_ids, support_count, contradict_count) \
                 VALUES (gen_random_uuid(), '{s}', 3, 't', 's', ARRAY[gen_random_uuid()], 0, 0)"
            ),
        ),
        (
            "syntheses",
            format!("UPDATE syntheses SET query = 'forged' WHERE id = '{s}'"),
        ),
        (
            "sample_claims",
            format!("INSERT INTO sample_claims (sample_id, claim_id) VALUES ('{sm}', '{claim}')"),
        ),
        (
            "samples",
            format!("UPDATE samples SET name = 'forged' WHERE id = '{sm}'"),
        ),
        (
            "syntheses",
            format!("DELETE FROM syntheses WHERE id = '{s}'"),
        ),
    ];
    for (table, sql) in &writes {
        let mut tx = pool.begin().await.unwrap();
        assert_unprivileged(&mut tx).await;
        sqlx::query(
            "SELECT set_config('epigraph.group_ids', $1, true), \
                    set_config('epigraph.writable_group_ids', $1, true)",
        )
        .bind(g.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
        let r = sqlx::query(sql).execute(&mut *tx).await;
        assert!(
            is_principal_refusal(&r),
            "{table}: groups without a principal: {sql}: {:?}",
            err(&r)
        );
    }

    // Control: the same writes through the production stamp succeed (the
    // DELETE last: it cascades to the children the inserts created, and the
    // sample delete detaches the blob, both by foreign-key action).
    let v1 = viewer_of(a, c.h1.agent).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    for (table, sql) in &writes {
        let r = sqlx::query(sql).execute(&mut *tx).await;
        assert_eq!(err(&r), (String::new(), String::new()), "{table}: {sql}");
    }
    sqlx::query("DELETE FROM samples WHERE id = $1")
        .bind(sm)
        .execute(&mut *tx)
        .await
        .expect("the owner deletes its sample; the blob is detached by FK action");
    tx.commit().await.unwrap();
    let (children, detached): (i64, bool) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM synthesis_clusters WHERE synthesis_id = $1) \
              + (SELECT count(*) FROM synthesis_staleness_events WHERE synthesis_id = $1), \
                (SELECT sample_id IS NULL FROM blobs WHERE id = $2)",
    )
    .bind(s)
    .bind(blob)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!((children, detached), (0, true), "the FK actions ran");
}

// ─── T-W5: a public row is readable by all, editable by its owners only ────

/// T-W5: H2 (not in H1pg) READS H1's public rows, and its UPDATE and DELETE
/// of each match NOTHING (the RESTRICTIVE owner policies filter the row in
/// USING; no error), leaving the rows unchanged; H1 updates and deletes each
/// (control, rolled back). Kills: dropping a table's `<t>_update_owner`
/// (H2's UPDATE would then reach the permissive WITH CHECK and fail with an
/// error instead of matching nothing) or `<t>_delete_owner` (H2's DELETE
/// would remove the row).
#[tokio::test]
async fn a_public_row_is_editable_by_its_owners_only() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let sp = admin_synthesis(a, c.h1.agent, "public", g).await;
    let cluster: Uuid = sqlx::query_scalar(
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
             member_claim_ids, support_count, contradict_count) \
         VALUES (gen_random_uuid(), $1, 0, 't', 's', ARRAY[gen_random_uuid()], 0, 0) RETURNING id",
    )
    .bind(sp)
    .fetch_one(a)
    .await
    .unwrap();
    let rows = [
        ("syntheses", sp, "query = 'changed'"),
        (
            "samples",
            admin_sample(a, c.h1.agent, "public", g).await,
            "name = 'changed'",
        ),
        (
            "protocols",
            admin_protocol(a, c.h1.agent, "public", g).await,
            "title = 'changed'",
        ),
        (
            "blobs",
            admin_blob(a, c.h1.agent, "public", g).await,
            "filename = 'changed'",
        ),
        ("synthesis_clusters", cluster, "title = 'changed'"),
    ];
    let v1 = viewer_of(a, c.h1.agent).await;
    let v2 = viewer_of(a, c.h2.agent).await;
    for (table, id, set) in rows {
        let mut tx = c.app.begin_as(&v2).await.unwrap();
        assert_unprivileged(&mut tx).await;
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE id = $1"))
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, 1, "{table}: H2 reads H1's public row");
        let u = sqlx::query(&format!("UPDATE {table} SET {set} WHERE id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await;
        assert_eq!(err(&u), (String::new(), String::new()), "{table}: UPDATE");
        assert_eq!(
            u.unwrap().rows_affected(),
            0,
            "{table}: H2's UPDATE matches nothing"
        );
        let d = sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await;
        assert_eq!(err(&d), (String::new(), String::new()), "{table}: DELETE");
        assert_eq!(
            d.unwrap().rows_affected(),
            0,
            "{table}: H2's DELETE matches nothing"
        );
        tx.commit().await.unwrap();
        let unchanged: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE id = $1 AND NOT ({set})"
        ))
        .bind(id)
        .fetch_one(a)
        .await
        .unwrap();
        assert_eq!(unchanged, 1, "{table}: unchanged and present");

        let mut tx = c.app.begin_as(&v1).await.unwrap();
        let u = sqlx::query(&format!("UPDATE {table} SET {set} WHERE id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{table}: the owner's UPDATE: {e}"));
        assert_eq!(u.rows_affected(), 1, "{table}: the owner edits");
        let d = sqlx::query(&format!("DELETE FROM {table} WHERE id = $1"))
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{table}: the owner's DELETE: {e}"));
        assert_eq!(d.rows_affected(), 1, "{table}: the owner deletes");
        drop(tx);
    }
}

// ─── T-W16: propagation through the bypass-only queue policy ────────────────

/// T-W16: H1 widens its group synthesis on the APPLICATION session; the
/// propagation (a maintenance-owned definer) moves every child to the new
/// pair, INCLUDING the job row, which the application may not update itself
/// (it holds no UPDATE privilege on the queue). Kills: dropping
/// `synthesis_jobs_bypass_update` (the propagation's count check then
/// refuses the widening), granting the application UPDATE on the queue.
#[tokio::test]
async fn a_widening_moves_every_child_including_the_job_the_app_cannot_update() {
    let c = cast().await;
    let a = &c.db.admin;
    let v1 = viewer_of(a, c.h1.agent).await;
    let claim = public_claim(a, &c.h1).await;
    let s = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let targets = Targets {
        owner: c.h1.personal_group,
        synthesis: s,
        sample: Uuid::nil(),
        claim,
    };
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    for case in [
        "synthesis_clusters",
        "synthesis_embeddings",
        "synthesis_staleness_events",
        "synthesis_provo_edges",
        "synthesis_claim_membership",
        "synthesis_jobs",
    ] {
        try_insert(&mut tx, case, c.h1.agent, &targets)
            .await
            .unwrap_or_else(|e| panic!("{case}: {e}"));
    }
    tx.commit().await.unwrap();

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let r = sqlx::query("UPDATE synthesis_jobs SET last_error = 'x' WHERE id = $1")
        .bind(s)
        .execute(&mut *tx)
        .await;
    assert!(
        is_privilege_refusal(&r),
        "the application cannot update the queue: {:?}",
        err(&r)
    );
    drop(tx);

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    sqlx::query("SELECT set_config('episcience.allow_widen', 'yes', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let u = sqlx::query("UPDATE syntheses SET visibility = 'public' WHERE id = $1")
        .bind(s)
        .execute(&mut *tx)
        .await
        .expect("widening with the interlock and public inputs");
    assert_eq!(u.rows_affected(), 1);
    tx.commit().await.unwrap();

    for (table, key) in [
        ("synthesis_clusters", "synthesis_id"),
        ("synthesis_embeddings", "synthesis_id"),
        ("synthesis_staleness_events", "synthesis_id"),
        ("synthesis_provo_edges", "synthesis_id"),
        ("synthesis_claim_membership", "synthesis_id"),
        ("synthesis_jobs", "id"),
    ] {
        let pairs: Vec<(Uuid, String)> = sqlx::query_as(&format!(
            "SELECT owner_group_id, visibility::text FROM {table} WHERE {key} = $1"
        ))
        .bind(s)
        .fetch_all(a)
        .await
        .unwrap();
        assert_eq!(
            pairs,
            vec![(c.h1.personal_group, "public".to_string())],
            "{table} follows its parent"
        );
    }
}

// ─── T-R7: rows citing a claim follow the claim's visibility ────────────────

/// T-R7: H2's rows citing H1's claim (a synthesis membership, a sample link,
/// an attestation, a PROV outbox row targeting the claim), all PUBLIC and
/// owned by H2, are readable by both while the claim is public. Once the
/// claim is narrowed to `group(H1pg)` out of band, H2 no longer sees any of
/// them (and still sees its outbox row that targets an agent), while H1
/// still sees all four. Kills: dropping any table's `<t>_claim_visible`, or
/// the provo edge policy's `target_kind` arm (the agent-target row would
/// disappear too).
#[tokio::test]
async fn rows_citing_a_claim_disappear_when_the_claim_narrows() {
    let c = cast().await;
    let a = &c.db.admin;
    let claim = public_claim(a, &c.h1).await;
    let g2 = c.h2.personal_group;
    let s = admin_synthesis(a, c.h2.agent, "public", g2).await;
    let sm = admin_sample(a, c.h2.agent, "public", g2).await;
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(s)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sm)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
             signature, countersigned_by, owner_group_id, visibility) \
         VALUES ($1, $2, 'witnessed', decode(repeat('01', 32), 'hex'), decode(repeat('02', 64), 'hex'), $2, $3, 'public')",
    )
    .bind(claim)
    .bind(c.h2.agent)
    .bind(g2)
    .execute(a)
    .await
    .unwrap();
    for (kind, target) in [("claim", claim), ("agent", c.h2.agent)] {
        sqlx::query(
            "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
             VALUES ($1, 'ATTRIBUTED_TO', $2, $3)",
        )
        .bind(s)
        .bind(kind)
        .bind(target)
        .execute(a)
        .await
        .unwrap();
    }

    async fn visible(app: &ScopedPool, v: &Viewer, s: Uuid, sm: Uuid, claim: Uuid) -> [i64; 5] {
        let mut tx = app.begin_as(v).await.unwrap();
        let mut out = [0i64; 5];
        for (i, (sql, id)) in [
            (
                "SELECT count(*) FROM synthesis_claim_membership WHERE synthesis_id = $1",
                s,
            ),
            ("SELECT count(*) FROM sample_claims WHERE sample_id = $1", sm),
            (
                "SELECT count(*) FROM countersignatures WHERE claim_id = $1",
                claim,
            ),
            (
                "SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND target_kind = 'claim'",
                s,
            ),
            (
                "SELECT count(*) FROM synthesis_provo_edges WHERE synthesis_id = $1 AND target_kind = 'agent'",
                s,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            out[i] = sqlx::query_scalar(sql)
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        }
        out
    }

    let v1 = viewer_of(a, c.h1.agent).await;
    let v2 = viewer_of(a, c.h2.agent).await;
    assert_eq!(visible(&c.app, &v2, s, sm, claim).await, [1, 1, 1, 1, 1]);
    assert_eq!(visible(&c.app, &v1, s, sm, claim).await, [1, 1, 1, 1, 1]);

    sqlx::query("UPDATE claims SET visibility = 'group' WHERE id = $1")
        .bind(claim)
        .execute(a)
        .await
        .expect("narrow the claim out of band");
    assert_eq!(
        support::claim_pair(a, claim).await,
        ("group".to_string(), c.h1.personal_group)
    );
    assert_eq!(
        visible(&c.app, &v2, s, sm, claim).await,
        [0, 0, 0, 0, 1],
        "H2 no longer sees what cites the narrowed claim"
    );
    assert_eq!(
        visible(&c.app, &v1, s, sm, claim).await,
        [1, 1, 1, 1, 1],
        "H1 (the claim's group) still sees every row"
    );
}

// ─── T-R6: the kernel application role ─────────────────────────────────────

/// T-R6: a login that is a member of the kernel application role ONLY (a
/// throwaway, the shape of the kernel API's own login), stamped as H1:
/// - the kernel's edge-reference check (`validate_edge_reference`, the
///   overload the edges trigger calls) is true for H1's group synthesis and
///   false for H2's: it reads `syntheses` under the policies, through the
///   kept-SELECT grant;
/// - SELECT is refused on every other EpiScience table, and INSERT, UPDATE
///   and DELETE on all 14 (permission denied, not row security).
///
/// The throwaway role is dropped before any assertion. Kills: the kernel app
/// role keeping a write on any table (a REVOKE removed), SELECT on a table
/// outside the kept set, the kept-SELECT grant lost (the check, which turns
/// any error into false, would then say false for H1's own synthesis).
#[tokio::test]
async fn the_kernel_app_role_reads_the_kept_select_set_and_writes_nothing() {
    let c = cast().await;
    let a = &c.db.admin;
    let kept: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM public.entity_types \
          WHERE schema_name = 'public' AND table_name = ANY($1) ORDER BY 1",
    )
    .bind(episcience_db::ledger::EPISCIENCE_TABLES.to_vec())
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(
        kept,
        vec!["syntheses".to_string()],
        "the registry names syntheses"
    );
    let s1 = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let s2 = admin_synthesis(a, c.h2.agent, "group", c.h2.personal_group).await;
    let mut columns = Vec::new();
    for table in episcience_db::ledger::EPISCIENCE_TABLES {
        let col: String = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
              WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position LIMIT 1",
        )
        .bind(table)
        .fetch_one(a)
        .await
        .unwrap();
        columns.push((table, col));
    }

    let tag = &Uuid::new_v4().simple().to_string()[..8];
    let role = format!("episcience_e1e_tmp_kapp_{tag}");
    let pw = Uuid::new_v4().simple().to_string();
    sqlx::raw_sql(&format!(
        "CREATE ROLE {role} LOGIN PASSWORD '{pw}' NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE INHERIT; \
         GRANT epigraph_app TO {role};"
    ))
    .execute(a)
    .await
    .expect("create the throwaway kernel-app login");

    // Everything is collected as text first; the role is dropped before any
    // assertion can panic.
    let mut seen: Vec<(String, String)> = Vec::new();
    let v1 = viewer_of(a, c.h1.agent).await;
    match ScopedPool::connect_with_options(
        &c.db.login_url((role.as_str(), pw.as_str())),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    {
        Err(e) => seen.push(("connect".into(), e.to_string())),
        Ok(kapp) => {
            let (kapp_ref, v1_ref) = (&kapp, &v1);
            let one = |sql: String, bind: Option<Uuid>| async move {
                {
                    let mut tx = match kapp_ref.begin_as(v1_ref).await {
                        Ok(tx) => tx,
                        Err(e) => return format!("begin: {e}"),
                    };
                    let mut q = sqlx::query_scalar::<_, String>(&sql);
                    if let Some(b) = bind {
                        q = q.bind(b);
                    }
                    match q.fetch_optional(&mut *tx).await {
                        Ok(v) => format!("ok {}", v.unwrap_or_default()),
                        Err(e) => {
                            let (code, msg) = err(&Err::<(), _>(e));
                            format!("{code} {msg}")
                        }
                    }
                }
            };
            seen.push((
                "identity".into(),
                one(
                    "SELECT concat_ws(',', r.rolsuper, r.rolbypassrls, \
                         pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER'), \
                         pg_has_role(session_user, 'epigraph_app', 'USAGE'), \
                         pg_has_role(session_user, 'episcience_rw', 'MEMBER')) \
                       FROM pg_roles r WHERE r.rolname = session_user"
                        .into(),
                    None,
                )
                .await,
            ));
            for (label, s) in [("h1", s1), ("h2", s2)] {
                seen.push((
                    format!("validate {label}"),
                    one(
                        "SELECT public.validate_edge_reference($1, 'synthesis'::varchar)::text"
                            .into(),
                        Some(s),
                    )
                    .await,
                ));
            }
            for (table, col) in &columns {
                seen.push((
                    format!("select {table}"),
                    one(
                        format!(
                            "SELECT count(*)::text FROM public.{table} WHERE {col} IS NOT NULL"
                        ),
                        None,
                    )
                    .await,
                ));
                seen.push((
                    format!("insert {table}"),
                    one(
                        format!("INSERT INTO public.{table} DEFAULT VALUES RETURNING ''"),
                        None,
                    )
                    .await,
                ));
                seen.push((
                    format!("update {table}"),
                    one(
                        format!("UPDATE public.{table} SET {col} = {col} WHERE false RETURNING ''"),
                        None,
                    )
                    .await,
                ));
                seen.push((
                    format!("delete {table}"),
                    one(
                        format!("DELETE FROM public.{table} WHERE false RETURNING ''"),
                        None,
                    )
                    .await,
                ));
            }
            kapp.inner().close().await;
        }
    }
    sqlx::raw_sql(&format!("DROP ROLE IF EXISTS {role};"))
        .execute(a)
        .await
        .expect("drop the throwaway login");

    let get = |k: &str| {
        seen.iter()
            .find(|(l, _)| l == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("no result for {k}: {seen:?}"))
    };
    assert_eq!(
        get("identity"),
        "ok f,f,f,t,f",
        "unprivileged, inherits the kernel app role only"
    );
    assert_eq!(get("validate h1"), "ok true", "H1's own group synthesis");
    assert_eq!(get("validate h2"), "ok false", "H2's group synthesis");
    for (table, _) in &columns {
        let s = get(&format!("select {table}"));
        if *table == "syntheses" {
            assert_eq!(s, "ok 1", "syntheses: the kept SELECT, under the policies");
        } else {
            assert!(
                s.starts_with("42501 permission denied for table"),
                "select {table}: {s}"
            );
        }
        for verb in ["insert", "update", "delete"] {
            let s = get(&format!("{verb} {table}"));
            assert!(
                s.starts_with("42501 permission denied for table"),
                "{verb} {table}: {s}"
            );
        }
    }
}
