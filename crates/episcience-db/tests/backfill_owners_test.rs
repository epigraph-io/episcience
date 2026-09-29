//! T-M2: the one-shot legacy re-own (`episcience_maint_backfill_owners`,
//! `episcience_maint_backfill_reverse`, migration 5034).
//!
//! Every case runs on a kernel-only clone migrated to 5034 EXACTLY (the
//! deploy's window between the expand step and the contract step), and calls
//! the definers through the real `episcience_maint` login, which holds no table
//! privilege of its own: the calls succeed only because the definers are
//! maintenance-owned and EXECUTE-able through `episcience_maint_ops`.
//!
//! The legacy rows are written on the admin pool with no ownership pair, the
//! way a legacy database holds them.
mod support;
use support::{Principal, TestDb, APP_LOGIN, MAINT_LOGIN};

use episcience_db::ledger;
use serde_json::Value;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

/// A kernel-only clone with the EpiScience migrations applied up to 5034.
async fn at_5034() -> TestDb {
    let db = TestDb::fresh_kernel_only().await;
    let mut c = ledger::connect_with(db.admin_options())
        .await
        .expect("ledger connection");
    ledger::run_to(&mut c, Some(ledger::TENANCY_EXPAND_VERSION))
        .await
        .expect("episcience migrations up to 5034");
    db
}

/// The maintenance login, asserted to be what the definers need it to be: no
/// table privilege of its own, not privileged.
async fn maint(db: &TestDb) -> PgConnection {
    let mut c = PgConnection::connect_with(&db.login_options(MAINT_LOGIN))
        .await
        .expect("connect as episcience_maint");
    let (sup, bypass, member, sel): (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT r.rolsuper, r.rolbypassrls, \
                pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER'), \
                has_table_privilege(session_user, 'public.syntheses', 'SELECT') \
           FROM pg_roles r WHERE r.rolname = session_user",
    )
    .fetch_one(&mut c)
    .await
    .expect("role attributes");
    assert!(
        !sup && !bypass && !member && !sel,
        "the maintenance login must be unprivileged and hold no table privilege"
    );
    c
}

async fn backfill(c: &mut PgConnection, principal: Uuid, apply: bool) -> Result<Value, String> {
    sqlx::query_scalar::<_, Value>(
        "SELECT manifest FROM public.episcience_maint_backfill_owners($1, $2)",
    )
    .bind(principal)
    .bind(apply)
    .fetch_one(c)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(d) => format!("{}: {}", d.code().unwrap_or_default(), d.message()),
        other => other.to_string(),
    })
}

async fn reverse(c: &mut PgConnection, manifest: &Value) -> Result<i32, String> {
    sqlx::query_scalar::<_, i32>("SELECT public.episcience_maint_backfill_reverse($1)")
        .bind(manifest)
        .fetch_one(c)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(d) => {
                format!("{}: {}", d.code().unwrap_or_default(), d.message())
            }
            other => other.to_string(),
        })
}

/// `(id, state, principal_id, owner_group_id, visibility)` of a job row.
type JobRow = (Uuid, String, Option<Uuid>, Option<Uuid>, Option<String>);

/// The legacy fixture: rows authored by a SHARED agent (no personal group of
/// its own is needed), none carrying a pair.
struct Legacy {
    shared: Uuid,
    private_synth: Uuid,
    public_synth: Uuid,
    complete_without_job: Uuid,
    protocol: Uuid,
    sample: Uuid,
    blob_root: Uuid,
    blob_on_sample: Uuid,
    cluster: Uuid,
    claim: Uuid,
}

