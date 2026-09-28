//! The row guards of migration 5035, exercised on the REAL application login
//! (`episcience_app`), stamped through the kernel's production path
//! (`Viewer::resolve` + `ScopedPool::begin_as`). Row security is not on yet
//! (the RLS migration comes next), so these pin the triggers and CHECKs
//! themselves. Each test names the mutation it kills.
//!
//! Cast: H1, H2 (personal groups H1pg, H2pg); team T (H1 admin, H2 writer, R
//! reader); a second team T2 (H1 admin only).
mod support;
use support::{principal, team_group, viewer_of, Principal, TestDb, APP_LOGIN};

use epigraph_core::TenancyDecl;
use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use episcience_db::ledger;
use sqlx::{Connection, PgConnection, PgPool, Row};
use uuid::Uuid;

const WORLD: &str = "00000000-0000-0000-0000-000000000000";
const SEED: &str = "00000000-0000-0000-0000-00000000dead";

struct Cast {
    db: TestDb,
    app: ScopedPool,
    h1: Principal,
    h2: Principal,
    r: Principal,
    t: Uuid,
    t2: Uuid,
}

async fn cast() -> Cast {
    let db = TestDb::fresh().await;
    let h1 = principal(&db.admin, "h1").await;
    let h2 = principal(&db.admin, "h2").await;
    let r = principal(&db.admin, "reader").await;
    let t = team_group(&db.admin, &h1, &[(h2.agent, "writer"), (r.agent, "reader")]).await;
    let t2 = team_group(&db.admin, &h1, &[]).await;
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
        t2,
    }
}

/// The brief's precondition for every app-role test: the session is not
/// privileged, and (before the RLS migration) it holds the table privileges
/// the kernel's default privileges give `epigraph_app`.
async fn assert_app_session(conn: &mut PgConnection) {
    let (sup, bypass, maint, ins): (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT r.rolsuper, r.rolbypassrls, pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER'), \
                has_table_privilege(session_user, 'public.syntheses', 'INSERT') \
           FROM pg_roles r WHERE r.rolname = session_user",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("role attributes");
    assert!(
        !sup && !bypass && !maint,
        "the login under test must be unprivileged"
    );
    assert!(
        ins,
        "the app login writes EpiScience tables (through episcience_rw)"
    );
    let privileged: bool = sqlx::query_scalar("SELECT public.episcience_session_is_privileged()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert!(!privileged);
}

/// The server's message of a failed statement ("" when it succeeded). Row
/// security, a missing privilege and the row guards all answer 42501, so a
/// test that means one of them asserts its message too.
fn message<T>(r: &Result<T, sqlx::Error>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(sqlx::Error::Database(d)) => d.message().to_string(),
        Err(e) => format!("non-db error: {e}"),
    }
}

/// SQLSTATE of a failed statement ("" when it succeeded).
fn code<T>(r: Result<T, sqlx::Error>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(sqlx::Error::Database(d)) => d.code().map(|c| c.to_string()).unwrap_or_default(),
        Err(e) => format!("non-db error: {e}"),
    }
}

const INSERT_SYNTHESIS: &str =
    "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, \
     clustering_method, llm_provider, llm_model, content_hash, visibility, owner_group_id, \
     parent_synthesis_id, prereq_synthesis_ids) \
     VALUES ($1, 'guard test', $2, 'pending', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
             decode(repeat('00', 32), 'hex'), $3, $4, $5, $6)";

/// A synthesis written on the ADMIN pool (a fixture, privileged).
async fn admin_synthesis(pool: &PgPool, author: Uuid, vis: &str, owner: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(INSERT_SYNTHESIS)
        .bind(id)
        .bind(author)
        .bind(vis)
        .bind(owner)
        .bind(None::<Uuid>)
        .bind(None::<Vec<Uuid>>)
        .execute(pool)
        .await
        .expect("admin synthesis");
    id
}

async fn cluster(
    conn: &mut PgConnection,
    synthesis: Uuid,
    declared_owner: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
             member_claim_ids, support_count, contradict_count, owner_group_id, visibility) \
         VALUES ($1, $2, (SELECT count(*) FROM synthesis_clusters WHERE synthesis_id = $2), \
                 't', 's', ARRAY[gen_random_uuid()], 0, 0, $3, CASE WHEN $3 IS NULL THEN NULL ELSE 'public' END)",
    )
    .bind(id)
    .bind(synthesis)
    .bind(declared_owner)
    .execute(&mut *conn)
    .await
    .map(|_| id)
}

