//! R1: every EpiScience relation is covered by the tenancy model, and the
//! set of EpiScience relations is exactly the 14.
//!
//! - The relations the EpiScience migrations add (a clone of the full
//!   template minus a clone of the kernel-only template, both built by the
//!   same `epigraph-migrate`) EQUAL `EPISCIENCE_TABLES`; the kernel-owned
//!   `experiments` / `experiment_results` are in the kernel-only set.
//! - Each table is TENANCY (both pair columns NOT NULL, row security enabled
//!   AND forced, a permissive policy for each of SELECT / INSERT / UPDATE /
//!   DELETE, every applicable policy (permissive or restrictive) opening with
//!   the bypass arms so a maintenance-owned definer is admitted to every
//!   command, and every command with no application arm listed in
//!   [`APP_UNCOVERED`] with its reason) or FROZEN (one FOR ALL policy, bypass
//!   arms only).
//! - Generator A': every column named `claim_id`, every FK to `claims` and
//!   every `uuid[]` named `%claim_ids` is covered by the claim guard trigger
//!   AND a RESTRICTIVE `<t>_claim_visible` policy, or listed in
//!   [`CLAIM_ARRAY_RESIDUAL`].
//!
//! Each predicate is also run against a clone with one drift applied, and
//! must name it (so a vacuous predicate fails here, not in review).
mod support;
use support::{bypass_first, bypass_only, mutated_clone, policies, TestDb};

use episcience_db::catalog::{FROZEN_TABLES, TENANCY_TABLES};
use episcience_db::ledger::EPISCIENCE_TABLES;
use sqlx::PgPool;

/// Commands a session never performs itself, with the reason: they are the
/// definers' only (bypass-only policies).
const APP_UNCOVERED: [(&str, &str, &str); 4] = [
    (
        "synthesis_jobs",
        "w",
        "job state moves through the queue definers",
    ),
    (
        "synthesis_jobs",
        "d",
        "a job row lives as long as its synthesis",
    ),
    ("countersignatures", "w", "attestations are append-only"),
    ("countersignatures", "d", "attestations are append-only"),
];

/// Claim-id arrays and snapshots that name claims without a per-row guard:
/// they are derived from rows the claim guard already admitted (a cluster's
/// members, a staleness event's affected claims) or a frozen snapshot of the
/// synthesis' own inputs; their visibility follows the synthesis.
const CLAIM_ARRAY_RESIDUAL: [(&str, &str); 3] = [
    ("synthesis_clusters", "member_claim_ids"),
    ("synthesis_staleness_events", "affected_claim_ids"),
    ("syntheses", "subgraph_snapshot"),
];

/// The kernel's own tables that EpiScience's old schema also created; the
/// kernel owns them now.
const KERNEL_OWNED: [&str; 2] = ["experiments", "experiment_results"];

async fn relations(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT relname::text FROM pg_class \
          WHERE relnamespace = 'public'::regnamespace AND relkind IN ('r', 'p', 'v', 'm', 'f') \
          ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// R1 on one database. Empty = covered.
async fn coverage_findings(pool: &PgPool) -> Vec<String> {
    let mut out = Vec::new();
    let pols = policies(pool).await;
    let flags: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT relname::text, relrowsecurity, relforcerowsecurity FROM pg_class \
          WHERE relnamespace = 'public'::regnamespace AND relname = ANY($1)",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(pool)
    .await
    .unwrap();
    for t in EPISCIENCE_TABLES {
        match flags.iter().find(|f| f.0 == t) {
            Some((_, true, true)) => {}
            other => out.push(format!(
                "{t}: row security not enabled and forced ({other:?})"
            )),
        }
        let mine: Vec<&support::Policy> = pols.iter().filter(|p| p.table == t).collect();
        if FROZEN_TABLES.contains(&t) {
            let ok = mine.len() == 1
                && mine[0].cmd == "*"
                && mine[0].permissive
                && mine[0].using.as_deref().is_some_and(bypass_only)
                && mine[0].check.as_deref().is_some_and(bypass_only);
            if !ok {
                out.push(format!(
                    "{t}: a frozen table has exactly one bypass-only FOR ALL policy, got {mine:?}"
                ));
            }
            continue;
        }
        assert!(TENANCY_TABLES.contains(&t), "{t} is classified");
        let notnull: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_attribute WHERE attrelid = ('public.' || $1)::regclass \
              AND attname IN ('owner_group_id', 'visibility') AND attnotnull AND NOT attisdropped",
        )
        .bind(t)
        .fetch_one(pool)
        .await
        .unwrap();
        if notnull != 2 {
            out.push(format!("{t}: the ownership pair is not NOT NULL"));
        }
        for cmd in ["r", "a", "w", "d"] {
            let permissive: Vec<&&support::Policy> = mine
                .iter()
                .filter(|p| p.permissive && p.applies_to(cmd))
                .collect();
            if permissive.is_empty() {
                out.push(format!("{t}: no permissive policy for command {cmd}"));
                continue;
            }
            for p in mine.iter().filter(|p| p.applies_to(cmd)) {
                for e in p.exprs_for(cmd) {
                    if !bypass_first(e) {
                        out.push(format!(
                            "{t}: policy {} (command {cmd}) does not open with the bypass arms: {e}",
                            p.name
                        ));
                    }
                }
            }
            let app_arm = permissive
                .iter()
                .any(|p| p.exprs_for(cmd).iter().any(|e| !bypass_only(e)));
            let listed = APP_UNCOVERED.iter().any(|(tt, c, _)| *tt == t && *c == cmd);
            if !app_arm && !listed {
                out.push(format!(
                    "{t}: command {cmd} admits only the bypass arms and is not in APP_UNCOVERED"
                ));
            }
            if app_arm && listed {
                out.push(format!(
                    "{t}: command {cmd} is in APP_UNCOVERED but a policy gives the application an arm"
                ));
            }
        }
    }

    // Generator A'.
    let cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT c.relname::text, a.attname::text \
           FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
          WHERE c.relnamespace = 'public'::regnamespace AND c.relname = ANY($1) \
            AND a.attnum > 0 AND NOT a.attisdropped \
            AND (a.attname = 'claim_id' \
                 OR (a.atttypid = 'uuid[]'::regtype AND a.attname LIKE '%claim_ids') \
                 OR EXISTS (SELECT 1 FROM pg_constraint k \
                             WHERE k.conrelid = c.oid AND k.contype = 'f' \
                               AND k.confrelid = 'public.claims'::regclass \
                               AND a.attnum = ANY (k.conkey))) \
          ORDER BY 1, 2",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(pool)
    .await
    .unwrap();
    for (t, col) in &cols {
        if CLAIM_ARRAY_RESIDUAL
            .iter()
            .any(|(rt, rc)| rt == t && rc == col)
        {
            continue;
        }
        let guard: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_trigger WHERE tgrelid = ('public.' || $1)::regclass \
                             AND tgname = 'tenancy_20_claim_guard' AND tgenabled <> 'D')",
        )
        .bind(t)
        .fetch_one(pool)
        .await
        .unwrap();
        let visible = pols
            .iter()
            .any(|p| p.table == *t && p.name == format!("{t}_claim_visible") && !p.permissive);
        if !guard || !visible {
            out.push(format!(
                "{t}.{col} names a claim: claim guard {guard}, RESTRICTIVE {t}_claim_visible {visible}"
            ));
        }
    }
    for (t, col) in CLAIM_ARRAY_RESIDUAL {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = ('public.' || $1)::regclass \
                             AND attname = $2 AND NOT attisdropped)",
        )
        .bind(t)
        .bind(col)
        .fetch_one(pool)
        .await
        .unwrap();
        if !exists {
            out.push(format!(
                "CLAIM_ARRAY_RESIDUAL names {t}.{col}, which no longer exists"
            ));
        }
    }
    out
}