async fn agent(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    let mut pk = [0u8; 32];
    pk[..16].copy_from_slice(id.as_bytes());
    pk[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    sqlx::query(
        "INSERT INTO public.agents (id, public_key, display_name, agent_type, role, state) \
         VALUES ($1, $2, $3, 'service', 'custom', 'active')",
    )
    .bind(id)
    .bind(&pk[..])
    .bind(format!("fixture-{label}-{id}"))
    .execute(pool)
    .await
    .expect("insert agent");
    id
}

async fn legacy_synthesis(pool: &PgPool, author: Uuid, visibility: &str, complete: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, narrative, completed_at) \
         VALUES ($1, 'q', $2, $3, '{}'::jsonb, 'signed_louvain', 'p', 'm', $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(author)
    .bind(if complete { "complete" } else { "failed" })
    .bind(&[7u8; 32][..])
    .bind(visibility)
    .bind(complete.then_some("n"))
    .bind(complete.then(chrono::Utc::now))
    .execute(pool)
    .await
    .expect("legacy synthesis");
    id
}

async fn seed_legacy(db: &TestDb) -> Legacy {
    let pool = &db.admin;
    let shared = agent(pool, "shared").await;
    let owner_of_claim = support::principal(pool, "claim-author").await;
    let claim = support::claim(
        pool,
        owner_of_claim.agent,
        &format!("legacy member claim {}", Uuid::new_v4()),
        0.8,
        epigraph_core::TenancyDecl::public(owner_of_claim.personal_group),
    )
    .await;

    let private_synth = legacy_synthesis(pool, shared, "private", false).await;
    let public_synth = legacy_synthesis(pool, shared, "public", true).await;
    let complete_without_job = legacy_synthesis(pool, shared, "shared", true).await;
    // One legacy job (failed) for the private synthesis, one (complete) for the
    // public one; none for the third.
    for (id, state) in [(private_synth, "failed"), (public_synth, "complete")] {
        sqlx::query(
            "INSERT INTO public.synthesis_jobs (id, job_type, payload, state) \
             VALUES ($1, 'synthesis', jsonb_build_object('synthesis_id', $1, 'agent_id', $2), $3)",
        )
        .bind(id)
        .bind(shared)
        .bind(state)
        .execute(pool)
        .await
        .expect("legacy job");
    }
    let cluster = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
             member_claim_ids, support_count, contradict_count) \
         VALUES ($1, $2, 0, 't', 's', ARRAY[$3]::uuid[], 1, 0)",
    )
    .bind(cluster)
    .bind(public_synth)
    .bind(claim)
    .execute(pool)
    .await
    .expect("legacy cluster");
    sqlx::query(
        "INSERT INTO public.synthesis_claim_membership (synthesis_id, claim_id) VALUES ($1, $2)",
    )
    .bind(public_synth)
    .bind(claim)
    .execute(pool)
    .await
    .expect("legacy membership");
    sqlx::query(
        "INSERT INTO public.synthesis_provo_edges (synthesis_id, predicate, target_kind, target_id) \
         VALUES ($1, 'WAS_DERIVED_FROM', 'claim', $2)",
    )
    .bind(public_synth)
    .bind(claim)
    .execute(pool)
    .await
    .expect("legacy outbox row");

    let protocol = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.protocols (id, title, authored_by, content_hash) VALUES ($1, 'p', $2, $3)",
    )
    .bind(protocol)
    .bind(shared)
    .bind(&[1u8; 32][..])
    .execute(pool)
    .await
    .expect("legacy protocol");
    let sample = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.samples (id, name, sample_type, prepared_by, content_hash) \
         VALUES ($1, 's', 'workflow_run', $2, $3)",
    )
    .bind(sample)
    .bind(shared)
    .bind(&[2u8; 32][..])
    .execute(pool)
    .await
    .expect("legacy sample");
    sqlx::query("INSERT INTO public.sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sample)
        .bind(claim)
        .execute(pool)
        .await
        .expect("legacy sample claim");
    let mut blobs = Vec::new();
    for on_sample in [None, Some(sample)] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO public.blobs (id, filename, mime_type, size_bytes, content_hash, uploader_id, sample_id) \
             VALUES ($1, 'f', 'text/plain', 1, $2, $3, $4)",
        )
        .bind(id)
        .bind(&[3u8; 32][..])
        .bind(shared)
        .bind(on_sample)
        .execute(pool)
        .await
        .expect("legacy blob");
        blobs.push(id);
    }
    Legacy {
        shared,
        private_synth,
        public_synth,
        complete_without_job,
        protocol,
        sample,
        blob_root: blobs[0],
        blob_on_sample: blobs[1],
        cluster,
        claim,
    }
}