async fn pair_of(pool: &PgPool, table: &str, id: Uuid) -> (Uuid, String) {
    sqlx::query_as(&format!(
        "SELECT owner_group_id, visibility::text FROM {table} WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ─── T-W2 / root declaration ────────────────────────────────────────────────

/// T-W2: a root row owned by the world or seed sentinel is refused: on the
/// application session by row security first (neither sentinel is ever a
/// writable group), and on a PRIVILEGED session, which row security does not
/// filter, by the CHECK (23514). An undeclared root is refused (23502); a
/// declared root in a real group is accepted. Kills: dropping
/// `<t>_group_needs_real_group` (the privileged insert would succeed), or the
/// "declared, pass, else 23502" arm.
#[tokio::test]
async fn a_root_row_needs_a_declared_real_group() {
    let c = cast().await;
    let v1 = viewer_of(&c.db.admin, c.h1.agent).await;
    for (vis, owner) in [("group", WORLD), ("group", SEED), ("public", WORLD)] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(INSERT_SYNTHESIS)
            .bind(Uuid::now_v7())
            .bind(c.h1.agent)
            .bind(vis)
            .bind(owner.parse::<Uuid>().unwrap())
            .bind(None::<Uuid>)
            .bind(None::<Vec<Uuid>>)
            .execute(&mut *tx)
            .await;
        let m = message(&r);
        assert_eq!(code(r), "42501", "({vis}, {owner}) must be refused");
        assert!(m.contains("row-level security"), "({vis}, {owner}): {m}");
        let r = sqlx::query(INSERT_SYNTHESIS)
            .bind(Uuid::now_v7())
            .bind(c.h1.agent)
            .bind(vis)
            .bind(owner.parse::<Uuid>().unwrap())
            .bind(None::<Uuid>)
            .bind(None::<Vec<Uuid>>)
            .execute(&c.db.admin)
            .await;
        assert_eq!(
            code(r),
            "23514",
            "({vis}, {owner}) must be refused by the CHECK on a privileged session"
        );
    }
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let r = sqlx::query(INSERT_SYNTHESIS)
        .bind(Uuid::now_v7())
        .bind(c.h1.agent)
        .bind(None::<String>)
        .bind(None::<Uuid>)
        .bind(None::<Uuid>)
        .bind(None::<Vec<Uuid>>)
        .execute(&mut *tx)
        .await;
    assert_eq!(code(r), "23502", "an undeclared root is refused");
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let r = sqlx::query(INSERT_SYNTHESIS)
        .bind(Uuid::now_v7())
        .bind(c.h1.agent)
        .bind("group")
        .bind(c.h1.personal_group)
        .bind(None::<Uuid>)
        .bind(None::<Vec<Uuid>>)
        .execute(&mut *tx)
        .await;
    assert_eq!(code(r), "", "a declared root in a real group is accepted");
}

// ─── T-W3 / T-W4 / T-W7: derived rows ───────────────────────────────────────

/// T-W3: a derived row declaring a FOREIGN owner is stored with its parent's
/// pair. T-W4: a derived row whose parent is missing is 23503 (the RLS
/// migration makes "invisible" the same). T-W7: a direct visibility UPDATE of
/// a derived row is 42501. Kills: the inherit trigger honouring a
/// declaration, the parent check dropped, the pin dropped.
#[tokio::test]
async fn derived_rows_always_take_their_parents_pair() {
    let c = cast().await;
    let s = admin_synthesis(&c.db.admin, c.h1.agent, "group", c.h1.personal_group).await;
    let v1 = viewer_of(&c.db.admin, c.h1.agent).await;

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    let id = cluster(&mut tx, s, Some(c.h2.personal_group))
        .await
        .expect("insert cluster");
    tx.commit().await.unwrap();
    assert_eq!(
        pair_of(&c.db.admin, "synthesis_clusters", id).await,
        (c.h1.personal_group, "group".to_string()),
        "a declared foreign owner is replaced by the parent's pair"
    );

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_eq!(code(cluster(&mut tx, Uuid::now_v7(), None).await), "23503");

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let r = sqlx::query("UPDATE synthesis_clusters SET visibility = 'public' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await;
    assert_eq!(code(r), "42501", "a derived row's visibility is pinned");
}

// ─── T-W6: owner immutability ───────────────────────────────────────────────

/// T-W6: re-owning a row on an application session is 42501, even into a
/// group the session may write; a privileged session (the re-own, the
/// propagation) may. Kills: the owner-immutable trigger dropped or keyed on
/// writability.
#[tokio::test]
async fn the_owner_is_immutable_on_an_application_session() {
    let c = cast().await;
    let s = admin_synthesis(&c.db.admin, c.h1.agent, "group", c.h1.personal_group).await;
    let v1 = viewer_of(&c.db.admin, c.h1.agent).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    let r = sqlx::query("UPDATE syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(s)
        .bind(c.t)
        .execute(&mut *tx)
        .await;
    assert_eq!(code(r), "42501");
    sqlx::query("UPDATE syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(s)
        .bind(c.t)
        .execute(&c.db.admin)
        .await
        .expect("a privileged session may re-own");
}

// ─── T-W8 / T-W18: principal and author binding ─────────────────────────────

/// T-W8: a job inserted by H1's session declaring principal H2 is stored as
/// H1, with the payload's agent_id H1. T-W18: H2's session inserting a
/// synthesis that names `agent_id = H1` is 42501, a NULL author becomes the
/// principal, and an UPDATE of the author column is 42501. Kills: the
/// principal forcing removed, the author check removed, either guard made a
/// DEFINER (the session principal would then be invisible to it).
#[tokio::test]
async fn jobs_act_as_and_rows_are_authored_by_the_session_principal() {
    let c = cast().await;
    let s = admin_synthesis(&c.db.admin, c.h1.agent, "group", c.h1.personal_group).await;
    let v1 = viewer_of(&c.db.admin, c.h1.agent).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, payload, state, principal_id) \
         VALUES ($1, jsonb_build_object('agent_id', $2), 'queued', $2)",
    )
    .bind(s)
    .bind(c.h2.agent)
    .execute(&mut *tx)
    .await
    .expect("job insert");
    tx.commit().await.unwrap();
    let (principal, payload_agent): (Uuid, String) = sqlx::query_as(
        "SELECT principal_id, payload->>'agent_id' FROM synthesis_jobs WHERE id = $1",
    )
    .bind(s)
    .fetch_one(&c.db.admin)
    .await
    .unwrap();
    assert_eq!(principal, c.h1.agent);
    assert_eq!(payload_agent, c.h1.agent.to_string());

    let v2 = viewer_of(&c.db.admin, c.h2.agent).await;
    let mut tx = c.app.begin_as(&v2).await.unwrap();
    let r = sqlx::query(INSERT_SYNTHESIS)
        .bind(Uuid::now_v7())
        .bind(c.h1.agent)
        .bind("group")
        .bind(c.h2.personal_group)
        .bind(None::<Uuid>)
        .bind(None::<Vec<Uuid>>)
        .execute(&mut *tx)
        .await;
    assert_eq!(code(r), "42501", "H2 may not author as H1");

    let mut tx = c.app.begin_as(&v2).await.unwrap();
    let id = Uuid::now_v7();
    sqlx::query(INSERT_SYNTHESIS)
        .bind(id)
        .bind(None::<Uuid>)
        .bind("group")
        .bind(c.h2.personal_group)
        .bind(None::<Uuid>)
        .bind(None::<Vec<Uuid>>)
        .execute(&mut *tx)
        .await
        .expect("a NULL author becomes the principal");
    tx.commit().await.unwrap();
    let author: Uuid = sqlx::query_scalar("SELECT agent_id FROM syntheses WHERE id = $1")
        .bind(id)
        .fetch_one(&c.db.admin)
        .await
        .unwrap();
    assert_eq!(author, c.h2.agent);

    let mut tx = c.app.begin_as(&v2).await.unwrap();
    let r = sqlx::query("UPDATE syntheses SET agent_id = $2 WHERE id = $1")
        .bind(id)
        .bind(c.h2.agent)
        .execute(&mut *tx)
        .await;
    assert_eq!(code(r), "42501", "the author column is immutable");
}

// ─── T-W9 / T-W11: claim attachment ─────────────────────────────────────────

/// T-W9: a membership row may cite a public claim, or a group claim owned by
/// the synthesis' own group; a claim of ANOTHER group the session can see is
/// 42501; a claim the session cannot see is 23503. Kills: the owner
/// comparison removed (a group(H1pg) synthesis would cite a group(T) claim),
/// or the visibility read made unfiltered.
#[tokio::test]
async fn a_membership_cites_public_or_own_group_claims_only() {
    let c = cast().await;
    let a = &c.db.admin;
    let s = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let public = support::claim(
        a,
        c.h1.agent,
        &format!("pub {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(c.h1.personal_group),
    )
    .await;
    let own = support::claim(
        a,
        c.h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(c.h1.personal_group),
    )
    .await;
    let team = support::claim(
        a,
        c.h1.agent,
        &format!("team {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(c.t),
    )
    .await;
    let hidden = support::claim(
        a,
        c.h2.agent,
        &format!("hidden {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(c.h2.personal_group),
    )
    .await;
    let v1 = viewer_of(a, c.h1.agent).await;
    let insert = "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)";
    for (claim, want) in [(public, ""), (own, ""), (team, "42501"), (hidden, "23503")] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(insert)
            .bind(s)
            .bind(claim)
            .execute(&mut *tx)
            .await;
        assert_eq!(code(r), want, "claim {claim}");
        if want.is_empty() {
            tx.commit().await.unwrap();
        }
    }
}

/// T-W11: a countersignature of a PUBLIC claim may carry the writer's pair; of
/// a GROUP claim it must be `('group', <claim group>)` with that group
/// writable by the writer: `public` is 42501, and a reader of the group is
/// 42501. Kills: the countersignature arm of the claim guard removed.
#[tokio::test]
async fn countersignature_attach_rules() {
    let c = cast().await;
    let a = &c.db.admin;
    let public = support::claim(
        a,
        c.h1.agent,
        &format!("pub {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(c.h1.personal_group),
    )
    .await;
    let team = support::claim(
        a,
        c.h1.agent,
        &format!("team {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(c.t),
    )
    .await;
    let insert =
        "INSERT INTO countersignatures (claim_id, signer_id, signature_meaning, content_hash, \
                  signature, countersigned_by, owner_group_id, visibility) \
                  VALUES ($1, $2, 'witnessed', decode(repeat('00', 32), 'hex'), \
                          decode(repeat('00', 64), 'hex'), $2, $3, $4)";
    let cases: [(&Principal, Uuid, Uuid, &str, &str); 4] = [
        (&c.h2, public, c.h2.personal_group, "public", ""),
        (&c.h2, team, c.t, "group", ""),
        (&c.h2, team, c.t, "public", "42501"),
        (&c.r, team, c.t, "group", "42501"),
    ];
    for (who, claim, owner, vis, want) in cases {
        let v = viewer_of(a, who.agent).await;
        let mut tx = c.app.begin_as(&v).await.unwrap();
        assert_app_session(&mut tx).await;
        // One countersignature per (claim, signer, meaning): vary the signer
        // row by deleting the previous accepted one first.
        sqlx::query("DELETE FROM countersignatures WHERE claim_id = $1 AND signer_id = $2")
            .bind(claim)
            .bind(who.agent)
            .execute(a)
            .await
            .unwrap();
        let r = sqlx::query(insert)
            .bind(claim)
            .bind(who.agent)
            .bind(owner)
            .bind(vis)
            .execute(&mut *tx)
            .await;
        assert_eq!(
            code(r),
            want,
            "{vis} countersignature by {} of {claim}",
            who.agent
        );
        if want.is_empty() {
            tx.commit().await.unwrap();
        }
    }
}

// ─── T-W10 / T-W16-lite / T-W21: widening, propagation, publish rule ────────

/// T-W10: widening group -> public is refused without the interlock; with
/// it, refused while a member claim is not public; allowed once every input
/// is public, and the children follow (count asserted). Kills: the interlock
/// check removed, the publishability check removed, the propagation dropped.
#[tokio::test]
async fn widening_needs_the_interlock_and_public_inputs_and_children_follow() {
    let c = cast().await;
    let a = &c.db.admin;
    let v1 = viewer_of(a, c.h1.agent).await;
    let own = support::claim(
        a,
        c.h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(c.h1.personal_group),
    )
    .await;
    let public = support::claim(
        a,
        c.h1.agent,
        &format!("pub {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(c.h1.personal_group),
    )
    .await;
    let blocked = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let open = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    for _ in 0..2 {
        cluster(&mut tx, open, None).await.unwrap();
    }
    for (s, claim) in [(blocked, own), (open, public)] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(s)
        .bind(claim)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let widen = "UPDATE syntheses SET visibility = 'public' WHERE id = $1";
    let interlock = "SELECT set_config('episcience.allow_widen', 'yes', true)";
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    assert_eq!(
        code(sqlx::query(widen).bind(open).execute(&mut *tx).await),
        "42501",
        "no interlock"
    );

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    sqlx::query(interlock).execute(&mut *tx).await.unwrap();
    assert_eq!(
        code(sqlx::query(widen).bind(blocked).execute(&mut *tx).await),
        "42501",
        "a group member claim keeps it unpublishable"
    );

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    sqlx::query(interlock).execute(&mut *tx).await.unwrap();
    sqlx::query(widen)
        .bind(open)
        .execute(&mut *tx)
        .await
        .expect("publishable + interlock: widened");
    tx.commit().await.unwrap();
    let children: Vec<(String,)> = sqlx::query_as(
        "SELECT visibility FROM synthesis_clusters WHERE synthesis_id = $1 \
         UNION ALL SELECT visibility FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(open)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(children.len(), 3);
    assert!(children.iter().all(|(v,)| v == "public"), "{children:?}");
}

/// Propagation on a re-own (the privileged path) reaches EVERY child table
/// on both branches: the six synthesis children (clusters, embeddings,
/// staleness events, outbox, membership, the job row) and, for a sample, its
/// claim links, a blob on it, a child sample and (by the recursion) a
/// grandchild sample with its own blob. Kills: any one child block removed
/// from the propagation (its count check compares the table with itself, so
/// only this test sees a missing block).
#[tokio::test]
async fn a_reown_propagates_to_every_child_on_both_branches() {
    let c = cast().await;
    let a = &c.db.admin;
    let claim = support::any_public_claim(a).await;

    // Synthesis branch.
    let s = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    for sql in [
        "INSERT INTO synthesis_jobs (id, payload, principal_id) VALUES ($1, '{}'::jsonb, $2)",
        "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
         VALUES ($1, 'ATTRIBUTED_TO', 'agent', $2)",
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, member_claim_ids, \
             support_count, contradict_count) VALUES (gen_random_uuid(), $1, 0, 't', 's', ARRAY[$2], 0, 0)",
        "INSERT INTO synthesis_embeddings (synthesis_id, embedding, embedding_model, embedding_input) \
         SELECT $1, (SELECT array_agg(0.0::real) FROM generate_series(1, 1536))::vector, 'm', 'narrative_head' \
          WHERE $2::uuid IS NOT NULL",
        "INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids) \
         VALUES (gen_random_uuid(), $1, 'belief_drift', ARRAY[$2])",
    ] {
        let second = if sql.contains("ATTRIBUTED_TO") || sql.contains("principal_id") {
            c.h1.agent
        } else {
            claim
        };
        sqlx::query(sql).bind(s).bind(second).execute(a).await.expect(sql);
    }
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(s)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    sqlx::query("UPDATE syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(s)
        .bind(c.t)
        .execute(a)
        .await
        .unwrap();
    let owners: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT 'job', owner_group_id FROM synthesis_jobs WHERE id = $1 \
         UNION ALL SELECT 'outbox', owner_group_id FROM synthesis_provo_edges WHERE synthesis_id = $1 \
         UNION ALL SELECT 'cluster', owner_group_id FROM synthesis_clusters WHERE synthesis_id = $1 \
         UNION ALL SELECT 'embedding', owner_group_id FROM synthesis_embeddings WHERE synthesis_id = $1 \
         UNION ALL SELECT 'staleness', owner_group_id FROM synthesis_staleness_events WHERE synthesis_id = $1 \
         UNION ALL SELECT 'membership', owner_group_id FROM synthesis_claim_membership WHERE synthesis_id = $1 \
         ORDER BY 1",
    )
    .bind(s)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(
        owners,
        [
            "cluster",
            "embedding",
            "job",
            "membership",
            "outbox",
            "staleness"
        ]
        .iter()
        .map(|k| (k.to_string(), c.t))
        .collect::<Vec<_>>(),
        "every synthesis child follows"
    );

    // Sample branch: S -> child C -> grandchild G, a claim link on S, a blob
    // on S and on G.
    let sample = |parent: Option<Uuid>| {
        let id = Uuid::now_v7();
        let q = sqlx::query(
            "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, parent_sample_id, \
                                  owner_group_id, visibility) \
             VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, $4, 'group')",
        )
        .bind(id)
        .bind(c.h1.agent)
        .bind(parent)
        .bind(c.h1.personal_group);
        (id, q)
    };
    let (root, q) = sample(None);
    q.execute(a).await.unwrap();
    let (child, q) = sample(Some(root));
    q.execute(a).await.unwrap();
    let (grandchild, q) = sample(Some(child));
    q.execute(a).await.unwrap();
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(root)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    for on in [root, grandchild] {
        sqlx::query(
            "INSERT INTO blobs (id, filename, mime_type, size_bytes, content_hash, uploader_id, sample_id) \
             VALUES (gen_random_uuid(), 'f', 'text/plain', 1, decode(repeat('05', 32), 'hex'), $1, $2)",
        )
        .bind(c.h1.agent)
        .bind(on)
        .execute(a)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE samples SET owner_group_id = $2 WHERE id = $1")
        .bind(root)
        .bind(c.t)
        .execute(a)
        .await
        .unwrap();
    let owners: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT 'child', owner_group_id FROM samples WHERE id = $2 \
         UNION ALL SELECT 'grandchild', owner_group_id FROM samples WHERE id = $3 \
         UNION ALL SELECT 'link', owner_group_id FROM sample_claims WHERE sample_id = $1 \
         UNION ALL SELECT 'blob:' || CASE WHEN sample_id = $1 THEN 'root' ELSE 'grandchild' END, owner_group_id \
           FROM blobs WHERE sample_id IN ($1, $3) \
         ORDER BY 1",
    )
    .bind(root)
    .bind(child)
    .bind(grandchild)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(
        owners,
        [
            "blob:grandchild",
            "blob:root",
            "child",
            "grandchild",
            "link"
        ]
        .iter()
        .map(|k| (k.to_string(), c.t))
        .collect::<Vec<_>>(),
        "every sample child follows, grandchildren through the recursion"
    );
}

/// T-W21 (the database half), three ways a public synthesis meets a
/// non-public prerequisite: (A) the prerequisite is `group` at birth -> the
/// synthesis is BORN `group` and completes `group`; (B) the prerequisite is
/// public at birth and narrowed afterwards -> completion narrows it to
/// `group`, marked `input_narrowed`; (C) no prerequisite -> it completes
/// public. Kills: the insert-time narrowing removed (A would be public until
/// completion), the publish rule removed or its prerequisite arm removed (B
/// would complete public).
#[tokio::test]
async fn completion_narrows_a_public_synthesis_with_a_private_prerequisite() {
    let c = cast().await;
    let a = &c.db.admin;
    let group_prereq = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let later_prereq = admin_synthesis(a, c.h1.agent, "public", c.h1.personal_group).await;
    let complete = "UPDATE syntheses SET status = 'complete', narrative = 'n', completed_at = now() WHERE id = $1";
    let mut ids = Vec::new();
    for prereqs in [vec![group_prereq], vec![later_prereq], vec![]] {
        let id = Uuid::now_v7();
        sqlx::query(INSERT_SYNTHESIS)
            .bind(id)
            .bind(c.h1.agent)
            .bind("public")
            .bind(c.h1.personal_group)
            .bind(None::<Uuid>)
            .bind(Some(prereqs))
            .execute(a)
            .await
            .unwrap();
        ids.push(id);
    }
    assert_eq!(
        pair_of(a, "syntheses", ids[0]).await.1,
        "group",
        "(A) born group"
    );
    assert_eq!(pair_of(a, "syntheses", ids[1]).await.1, "public");
    sqlx::query("UPDATE syntheses SET visibility = 'group' WHERE id = $1")
        .bind(later_prereq)
        .execute(a)
        .await
        .unwrap();
    for id in &ids {
        sqlx::query(complete).bind(id).execute(a).await.unwrap();
    }
    let state = |id: Uuid| async move {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT visibility, stale_reason FROM syntheses WHERE id = $1",
        )
        .bind(id)
        .fetch_one(a)
        .await
        .unwrap()
    };
    assert_eq!(state(ids[0]).await, ("group".into(), None), "(A)");
    assert_eq!(
        state(ids[1]).await,
        ("group".into(), Some("input_narrowed".into())),
        "(B)"
    );
    assert_eq!(state(ids[2]).await, ("public".into(), None), "(C)");
}

// ─── T-W13 / T-W19 / T-W20: parents ─────────────────────────────────────────

/// T-W13: H2 superseding H1's protocol is 42501; H1 superseding its own is
/// accepted. Kills: the supersede writability arm removed.
#[tokio::test]
async fn superseding_needs_write_access_to_the_superseded_protocol() {
    let c = cast().await;
    let a = &c.db.admin;
    let p1 = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO protocols (id, title, authored_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 'p', $2, decode(repeat('01', 32), 'hex'), $3, 'public')",
    )
    .bind(p1)
    .bind(c.h1.agent)
    .bind(c.h1.personal_group)
    .execute(a)
    .await
    .unwrap();
    let insert = "INSERT INTO protocols (id, title, version, authored_by, content_hash, supersedes, owner_group_id, visibility) \
                  VALUES ($1, 'p2', 2, $2, decode(repeat('02', 32), 'hex'), $3, $4, 'public')";
    for (who, want) in [(&c.h2, "42501"), (&c.h1, "")] {
        let v = viewer_of(a, who.agent).await;
        let mut tx = c.app.begin_as(&v).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(insert)
            .bind(Uuid::now_v7())
            .bind(who.agent)
            .bind(p1)
            .bind(who.personal_group)
            .execute(&mut *tx)
            .await;
        assert_eq!(code(r), want, "supersede by {}", who.agent);
    }
}

/// T-W19: refining a `group(T)` synthesis while declaring `group(T2)` is
/// 42501; declaring `group(T)` is accepted; an undeclared refinement takes the
/// parent's pair. T-W20: a child sample of a `group(H1pg)` sample, by H2
/// (who cannot see it under row security), is 23503 like a missing parent; a
/// child of a `group(T)` sample H2 CAN see (writer in T), declared in H2's
/// own group, is 42501 from the child-sample arm. Kills: the refinement /
/// child-sample owner arms removed.
#[tokio::test]
async fn a_child_of_a_non_public_parent_stays_in_the_parents_group() {
    let c = cast().await;
    let a = &c.db.admin;
    let parent = admin_synthesis(a, c.h1.agent, "group", c.t).await;
    let v1 = viewer_of(a, c.h1.agent).await;
    let refine = |owner: Option<Uuid>| {
        let id = Uuid::now_v7();
        (
            id,
            sqlx::query(INSERT_SYNTHESIS)
                .bind(id)
                .bind(c.h1.agent)
                .bind(owner.map(|_| "group"))
                .bind(owner)
                .bind(Some(parent))
                .bind(None::<Vec<Uuid>>),
        )
    };
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    let (_, q) = refine(Some(c.t2));
    assert_eq!(code(q.execute(&mut *tx).await), "42501");
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let (_, q) = refine(Some(c.t));
    assert_eq!(code(q.execute(&mut *tx).await), "");
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    let (id, q) = refine(None);
    q.execute(&mut *tx).await.expect("undeclared refinement");
    tx.commit().await.unwrap();
    assert_eq!(
        pair_of(a, "syntheses", id).await,
        (c.t, "group".to_string())
    );

    let sample = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(repeat('03', 32), 'hex'), $3, 'group')",
    )
    .bind(sample)
    .bind(c.h1.agent)
    .bind(c.h1.personal_group)
    .execute(a)
    .await
    .unwrap();
    let v2 = viewer_of(a, c.h2.agent).await;
    let mut tx = c.app.begin_as(&v2).await.unwrap();
    let r = sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, parent_sample_id, owner_group_id, visibility) \
         VALUES ($1, 'child', 'chemical', $2, decode(repeat('04', 32), 'hex'), $3, $4, 'group')",
    )
    .bind(Uuid::now_v7())
    .bind(c.h2.agent)
    .bind(sample)
    .bind(c.h2.personal_group)
    .execute(&mut *tx)
    .await;
    assert_eq!(code(r), "23503", "H2 cannot see H1's group sample");

    let team_sample = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(repeat('05', 32), 'hex'), $3, 'group')",
    )
    .bind(team_sample)
    .bind(c.h1.agent)
    .bind(c.t)
    .execute(a)
    .await
    .unwrap();
    let mut tx = c.app.begin_as(&v2).await.unwrap();
    let r = sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, parent_sample_id, owner_group_id, visibility) \
         VALUES ($1, 'child', 'chemical', $2, decode(repeat('06', 32), 'hex'), $3, $4, 'group')",
    )
    .bind(Uuid::now_v7())
    .bind(c.h2.agent)
    .bind(team_sample)
    .bind(c.h2.personal_group)
    .execute(&mut *tx)
    .await;
    let m = message(&r);
    assert_eq!(code(r), "42501", "{m}");
    assert!(m.contains("a child of a group sample must be"), "{m}");
}

