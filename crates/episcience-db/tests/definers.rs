//! R5 (with R5b) through `episcience-migrate verify`, the deploy guard: the
//! SECURITY DEFINER `episcience_*` functions are exactly the closed set
//! `catalog::DEFINERS` (brief 7.4), each owned by the kernel maintenance
//! role, with `search_path` pinned and EXECUTE held by exactly its one role;
//! no trigger on an EpiScience table runs a definer other than the
//! statement-level propagation. `verify` also refuses row security that is
//! not enabled and forced, table ACLs that differ from the grant matrix, a
//! ledger schema that grants anything, and a sentinel-owned row.
mod support;
use support::{mutated_clone, principal, TestDb};

use episcience_db::catalog::{self, DEFINERS};
use episcience_db::ledger;

/// The literal is the brief's set, the catalog agrees on the template, and
/// `verify` passes there. Kills: a definer dropped from (or added to) the
/// literal without the migration, or the reverse.
#[tokio::test]
async fn the_definer_set_is_closed_and_verify_passes_on_the_template() {
    let mut names: Vec<&str> = DEFINERS
        .iter()
        .map(|d| d.signature.split('(').next().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "episcience_countersign_chain_head",
            "episcience_maint_backfill_owners",
            "episcience_maint_backfill_reverse",
            "episcience_maint_sweep_narrowed",
            "episcience_owner_worklist",
            "episcience_propagate_parent_tenancy",
            "episcience_queue_claim",
            "episcience_queue_finish",
            "episcience_queue_retry",
        ]
    );
    let db = TestDb::fresh().await;
    let definers: Vec<String> = sqlx::query_scalar(
        "SELECT regexp_replace(oid::regprocedure::text, '^public\\.', '') FROM pg_proc \
          WHERE pronamespace = 'public'::regnamespace AND prosecdef AND proname LIKE 'episcience\\_%' \
          ORDER BY 1",
    )
    .fetch_all(&db.admin)
    .await
    .unwrap();
    let mut want: Vec<String> = DEFINERS.iter().map(|d| d.signature.to_string()).collect();
    want.sort();
    assert_eq!(definers, want);
    let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
    assert_eq!(
        catalog::findings(&mut conn).await.unwrap(),
        Vec::<String>::new()
    );
    ledger::verify(&mut conn).await.expect("verify passes");
}

/// `verify` refuses each drift and names it. Kills, one per case: an extra
/// definer, a definer owned by the migration superuser, a queue definer
/// executable by the application role or by PUBLIC, a definer whose
/// `search_path` is not pinned, a definer turned INVOKER (a missing member
/// of the set), a BEFORE ROW guard made a maintenance-owned definer (R5b),
/// FORCE dropped, row security disabled, an extra table grant, the ledger
/// schema opened, a sentinel-owned row.
#[tokio::test]
async fn verify_refuses_each_catalog_drift_and_names_it() {
    let cases: [(&str, &[&str]); 12] = [
        (
            "CREATE FUNCTION public.episcience_extra() RETURNS integer LANGUAGE sql \
             SECURITY DEFINER SET search_path = public, pg_temp AS 'SELECT 1'",
            &["R5: episcience_extra() is SECURITY DEFINER but not in the definer set"],
        ),
        (
            "ALTER FUNCTION public.episcience_queue_claim(text) OWNER TO CURRENT_USER",
            &["R5: episcience_queue_claim(text) is owned by"],
        ),
        (
            "GRANT EXECUTE ON FUNCTION public.episcience_queue_finish(uuid, text, text) TO episcience_rw",
            &["R5: episcience_queue_finish(uuid,text,text) is executable by"],
        ),
        (
            "GRANT EXECUTE ON FUNCTION public.episcience_maint_sweep_narrowed() TO PUBLIC",
            &[
                "R5: episcience_maint_sweep_narrowed() is executable by",
                "\"PUBLIC\"",
            ],
        ),
        (
            "ALTER FUNCTION public.episcience_owner_worklist(text, integer) RESET search_path",
            &["R5: episcience_owner_worklist(text,integer) pins []"],
        ),
        (
            "ALTER FUNCTION public.episcience_queue_retry(uuid, interval, text) SECURITY INVOKER",
            &["R5: episcience_queue_retry(uuid,interval,text) is missing or not SECURITY DEFINER"],
        ),
        (
            "ALTER FUNCTION public.episcience_author_is_principal() SECURITY DEFINER; \
             ALTER FUNCTION public.episcience_author_is_principal() OWNER TO epigraph_maintenance",
            &[
                "R5: episcience_author_is_principal() is SECURITY DEFINER but not in the definer set",
                "R5b: trigger tenancy_15_author on syntheses runs the SECURITY DEFINER episcience_author_is_principal",
            ],
        ),
        (
            "ALTER TABLE public.syntheses NO FORCE ROW LEVEL SECURITY",
            &["rls: row security is not forced on syntheses"],
        ),
        (
            "ALTER TABLE public.blobs DISABLE ROW LEVEL SECURITY",
            &["rls: row security is not enabled on blobs"],
        ),
        (
            "GRANT INSERT ON public.countersignatures TO epigraph_app",
            &["grants: epigraph_app holds {\"INSERT\"} on countersignatures"],
        ),
        (
            "GRANT USAGE ON SCHEMA episcience_meta TO PUBLIC",
            &["grants: schema episcience_meta grants PUBLIC USAGE beyond its owner"],
        ),
        (
            "ALTER TABLE public.protocols DROP CONSTRAINT protocols_group_needs_real_group",
            &["sentinel: 1 protocols rows are owned by the world or seed group"],
        ),
    ];
    for (sql, wants) in cases {
        let db = mutated_clone(sql).await;
        if sql.contains("protocols_group_needs_real_group") {
            let p = principal(&db.admin, "author").await;
            sqlx::query(
                "INSERT INTO public.protocols (title, authored_by, content_hash, owner_group_id, visibility) \
                 VALUES ('p', $1, decode(repeat('01', 32), 'hex'), '00000000-0000-0000-0000-000000000000', 'public')",
            )
            .bind(p.agent)
            .execute(&db.admin)
            .await
            .expect("a world-owned row once the CHECK is gone");
        }
        let mut conn = ledger::connect_with(db.admin_options()).await.unwrap();
        let e = ledger::verify(&mut conn).await.expect_err(sql).to_string();
        for want in wants {
            assert!(e.contains(want), "{sql}: expected {want:?} in {e}");
        }
    }
}