/// Every row's pair across the twelve tenancy tables, keyed by table + key,
/// plus each job's principal: a whole-state fingerprint for "nothing changed".
async fn all_pairs(pool: &PgPool) -> Vec<(String, Option<Uuid>, Option<String>)> {
    sqlx::query_as(
        "SELECT 'syntheses:' || id, owner_group_id, visibility::text FROM public.syntheses \
         UNION ALL SELECT 'synthesis_clusters:' || id, owner_group_id, visibility FROM public.synthesis_clusters \
         UNION ALL SELECT 'synthesis_embeddings:' || synthesis_id, owner_group_id, visibility FROM public.synthesis_embeddings \
         UNION ALL SELECT 'synthesis_staleness_events:' || id, owner_group_id, visibility FROM public.synthesis_staleness_events \
         UNION ALL SELECT 'synthesis_provo_edges:' || synthesis_id || target_id, owner_group_id, visibility FROM public.synthesis_provo_edges \
         UNION ALL SELECT 'synthesis_claim_membership:' || synthesis_id || claim_id, owner_group_id, visibility FROM public.synthesis_claim_membership \
         UNION ALL SELECT 'synthesis_jobs:' || id, owner_group_id, visibility || '/' || coalesce(principal_id::text, '-') FROM public.synthesis_jobs \
         UNION ALL SELECT 'samples:' || id, owner_group_id, visibility FROM public.samples \
         UNION ALL SELECT 'sample_claims:' || sample_id || claim_id, owner_group_id, visibility FROM public.sample_claims \
         UNION ALL SELECT 'protocols:' || id, owner_group_id, visibility FROM public.protocols \
         UNION ALL SELECT 'blobs:' || id, owner_group_id, visibility FROM public.blobs \
         UNION ALL SELECT 'countersignatures:' || id, owner_group_id, visibility FROM public.countersignatures \
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .expect("pairs")
}

async fn pair(
    pool: &PgPool,
    table: &str,
    key_col: &str,
    key: Uuid,
) -> (Option<Uuid>, Option<String>) {
    sqlx::query_as(&format!(
        "SELECT owner_group_id, visibility::text FROM public.{table} WHERE {key_col} = $1"
    ))
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("pair of {table} {key}: {e}"))
}

async fn backfill_events(pool: &PgPool, kind: &str) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT details->>'table', (details->>'rows')::bigint FROM public.security_events \
          WHERE event_type = $1 ORDER BY 1",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("security events")
}

/// Dry run: the manifest names every row an apply would change, and NOTHING
/// persists (pairs, job principals, inserted job rows, audit rows). Kills: a
/// dry run that applies (the rollback removed), or one that returns an empty
/// manifest (the statements skipped).
#[tokio::test]
async fn a_dry_run_reports_the_changes_and_changes_nothing() {
    let db = at_5034().await;
    let l = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let before = all_pairs(&db.admin).await;
    let jobs_before: i64 = sqlx::query_scalar("SELECT count(*) FROM public.synthesis_jobs")
        .fetch_one(&db.admin)
        .await
        .unwrap();

    let mut m = maint(&db).await;
    let manifest = backfill(&mut m, p_h.agent, false).await.expect("dry run");

    assert_eq!(manifest["applied"], Value::Bool(false));
    assert_eq!(
        manifest["target_group"],
        serde_json::json!(p_h.personal_group)
    );
    assert_eq!(manifest["counts"]["syntheses"], 3, "{manifest}");
    assert_eq!(
        manifest["counts"]["synthesis_jobs"], 3,
        "two updated + one inserted: {manifest}"
    );
    assert_eq!(manifest["counts"]["blobs"], 2);
    assert_eq!(
        all_pairs(&db.admin).await,
        before,
        "a dry run must change no pair"
    );
    let jobs_after: i64 = sqlx::query_scalar("SELECT count(*) FROM public.synthesis_jobs")
        .fetch_one(&db.admin)
        .await
        .unwrap();
    assert_eq!(jobs_after, jobs_before, "a dry run must insert no job row");
    assert!(
        backfill_events(&db.admin, "episcience.maint.backfill_owners")
            .await
            .is_empty()
    );
    let _ = l;
}