// ─── T-W17: privileged enqueue ──────────────────────────────────────────────

/// T-W17 (the database half): on a privileged, unstamped session a job
/// insert must name its principal (23502 otherwise); with one it succeeds and
/// the payload follows it. Kills: a privileged arm that accepts a NULL
/// principal (a job acting as nobody).
#[tokio::test]
async fn a_privileged_enqueue_must_name_its_principal() {
    let c = cast().await;
    let a = &c.db.admin;
    let s = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let r = sqlx::query("INSERT INTO synthesis_jobs (id, payload) VALUES ($1, '{}'::jsonb)")
        .bind(s)
        .execute(a)
        .await;
    assert_eq!(code(r), "23502");
    let mut tx = a.begin().await.unwrap();
    episcience_db::SynthesisJobsRepository::enqueue_tx(
        &mut tx,
        s,
        c.h1.agent,
        &serde_json::json!({"agent_id": c.h2.agent}),
    )
    .await
    .expect("an explicit principal");
    tx.commit().await.unwrap();
    let (p, payload_agent): (Uuid, String) = sqlx::query_as(
        "SELECT principal_id, payload->>'agent_id' FROM synthesis_jobs WHERE id = $1",
    )
    .bind(s)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!((p, payload_agent), (c.h1.agent, c.h1.agent.to_string()));
}

