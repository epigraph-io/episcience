//! Publishability when a cited claim is hidden from the owner (5037
//! `episcience_members_all_public`), on the REAL application login stamped
//! through the kernel's production path (`Viewer::resolve` +
//! `ScopedPool::begin_as`).
//!
//! Under row security a claim narrowed out of the owner's reach hides the
//! MEMBERSHIP row that cites it (the RESTRICTIVE `<t>_claim_visible`
//! policies), so a publishability rule that counted only the session's
//! membership rows found nothing to object to. Each test builds exactly that
//! state: H1 owns a synthesis or sample citing a claim of another principal
//! X, public when it was cited and narrowed afterwards to X's own group, so
//! H1 can read neither the claim nor the row that cites it.
//!
//! Cast: H1, X (personal groups H1pg, Xpg), H2 (a bystander).
mod support;
use support::{principal, viewer_of, Principal, TestDb, APP_LOGIN, MAINT_LOGIN};

use epigraph_core::TenancyDecl;
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

struct Cast {
    db: TestDb,
    app: ScopedPool,
    h1: Principal,
    x: Principal,
    h2: Principal,
}

async fn cast() -> Cast {
    let db = TestDb::fresh().await;
    let h1 = principal(&db.admin, "h1").await;
    let x = principal(&db.admin, "x").await;
    let h2 = principal(&db.admin, "h2").await;
    let app = ScopedPool::connect_with_options(
        &db.login_url(APP_LOGIN),
        SessionGucMode::Session,
        ScopedPoolOptions::default(),
    )
    .await
    .expect("ScopedPool on the app login");
    Cast { db, app, h1, x, h2 }
}

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

/// A claim of `author`, public in `author`'s personal group.
async fn public_claim(a: &PgPool, author: &Principal) -> Uuid {
    support::claim(
        a,
        author.agent,
        &format!("publishability claim {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(author.personal_group),
    )
    .await
}

/// Narrow `claim` to its owner's group, out of band (the kernel's own
/// privatization path is not what is under test).
async fn narrow(a: &PgPool, claim: Uuid) {
    sqlx::query("UPDATE claims SET visibility = 'group' WHERE id = $1")
        .bind(claim)
        .execute(a)
        .await
        .expect("narrow the claim out of band");
}

async fn admin_synthesis(a: &PgPool, author: Uuid, status: &str, vis: &str, owner: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id, narrative, completed_at) \
         VALUES ($1, 'publishability', $2, $3, '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), $4, $5, \
                 CASE WHEN $3 = 'complete' THEN 'n' END, CASE WHEN $3 = 'complete' THEN now() END)",
    )
    .bind(id)
    .bind(author)
    .bind(status)
    .bind(vis)
    .bind(owner)
    .execute(a)
    .await
    .expect("admin synthesis");
    id
}

async fn member(a: &PgPool, synthesis: Uuid, claim: Uuid) {
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(synthesis)
        .bind(claim)
        .execute(a)
        .await
        .expect("membership");
}

async fn admin_sample(
    a: &PgPool,
    author: Uuid,
    vis: &str,
    owner: Uuid,
    parent: Option<Uuid>,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, parent_sample_id, \
                              owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, $4, $5)",
    )
    .bind(id)
    .bind(author)
    .bind(parent)
    .bind(owner)
    .bind(vis)
    .execute(a)
    .await
    .expect("admin sample");
    id
}

async fn attach(a: &PgPool, sample: Uuid, claim: Uuid) {
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sample)
        .bind(claim)
        .execute(a)
        .await
        .expect("sample claim");
}