/// Apply: roots to the target group with the brief's visibility mapping,
/// derived rows from their parent, jobs get the principal, a missing job row
/// is inserted complete, authorship untouched, one audit row per table.
/// Kills: the visibility mapping inverted, a derived table left out, the job
/// principal not set, the complete job not inserted, an author column
/// rewritten, the audit row dropped.
#[tokio::test]
async fn apply_reowns_roots_and_derives_children() {
    let db = at_5034().await;
    let l = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let g = p_h.personal_group;
    let mut m = maint(&db).await;
    let manifest = backfill(&mut m, p_h.agent, true).await.expect("apply");
    assert_eq!(manifest["applied"], Value::Bool(true));

    let a = &db.admin;
    assert_eq!(
        pair(a, "syntheses", "id", l.private_synth).await,
        (Some(g), Some("group".into()))
    );
    assert_eq!(
        pair(a, "syntheses", "id", l.public_synth).await,
        (Some(g), Some("public".into()))
    );
    assert_eq!(
        pair(a, "syntheses", "id", l.complete_without_job).await,
        (Some(g), Some("group".into())),
        "legacy 'shared' maps to group"
    );
    for (t, k, id) in [
        ("protocols", "id", l.protocol),
        ("samples", "id", l.sample),
        ("blobs", "id", l.blob_root),
        ("blobs", "id", l.blob_on_sample),
        ("synthesis_clusters", "id", l.cluster),
    ] {
        assert_eq!(
            pair(a, t, k, id).await,
            (Some(g), Some("public".into())),
            "{t}"
        );
    }
    let (o, v): (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT owner_group_id, visibility FROM public.sample_claims WHERE sample_id = $1 AND claim_id = $2",
    )
    .bind(l.sample)
    .bind(l.claim)
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!((o, v.as_deref()), (Some(g), Some("public")));
    let unowned: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM public.synthesis_claim_membership WHERE owner_group_id IS NULL) \
              + (SELECT count(*) FROM public.synthesis_provo_edges WHERE owner_group_id IS NULL)",
    )
    .fetch_one(a)
    .await
    .unwrap();
    assert_eq!(unowned, 0);

    let jobs: Vec<JobRow> = sqlx::query_as(
        "SELECT id, state, principal_id, owner_group_id, visibility FROM public.synthesis_jobs ORDER BY id",
    )
    .fetch_all(a)
    .await
    .unwrap();
    assert_eq!(jobs.len(), 3, "exactly one job row per synthesis");
    for (id, _, principal, owner, _) in &jobs {
        assert_eq!(*principal, Some(p_h.agent), "job {id}");
        assert_eq!(*owner, Some(g));
    }
    let inserted = jobs
        .iter()
        .find(|j| j.0 == l.complete_without_job)
        .expect("inserted job");
    assert_eq!(inserted.1, "complete");
    assert_eq!(inserted.4.as_deref(), Some("group"));

    let authors: Vec<Uuid> = sqlx::query_scalar(
        "SELECT agent_id FROM public.syntheses UNION ALL SELECT authored_by FROM public.protocols \
         UNION ALL SELECT prepared_by FROM public.samples UNION ALL SELECT uploader_id FROM public.blobs",
    )
    .fetch_all(a)
    .await
    .unwrap();
    assert!(
        authors.iter().all(|x| *x == l.shared),
        "authorship must be untouched"
    );

    let events = backfill_events(a, "episcience.maint.backfill_owners").await;
    let mut names: Vec<&str> = events.iter().map(|e| e.0.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "blobs",
            "protocols",
            "sample_claims",
            "samples",
            "syntheses",
            "synthesis_claim_membership",
            "synthesis_clusters",
            "synthesis_jobs",
            "synthesis_provo_edges"
        ]
    );
    assert!(events.contains(&("syntheses".to_string(), 3)));
    let agentless: bool = sqlx::query_scalar(
        "SELECT bool_and(agent_id IS NULL) FROM public.security_events WHERE event_type = 'episcience.maint.backfill_owners'",
    )
    .fetch_one(a)
    .await
    .unwrap();
    assert!(agentless);

    // Idempotent: a second apply finds nothing without an owner.
    let again = backfill(&mut m, p_h.agent, true)
        .await
        .expect("second apply");
    for (t, n) in again["counts"].as_object().unwrap() {
        assert_eq!(n, 0, "second apply changed {t}");
    }
}