// ─── T-M3: the contract migration's own data step ───────────────────────────

/// T-M3: on a database at 5034, a derived row with no pair under an OWNED
/// parent gets the parent's pair from 5035, and legacy `private` becomes
/// `group`; with an ownerless ROOT row, 5035 refuses and applies nothing.
/// After 5035 the reverse definer refuses. Kills: the derivation step
/// removed (5035 would refuse on the derived row), or the assertion removed
/// (an ownerless root would slip through the NOT NULL... as an error without
/// the hint).
#[tokio::test]
async fn the_contract_migration_derives_children_and_refuses_an_ownerless_root() {
    let at_5034 = || async {
        let db = TestDb::fresh_kernel_only().await;
        let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
        ledger::run_to(&mut conn, Some(ledger::TENANCY_EXPAND_VERSION))
            .await
            .unwrap();
        (db, conn)
    };

    let (db, mut conn) = at_5034().await;
    let h1 = principal(&db.admin, "h1").await;
    let s = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id) \
         VALUES ($1, 'q', $2, 'pending', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), 'private', $3)",
    )
    .bind(s)
    .bind(h1.agent)
    .bind(h1.personal_group)
    .execute(&db.admin)
    .await
    .unwrap();
    let cl = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, member_claim_ids, \
             support_count, contradict_count) VALUES ($1, $2, 0, 't', 's', ARRAY[gen_random_uuid()], 0, 0)",
    )
    .bind(cl)
    .bind(s)
    .execute(&db.admin)
    .await
    .unwrap();
    ledger::run(&mut conn)
        .await
        .expect("5035 applies over owned roots");
    assert_eq!(
        pair_of(&db.admin, "syntheses", s).await,
        (h1.personal_group, "group".to_string())
    );
    assert_eq!(
        pair_of(&db.admin, "synthesis_clusters", cl).await,
        (h1.personal_group, "group".to_string())
    );
    let mut m = sqlx::PgConnection::connect_with(&db.login_options(support::MAINT_LOGIN))
        .await
        .unwrap();
    let e = sqlx::query_scalar::<_, i32>("SELECT public.episcience_maint_backfill_reverse($1)")
        .bind(serde_json::json!({"kind": "episcience.backfill_owners.v1", "applied": true, "tables": {}}))
        .fetch_one(&mut m)
        .await;
    assert_eq!(
        code(e),
        "55000",
        "reverse refuses once the pair is mandatory"
    );

    let (db, mut conn) = at_5034().await;
    let h1 = principal(&db.admin, "h1").await;
    sqlx::query(
        "INSERT INTO protocols (id, title, authored_by, content_hash) \
         VALUES (gen_random_uuid(), 'legacy', $1, decode(repeat('01', 32), 'hex'))",
    )
    .bind(h1.agent)
    .execute(&db.admin)
    .await
    .unwrap();
    let err = ledger::run(&mut conn)
        .await
        .expect_err("an ownerless root refuses 5035");
    assert!(
        err.to_string()
            .contains("protocols rows have no ownership pair"),
        "{err}"
    );
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM episcience_meta._sqlx_migrations ORDER BY 1")
            .fetch_all(&db.admin)
            .await
            .unwrap();
    assert_eq!(
        *applied.last().unwrap(),
        ledger::TENANCY_EXPAND_VERSION,
        "nothing of 5035 recorded"
    );
}

// ─── R5b ────────────────────────────────────────────────────────────────────

/// R5b: no function a BEFORE ROW trigger on an EpiScience table calls is
/// SECURITY DEFINER (a definer row guard would check its owner, not the
/// session). Kills: making `episcience_author_is_principal` (or any guard) a
/// definer.
#[tokio::test]
async fn no_before_row_guard_is_a_definer() {
    let db = TestDb::fresh().await;
    let tables: Vec<String> = ledger::EPISCIENCE_TABLES
        .iter()
        .map(|t| t.to_string())
        .collect();
    let definers: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, t.tgname::text FROM pg_trigger t \
           JOIN pg_class c ON c.oid = t.tgrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'public' \
           JOIN pg_proc p ON p.oid = t.tgfoid \
          WHERE NOT t.tgisinternal AND c.relname = ANY($1) \
            AND (t.tgtype & 1) = 1 AND (t.tgtype & 2) = 2 AND p.prosecdef \
          ORDER BY 1, 2",
    )
    .bind(&tables)
    .fetch_all(&db.admin)
    .await
    .unwrap();
    assert!(
        definers.is_empty(),
        "BEFORE ROW definer guards: {definers:?}"
    );
    // The one definer trigger is the statement-level propagation.
    let prop: Vec<(String, bool)> = sqlx::query_as(
        "SELECT c.relname::text, p.prosecdef FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
           JOIN pg_proc p ON p.oid = t.tgfoid WHERE t.tgname = 'tenancy_90_propagate' ORDER BY 1",
    )
    .fetch_all(&db.admin)
    .await
    .unwrap();
    assert_eq!(
        prop,
        vec![
            ("samples".to_string(), true),
            ("syntheses".to_string(), true)
        ]
    );
}

/// The compensating script `docs/runbooks/5035-undo.sql` reverts to the
/// expand state (no guard, nullable pair, legacy words admitted, no 5035 row)
/// and 5035 applies again afterwards. Kills: an undo that misses a trigger or
/// a constraint (the re-apply would fail on "already exists").
#[tokio::test]
async fn the_5035_undo_script_reverts_and_5035_reapplies() {
    let db = TestDb::fresh().await;
    let h1 = principal(&db.admin, "h1").await;
    let s = admin_synthesis(&db.admin, h1.agent, "group", h1.personal_group).await;
    sqlx::raw_sql(include_str!("../../../docs/runbooks/5035-undo.sql"))
        .execute(&db.admin)
        .await
        .expect("the undo script applies");
    let guards: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'tenancy\\_%' AND NOT tgisinternal",
    )
    .fetch_one(&db.admin)
    .await
    .unwrap();
    assert_eq!(guards, 0);
    sqlx::query("UPDATE syntheses SET visibility = 'private', owner_group_id = NULL WHERE id = $1")
        .bind(s)
        .execute(&db.admin)
        .await
        .expect("the expand state admits a legacy, ownerless row");
    sqlx::query("UPDATE syntheses SET visibility = 'group', owner_group_id = $2 WHERE id = $1")
        .bind(s)
        .bind(h1.personal_group)
        .execute(&db.admin)
        .await
        .unwrap();
    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    ledger::run(&mut conn)
        .await
        .expect("5035 re-applies after the undo");
    ledger::verify(&mut conn).await.expect("and verifies");
}

// ─── Review round: samples, authors, pins, narrowing, parents ───────────────

/// A sample written on the ADMIN pool (a fixture, privileged).
async fn admin_sample(
    pool: &PgPool,
    author: Uuid,
    owner: Uuid,
    vis: &str,
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
    .execute(pool)
    .await
    .expect("admin sample");
    id
}

const INSERT_BLOB: &str =
    "INSERT INTO blobs (id, filename, mime_type, size_bytes, content_hash, uploader_id, sample_id, \
                        owner_group_id, visibility) \
     VALUES ($1, 'f', 'text/plain', 1, decode(md5($1::text) || md5($1::text), 'hex'), $2, $3, $4, $5)";

const INSERT_PROTOCOL: &str =
    "INSERT INTO protocols (id, title, authored_by, content_hash, owner_group_id, visibility) \
     VALUES ($1, 'p', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, 'public')";