async fn visibility(a: &PgPool, table: &str, id: Uuid) -> String {
    sqlx::query_scalar(&format!(
        "SELECT visibility::text FROM {table} WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(a)
    .await
    .unwrap()
}

/// Widen `id` in `table` to public as `viewer`, with the interlock set.
async fn widen(
    app: &ScopedPool,
    viewer: &epigraph_db::Viewer,
    table: &str,
    id: Uuid,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    let mut tx = app.begin_as(viewer).await.unwrap();
    assert_unprivileged(&mut tx).await;
    sqlx::query("SELECT set_config('episcience.allow_widen', 'yes', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    let r = sqlx::query(&format!(
        "UPDATE {table} SET visibility = 'public' WHERE id = $1"
    ))
    .bind(id)
    .execute(&mut *tx)
    .await;
    if r.is_ok() {
        tx.commit().await.unwrap();
    }
    r
}

/// T-W10b: H1 widens its `group` synthesis and its `group` sample, each
/// citing a claim of X that was public when cited and is now narrowed to
/// X's group. Stamped as H1 the citing rows are invisible (the state that
/// made the old member check vacuous) and both widenings are refused (42501,
/// the widening guard); the rows stay `group`. Control: the same H1 widens a
/// synthesis and a sample citing a claim of X that is STILL public. Kills:
/// the member half of either helper restored to a count over the session's
/// own rows (both widenings then succeed), a definer that answers false for
/// everything (the controls then fail).
#[tokio::test]
async fn widening_is_refused_when_a_cited_claim_is_hidden_from_the_owner() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let hidden = public_claim(a, &c.x).await;
    let hidden2 = public_claim(a, &c.x).await;
    let open = public_claim(a, &c.x).await;
    let s = admin_synthesis(a, c.h1.agent, "complete", "group", g).await;
    member(a, s, hidden).await;
    let s_ok = admin_synthesis(a, c.h1.agent, "complete", "group", g).await;
    member(a, s_ok, open).await;
    let sm = admin_sample(a, c.h1.agent, "group", g, None).await;
    attach(a, sm, hidden2).await;
    let sm_ok = admin_sample(a, c.h1.agent, "group", g, None).await;
    attach(a, sm_ok, open).await;
    narrow(a, hidden).await;
    narrow(a, hidden2).await;
    assert_eq!(
        support::claim_pair(a, hidden).await,
        ("group".to_string(), c.x.personal_group)
    );

    let v1 = viewer_of(a, c.h1.agent).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let (members, links): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM synthesis_claim_membership WHERE synthesis_id = $1), \
                (SELECT count(*) FROM sample_claims WHERE sample_id = $2)",
    )
    .bind(s)
    .bind(sm)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        (members, links),
        (0, 0),
        "H1 cannot see the rows that cite the narrowed claims"
    );
    drop(tx);

    for (table, id, what) in [("syntheses", s, "synthesis"), ("samples", sm, "sample")] {
        let r = widen(&c.app, &v1, table, id).await;
        let (code, msg) = err(&r);
        assert_eq!(code, "42501", "{table}: {msg}");
        assert!(
            msg.contains(&format!("{what} {id} cannot be public")),
            "{table}: refused by the widening guard: {msg}"
        );
        assert_eq!(visibility(a, table, id).await, "group", "{table}");
    }
    for (table, id) in [("syntheses", s_ok), ("samples", sm_ok)] {
        let r = widen(&c.app, &v1, table, id).await;
        assert_eq!(err(&r), (String::new(), String::new()), "{table} control");
        assert_eq!(visibility(a, table, id).await, "public", "{table} control");
    }
}

/// The publish rule (completion) on the application login: H1's PUBLIC
/// running synthesis cites X's claim, which is narrowed before the job
/// completes; H1's completing UPDATE stores the synthesis as `group`, stale
/// `input_narrowed`. And the stage-6 gate `publish::is_publishable`, asked on
/// H1's stamped transaction, says false for it before completion (so no
/// kernel edge or event names it). Control: a public synthesis citing a
/// still-public claim completes public and the gate says true. Kills: the
/// publish rule's member half restored to the session's own rows (the
/// synthesis completes public), `is_publishable` restored to its own inline
/// member count over the session's rows (it answers true).
#[tokio::test]
async fn completion_narrows_and_stage_six_withholds_a_synthesis_citing_a_hidden_claim() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let hidden = public_claim(a, &c.x).await;
    let open = public_claim(a, &c.x).await;
    let s = admin_synthesis(a, c.h1.agent, "running", "public", g).await;
    member(a, s, hidden).await;
    let s_ok = admin_synthesis(a, c.h1.agent, "running", "public", g).await;
    member(a, s_ok, open).await;
    narrow(a, hidden).await;

    let v1 = viewer_of(a, c.h1.agent).await;
    for (id, want_gate, want_vis) in [(s, false, "group"), (s_ok, true, "public")] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_unprivileged(&mut tx).await;
        let gate = episcience_db::synthesis::publish::is_publishable(&mut *tx, id)
            .await
            .expect("the stage-6 gate on the stamped transaction");
        assert_eq!(gate, want_gate, "{id}: the stage-6 gate");
        sqlx::query(
            "UPDATE syntheses SET status = 'complete', narrative = 'n', completed_at = now() \
              WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect("the owner completes its synthesis");
        tx.commit().await.unwrap();
        assert_eq!(visibility(a, "syntheses", id).await, want_vis, "{id}");
    }
    let reason: Option<String> =
        sqlx::query_scalar("SELECT stale_reason FROM syntheses WHERE id = $1")
            .bind(s)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("input_narrowed"));
}