/// A synthesis a DIFFERENT principal created during the deploy window (its
/// pair declared by the new binary) keeps its owner, and so do its derived
/// rows, which the backfill derives from the parent and never from the target
/// group. Kills: derived rows set to the target group directly.
#[tokio::test]
async fn a_derived_row_of_another_owners_synthesis_keeps_that_owner() {
    let db = at_5034().await;
    let _ = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let h2 = support::principal(&db.admin, "h2").await;
    let theirs = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id) \
         VALUES ($1, 'q', $2, 'running', '{}'::jsonb, 'signed_louvain', 'p', 'm', $3, 'group', $4)",
    )
    .bind(theirs)
    .bind(h2.agent)
    .bind(&[9u8; 32][..])
    .bind(h2.personal_group)
    .execute(&db.admin)
    .await
    .unwrap();
    let their_cluster = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.synthesis_clusters (id, synthesis_id, cluster_index, title, summary, \
             member_claim_ids, support_count, contradict_count) \
         VALUES ($1, $2, 0, 't', 's', ARRAY[gen_random_uuid()], 0, 0)",
    )
    .bind(their_cluster)
    .bind(theirs)
    .execute(&db.admin)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO public.synthesis_jobs (id, job_type, payload, state, principal_id) \
         VALUES ($1, 'synthesis', '{}'::jsonb, 'queued', $2)",
    )
    .bind(theirs)
    .bind(h2.agent)
    .execute(&db.admin)
    .await
    .unwrap();

    let mut m = maint(&db).await;
    backfill(&mut m, p_h.agent, true).await.expect("apply");
    assert_eq!(
        pair(&db.admin, "syntheses", "id", theirs).await,
        (Some(h2.personal_group), Some("group".into()))
    );
    assert_eq!(
        pair(&db.admin, "synthesis_clusters", "id", their_cluster).await,
        (Some(h2.personal_group), Some("group".into())),
        "the derived row takes its parent's owner, not the target group"
    );
    let (principal, owner): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT principal_id, owner_group_id FROM public.synthesis_jobs WHERE id = $1",
    )
    .bind(theirs)
    .fetch_one(&db.admin)
    .await
    .unwrap();
    assert_eq!(
        principal,
        Some(h2.agent),
        "another principal's job keeps its principal"
    );
    assert_eq!(owner, Some(h2.personal_group));
}

/// Refusals, each before anything changes. Kills: provisioning the missing
/// group, accepting a revoked membership, skipping the operator-link check,
/// assigning ownerless countersignatures.
#[tokio::test]
async fn the_backfill_refuses_without_a_live_personal_group_or_for_an_operated_principal() {
    let db = at_5034().await;
    let _ = seed_legacy(&db).await;
    let before = all_pairs(&db.admin).await;
    let mut m = maint(&db).await;

    // No personal group at all: refused, and never provisioned.
    let bare = agent(&db.admin, "no-group").await;
    let e = backfill(&mut m, bare, true).await.expect_err("no group");
    assert!(
        e.starts_with("55000") && e.contains("no personal group"),
        "{e}"
    );
    let provisioned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM public.groups WHERE did_key = 'did:epigraph:personal:' || $1::text)",
    )
    .bind(bare)
    .fetch_one(&db.admin)
    .await
    .unwrap();
    assert!(!provisioned, "the backfill must never provision a group");

    // A revoked admin membership: refused.
    let revoked = support::principal(&db.admin, "revoked").await;
    sqlx::query("UPDATE public.group_memberships SET revoked_at = now() WHERE group_id = $1 AND agent_id = $2")
        .bind(revoked.personal_group)
        .bind(revoked.agent)
        .execute(&db.admin)
        .await
        .unwrap();
    let e = backfill(&mut m, revoked.agent, true)
        .await
        .expect_err("revoked");
    assert!(e.contains("no personal group"), "{e}");

    // An operated principal: refused (its tokens are refused by the kernel).
    let operated = support::principal(&db.admin, "operated").await;
    let operator = support::principal(&db.admin, "its-operator").await;
    sqlx::query("SELECT * FROM public.epigraph_link_operator($1, $2)")
        .bind(operated.agent)
        .bind(operator.agent)
        .execute(&db.admin)
        .await
        .expect("link the fixture agent to an operator");
    let e = backfill(&mut m, operated.agent, true)
        .await
        .expect_err("operated");
    assert!(e.starts_with("55000") && e.contains("operated"), "{e}");

    // Ownerless countersignatures: refused, naming them.
    let p_h = support::principal(&db.admin, "operator").await;
    let claim = support::claim(
        &db.admin,
        p_h.agent,
        &format!("countersigned {}", Uuid::new_v4()),
        0.7,
        epigraph_core::TenancyDecl::public(p_h.personal_group),
    )
    .await;
    sqlx::query(
        "INSERT INTO public.countersignatures (claim_id, signer_id, signature_meaning, content_hash, signature) \
         VALUES ($1, $2, 'witnessed', $3, $4)",
    )
    .bind(claim)
    .bind(p_h.agent)
    .bind(&[4u8; 32][..])
    .bind(&[5u8; 64][..])
    .execute(&db.admin)
    .await
    .unwrap();
    let e = backfill(&mut m, p_h.agent, true)
        .await
        .expect_err("countersignatures");
    assert!(e.contains("countersignature"), "{e}");

    let mut after = all_pairs(&db.admin).await;
    after.retain(|r| !r.0.starts_with("countersignatures:"));
    assert_eq!(after, before, "a refused backfill changes nothing");
}