const INSERT_COUNTERSIGNATURE: &str =
    "INSERT INTO countersignatures (id, claim_id, signer_id, signature_meaning, content_hash, \
                                    signature, countersigned_by, owner_group_id, visibility) \
     VALUES ($1, $2, $3, 'witnessed', decode(repeat('00', 32), 'hex'), \
             decode(repeat('00', 64), 'hex'), $4, $5, 'public')";

/// R5 / T-W19 (visibility): a synthesis asking to be PUBLIC whose parent or a
/// prerequisite is not public is BORN `group` (the parent's group, for a
/// refinement), on the application session; with a public parent and public
/// prerequisites it stays public. Kills: the insert-time narrowing removed
/// (the refinement would copy a group parent's query into a world-readable
/// row), or publishability ignoring the parent.
#[tokio::test]
async fn a_public_synthesis_is_born_group_when_its_parent_or_a_prerequisite_is_not() {
    let c = cast().await;
    let a = &c.db.admin;
    let group_parent = admin_synthesis(a, c.h1.agent, "group", c.t).await;
    let public_parent = admin_synthesis(a, c.h1.agent, "public", c.t).await;
    let group_prereq = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let v1 = viewer_of(a, c.h1.agent).await;
    // (owner, parent, prerequisites, stored visibility)
    type Case<'a> = (Uuid, Option<Uuid>, Option<Vec<Uuid>>, &'a str);
    let cases: [Case; 4] = [
        (c.t, Some(group_parent), None, "group"),
        (c.h1.personal_group, None, Some(vec![group_prereq]), "group"),
        (
            c.t,
            Some(public_parent),
            Some(vec![public_parent]),
            "public",
        ),
        (c.h1.personal_group, None, None, "public"),
    ];
    for (owner, parent, prereqs, want) in cases {
        let id = Uuid::now_v7();
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        sqlx::query(INSERT_SYNTHESIS)
            .bind(id)
            .bind(c.h1.agent)
            .bind("public")
            .bind(owner)
            .bind(parent)
            .bind(prereqs.clone())
            .execute(&mut *tx)
            .await
            .expect("insert");
        tx.commit().await.unwrap();
        assert_eq!(
            pair_of(a, "syntheses", id).await,
            (owner, want.to_string()),
            "parent {parent:?} prereqs {prereqs:?}"
        );
    }
}

/// R1 (member arm): a member claim that is not public narrows a PUBLIC
/// synthesis the moment it attaches, with its children, marked
/// `input_narrowed`; the synthesis then stays `group` when it FAILS (no
/// completion needed). A public member leaves a public sibling public.
/// Kills: tenancy_50_narrow_parent dropped (the failed synthesis, its
/// clusters and membership would stay world-readable).
#[tokio::test]
async fn a_non_public_member_claim_narrows_a_public_synthesis_as_it_attaches() {
    let c = cast().await;
    let a = &c.db.admin;
    let own = support::claim(
        a,
        c.h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(c.h1.personal_group),
    )
    .await;
    let public = support::any_public_claim(a).await;
    let narrowed = admin_synthesis(a, c.h1.agent, "public", c.h1.personal_group).await;
    let sibling = admin_synthesis(a, c.h1.agent, "public", c.h1.personal_group).await;
    let v1 = viewer_of(a, c.h1.agent).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    let cl = cluster(&mut tx, narrowed, None).await.unwrap();
    for (s, claim) in [(narrowed, public), (narrowed, own), (sibling, public)] {
        sqlx::query(
            "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        )
        .bind(s)
        .bind(claim)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    // Already narrowed at the attach, BEFORE any status change (the status
    // arm of the publish rule would otherwise mask a missing attach arm).
    assert_eq!(
        pair_of(a, "syntheses", narrowed).await.1,
        "group",
        "narrowed as the member attached"
    );
    assert_eq!(pair_of(a, "synthesis_clusters", cl).await.1, "group");
    sqlx::query("UPDATE syntheses SET status = 'failed' WHERE id = $1")
        .bind(narrowed)
        .execute(a)
        .await
        .unwrap();
    let (vis, reason): (String, Option<String>) =
        sqlx::query_as("SELECT visibility, stale_reason FROM syntheses WHERE id = $1")
            .bind(narrowed)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(
        (vis.as_str(), reason.as_deref()),
        ("group", Some("input_narrowed"))
    );
    assert_eq!(pair_of(a, "synthesis_clusters", cl).await.1, "group");
    let members: Vec<String> = sqlx::query_scalar(
        "SELECT visibility FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(narrowed)
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(members, vec!["group".to_string(); 2]);
    assert_eq!(pair_of(a, "syntheses", sibling).await.1, "public");
}

/// R1 (status arm) and the parent arm of publishability: a public synthesis
/// whose PARENT is narrowed after its birth is narrowed when its status
/// changes to `failed` (not only `complete`). Kills: the publish rule firing
/// only on completion, or publishability ignoring the parent.
#[tokio::test]
async fn a_status_change_narrows_a_public_synthesis_whose_parent_was_narrowed() {
    let c = cast().await;
    let a = &c.db.admin;
    let parent = admin_synthesis(a, c.h1.agent, "public", c.h1.personal_group).await;
    let child = Uuid::now_v7();
    sqlx::query(INSERT_SYNTHESIS)
        .bind(child)
        .bind(c.h1.agent)
        .bind("public")
        .bind(c.h1.personal_group)
        .bind(Some(parent))
        .bind(None::<Vec<Uuid>>)
        .execute(a)
        .await
        .unwrap();
    assert_eq!(pair_of(a, "syntheses", child).await.1, "public");
    sqlx::query("UPDATE syntheses SET visibility = 'group' WHERE id = $1")
        .bind(parent)
        .execute(a)
        .await
        .unwrap();
    sqlx::query("UPDATE syntheses SET status = 'failed' WHERE id = $1")
        .bind(child)
        .execute(a)
        .await
        .unwrap();
    assert_eq!(pair_of(a, "syntheses", child).await.1, "group");
}

/// R8: the sample half of the widening guard: without the interlock 42501;
/// with it, a sample citing a GROUP claim is 42501 (claims arm), a sample
/// under a GROUP parent is 42501 (parent arm); a sample with public inputs
/// widens and its link and blob follow. Kills: tenancy_40_widening_guard on
/// samples dropped, or episcience_sample_is_publishable ignoring its claims
/// or its parent.
#[tokio::test]
async fn widening_a_sample_needs_the_interlock_public_claims_and_a_public_parent() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let own = support::claim(
        a,
        c.h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(g),
    )
    .await;
    let public = support::any_public_claim(a).await;
    let with_group_claim = admin_sample(a, c.h1.agent, g, "group", None).await;
    let group_parent = admin_sample(a, c.h1.agent, g, "group", None).await;
    let under_group_parent = admin_sample(a, c.h1.agent, g, "group", Some(group_parent)).await;
    let open = admin_sample(a, c.h1.agent, g, "group", None).await;
    for (s, claim) in [(with_group_claim, own), (open, public)] {
        sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
            .bind(s)
            .bind(claim)
            .execute(a)
            .await
            .unwrap();
    }
    let blob = Uuid::now_v7();
    sqlx::query(INSERT_BLOB)
        .bind(blob)
        .bind(c.h1.agent)
        .bind(Some(open))
        .bind(None::<Uuid>)
        .bind(None::<String>)
        .execute(a)
        .await
        .unwrap();
    let v1 = viewer_of(a, c.h1.agent).await;
    let widen = "UPDATE samples SET visibility = 'public' WHERE id = $1";
    let interlock = "SELECT set_config('episcience.allow_widen', 'yes', true)";

    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    assert_eq!(
        code(sqlx::query(widen).bind(open).execute(&mut *tx).await),
        "42501",
        "no interlock"
    );
    for (s, why) in [
        (with_group_claim, "a group claim"),
        (under_group_parent, "a group parent"),
    ] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        sqlx::query(interlock).execute(&mut *tx).await.unwrap();
        assert_eq!(
            code(sqlx::query(widen).bind(s).execute(&mut *tx).await),
            "42501",
            "{why} keeps the sample unpublishable"
        );
    }
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    sqlx::query(interlock).execute(&mut *tx).await.unwrap();
    sqlx::query(widen)
        .bind(open)
        .execute(&mut *tx)
        .await
        .expect("public inputs + interlock: widened");
    tx.commit().await.unwrap();
    assert_eq!(pair_of(a, "blobs", blob).await, (g, "public".to_string()));
    let link: String =
        sqlx::query_scalar("SELECT visibility FROM sample_claims WHERE sample_id = $1")
            .bind(open)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(link, "public");
}

/// R8: the author guard on EVERY root table with an author column besides
/// syntheses (samples, protocols, blobs, countersignatures): H2's session
/// naming H1 as the author is 42501, a NULL author becomes H2, and an UPDATE
/// of the author column is 42501. Kills: tenancy_15_author dropped on any one
/// of the four tables.
#[tokio::test]
async fn every_author_column_is_the_session_principal() {
    let c = cast().await;
    let a = &c.db.admin;
    let g2 = c.h2.personal_group;
    let claim = support::any_public_claim(a).await;
    let v2 = viewer_of(a, c.h2.agent).await;
    // $1 id, $2 author, $3 owner; countersignatures also $4 claim, $5 signer.
    let tables: [(&str, &str, &str); 4] = [
        (
            "samples",
            "prepared_by",
            "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
             VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, 'public')",
        ),
        ("protocols", "authored_by", INSERT_PROTOCOL),
        (
            "blobs",
            "uploader_id",
            "INSERT INTO blobs (id, filename, mime_type, size_bytes, content_hash, uploader_id, \
                                owner_group_id, visibility) \
             VALUES ($1, 'f', 'text/plain', 1, decode(md5($1::text) || md5($1::text), 'hex'), $2, $3, 'public')",
        ),
        (
            "countersignatures",
            "countersigned_by",
            "INSERT INTO countersignatures (id, claim_id, signer_id, signature_meaning, content_hash, \
                                            signature, countersigned_by, owner_group_id, visibility) \
             VALUES ($1, $4, $5, 'witnessed', decode(repeat('00', 32), 'hex'), \
                     decode(repeat('00', 64), 'hex'), $2, $3, 'public')",
        ),
    ];
    for (table, col, sql) in tables {
        let insert = |id: Uuid, author: Option<Uuid>| {
            let q = sqlx::query(sql).bind(id).bind(author).bind(g2);
            if table == "countersignatures" {
                q.bind(claim).bind(c.h2.agent)
            } else {
                q
            }
        };
        let mut tx = c.app.begin_as(&v2).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = insert(Uuid::now_v7(), Some(c.h1.agent))
            .execute(&mut *tx)
            .await;
        assert_eq!(code(r), "42501", "{table}.{col} = H1 on H2's session");

        let id = Uuid::now_v7();
        let mut tx = c.app.begin_as(&v2).await.unwrap();
        insert(id, None)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{table}: NULL author: {e}"));
        tx.commit().await.unwrap();
        let author: Uuid = sqlx::query_scalar(&format!("SELECT {col} FROM {table} WHERE id = $1"))
            .bind(id)
            .fetch_one(a)
            .await
            .unwrap();
        assert_eq!(
            author, c.h2.agent,
            "{table}: a NULL author is the principal"
        );

        let mut tx = c.app.begin_as(&v2).await.unwrap();
        let r = sqlx::query(&format!("UPDATE {table} SET {col} = $2 WHERE id = $1"))
            .bind(id)
            .bind(c.h2.agent)
            .execute(&mut *tx)
            .await;
        assert_eq!(code(r), "42501", "{table}.{col} is immutable");
    }
}