/// The definer answers only about a row the caller may read: H2 (no
/// membership in H1pg), stamped, asking about H1's `group` synthesis and
/// sample that cite a now-hidden claim, gets `true` (the answer for a row
/// with no members, and for an id that does not exist), never the `false` H1
/// gets. The privileged runtime (the migration owner, unstamped: E1e's live
/// runtime) widens a `group` synthesis whose members are all public.
/// Kills: the caller-visibility check removed from the definer (H2 learns
/// `false`), that check made to accept the definer's own maintenance bypass
/// (the same), a check that refuses the privileged runtime (its widening
/// fails).
#[tokio::test]
async fn the_member_definer_says_nothing_about_a_row_the_caller_cannot_read() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let hidden = public_claim(a, &c.x).await;
    let s = admin_synthesis(a, c.h1.agent, "complete", "group", g).await;
    member(a, s, hidden).await;
    let sm = admin_sample(a, c.h1.agent, "group", g, None).await;
    attach(a, sm, hidden).await;
    narrow(a, hidden).await;
    let ask = "SELECT public.episcience_members_all_public($1, $2)";
    for (who, want) in [(c.h2.agent, true), (c.h1.agent, false)] {
        let v = viewer_of(a, who).await;
        let mut tx = c.app.begin_as(&v).await.unwrap();
        for (kind, id) in [("synthesis", s), ("sample", sm)] {
            let got: bool = sqlx::query_scalar(ask)
                .bind(kind)
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(got, want, "{kind} asked by {who}");
        }
        let none: bool = sqlx::query_scalar(ask)
            .bind("synthesis")
            .bind(Uuid::now_v7())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert!(none, "an id that does not exist");
    }

    let open = public_claim(a, &c.x).await;
    let s_ok = admin_synthesis(a, c.h1.agent, "complete", "group", g).await;
    member(a, s_ok, open).await;
    let mut tx = a.begin().await.unwrap();
    sqlx::query("SELECT set_config('episcience.allow_widen', 'yes', true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE syntheses SET visibility = 'public' WHERE id = $1")
        .bind(s_ok)
        .execute(&mut *tx)
        .await
        .expect("the privileged runtime widens a publishable synthesis");
    let r = sqlx::query("UPDATE syntheses SET visibility = 'public' WHERE id = $1")
        .bind(s)
        .execute(&mut *tx)
        .await;
    assert_eq!(
        err(&r).0,
        "42501",
        "and is refused one citing a non-public claim"
    );
}

/// The narrowing sweep covers samples: X's claim attached to H1's PUBLIC
/// sample is narrowed out of band; the sweep (on the maintenance login)
/// narrows the sample, and its claim rows, its blob and its public child
/// sample (which carried the parent's pair) follow through the propagation;
/// one audit row for the narrowed sample, none for a sample citing a
/// still-public claim; a second run changes nothing. Kills: the sample half
/// of the sweep removed (the sample stays public with nothing to re-narrow
/// it), its audit row removed, a sweep that narrows every public sample.
#[tokio::test]
async fn the_sweep_narrows_a_public_sample_whose_cited_claim_stopped_being_public() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let hidden = public_claim(a, &c.x).await;
    let open = public_claim(a, &c.x).await;
    let sm = admin_sample(a, c.h1.agent, "public", g, None).await;
    attach(a, sm, hidden).await;
    let child = admin_sample(a, c.h1.agent, "public", g, Some(sm)).await;
    let untouched = admin_sample(a, c.h1.agent, "public", g, None).await;
    attach(a, untouched, open).await;
    let blob: Uuid = sqlx::query_scalar(
        "INSERT INTO blobs (filename, mime_type, size_bytes, content_hash, uploader_id, sample_id, \
                            owner_group_id, visibility) \
         VALUES ('f', 'text/plain', 1, decode(repeat('03', 32), 'hex'), $1, $2, $3, 'public') \
         RETURNING id",
    )
    .bind(c.h1.agent)
    .bind(sm)
    .bind(g)
    .fetch_one(a)
    .await
    .expect("blob under the sample");
    narrow(a, hidden).await;

    let maint = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(c.db.login_options(MAINT_LOGIN))
        .await
        .unwrap();
    let mut m = maint.acquire().await.unwrap();
    assert_unprivileged(&mut m).await;
    let n: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(&mut *m)
        .await
        .expect("sweep");
    assert_eq!(n, 1, "the sample (its child follows by propagation)");
    for (table, id, want) in [
        ("samples", sm, "group"),
        ("samples", child, "group"),
        ("samples", untouched, "public"),
        ("blobs", blob, "group"),
    ] {
        assert_eq!(visibility(a, table, id).await, want, "{table} {id}");
    }
    let link_vis: String =
        sqlx::query_scalar("SELECT visibility::text FROM sample_claims WHERE sample_id = $1")
            .bind(sm)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(link_vis, "group", "the claim rows follow");
    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT details->>'sample_id' FROM security_events \
          WHERE event_type = 'episcience.maint.sweep_narrowed' AND details ? 'sample_id'",
    )
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(
        audited,
        vec![sm.to_string()],
        "one audit row for the sample"
    );
    let again: i32 = sqlx::query_scalar("SELECT public.episcience_maint_sweep_narrowed()")
        .fetch_one(&mut *m)
        .await
        .unwrap();
    assert_eq!(again, 0, "idempotent");
}