/// Reverse restores exactly what the manifest recorded, leaves a row changed
/// after the manifest alone, deletes the job row the backfill inserted, and
/// clears the principal it set. Kills: a reverse that ignores the current
/// pair (it would undo a later re-own), or that leaves inserted jobs behind.
#[tokio::test]
async fn reverse_restores_the_manifest_and_leaves_later_changes_alone() {
    let db = at_5034().await;
    let l = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let other = support::principal(&db.admin, "later-owner").await;
    let before = all_pairs(&db.admin).await;
    let mut m = maint(&db).await;
    let manifest = backfill(&mut m, p_h.agent, true).await.expect("apply");

    // After the manifest was written, one protocol is re-owned elsewhere.
    sqlx::query("UPDATE public.protocols SET owner_group_id = $2 WHERE id = $1")
        .bind(l.protocol)
        .bind(other.personal_group)
        .execute(&db.admin)
        .await
        .unwrap();

    let restored = reverse(&mut m, &manifest).await.expect("reverse");
    assert!(restored > 0);
    assert_eq!(
        pair(&db.admin, "protocols", "id", l.protocol).await,
        (Some(other.personal_group), Some("public".into())),
        "a row changed after the manifest keeps its new owner"
    );
    let mut after = all_pairs(&db.admin).await;
    let mut want = before.clone();
    after.retain(|r| r.0 != format!("protocols:{}", l.protocol));
    want.retain(|r| r.0 != format!("protocols:{}", l.protocol));
    assert_eq!(
        after, want,
        "every other row is back to its pre-backfill state"
    );
    let legacy_vis: String =
        sqlx::query_scalar("SELECT visibility FROM public.syntheses WHERE id = $1")
            .bind(l.private_synth)
            .fetch_one(&db.admin)
            .await
            .unwrap();
    assert_eq!(legacy_vis, "private", "the legacy visibility is restored");
    let job: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM public.synthesis_jobs WHERE id = $1")
            .bind(l.complete_without_job)
            .fetch_optional(&db.admin)
            .await
            .unwrap();
    assert_eq!(job, None, "the job row the backfill inserted is removed");
    assert!(
        !backfill_events(&db.admin, "episcience.maint.backfill_reverse")
            .await
            .is_empty()
    );

    // A dry-run manifest is refused outright.
    let dry = backfill(&mut m, p_h.agent, false).await.expect("dry run");
    let e = reverse(&mut m, &dry).await.expect_err("dry manifest");
    assert!(e.contains("dry run"), "{e}");
}

/// The application login cannot reach the one-shot definers. Kills: a GRANT
/// to PUBLIC or to the application roles.
#[tokio::test]
async fn the_application_login_cannot_execute_the_backfill() {
    let db = at_5034().await;
    let p: Principal = support::principal(&db.admin, "operator").await;
    let mut app = PgConnection::connect_with(&db.login_options(APP_LOGIN))
        .await
        .expect("connect as episcience_app");
    let e = backfill(&mut app, p.agent, false)
        .await
        .expect_err("app login");
    assert!(e.starts_with("42501"), "{e}");
    let e = reverse(&mut app, &serde_json::json!({}))
        .await
        .expect_err("app login");
    assert!(e.starts_with("42501"), "{e}");
}

