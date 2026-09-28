//! `episcience-maint tick` (batch E1f): the narrowing sweep plus the ALERT on
//! rows it could not narrow (brief amendment 2026-09-28: E1f alerts on
//! `episcience.maint.sweep_blocked`). The REAL binary, on the real
//! `episcience_maint` login, against a fresh clone of the E1 template.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod support;

use std::process::Command;

use epigraph_core::TenancyDecl;
use sqlx::PgPool;
use support::{principal, Principal, TestDb, MAINT_LOGIN};
use uuid::Uuid;

const BIN: &str = env!("CARGO_BIN_EXE_episcience-maint");

/// Run `episcience-maint tick` on the maintenance login; `(exit code, output)`.
fn tick(db: &TestDb) -> (i32, String) {
    tick_on(&db.login_url(MAINT_LOGIN), &[])
}

/// Run `episcience-maint tick` with `url` as its DSN plus `extra` variables;
/// `(exit code, output with the DSN masked)`.
fn tick_on(url: &str, extra: &[(&str, &std::ffi::OsStr)]) -> (i32, String) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .current_dir(dir.path())
        .arg("tick")
        .env("EPISCIENCE_MAINT_DATABASE_URL", url);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run episcience-maint");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .replace(url, "<dsn>");
    (out.status.code().unwrap_or(-1), text)
}

async fn public_claim(a: &PgPool, author: &Principal) -> Uuid {
    support::claim(
        a,
        author.agent,
        &format!("tick claim {}", Uuid::new_v4()),
        0.8,
        TenancyDecl::public(author.personal_group),
    )
    .await
}

async fn narrow(a: &PgPool, claim: Uuid) {
    sqlx::query("UPDATE claims SET visibility = 'group' WHERE id = $1")
        .bind(claim)
        .execute(a)
        .await
        .expect("narrow the claim out of band");
}

async fn complete_public_synthesis(a: &PgPool, author: &Principal) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id, narrative, completed_at) \
         VALUES ($1, 'tick', $2, 'complete', '{}'::jsonb, 'signed_louvain', 'p', 'm', \
                 decode(repeat('00', 32), 'hex'), 'public', $3, 'n', now())",
    )
    .bind(id)
    .bind(author.agent)
    .bind(author.personal_group)
    .execute(a)
    .await
    .expect("synthesis");
    id
}

async fn sample(a: &PgPool, author: &Principal, parent: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO samples (id, name, sample_type, prepared_by, content_hash, parent_sample_id, \
                              owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, decode(md5($1::text) || md5($1::text), 'hex'), $3, $4, 'public')",
    )
    .bind(id)
    .bind(author.agent)
    .bind(parent)
    .bind(author.personal_group)
    .execute(a)
    .await
    .expect("sample");
    id
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

async fn audit(a: &PgPool, event: &str, key: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM security_events WHERE event_type = $1 AND details->>$2 = $3::text",
    )
    .bind(event)
    .bind(key)
    .bind(id)
    .fetch_one(a)
    .await
    .unwrap()
}

/// T-M1. A member claim narrowed out of band: the tick narrows the public
/// synthesis to `group`, its derived rows follow (propagation), one
/// `sweep_narrowed` audit row is written, and it exits 0 with nothing
/// blocked; a second tick changes nothing and writes nothing (idempotent).
/// Kills: the tick not calling the sweep, and an alert on a clean run.
#[tokio::test]
async fn t_m1_the_tick_narrows_a_synthesis_whose_member_stopped_being_public() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let x = principal(a, "x").await;
    let c = public_claim(a, &x).await;
    let s = complete_public_synthesis(a, &h1).await;
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(s)
        .bind(c)
        .execute(a)
        .await
        .unwrap();
    narrow(a, c).await;

    let (code, out) = tick(&db);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("narrowed 1; blocked 0"), "{out}");
    assert_eq!(visibility(a, "syntheses", s).await, "group");
    let member_vis: String = sqlx::query_scalar(
        "SELECT visibility::text FROM synthesis_claim_membership WHERE synthesis_id = $1",
    )
    .bind(s)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(member_vis, "group", "the derived rows follow");
    assert_eq!(
        audit(a, "episcience.maint.sweep_narrowed", "synthesis_id", s).await,
        1
    );

    let (code, out) = tick(&db);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("narrowed 0; blocked 0"), "{out}");
    assert_eq!(
        audit(a, "episcience.maint.sweep_narrowed", "synthesis_id", s).await,
        1
    );
}