/// The EpiScience migrations add exactly the 14 tables; the kernel owns
/// `experiments` / `experiment_results`. Kills: a table created by a future
/// EpiScience migration without being added to the model (it would appear
/// here and nowhere else).
#[tokio::test]
async fn the_episcience_relations_are_exactly_the_fourteen() {
    let full = TestDb::fresh().await;
    let kernel = TestDb::fresh_kernel_only().await;
    let all = relations(&full.admin).await;
    let kernel_rel = relations(&kernel.admin).await;
    let added: Vec<&String> = all.iter().filter(|r| !kernel_rel.contains(r)).collect();
    let mut want: Vec<String> = EPISCIENCE_TABLES.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(added, want.iter().collect::<Vec<_>>());
    for k in KERNEL_OWNED {
        assert!(
            kernel_rel.iter().any(|r| r == k),
            "{k} is created by the kernel"
        );
    }
}

/// R1 holds on the template. Kills: any of the drifts below, shipped.
#[tokio::test]
async fn every_episcience_table_is_covered_by_the_tenancy_model() {
    let db = TestDb::fresh().await;
    assert_eq!(coverage_findings(&db.admin).await, Vec::<String>::new());
}

/// Each drift is named by the predicate (so the predicate is not vacuous).
/// Kills, one per case: FORCE dropped (only a catalog check can see it: the
/// table owner is a superuser in every test and deploy), a command left
/// without a permissive policy, the definer bypass missing from a
/// RESTRICTIVE policy (R-M2: the definers would be default-denied), a
/// bypass-only command given an application arm without the register
/// knowing, a claim-visibility policy or claim guard dropped, a frozen table
/// opened, the pair made nullable.
#[tokio::test]
async fn the_coverage_predicate_names_each_drift() {
    let cases: [(&str, &str); 8] = [
        (
            "ALTER TABLE public.samples NO FORCE ROW LEVEL SECURITY",
            "samples: row security not enabled and forced",
        ),
        (
            "DROP POLICY synthesis_jobs_insert ON public.synthesis_jobs",
            "synthesis_jobs: no permissive policy for command a",
        ),
        (
            "ALTER POLICY syntheses_update_owner ON public.syntheses \
             USING (owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))",
            "policy syntheses_update_owner (command w) does not open with the bypass arms",
        ),
        (
            "ALTER POLICY countersignatures_bypass_delete ON public.countersignatures \
             USING ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR countersigned_by = (SELECT public.epigraph_principal_id()))",
            "countersignatures: command d is in APP_UNCOVERED but a policy gives the application an arm",
        ),
        (
            "DROP POLICY sample_claims_claim_visible ON public.sample_claims",
            "sample_claims.claim_id names a claim",
        ),
        (
            "DROP TRIGGER tenancy_20_claim_guard ON public.countersignatures",
            "countersignatures.claim_id names a claim",
        ),
        (
            "CREATE POLICY synthesis_shares_open ON public.synthesis_shares FOR SELECT USING (true)",
            "synthesis_shares: a frozen table has exactly one bypass-only FOR ALL policy",
        ),
        (
            "ALTER TABLE public.protocols ALTER COLUMN owner_group_id DROP NOT NULL",
            "protocols: the ownership pair is not NOT NULL",
        ),
    ];
    for (sql, want) in cases {
        let db = mutated_clone(sql).await;
        let f = coverage_findings(&db.admin).await;
        assert!(
            f.iter().any(|x| x.contains(want)),
            "{sql}: expected a finding containing {want:?}, got {f:?}"
        );
    }
}