/// R8: the claim guard on sample links. On a GROUP sample: its own group's
/// claim attaches, another group's claim the session can see is 42501, a
/// claim it cannot see is 23503. On a PUBLIC sample: a group claim (even of
/// the sample's own group) is 42501, a public claim attaches. Kills:
/// tenancy_20_claim_guard on sample_claims dropped, or its public-sample arm
/// removed.
#[tokio::test]
async fn a_sample_link_cites_public_or_own_group_claims_and_a_public_sample_public_ones() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let mk = |who: Uuid, decl: TenancyDecl, label: &'static str| async move {
        support::claim(a, who, &format!("{label} {}", Uuid::new_v4()), 0.8, decl).await
    };
    let own = mk(c.h1.agent, TenancyDecl::group(g), "own").await;
    let team = mk(c.h1.agent, TenancyDecl::group(c.t), "team").await;
    let hidden = mk(
        c.h2.agent,
        TenancyDecl::group(c.h2.personal_group),
        "hidden",
    )
    .await;
    let public = support::any_public_claim(a).await;
    let group_sample = admin_sample(a, c.h1.agent, g, "group", None).await;
    let public_sample = admin_sample(a, c.h1.agent, g, "public", None).await;
    let v1 = viewer_of(a, c.h1.agent).await;
    let insert = "INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)";
    for (s, claim, want) in [
        (group_sample, own, ""),
        (group_sample, team, "42501"),
        (group_sample, hidden, "23503"),
        (public_sample, own, "42501"),
        (public_sample, public, ""),
    ] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(insert)
            .bind(s)
            .bind(claim)
            .execute(&mut *tx)
            .await;
        assert_eq!(code(r), want, "sample {s} claim {claim}");
    }
}

/// R8: the owner is immutable on every ROOT table besides syntheses (samples,
/// protocols, a blob WITHOUT a sample, countersignatures) on an application
/// session, even into a group the session may write. Kills:
/// tenancy_30_owner_immutable dropped on samples, protocols or blobs (a blob
/// without a sample isolates it from the derived pin, which fires first).
/// Since row security, the application holds no UPDATE on countersignatures
/// at all, so the refusal there is the missing privilege (asserted by its
/// message); the trigger on that table is defence in depth that no
/// non-privileged session can reach (an equivalent mutant, recorded).
#[tokio::test]
async fn the_owner_is_immutable_on_every_root_table() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let sample = admin_sample(a, c.h1.agent, g, "public", None).await;
    let protocol = Uuid::now_v7();
    sqlx::query(INSERT_PROTOCOL)
        .bind(protocol)
        .bind(c.h1.agent)
        .bind(g)
        .execute(a)
        .await
        .unwrap();
    let blob = Uuid::now_v7();
    sqlx::query(INSERT_BLOB)
        .bind(blob)
        .bind(c.h1.agent)
        .bind(None::<Uuid>)
        .bind(g)
        .bind("public")
        .execute(a)
        .await
        .unwrap();
    let cs = Uuid::now_v7();
    sqlx::query(INSERT_COUNTERSIGNATURE)
        .bind(cs)
        .bind(support::any_public_claim(a).await)
        .bind(c.h1.agent)
        .bind(c.h1.agent)
        .bind(g)
        .execute(a)
        .await
        .unwrap();
    let v1 = viewer_of(a, c.h1.agent).await;
    for (table, id) in [
        ("samples", sample),
        ("protocols", protocol),
        ("blobs", blob),
        ("countersignatures", cs),
    ] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(&format!(
            "UPDATE {table} SET owner_group_id = $2 WHERE id = $1"
        ))
        .bind(id)
        .bind(c.t)
        .execute(&mut *tx)
        .await;
        let m = message(&r);
        assert_eq!(code(r), "42501", "{table}");
        let layer = if table == "countersignatures" {
            "permission denied for table countersignatures"
        } else {
            "is immutable"
        };
        assert!(m.contains(layer), "{table}: {m}");
    }
}

/// R8: a derived row's VISIBILITY (alone, so the owner pin does not fire) is
/// pinned on every derived table: sample links, the job row, membership,
/// embeddings, staleness events, the outbox, and a blob on a sample. Kills:
/// tenancy_30_derived_pinned dropped on any one of them except the job row:
/// since row security the application holds no UPDATE on the queue, so the
/// refusal there is the missing privilege (asserted by its message) and the
/// trigger is defence in depth no non-privileged session can reach.
#[tokio::test]
async fn a_derived_rows_visibility_is_pinned_on_every_derived_table() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let claim = support::any_public_claim(a).await;
    let s = admin_synthesis(a, c.h1.agent, "group", g).await;
    for sql in [
        "INSERT INTO synthesis_jobs (id, payload, principal_id) VALUES ($1, '{}'::jsonb, $2)",
        "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
         VALUES ($1, 'ATTRIBUTED_TO', 'agent', $2)",
    ] {
        sqlx::query(sql)
            .bind(s)
            .bind(c.h1.agent)
            .execute(a)
            .await
            .unwrap();
    }
    for sql in [
        "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        "INSERT INTO synthesis_embeddings (synthesis_id, embedding, embedding_model, embedding_input) \
         SELECT $1, (SELECT array_agg(0.0::real) FROM generate_series(1, 1536))::vector, 'm', 'narrative_head' \
          WHERE $2::uuid IS NOT NULL",
        "INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids) \
         VALUES (gen_random_uuid(), $1, 'belief_drift', ARRAY[$2])",
    ] {
        sqlx::query(sql).bind(s).bind(claim).execute(a).await.unwrap();
    }
    let sample = admin_sample(a, c.h1.agent, g, "group", None).await;
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sample)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    sqlx::query(INSERT_BLOB)
        .bind(Uuid::now_v7())
        .bind(c.h1.agent)
        .bind(Some(sample))
        .bind(None::<Uuid>)
        .bind(None::<String>)
        .execute(a)
        .await
        .unwrap();
    let v1 = viewer_of(a, c.h1.agent).await;
    for (table, key, id) in [
        ("sample_claims", "sample_id", sample),
        ("blobs", "sample_id", sample),
        ("synthesis_jobs", "id", s),
        ("synthesis_claim_membership", "synthesis_id", s),
        ("synthesis_embeddings", "synthesis_id", s),
        ("synthesis_staleness_events", "synthesis_id", s),
        ("synthesis_provo_edges", "synthesis_id", s),
    ] {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(&format!(
            "UPDATE {table} SET visibility = 'public' WHERE {key} = $1"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await;
        let m = message(&r);
        assert_eq!(code(r), "42501", "{table}");
        let layer = if table == "synthesis_jobs" {
            "permission denied for table synthesis_jobs"
        } else {
            "takes its parent's ownership pair"
        };
        assert!(m.contains(layer), "{table}: {m}");
    }
}

/// R8: a blob attached to a sample and a sample link take the SAMPLE's pair
/// whatever they declare (a foreign `(T, public)`). Kills: the blobs arm of
/// episcience_root_require_tenancy honouring a declared pair, or
/// episcience_inherit_from_sample honouring a declaration.
#[tokio::test]
async fn a_blob_on_a_sample_and_a_sample_link_take_the_samples_pair() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let sample = admin_sample(a, c.h1.agent, g, "group", None).await;
    let v1 = viewer_of(a, c.h1.agent).await;
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    assert_app_session(&mut tx).await;
    let blob = Uuid::now_v7();
    sqlx::query(INSERT_BLOB)
        .bind(blob)
        .bind(c.h1.agent)
        .bind(Some(sample))
        .bind(c.t)
        .bind("public")
        .execute(&mut *tx)
        .await
        .expect("blob on a sample");
    sqlx::query(
        "INSERT INTO sample_claims (sample_id, claim_id, owner_group_id, visibility) \
         VALUES ($1, $2, $3, 'public')",
    )
    .bind(sample)
    .bind(support::any_public_claim(a).await)
    .bind(c.t)
    .execute(&mut *tx)
    .await
    .expect("sample link");
    tx.commit().await.unwrap();
    let want = (g, "group".to_string());
    assert_eq!(pair_of(a, "blobs", blob).await, want);
    let link: (Uuid, String) =
        sqlx::query_as("SELECT owner_group_id, visibility FROM sample_claims WHERE sample_id = $1")
            .bind(sample)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(link, want);
}

