//! R4: the privilege matrix, EXACTLY, as `has_table_privilege` /
//! `has_function_privilege` / `has_schema_privilege` answer it (membership
//! included):
//! - {kernel app role, `episcience_rw`, `episcience_queue`,
//!   `episcience_maint_ops`, kernel maintenance, PUBLIC} x the 16 tables x
//!   {S, I, U, D}: brief 7.5 for the 14 (the kept-SELECT set computed from
//!   the kernel's entity registry, as 5036 computes it); the two kernel-owned
//!   tables exactly as the kernel-only schema has them (untouched);
//! - every `episcience_*` function: a definer only by its one role (and its
//!   owner); an INVOKER function by everyone (it runs as the caller);
//! - schema `episcience_meta`: nothing for anyone but its owner.
//!
//! A future table-creating migration that forgets the REVOKE (the kernel's
//! default privileges would give its table to the kernel app role) fails
//! here.
mod support;
use support::{mutated_clone, TestDb};

use episcience_db::catalog::{expected_grants, kept_select_tables, DEFINERS, DEFINER_OWNER};
use episcience_db::ledger::EPISCIENCE_TABLES;
use sqlx::PgPool;

const ROLES: [&str; 6] = [
    "epigraph_app",
    "episcience_rw",
    "episcience_queue",
    "episcience_maint_ops",
    "epigraph_maintenance",
    "public",
];
const PRIVS: [&str; 4] = ["SELECT", "INSERT", "UPDATE", "DELETE"];
const KERNEL_OWNED: [&str; 2] = ["experiments", "experiment_results"];

async fn table_privs(pool: &PgPool, tables: &[&str]) -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    for t in tables {
        for r in ROLES {
            for p in PRIVS {
                let has: bool = sqlx::query_scalar("SELECT has_table_privilege($1, $2, $3)")
                    .bind(r)
                    .bind(format!("public.{t}"))
                    .bind(p)
                    .fetch_one(pool)
                    .await
                    .unwrap();
                out.push((t.to_string(), r.to_string(), p.to_string(), has));
            }
        }
    }
    out
}

async fn r4_findings(db: &TestDb, kernel_only: &TestDb) -> Vec<String> {
    let pool = &db.admin;
    let mut out = Vec::new();
    let mut c = pool.acquire().await.unwrap();
    let kept = kept_select_tables(&mut c).await.unwrap();
    drop(c);
    let matrix = expected_grants(&kept);
    for (t, r, p, has) in table_privs(pool, &EPISCIENCE_TABLES).await {
        let want = matrix[&t].get(&r).is_some_and(|s| s.contains(&p));
        if has != want {
            out.push(format!("{r} {p} on {t}: {has}, the matrix says {want}"));
        }
    }
    let now = table_privs(pool, &KERNEL_OWNED).await;
    let before = table_privs(&kernel_only.admin, &KERNEL_OWNED).await;
    for (a, b) in now.iter().zip(before.iter()) {
        if a != b {
            out.push(format!(
                "kernel-owned {} changed: {a:?} (kernel schema: {b:?})",
                a.0
            ));
        }
    }

    let fns: Vec<(String, bool)> = sqlx::query_as(
        "SELECT regexp_replace(oid::regprocedure::text, '^public\\.', ''), prosecdef FROM pg_proc \
          WHERE pronamespace = 'public'::regnamespace AND proname LIKE 'episcience\\_%' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for (f, secdef) in fns {
        let definer = DEFINERS.iter().find(|d| d.signature == f);
        for r in ROLES {
            let has: bool = sqlx::query_scalar("SELECT has_function_privilege($1, $2, 'EXECUTE')")
                .bind(r)
                .bind(format!("public.{f}"))
                .fetch_one(pool)
                .await
                .unwrap();
            let want = match (secdef, definer) {
                (false, _) => true,
                (true, Some(d)) => r == DEFINER_OWNER || d.execute == Some(r),
                (true, None) => false,
            };
            if has != want {
                out.push(format!("{r} EXECUTE {f}: {has}, expected {want}"));
            }
        }
    }
    for r in ROLES {
        for p in ["USAGE", "CREATE"] {
            let has: bool =
                sqlx::query_scalar("SELECT has_schema_privilege($1, 'episcience_meta', $2)")
                    .bind(r)
                    .bind(p)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            if has {
                out.push(format!("{r} holds {p} on schema episcience_meta"));
            }
        }
    }
    out
}

/// R4 holds on the template, the kept-SELECT set is `syntheses`, and the
/// ACL-level check `verify` runs agrees. Kills: the drifts below, shipped.
#[tokio::test]
async fn the_privilege_matrix_is_exact() {
    let db = TestDb::fresh().await;
    let kernel = TestDb::fresh_kernel_only().await;
    let mut c = db.admin.acquire().await.unwrap();
    assert_eq!(
        kept_select_tables(&mut c).await.unwrap(),
        vec!["syntheses".to_string()]
    );
    assert_eq!(
        episcience_db::catalog::grant_findings(&mut c)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    drop(c);
    assert_eq!(r4_findings(&db, &kernel).await, Vec::<String>::new());
}

/// Kills, one per case: a queue definer granted to `episcience_rw` (the
/// application would drive the queue), a non-kept table readable by the
/// kernel app role, the kept SELECT lost, UPDATE on the queue for the
/// application, a table privilege for the maintenance login's role, the
/// ledger schema opened, an INVOKER guard's EXECUTE revoked (the guards run
/// as the caller and would fail every write), a kernel-owned table's grants
/// changed.
#[tokio::test]
async fn the_privilege_predicate_names_each_drift() {
    let kernel = TestDb::fresh_kernel_only().await;
    let cases: [(&str, &str); 8] = [
        (
            "GRANT EXECUTE ON FUNCTION public.episcience_queue_claim(text) TO episcience_rw",
            "episcience_rw EXECUTE episcience_queue_claim(text): true, expected false",
        ),
        (
            "GRANT SELECT ON public.samples TO epigraph_app",
            "epigraph_app SELECT on samples: true, the matrix says false",
        ),
        (
            "REVOKE SELECT ON public.syntheses FROM epigraph_app",
            "epigraph_app SELECT on syntheses: false, the matrix says true",
        ),
        (
            "GRANT UPDATE ON public.synthesis_jobs TO episcience_rw",
            "episcience_rw UPDATE on synthesis_jobs: true, the matrix says false",
        ),
        (
            "GRANT SELECT ON public.protocols TO episcience_maint_ops",
            "episcience_maint_ops SELECT on protocols: true, the matrix says false",
        ),
        (
            "GRANT USAGE ON SCHEMA episcience_meta TO episcience_rw",
            "episcience_rw holds USAGE on schema episcience_meta",
        ),
        (
            "REVOKE EXECUTE ON FUNCTION public.episcience_author_is_principal() FROM PUBLIC",
            "EXECUTE episcience_author_is_principal(): false, expected true",
        ),
        (
            "REVOKE ALL ON public.experiments FROM epigraph_app",
            "kernel-owned experiments changed",
        ),
    ];
    for (sql, want) in cases {
        let db = mutated_clone(sql).await;
        let f = r4_findings(&db, &kernel).await;
        assert!(
            f.iter().any(|x| x.contains(want)),
            "{sql}: expected {want:?}, got {f:?}"
        );
    }
}
