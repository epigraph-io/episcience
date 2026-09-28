//! R2: a table whose read policy admits PUBLIC rows lets only the row's
//! owners change it: it has RESTRICTIVE UPDATE and DELETE policies on the
//! session's WRITABLE groups, or the application roles hold no UPDATE /
//! DELETE privilege on it at all.
//!
//! Without the restrictive half, "readable because public" would be
//! "editable because readable" (a permissive FOR ALL policy's USING admits
//! every public row to UPDATE and DELETE).
mod support;
use support::{mutated_clone, policies, TestDb};

use episcience_db::ledger::EPISCIENCE_TABLES;
use sqlx::PgPool;

/// The public-admitting arm as the catalog renders it.
const PUBLIC_ARM: &str = "((visibility)::text = 'public'::text)";
/// The owner arm a RESTRICTIVE write policy must carry.
const WRITABLE: &str = "epigraph_writable_groups()";

async fn r2_findings(pool: &PgPool) -> Vec<String> {
    let pols = policies(pool).await;
    let mut out = Vec::new();
    for t in EPISCIENCE_TABLES {
        let mine: Vec<&support::Policy> = pols.iter().filter(|p| p.table == t).collect();
        let public_read = mine.iter().any(|p| {
            p.permissive
                && p.applies_to("r")
                && p.using.as_deref().is_some_and(|u| u.contains(PUBLIC_ARM))
        });
        if !public_read {
            continue;
        }
        for (cmd, privilege) in [("w", "UPDATE"), ("d", "DELETE")] {
            let owner_scoped = mine.iter().any(|p| {
                !p.permissive
                    && p.applies_to(cmd)
                    && p.exprs_for(cmd).iter().all(|e| e.contains(WRITABLE))
            });
            if owner_scoped {
                continue;
            }
            for role in ["episcience_rw", "epigraph_app"] {
                let has: bool = sqlx::query_scalar("SELECT has_table_privilege($1, $2, $3)")
                    .bind(role)
                    .bind(format!("public.{t}"))
                    .bind(privilege)
                    .fetch_one(pool)
                    .await
                    .unwrap();
                if has {
                    out.push(format!(
                        "{t}: public rows are readable, {role} holds {privilege}, and no RESTRICTIVE {privilege} policy scopes it to the writable groups"
                    ));
                }
            }
        }
    }
    out
}

/// R2 holds on the template (and some table is actually in scope, so the
/// check is not vacuous). Kills: the drifts below, shipped.
#[tokio::test]
async fn public_rows_are_writable_by_their_owners_only() {
    let db = TestDb::fresh().await;
    let in_scope = policies(&db.admin)
        .await
        .iter()
        .filter(|p| p.permissive && p.using.as_deref().is_some_and(|u| u.contains(PUBLIC_ARM)))
        .count();
    assert!(
        in_scope >= 11,
        "the public-admitting read policies: {in_scope}"
    );
    assert_eq!(r2_findings(&db.admin).await, Vec::<String>::new());
}

/// Kills, one per case: a table's `<t>_update_owner` / `<t>_delete_owner`
/// dropped, one flipped from RESTRICTIVE to PERMISSIVE (it would then WIDEN
/// the permissive set instead of narrowing it), an UPDATE grant on an
/// append-only table that has no owner policy.
#[tokio::test]
async fn the_owner_scope_predicate_names_each_drift() {
    let cases: [(&str, &str); 4] = [
        (
            "DROP POLICY samples_update_owner ON public.samples",
            "samples: public rows are readable, episcience_rw holds UPDATE",
        ),
        (
            "DROP POLICY protocols_delete_owner ON public.protocols",
            "protocols: public rows are readable, episcience_rw holds DELETE",
        ),
        (
            "DROP POLICY blobs_update_owner ON public.blobs; \
             CREATE POLICY blobs_update_owner ON public.blobs AS PERMISSIVE FOR UPDATE \
             USING ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[])) \
             WITH CHECK ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))",
            "blobs: public rows are readable, episcience_rw holds UPDATE",
        ),
        (
            "GRANT UPDATE ON public.countersignatures TO episcience_rw",
            "countersignatures: public rows are readable, episcience_rw holds UPDATE",
        ),
    ];
    for (sql, want) in cases {
        let db = mutated_clone(sql).await;
        let f = r2_findings(&db.admin).await;
        assert!(
            f.iter().any(|x| x.contains(want)),
            "{sql}: expected {want:?}, got {f:?}"
        );
    }
}