/// R9: a change to a parent sample's pair moves only the child samples that
/// carry the parent's OLD pair; a child another group owns (under a PUBLIC
/// parent) keeps its own owner, and a change that would put it under a GROUP
/// parent is refused (42501) with nothing changed. Kills: the propagation
/// re-owning every child sample, or the orphan refusal removed.
#[tokio::test]
async fn a_parent_samples_change_never_reowns_another_owners_child() {
    let c = cast().await;
    let a = &c.db.admin;
    let g1 = c.h1.personal_group;
    let parent = admin_sample(a, c.h1.agent, g1, "public", None).await;
    let theirs = admin_sample(a, c.h2.agent, c.h2.personal_group, "public", Some(parent)).await;
    let mine = admin_sample(a, c.h1.agent, g1, "public", Some(parent)).await;

    // A re-own that stays public: the same-owner child follows, theirs stays.
    sqlx::query("UPDATE samples SET owner_group_id = $2 WHERE id = $1")
        .bind(parent)
        .bind(c.t)
        .execute(a)
        .await
        .unwrap();
    assert_eq!(
        pair_of(a, "samples", mine).await,
        (c.t, "public".to_string())
    );
    assert_eq!(
        pair_of(a, "samples", theirs).await,
        (c.h2.personal_group, "public".to_string()),
        "another group's child is never re-owned"
    );

    // Narrowing the parent to group would strand theirs: refused.
    let r = sqlx::query("UPDATE samples SET visibility = 'group' WHERE id = $1")
        .bind(parent)
        .execute(a)
        .await;
    assert_eq!(code(r), "42501");
    assert_eq!(
        pair_of(a, "samples", parent).await,
        (c.t, "public".to_string())
    );
    assert_eq!(
        pair_of(a, "samples", theirs).await,
        (c.h2.personal_group, "public".to_string())
    );
}

/// R14: the columns naming a row's parent, prerequisites, superseded
/// protocol or attached claim are fixed at insert on an application session
/// (42501), on every table that has one; the FK's `ON DELETE SET NULL` still
/// detaches a blob when its sample is deleted (the blob keeps its pair); a
/// privileged session may change them. Kills: tenancy_12_parent_pinned
/// dropped on any table below (a later UPDATE would bypass the insert-time
/// parent, supersede and claim arms).
#[tokio::test]
async fn a_rows_parent_prerequisites_and_claim_are_fixed_at_insert() {
    let c = cast().await;
    let a = &c.db.admin;
    let g = c.h1.personal_group;
    let claim = support::any_public_claim(a).await;
    let other_claim = support::claim(
        a,
        c.h1.agent,
        &format!("other {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(g),
    )
    .await;
    let s = admin_synthesis(a, c.h1.agent, "group", g).await;
    let other_s = admin_synthesis(a, c.h1.agent, "group", g).await;
    for sql in [
        "INSERT INTO synthesis_jobs (id, payload, principal_id) VALUES ($1, '{}'::jsonb, $2)",
        "INSERT INTO synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
         VALUES ($1, 'ATTRIBUTED_TO', 'agent', $2)",
    ] {
        sqlx::query(sql)
            .bind(s)
            .bind(c.h1.agent)
            .execute(a)
            .await
            .unwrap();
    }
    for sql in [
        "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
        "INSERT INTO synthesis_embeddings (synthesis_id, embedding, embedding_model, embedding_input) \
         SELECT $1, (SELECT array_agg(0.0::real) FROM generate_series(1, 1536))::vector, 'm', 'narrative_head' \
          WHERE $2::uuid IS NOT NULL",
        "INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids) \
         VALUES (gen_random_uuid(), $1, 'belief_drift', ARRAY[$2])",
    ] {
        sqlx::query(sql).bind(s).bind(claim).execute(a).await.unwrap();
    }
    let mut v = c
        .app
        .begin_as(&viewer_of(a, c.h1.agent).await)
        .await
        .unwrap();
    let cl = cluster(&mut v, s, None).await.unwrap();
    v.commit().await.unwrap();
    let sample = admin_sample(a, c.h1.agent, g, "group", None).await;
    let other_sample = admin_sample(a, c.h1.agent, g, "group", None).await;
    let child = admin_sample(a, c.h1.agent, g, "group", Some(sample)).await;
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sample)
        .bind(claim)
        .execute(a)
        .await
        .unwrap();
    let blob = Uuid::now_v7();
    sqlx::query(INSERT_BLOB)
        .bind(blob)
        .bind(c.h1.agent)
        .bind(Some(sample))
        .bind(None::<Uuid>)
        .bind(None::<String>)
        .execute(a)
        .await
        .unwrap();
    let p1 = Uuid::now_v7();
    let p2 = Uuid::now_v7();
    for p in [p1, p2] {
        sqlx::query(INSERT_PROTOCOL)
            .bind(p)
            .bind(c.h1.agent)
            .bind(g)
            .execute(a)
            .await
            .unwrap();
    }
    let cs = Uuid::now_v7();
    sqlx::query(INSERT_COUNTERSIGNATURE)
        .bind(cs)
        .bind(claim)
        .bind(c.h1.agent)
        .bind(c.h1.agent)
        .bind(g)
        .execute(a)
        .await
        .unwrap();

    let s_txt = s.to_string();
    let cases: Vec<(String, &str)> = vec![
        (format!("UPDATE syntheses SET parent_synthesis_id = '{other_s}' WHERE id = '{s}'"), "syntheses.parent"),
        (format!("UPDATE syntheses SET prereq_synthesis_ids = ARRAY['{other_s}'::uuid] WHERE id = '{s}'"), "syntheses.prereqs"),
        (format!("UPDATE samples SET parent_sample_id = '{other_sample}' WHERE id = '{child}'"), "samples.parent"),
        (format!("UPDATE blobs SET sample_id = '{other_sample}' WHERE id = '{blob}'"), "blobs.sample_id"),
        (format!("UPDATE protocols SET supersedes = '{p1}' WHERE id = '{p2}'"), "protocols.supersedes"),
        (format!("UPDATE synthesis_claim_membership SET claim_id = '{other_claim}' WHERE synthesis_id = '{s}'"), "membership.claim_id"),
        (format!("UPDATE synthesis_claim_membership SET synthesis_id = '{other_s}' WHERE synthesis_id = '{s}'"), "membership.synthesis_id"),
        (format!("UPDATE sample_claims SET claim_id = '{other_claim}' WHERE sample_id = '{sample}'"), "sample_claims.claim_id"),
        (format!("UPDATE sample_claims SET sample_id = '{other_sample}' WHERE sample_id = '{sample}'"), "sample_claims.sample_id"),
        (format!("UPDATE countersignatures SET claim_id = '{other_claim}' WHERE id = '{cs}'"), "countersignatures.claim_id"),
        (format!("UPDATE synthesis_clusters SET synthesis_id = '{other_s}' WHERE id = '{cl}'"), "clusters.synthesis_id"),
        (format!("UPDATE synthesis_embeddings SET synthesis_id = '{other_s}' WHERE synthesis_id = '{s_txt}'"), "embeddings.synthesis_id"),
        (format!("UPDATE synthesis_staleness_events SET synthesis_id = '{other_s}' WHERE synthesis_id = '{s_txt}'"), "staleness.synthesis_id"),
        (format!("UPDATE synthesis_provo_edges SET synthesis_id = '{other_s}' WHERE synthesis_id = '{s_txt}'"), "outbox.synthesis_id"),
        (format!("UPDATE synthesis_jobs SET id = '{other_s}' WHERE id = '{s_txt}'"), "jobs.id"),
    ];
    let v1 = viewer_of(a, c.h1.agent).await;
    for (sql, what) in &cases {
        let mut tx = c.app.begin_as(&v1).await.unwrap();
        assert_app_session(&mut tx).await;
        let r = sqlx::query(sql).execute(&mut *tx).await;
        assert_eq!(code(r), "42501", "{what}");
    }
    let pinned: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT tgrelid) FROM pg_trigger WHERE tgname = 'tenancy_12_parent_pinned'",
    )
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(
        pinned, 12,
        "every table with a parent, prerequisite or claim column"
    );

    // The FK detach on an application session: the blob loses its sample
    // and keeps its pair.
    let mut tx = c.app.begin_as(&v1).await.unwrap();
    sqlx::query("DELETE FROM sample_claims WHERE sample_id = $1")
        .bind(sample)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM samples WHERE id = $1")
        .bind(child)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM samples WHERE id = $1")
        .bind(sample)
        .execute(&mut *tx)
        .await
        .expect("deleting a sample detaches its blobs");
    tx.commit().await.unwrap();
    let (on, owner, vis): (Option<Uuid>, Uuid, String) =
        sqlx::query_as("SELECT sample_id, owner_group_id, visibility FROM blobs WHERE id = $1")
            .bind(blob)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!((on, owner, vis.as_str()), (None, g, "group"));

    // A privileged session may.
    sqlx::query("UPDATE protocols SET supersedes = $1 WHERE id = $2")
        .bind(p1)
        .bind(p2)
        .execute(a)
        .await
        .expect("privileged");
}