/// The reviewer's forgery: the narrow maintenance login hands the reverse a
/// manifest no backfill applied (naming another principal's declared group
/// synthesis with a `public` before-visibility). It is refused (22023) and
/// the row keeps its pair; so is a real applied manifest with one entry
/// removed (its hash no longer matches). The maintenance login cannot write
/// the audit rows the hash is checked against. When an audit row IS present
/// for a forged manifest (the application-role forgery residual until the
/// kernel restricts the `episcience.` prefix), the reverse still refuses an
/// entry that maps visibility in a way the backfill never does. Kills: the
/// recorded-hash check removed (the forged manifest would widen the row to
/// `(NULL, public)`), the principal/target comparison removed, or the
/// mapping check removed.
#[tokio::test]
async fn reverse_refuses_a_manifest_no_applied_backfill_recorded() {
    let db = at_5034().await;
    let _ = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let h2 = support::principal(&db.admin, "h2").await;
    let theirs = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.syntheses (id, query, agent_id, status, subgraph_snapshot, clustering_method, \
             llm_provider, llm_model, content_hash, visibility, owner_group_id) \
         VALUES ($1, 'q', $2, 'running', '{}'::jsonb, 'signed_louvain', 'p', 'm', $3, 'group', $4)",
    )
    .bind(theirs)
    .bind(h2.agent)
    .bind(&[9u8; 32][..])
    .bind(h2.personal_group)
    .execute(&db.admin)
    .await
    .unwrap();
    let mut m = maint(&db).await;
    let applied = backfill(&mut m, p_h.agent, true).await.expect("apply");

    let forged = serde_json::json!({
        "kind": "episcience.backfill_owners.v1",
        "applied": true,
        "principal": p_h.agent,
        "target_group": applied["target_group"],
        "tables": {"syntheses": [{
            "id": theirs,
            "after_owner": h2.personal_group,
            "after_visibility": "group",
            "before_visibility": "public"
        }]}
    });
    let e = reverse(&mut m, &forged).await.expect_err("forged manifest");
    assert!(
        e.starts_with("22023") && e.contains("no applied backfill"),
        "{e}"
    );
    assert_eq!(
        pair(&db.admin, "syntheses", "id", theirs).await,
        (Some(h2.personal_group), Some("group".into())),
        "the forged manifest changed nothing"
    );

    let mut trimmed = applied.clone();
    trimmed["tables"]["protocols"]
        .as_array_mut()
        .expect("protocols entries")
        .pop();
    let e = reverse(&mut m, &trimmed)
        .await
        .expect_err("tampered manifest");
    assert!(e.contains("no applied backfill"), "{e}");

    let mut other_principal = applied.clone();
    other_principal["principal"] = serde_json::json!(h2.agent);
    let e = reverse(&mut m, &other_principal)
        .await
        .expect_err("another principal");
    assert!(e.contains("no applied backfill"), "{e}");

    let can_insert: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('episcience_maint', 'public.security_events', 'INSERT')",
    )
    .fetch_one(&db.admin)
    .await
    .unwrap();
    assert!(
        !can_insert,
        "the maintenance login must not write the audit rows the reverse trusts"
    );
    let direct = sqlx::query(
        "INSERT INTO public.security_events (event_type, agent_id, success, details) \
         VALUES ('episcience.maint.backfill_owners', NULL, true, '{}'::jsonb)",
    )
    .execute(&mut m)
    .await;
    assert!(direct.is_err(), "the maintenance login wrote an audit row");

    // The residual: an audit row for the forged manifest written by a
    // broader role. The mapping check still refuses the widening entry.
    sqlx::query(
        "INSERT INTO public.security_events (event_type, agent_id, success, details) \
         VALUES ('episcience.maint.backfill_owners', NULL, true, jsonb_build_object( \
            'table', 'syntheses', 'rows', 1, 'applied', true, 'principal', $1::text, \
            'target_group', $2::text, \
            'manifest_sha256', encode(sha256(convert_to($3::jsonb::text, 'UTF8')), 'hex')))",
    )
    .bind(p_h.agent)
    .bind(applied["target_group"].as_str().unwrap())
    .bind(&forged["tables"])
    .execute(&db.admin)
    .await
    .unwrap();
    let e = reverse(&mut m, &forged)
        .await
        .expect_err("widening mapping");
    assert!(
        e.starts_with("22023") && e.contains("maps visibility"),
        "{e}"
    );
    assert_eq!(
        pair(&db.admin, "syntheses", "id", theirs).await,
        (Some(h2.personal_group), Some("group".into()))
    );

    // The genuine manifest still reverses.
    assert!(reverse(&mut m, &applied).await.expect("the real manifest") > 0);
}

/// The target group must be one the principal ADMINISTERS: a principal whose
/// own personal-group membership is `writer` (or `reader`) is refused, and
/// nothing changes. Kills: the `role = 'admin'` condition dropped from the
/// target-group lookup.
#[tokio::test]
async fn the_backfill_refuses_a_principal_that_is_not_admin_of_its_personal_group() {
    let db = at_5034().await;
    let _ = seed_legacy(&db).await;
    let before = all_pairs(&db.admin).await;
    let mut m = maint(&db).await;
    for role in ["writer", "reader"] {
        let p = support::principal(&db.admin, role).await;
        sqlx::query(
            "UPDATE public.group_memberships SET role = $3 WHERE group_id = $1 AND agent_id = $2",
        )
        .bind(p.personal_group)
        .bind(p.agent)
        .bind(role)
        .execute(&db.admin)
        .await
        .unwrap();
        let e = backfill(&mut m, p.agent, true).await.expect_err(role);
        assert!(
            e.starts_with("55000") && e.contains("no personal group"),
            "{role}: {e}"
        );
    }
    assert_eq!(all_pairs(&db.admin).await, before);
}