/// The ALERT. A public sample the sweep cannot narrow (it cites a narrowed
/// claim, and another owner hung a public child sample under it): the tick
/// still narrows the unrelated synthesis, names the blocked sample, and exits
/// 3 (the unit fails, its hook alerts); on every run while the block
/// persists. Once an operator detaches the child, the next tick narrows the
/// sample and exits 0. Kills: the detector returning nothing (no alert), an
/// alert that stops after the first run, the tick exiting 0 while a row is
/// blocked, and an alert that outlives the remedy.
#[tokio::test]
async fn the_tick_alerts_while_the_sweep_is_blocked_and_clears_after_the_remedy() {
    let db = TestDb::fresh().await;
    let a = &db.admin;
    let h1 = principal(a, "h1").await;
    let h2 = principal(a, "h2").await;
    let x = principal(a, "x").await;
    let cited_by_sample = public_claim(a, &x).await;
    let cited_by_synthesis = public_claim(a, &x).await;
    let sm = sample(a, &h1, None).await;
    sqlx::query("INSERT INTO sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sm)
        .bind(cited_by_sample)
        .execute(a)
        .await
        .unwrap();
    let child = sample(a, &h2, Some(sm)).await;
    let s = complete_public_synthesis(a, &h1).await;
    sqlx::query("INSERT INTO synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)")
        .bind(s)
        .bind(cited_by_synthesis)
        .execute(a)
        .await
        .unwrap();
    narrow(a, cited_by_sample).await;
    narrow(a, cited_by_synthesis).await;

    for run in 1..=2 {
        let (code, out) = tick(&db);
        assert_eq!(code, 3, "run {run}: the blocked row alerts:\n{out}");
        assert!(out.contains(&format!("sample {sm}")), "{out}");
        assert!(out.contains("blocked 1"), "{out}");
        assert!(
            !out.contains(&s.to_string()),
            "the narrowed synthesis is not reported"
        );
        assert_eq!(visibility(a, "samples", sm).await, "public");
        assert_eq!(
            audit(a, "episcience.maint.sweep_blocked", "sample_id", sm).await,
            run,
            "one blocked audit row per run"
        );
    }
    assert_eq!(visibility(a, "syntheses", s).await, "group");

    // The runbook remedy: a privileged session detaches the foreign child.
    sqlx::query("UPDATE samples SET parent_sample_id = NULL WHERE id = $1")
        .bind(child)
        .execute(a)
        .await
        .expect("detach the child");
    let (code, out) = tick(&db);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("narrowed 1; blocked 0"), "{out}");
    assert_eq!(visibility(a, "samples", sm).await, "group");
}

/// The maintenance login's privileged-session refusal judges the EFFECTIVE
/// role (the reviewer's reproduction, on `episcience-maint`): a login that is
/// a member of a BYPASSRLS role, switched onto it by the DSN
/// (`options=-c role=…`), is refused with exit 2 before the sweep runs, and
/// the refusal names the switch and the attribute. Kills: the maintenance
/// binary judging `session_user` only (it then ran the sweep as a bypassing
/// role), and a second, weaker copy of the check.
#[tokio::test]
async fn the_tick_refuses_a_role_switch_onto_a_bypassrls_role() {
    let db = TestDb::fresh().await;
    let roles = db.privileged_roles().await;
    let url = db.url_as(&roles.login, &roles.password, Some(&roles.bypass));
    let (code, out) = tick_on(&url, &[]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("role switch"), "{out}");
    assert!(out.contains("BYPASSRLS"), "{out}");
    assert!(
        !out.contains("tick: narrowed"),
        "the sweep never ran:\n{out}"
    );
}

/// Brief 7.6: no EpiScience login is ever a member of the kernel seed role.
/// A login holding the maintenance ops role AND `epigraph_seed` (no
/// privileged attribute, no role switch) is refused with exit 2 before the
/// sweep runs, and the refusal names the seed role. Control: the same run on
/// the real maintenance login passes the refusal. Kills: dropping
/// `epigraph_seed` from the privileged-role set of
/// `refuse_privileged_session`.
#[tokio::test]
async fn the_tick_refuses_a_login_that_is_a_member_of_the_seed_role() {
    let db = TestDb::fresh().await;
    let seed = db.seed_member_login().await;
    let (code, out) = tick_on(&db.url_as(&seed.login, &seed.password, None), &[]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("membership reaching a privileged role") && out.contains("epigraph_seed"),
        "{out}"
    );
    assert!(
        !out.contains("tick: narrowed"),
        "the sweep never ran:\n{out}"
    );
    let (code, out) = tick(&db);
    assert_eq!(code, 0, "control: the maintenance login ticks:\n{out}");
}