/// R6 / R3 (5035's data steps): rows written at 5034 without the guards are
/// brought under the rules: a public synthesis citing a group claim is
/// narrowed (marked `input_narrowed`) with its cluster; a derived job row
/// whose pair differs from its synthesis takes the synthesis' pair; a public
/// sample linking its own group's claim is narrowed. A sample link citing
/// ANOTHER group's claim refuses 5035 (nothing recorded). Kills: the
/// narrowing step removed, the derivation limited to NULL pairs, or the
/// claim-attach assertion removed.
#[tokio::test]
async fn the_contract_migration_narrows_and_refuses_what_the_window_wrote() {
    let at_5034 = || async {
        let db = TestDb::fresh_kernel_only().await;
        let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
        ledger::run_to(&mut conn, Some(ledger::TENANCY_EXPAND_VERSION))
            .await
            .unwrap();
        (db, conn)
    };
    let window_synthesis = |a: PgPool, author: Uuid, g: Uuid| async move {
        let s = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
                 llm_provider, llm_model, content_hash, visibility, owner_group_id) \
             VALUES ($1, 'q', $2, 'failed', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                     decode(repeat('00', 32), 'hex'), 'public', $3)",
        )
        .bind(s)
        .bind(author)
        .bind(g)
        .execute(&a)
        .await
        .unwrap();
        s
    };

    let (db, mut conn) = at_5034().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let h2 = principal(a, "h2").await;
    let g = h1.personal_group;
    let own = support::claim(
        a,
        h1.agent,
        &format!("own {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(g),
    )
    .await;
    let s = window_synthesis(a.clone(), h1.agent, g).await;
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id, owner_group_id, visibility) VALUES ($1, $2, $3, 'public')")
        .bind(s)
        .bind(own)
        .bind(g)
        .execute(a)
        .await
        .unwrap();
    let cl = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO synthesis_clusters (id, synthesis_id, cluster_index, title, summary, member_claim_ids, \
             support_count, contradict_count, owner_group_id, visibility) \
         VALUES ($1, $2, 0, 't', 's', ARRAY[$3]::uuid[], 0, 0, $4, 'public')",
    )
    .bind(cl)
    .bind(s)
    .bind(own)
    .bind(g)
    .execute(a)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO synthesis_jobs (id, payload, state, principal_id, owner_group_id, visibility) \
         VALUES ($1, '{}'::jsonb, 'failed', $2, $3, 'public')",
    )
    .bind(s)
    .bind(h1.agent)
    .bind(h2.personal_group)
    .execute(a)
    .await
    .unwrap();
    let sample = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(repeat('0a', 32), 'hex'), $3, 'public')",
    )
    .bind(sample)
    .bind(h1.agent)
    .bind(g)
    .execute(a)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id, owner_group_id, visibility) VALUES ($1, $2, $3, 'public')")
        .bind(sample)
        .bind(own)
        .bind(g)
        .execute(a)
        .await
        .unwrap();
    ledger::run(&mut conn).await.expect("5035 applies");
    let (vis, reason): (String, Option<String>) =
        sqlx::query_as("SELECT visibility, stale_reason FROM syntheses WHERE id = $1")
            .bind(s)
            .fetch_one(a)
            .await
            .unwrap();
    assert_eq!(
        (vis.as_str(), reason.as_deref()),
        ("group", Some("input_narrowed"))
    );
    assert_eq!(
        pair_of(a, "synthesis_clusters", cl).await,
        (g, "group".to_string())
    );
    assert_eq!(
        pair_of(a, "synthesis_jobs", s).await,
        (g, "group".to_string()),
        "the job takes its synthesis' pair"
    );
    assert_eq!(
        pair_of(a, "samples", sample).await,
        (g, "group".to_string())
    );

    let (db, mut conn) = at_5034().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let h2 = principal(a, "h2").await;
    let theirs = support::claim(
        a,
        h1.agent,
        &format!("theirs {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::group(h1.personal_group),
    )
    .await;
    let sample = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(repeat('0b', 32), 'hex'), $3, 'public')",
    )
    .bind(sample)
    .bind(h2.agent)
    .bind(h2.personal_group)
    .execute(a)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sample)
        .bind(theirs)
        .execute(a)
        .await
        .unwrap();
    let err = ledger::run(&mut conn)
        .await
        .expect_err("a link to another group's claim refuses 5035");
    assert!(
        err.to_string()
            .contains("sample_claims rows cite a group claim of another group"),
        "{err}"
    );
    let head: i64 = sqlx::query_scalar("SELECT max(version) FROM episcience_meta._sqlx_migrations")
        .fetch_one(a)
        .await
        .unwrap();
    assert_eq!(
        head,
        ledger::TENANCY_EXPAND_VERSION,
        "nothing of 5035 recorded"
    );
}

/// R6 (5035's refusals beyond sample links), each on its own 5034 clone: a
/// membership citing ANOTHER group's claim, a countersignature of a group
/// claim that is not `(group, the claim's group)`, and another group's child
/// sample under a GROUP parent each refuse 5035 with its own message, and
/// nothing of 5035 is recorded. Kills: any one of those three assertions
/// removed (the row would pass into the contract state unguarded).
#[tokio::test]
async fn the_contract_migration_refuses_each_row_a_guard_would_have_refused() {
    for case in ["membership", "countersignature", "child sample"] {
        let db = TestDb::fresh_kernel_only().await;
        let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
        ledger::run_to(&mut conn, Some(ledger::TENANCY_EXPAND_VERSION))
            .await
            .unwrap();
        let a = &db.admin;
        let h1 = principal(a, "h1").await;
        let h2 = principal(a, "h2").await;
        let theirs = support::claim(
            a,
            h1.agent,
            &format!("theirs {}", Uuid::new_v4()),
            0.8,
            TenancyDecl::group(h1.personal_group),
        )
        .await;
        let want = match case {
            "membership" => {
                let s = admin_synthesis(a, h2.agent, "group", h2.personal_group).await;
                sqlx::query(
                    "INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
                )
                .bind(s)
                .bind(theirs)
                .execute(a)
                .await
                .unwrap();
                "synthesis_claim_membership rows cite a group claim of another group"
            }
            "countersignature" => {
                sqlx::query(INSERT_COUNTERSIGNATURE)
                    .bind(Uuid::now_v7())
                    .bind(theirs)
                    .bind(h2.agent)
                    .bind(h2.agent)
                    .bind(h2.personal_group)
                    .execute(a)
                    .await
                    .unwrap();
                "countersignatures of a group claim are not (group, the claim's group)"
            }
            _ => {
                let parent = admin_sample(a, h1.agent, h1.personal_group, "group", None).await;
                admin_sample(a, h2.agent, h2.personal_group, "public", Some(parent)).await;
                "child samples of a group sample are not (group, the parent's group)"
            }
        };
        let err = ledger::run(&mut conn)
            .await
            .expect_err("the contract step refuses");
        assert!(err.to_string().contains(want), "{case}: {err}");
        let head: i64 =
            sqlx::query_scalar("SELECT max(version) FROM episcience_meta._sqlx_migrations")
                .fetch_one(a)
                .await
                .unwrap();
        assert_eq!(head, ledger::TENANCY_EXPAND_VERSION, "{case}");
    }
}

/// R2: the rollback leaves data the previous (E1c) binary can decode. After
/// 5035-undo.sql (which clears the `input_narrowed` mark it cannot keep) and
/// e1c-rollback-vocabulary.sql, every synthesis visibility is one E1c's
/// `Visibility::from_str` accepts (`private`, `shared`, `public`) and no
/// stale reason is outside the pre-5035 vocabulary; the vocabulary script
/// refuses while 5035 is applied. E1d delta D5: before converting, the
/// script reports how many `group` rows sit in each of the four tables E1c
/// reads without ownership (one `group` and two public samples here:
/// samples 1, the others 0).
/// Kills: the conversion dropped (one `group` row fails a whole E1c list),
/// the undo refusing a narrowed row, or the exposure count dropped or
/// counting the wrong rows.
#[tokio::test]
async fn the_rollback_leaves_values_the_previous_binary_decodes() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let g = h1.personal_group;
    admin_synthesis(a, h1.agent, "group", g).await;
    let narrowed = admin_synthesis(a, h1.agent, "public", g).await;
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(narrowed)
        .bind(
            support::claim(
                a,
                h1.agent,
                &format!("own {}", Uuid::new_v4()),
                0.8,
                TenancyDecl::group(g),
            )
            .await,
        )
        .execute(a)
        .await
        .unwrap();
    let vocab = include_str!("../../../docs/runbooks/e1c-rollback-vocabulary.sql");
    assert!(
        sqlx::raw_sql(vocab).execute(a).await.is_err(),
        "the vocabulary script refuses while 5035 is applied"
    );
    sqlx::raw_sql(include_str!("../../../docs/runbooks/5035-undo.sql"))
        .execute(a)
        .await
        .expect("undo applies with a narrowed row present");
    // One `group` sample (the E1d binary's kind of row) and TWO public ones,
    // so a report counting the wrong rows (public, or every owned row) gives
    // 2 or 3, never the expected 1.
    for vis in ["group", "public", "public"] {
        sqlx::query(
            "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
             VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(h1.agent)
        .bind(g)
        .bind(vis)
        .execute(a)
        .await
        .unwrap();
    }
    let report: Vec<(String, i64)> = sqlx::raw_sql(vocab)
        .fetch_all(a)
        .await
        .expect("vocabulary script")
        .iter()
        .map(|r| (r.get("table_name"), r.get("group_rows")))
        .collect();
    assert_eq!(
        report,
        vec![
            ("blobs".to_string(), 0),
            ("countersignatures".to_string(), 0),
            ("protocols".to_string(), 0),
            ("samples".to_string(), 1),
        ],
        "the script reports the rows the previous binary would expose"
    );
    let values: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT DISTINCT visibility, stale_reason FROM syntheses ORDER BY 1")
            .fetch_all(a)
            .await
            .unwrap();
    // E1c: crates/episcience-core/src/synthesis/mod.rs::Visibility::from_str.
    const E1C_VISIBILITY: [&str; 3] = ["private", "shared", "public"];
    assert!(!values.is_empty());
    for (vis, reason) in &values {
        assert!(
            E1C_VISIBILITY.contains(&vis.as_str()),
            "{vis} is not decodable by E1c"
        );
        assert!(reason.is_none(), "{reason:?}");
    }
}