/// Reverse leaves a SYNTHESIS changed after the apply alone (narrowed, or
/// re-owned), like any other row: its current pair no longer equals the
/// manifest's after-pair. Kills: the synthesis reverse ignoring the current
/// pair (it would clear the owner of a row someone re-owned since).
#[tokio::test]
async fn reverse_leaves_a_synthesis_changed_after_the_apply_alone() {
    let db = at_5034().await;
    let l = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let other = support::principal(&db.admin, "later-owner").await;
    let mut m = maint(&db).await;
    let manifest = backfill(&mut m, p_h.agent, true).await.expect("apply");
    sqlx::query("UPDATE public.syntheses SET visibility = 'group' WHERE id = $1")
        .bind(l.public_synth)
        .execute(&db.admin)
        .await
        .unwrap();
    sqlx::query("UPDATE public.syntheses SET owner_group_id = $2 WHERE id = $1")
        .bind(l.private_synth)
        .bind(other.personal_group)
        .execute(&db.admin)
        .await
        .unwrap();
    reverse(&mut m, &manifest).await.expect("reverse");
    assert_eq!(
        pair(&db.admin, "syntheses", "id", l.public_synth).await,
        (
            Some(manifest["target_group"].as_str().unwrap().parse().unwrap()),
            Some("group".into())
        ),
        "narrowed after the apply: left alone"
    );
    assert_eq!(
        pair(&db.admin, "syntheses", "id", l.private_synth).await,
        (Some(other.personal_group), Some("group".into())),
        "re-owned after the apply: left alone"
    );
    assert_eq!(
        pair(&db.admin, "syntheses", "id", l.complete_without_job).await,
        (None, Some("shared".into())),
        "an unchanged row is restored"
    );
}

/// A sample ANOTHER principal created in the deploy window (declared, in its
/// own group) whose observation link and blob were written without a pair
/// keep THAT sample's owner: the backfill derives them from their parent,
/// never from the target group. Kills: sample_claims or an attached blob
/// taking the target group directly.
#[tokio::test]
async fn window_children_of_another_owners_sample_keep_that_owner() {
    let db = at_5034().await;
    let l = seed_legacy(&db).await;
    let p_h = support::principal(&db.admin, "operator").await;
    let h2 = support::principal(&db.admin, "h2").await;
    let sample = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.samples (id, name, sample_type, prepared_by, content_hash, owner_group_id, visibility) \
         VALUES ($1, 's', 'chemical', $2, $3, $4, 'public')",
    )
    .bind(sample)
    .bind(h2.agent)
    .bind(&[8u8; 32][..])
    .bind(h2.personal_group)
    .execute(&db.admin)
    .await
    .unwrap();
    sqlx::query("INSERT INTO public.sample_claims (sample_id, claim_id) VALUES ($1, $2)")
        .bind(sample)
        .bind(l.claim)
        .execute(&db.admin)
        .await
        .unwrap();
    let blob = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO public.blobs (id, filename, mime_type, size_bytes, content_hash, uploader_id, sample_id) \
         VALUES ($1, 'f', 'text/plain', 1, $2, $3, $4)",
    )
    .bind(blob)
    .bind(&[6u8; 32][..])
    .bind(h2.agent)
    .bind(sample)
    .execute(&db.admin)
    .await
    .unwrap();
    let mut m = maint(&db).await;
    backfill(&mut m, p_h.agent, true).await.expect("apply");
    let link: (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT owner_group_id, visibility FROM public.sample_claims WHERE sample_id = $1",
    )
    .bind(sample)
    .fetch_one(&db.admin)
    .await
    .unwrap();
    let theirs = (Some(h2.personal_group), Some("public".to_string()));
    assert_eq!(link, theirs, "the link follows its sample's owner");
    assert_eq!(
        pair(&db.admin, "blobs", "id", blob).await,
        theirs,
        "the blob follows its sample's owner"
    );
}
