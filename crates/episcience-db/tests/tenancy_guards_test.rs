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
        "the app login writes EpiScience tables before the RLS migration"
    );
    let privileged: bool = sqlx::query_scalar("SELECT public.episcience_session_is_privileged()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert!(!privileged);
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

/// T-W2: a root row owned by the world or seed sentinel is refused (CHECK,
/// 23514), and an undeclared root is refused (23502); a declared root in a
/// real group is accepted. Kills: dropping `<t>_group_needs_real_group`, or
/// the "declared, pass, else 23502" arm.
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
        assert_eq!(code(r), "23514", "({vis}, {owner}) must be refused");
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

/// T-W21 (the database half): a PUBLIC synthesis whose prerequisite is a
/// GROUP synthesis completes as `group`, marked `input_narrowed`; one whose
/// inputs are all public completes public. Kills: the publish rule removed,
/// or its prerequisite arm removed.
#[tokio::test]
async fn completion_narrows_a_public_synthesis_with_a_private_prerequisite() {
    let c = cast().await;
    let a = &c.db.admin;
    let prereq = admin_synthesis(a, c.h1.agent, "group", c.h1.personal_group).await;
    let complete = "UPDATE syntheses SET status = 'complete', narrative = 'n', completed_at = now() WHERE id = $1";
    let mut ids = Vec::new();
    for prereqs in [vec![prereq], vec![]] {
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
        sqlx::query(complete).bind(id).execute(a).await.unwrap();
        ids.push(id);
    }
    let narrowed = sqlx::query("SELECT visibility, stale_reason FROM syntheses WHERE id = $1")
        .bind(ids[0])
        .fetch_one(a)
        .await
        .unwrap();
    assert_eq!(narrowed.get::<String, _>(0), "group");
    assert_eq!(
        narrowed.get::<Option<String>, _>(1).as_deref(),
        Some("input_narrowed")
    );
    assert_eq!(pair_of(a, "syntheses", ids[1]).await.1, "public");
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
/// parent's pair. T-W20 (this batch's form, before row security): a child
/// sample of a `group(H1pg)` sample declared in another group is 42501 (with
/// row security the parent becomes invisible and it is 23503). Kills: the
/// refinement / child-sample owner arms removed.
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
    assert_eq!(code(r), "42501");
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
