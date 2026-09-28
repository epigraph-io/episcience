//! R3: the shape of every policy arm.
//!
//! - Every expression opens with the two bypass arms; every PERMISSIVE
//!   expression that is not bypass-only names a contract-v1 session helper.
//! - Policies call no function outside the contract-v1 helpers: the kernel's
//!   edge-reference check reads the kept-SELECT table (`syntheses`) as the
//!   kernel application role and turns ANY error into "does not exist", so
//!   a policy calling a function that role may not execute would silently
//!   hide every row from it. (Checked for every table, not only the kept
//!   set: the set is computed and could grow.)
//! - No world arm: no `true` disjunct, no sentinel group.
//! - The T-PUB read and write shapes are exactly the kernel's (and so the
//!   order of `Viewer::predicate_fragment`, which the splices rely on).
//! - Every `<t>_claim_visible` is RESTRICTIVE, on exactly the four tables
//!   whose rows cite a claim.
mod support;
use support::{bypass_first, bypass_only, mutated_clone, policies, TestDb, BYPASS_ARMS};

use regex::Regex;
use sqlx::PgPool;

const C2: [&str; 5] = [
    "epigraph_bypass",
    "epigraph_definer_bypass",
    "epigraph_session_groups",
    "epigraph_writable_groups",
    "epigraph_principal_id",
];

const CLAIM_VISIBLE: [&str; 4] = [
    "countersignatures",
    "sample_claims",
    "synthesis_claim_membership",
    "synthesis_provo_edges",
];

fn read_shape() -> String {
    format!(
        "{BYPASS_ARMS} OR ((visibility)::text = 'public'::text) OR (owner_group_id = ANY (( SELECT epigraph_session_groups() AS epigraph_session_groups)::uuid[])))"
    )
}

fn write_shape() -> String {
    format!(
        "{BYPASS_ARMS} OR (owner_group_id = ANY (( SELECT epigraph_writable_groups() AS epigraph_writable_groups)::uuid[])))"
    )
}

async fn r3_findings(pool: &PgPool) -> Vec<String> {
    let pols = policies(pool).await;
    let call = Regex::new(r"([A-Za-z_][A-Za-z0-9_.]*)\(").unwrap();
    let world = Regex::new(r"(?i)\btrue\b|00000000-0000-0000-0000-0000000000(00|de)").unwrap();
    let mut out = Vec::new();
    for p in &pols {
        let who = format!("{}.{}", p.table, p.name);
        for e in p.using.iter().chain(p.check.iter()) {
            if !bypass_first(e) {
                out.push(format!("{who}: does not open with the bypass arms"));
            }
            if p.permissive
                && !bypass_only(e)
                && ![
                    "epigraph_session_groups()",
                    "epigraph_writable_groups()",
                    "epigraph_principal_id()",
                    "visibility)::text = 'public'",
                ]
                .iter()
                .any(|h| e.contains(h))
            {
                out.push(format!(
                    "{who}: a permissive arm names no session helper: {e}"
                ));
            }
            for m in call.captures_iter(e) {
                let f = m[1].trim_start_matches("public.");
                if !C2.contains(&f) {
                    out.push(format!(
                        "{who}: calls {f}, which is not a contract-v1 helper"
                    ));
                }
            }
            if world.is_match(e) {
                out.push(format!("{who}: carries a world arm: {e}"));
            }
        }
        if p.name == format!("{}_tenancy", p.table) {
            if p.using.as_deref() != Some(read_shape().as_str()) {
                out.push(format!("{who}: the read shape differs from the kernel's"));
            }
            if p.check.as_deref() != Some(write_shape().as_str()) {
                out.push(format!("{who}: the write shape differs from the kernel's"));
            }
        }
        if p.name.ends_with("_claim_visible") && p.permissive {
            out.push(format!(
                "{who}: a claim-visibility policy must be RESTRICTIVE"
            ));
        }
    }
    let mut cv: Vec<&str> = pols
        .iter()
        .filter(|p| p.name == format!("{}_claim_visible", p.table))
        .map(|p| p.table.as_str())
        .collect();
    cv.sort_unstable();
    if cv != CLAIM_VISIBLE {
        out.push(format!(
            "claim-visibility policies on {cv:?}, expected {CLAIM_VISIBLE:?}"
        ));
    }
    out
}

/// R3 holds on the template, and the T-PUB shapes are present on the ten
/// ownership tables. Kills: the drifts below, shipped.
#[tokio::test]
async fn every_policy_arm_has_the_kernel_shape() {
    let db = TestDb::fresh().await;
    let tenancy = policies(&db.admin)
        .await
        .iter()
        .filter(|p| p.name == format!("{}_tenancy", p.table))
        .count();
    assert_eq!(tenancy, 10, "one T-PUB policy per ownership table");
    assert_eq!(r3_findings(&db.admin).await, Vec::<String>::new());
}

/// Kills, one per case: `OR true` added to a read policy, the WITH CHECK's
/// writable set swapped for the session (read) set, a policy calling an
/// EpiScience function the kernel app role cannot rely on, a claim policy
/// flipped to PERMISSIVE (it would widen reads instead of narrowing them).
#[tokio::test]
async fn the_policy_shape_predicate_names_each_drift() {
    let cases: [(&str, &str); 4] = [
        (
            "ALTER POLICY syntheses_tenancy ON public.syntheses \
             USING ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR visibility = 'public' \
                    OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]) OR true)",
            "syntheses.syntheses_tenancy: carries a world arm",
        ),
        (
            "ALTER POLICY samples_tenancy ON public.samples \
             WITH CHECK ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))",
            "samples.samples_tenancy: the write shape differs",
        ),
        (
            "ALTER POLICY syntheses_delete_owner ON public.syntheses \
             USING ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR public.episcience_session_is_privileged())",
            "calls episcience_session_is_privileged, which is not a contract-v1 helper",
        ),
        (
            "DROP POLICY sample_claims_claim_visible ON public.sample_claims; \
             CREATE POLICY sample_claims_claim_visible ON public.sample_claims AS PERMISSIVE FOR ALL \
             USING ((SELECT public.epigraph_bypass()) OR (SELECT public.epigraph_definer_bypass()) \
                    OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = sample_claims.claim_id))",
            "sample_claims.sample_claims_claim_visible: a claim-visibility policy must be RESTRICTIVE",
        ),
    ];
    for (sql, want) in cases {
        let db = mutated_clone(sql).await;
        let f = r3_findings(&db.admin).await;
        assert!(
            f.iter().any(|x| x.contains(want)),
            "{sql}: expected {want:?}, got {f:?}"
        );
    }
}
