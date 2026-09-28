//! The tenancy catalog checks that `episcience-migrate verify` runs as the
//! deploy guard after the row-security migrations (and that the ratchet
//! tests share): each returns a list of findings, empty when the database
//! holds exactly what the tenancy model says.
//!
//! - [`definer_findings`] (ratchet R5): the SECURITY DEFINER `episcience_*`
//!   functions are exactly [`DEFINERS`], each owned by the kernel maintenance
//!   role, with `search_path` pinned and EXECUTE held by exactly its one
//!   EpiScience role; no trigger on an EpiScience table runs a definer other
//!   than the statement-level propagation (R5b: row guards are INVOKER).
//! - [`row_security_findings`]: row security is enabled AND forced on all 14
//!   tables.
//! - [`grant_findings`]: the table ACLs are exactly the grant matrix (only
//!   the owner, the kernel application role on the kept-SELECT set,
//!   `episcience_rw` and the kernel maintenance role hold anything), and the
//!   ledger schema grants nothing beyond its owner.
//! - [`sentinel_findings`]: no EpiScience row is owned by the kernel's world
//!   or seed sentinel group.
//!
//! Every query is schema-qualified: the migrator's session runs with
//! `search_path = episcience_meta`. The reads need a session that row
//! security does not filter (the migration owner).
use std::collections::{BTreeMap, BTreeSet};

use sqlx::{PgConnection, Row};

use crate::ledger::{EPISCIENCE_TABLES, LEDGER_SCHEMA};

/// One maintenance-owned definer: its signature (as `regprocedure` renders
/// it without the schema) and the one EpiScience role that may EXECUTE it
/// (`None`: a trigger function nobody calls directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definer {
    pub signature: &'static str,
    pub execute: Option<&'static str>,
}

/// The closed definer set (brief 7.4, plus `episcience_members_all_public`:
/// the member half of publishability counted over rows the session cannot
/// see). Adding a definer means adding it here in the same change, where
/// review sees it.
pub const DEFINERS: [Definer; 10] = [
    Definer {
        signature: "episcience_members_all_public(text,uuid)",
        execute: Some("episcience_rw"),
    },
    Definer {
        signature: "episcience_maint_backfill_owners(uuid,boolean)",
        execute: Some("episcience_maint_ops"),
    },
    Definer {
        signature: "episcience_maint_backfill_reverse(jsonb)",
        execute: Some("episcience_maint_ops"),
    },
    Definer {
        signature: "episcience_propagate_parent_tenancy()",
        execute: None,
    },
    Definer {
        signature: "episcience_queue_claim(text)",
        execute: Some("episcience_queue"),
    },
    Definer {
        signature: "episcience_queue_finish(uuid,text,text)",
        execute: Some("episcience_queue"),
    },
    Definer {
        signature: "episcience_queue_retry(uuid,interval,text)",
        execute: Some("episcience_queue"),
    },
    Definer {
        signature: "episcience_owner_worklist(text,integer)",
        execute: Some("episcience_queue"),
    },
    Definer {
        signature: "episcience_countersign_chain_head(uuid)",
        execute: Some("episcience_rw"),
    },
    Definer {
        signature: "episcience_maint_sweep_narrowed()",
        execute: Some("episcience_maint_ops"),
    },
];

/// The trigger function that is the one definer a trigger may run, and only
/// from an AFTER ... FOR EACH STATEMENT trigger.
pub const PROPAGATION: &str = "episcience_propagate_parent_tenancy";

/// The role every definer is owned by.
pub const DEFINER_OWNER: &str = "epigraph_maintenance";

/// The one `search_path` every EpiScience function pins.
pub const PINNED_SEARCH_PATH: &str = "search_path=public, pg_temp";

/// The ownership tables whose rows the application reads and writes in full.
pub const OWNERSHIP_TABLES: [&str; 10] = [
    "syntheses",
    "synthesis_clusters",
    "synthesis_embeddings",
    "synthesis_staleness_events",
    "synthesis_provo_edges",
    "synthesis_claim_membership",
    "samples",
    "sample_claims",
    "protocols",
    "blobs",
];

/// Read and append only for the application (their updates are definers').
pub const APPEND_TABLES: [&str; 2] = ["synthesis_jobs", "countersignatures"];

/// Frozen: no application privilege at all.
pub const FROZEN_TABLES: [&str; 2] = ["synthesis_shares", "episcience_worker_state"];

/// The kernel's world and seed sentinel groups (contract item C6).
pub const SENTINEL_GROUPS: [&str; 2] = [
    "00000000-0000-0000-0000-000000000000",
    "00000000-0000-0000-0000-00000000dead",
];

const S: &str = "SELECT";
const I: &str = "INSERT";
const U: &str = "UPDATE";
const D: &str = "DELETE";

/// The EpiScience tables the kernel's entity registry names: the kept-SELECT
/// set, readable by the kernel application role (computed exactly as 5036
/// computes it).
pub async fn kept_select_tables(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT e.table_name::text FROM public.entity_types e \
          WHERE e.schema_name = 'public' AND e.table_name = ANY($1) ORDER BY 1",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(conn)
    .await
}

/// The grant matrix of brief 7.5: table -> grantee -> privileges, for every
/// grantee other than the table's owner. Everything absent is expected to be
/// absent.
pub fn expected_grants(kept: &[String]) -> BTreeMap<String, BTreeMap<String, BTreeSet<String>>> {
    let set = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
    let mut out = BTreeMap::new();
    for t in EPISCIENCE_TABLES {
        let mut g: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        g.insert(DEFINER_OWNER.to_string(), set(&[S, I, U, D]));
        if OWNERSHIP_TABLES.contains(&t) {
            g.insert("episcience_rw".to_string(), set(&[S, I, U, D]));
        } else if APPEND_TABLES.contains(&t) {
            g.insert("episcience_rw".to_string(), set(&[S, I]));
        }
        if kept.iter().any(|k| k == t) {
            g.insert("epigraph_app".to_string(), set(&[S]));
        }
        out.insert(t.to_string(), g);
    }
    out
}

/// R5 + R5b. See the module doc.
pub async fn definer_findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let mut out = Vec::new();
    let rows = sqlx::query(
        "SELECT regexp_replace(p.oid::pg_catalog.regprocedure::text, '^public\\.', '') AS sig, \
                pg_catalog.pg_get_userbyid(p.proowner) AS owner, \
                coalesce(p.proconfig, ARRAY[]::text[]) AS config, \
                p.proacl IS NULL AS default_acl, \
                coalesce((SELECT array_agg(CASE WHEN a.grantee = 0 THEN 'PUBLIC' \
                                                ELSE pg_catalog.pg_get_userbyid(a.grantee) END ORDER BY 1) \
                            FROM pg_catalog.aclexplode(p.proacl) a \
                           WHERE a.privilege_type = 'EXECUTE' AND a.grantee <> p.proowner), \
                         ARRAY[]::text[]) AS executors \
           FROM pg_catalog.pg_proc p \
          WHERE p.pronamespace = 'public'::pg_catalog.regnamespace \
            AND p.proname LIKE 'episcience\\_%' AND p.prosecdef \
          ORDER BY 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    let expected: BTreeMap<&str, Option<&str>> =
        DEFINERS.iter().map(|d| (d.signature, d.execute)).collect();
    let mut seen = BTreeSet::new();
    for r in &rows {
        let sig: String = r.get("sig");
        let owner: String = r.get("owner");
        let config: Vec<String> = r.get("config");
        let default_acl: bool = r.get("default_acl");
        let executors: Vec<String> = r.get("executors");
        let Some(want) = expected.get(sig.as_str()) else {
            out.push(format!(
                "R5: {sig} is SECURITY DEFINER but not in the definer set"
            ));
            continue;
        };
        seen.insert(sig.clone());
        if owner != DEFINER_OWNER {
            out.push(format!(
                "R5: {sig} is owned by {owner}, not {DEFINER_OWNER}"
            ));
        }
        if config != [PINNED_SEARCH_PATH.to_string()] {
            out.push(format!(
                "R5: {sig} pins {config:?}, not [{PINNED_SEARCH_PATH}]"
            ));
        }
        let want_exec: Vec<String> = want.iter().map(|w| w.to_string()).collect();
        if default_acl {
            out.push(format!(
                "R5: {sig} keeps the default EXECUTE for PUBLIC (never revoked)"
            ));
        } else if executors != want_exec {
            out.push(format!(
                "R5: {sig} is executable by {executors:?}, expected exactly {want_exec:?}"
            ));
        }
    }
    for d in DEFINERS {
        if !seen.contains(d.signature) {
            out.push(format!(
                "R5: {} is missing or not SECURITY DEFINER",
                d.signature
            ));
        }
    }

    // R5b: a trigger on an EpiScience table runs a definer only as the
    // statement-level AFTER propagation. `tgtype`: bit 0 = FOR EACH ROW,
    // bit 1 = BEFORE, bit 6 = INSTEAD OF.
    let trig = sqlx::query(
        "SELECT c.relname::text AS rel, t.tgname::text AS tg, p.proname::text AS fn, \
                (t.tgtype & 1) = 1 AS per_row, (t.tgtype & 66) <> 0 AS before \
           FROM pg_catalog.pg_trigger t \
           JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
           JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
          WHERE NOT t.tgisinternal AND p.prosecdef \
            AND c.relnamespace = 'public'::pg_catalog.regnamespace \
            AND c.relname = ANY($1) \
          ORDER BY 1, 2",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(&mut *conn)
    .await?;
    for r in &trig {
        let (rel, tg, f): (String, String, String) = (r.get("rel"), r.get("tg"), r.get("fn"));
        let (per_row, before): (bool, bool) = (r.get("per_row"), r.get("before"));
        if f != PROPAGATION || per_row || before {
            out.push(format!(
                "R5b: trigger {tg} on {rel} runs the SECURITY DEFINER {f} (only the statement-level AFTER propagation may)"
            ));
        }
    }
    Ok(out)
}

/// Row security enabled and forced on all 14 tables.
pub async fn row_security_findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT c.relname::text, c.relrowsecurity, c.relforcerowsecurity \
           FROM pg_catalog.pg_class c \
          WHERE c.relnamespace = 'public'::pg_catalog.regnamespace AND c.relname = ANY($1) \
          ORDER BY 1",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for t in EPISCIENCE_TABLES {
        match rows.iter().find(|r| r.0 == t) {
            None => out.push(format!("rls: table {t} does not exist")),
            Some((_, enabled, forced)) => {
                if !enabled {
                    out.push(format!("rls: row security is not enabled on {t}"));
                }
                if !forced {
                    out.push(format!("rls: row security is not forced on {t}"));
                }
            }
        }
    }
    Ok(out)
}

/// The table ACLs equal the grant matrix, and the ledger schema grants
/// nothing beyond its owner.
pub async fn grant_findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let kept = kept_select_tables(conn).await?;
    let expected = expected_grants(&kept);
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, \
                CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee) END, \
                a.privilege_type \
           FROM pg_catalog.pg_class c \
           CROSS JOIN LATERAL pg_catalog.aclexplode(c.relacl) a \
          WHERE c.relnamespace = 'public'::pg_catalog.regnamespace AND c.relname = ANY($1) \
            AND a.grantee <> c.relowner",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(&mut *conn)
    .await?;
    let mut actual: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for (t, g, p) in rows {
        actual.entry(t).or_default().entry(g).or_default().insert(p);
    }
    let mut out = Vec::new();
    for (t, want) in &expected {
        let empty = BTreeMap::new();
        let have = actual.get(t).unwrap_or(&empty);
        let grantees: BTreeSet<&String> = want.keys().chain(have.keys()).collect();
        for g in grantees {
            let w = want.get(g).cloned().unwrap_or_default();
            let h = have.get(g).cloned().unwrap_or_default();
            if w != h {
                out.push(format!(
                    "grants: {g} holds {h:?} on {t}, the matrix says {w:?}"
                ));
            }
        }
    }
    let schema: Vec<String> = sqlx::query_scalar(
        "SELECT CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee) END \
                || ' ' || a.privilege_type \
           FROM pg_catalog.pg_namespace n \
           CROSS JOIN LATERAL pg_catalog.aclexplode(n.nspacl) a \
          WHERE n.nspname = $1 AND a.grantee <> n.nspowner \
          ORDER BY 1",
    )
    .bind(LEDGER_SCHEMA)
    .fetch_all(&mut *conn)
    .await?;
    for s in schema {
        out.push(format!(
            "grants: schema {LEDGER_SCHEMA} grants {s} beyond its owner"
        ));
    }
    Ok(out)
}

/// The 12 tables that carry the ownership pair.
pub const TENANCY_TABLES: [&str; 12] = [
    "syntheses",
    "synthesis_clusters",
    "synthesis_embeddings",
    "synthesis_staleness_events",
    "synthesis_provo_edges",
    "synthesis_claim_membership",
    "synthesis_jobs",
    "samples",
    "sample_claims",
    "protocols",
    "blobs",
    "countersignatures",
];

/// No EpiScience row owned by the world or seed sentinel.
pub async fn sentinel_findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let mut out = Vec::new();
    for t in TENANCY_TABLES {
        // `t` is one of the fixed names above, never input.
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM public.{t} WHERE owner_group_id = ANY($1::uuid[])"
        ))
        .bind(SENTINEL_GROUPS.to_vec())
        .fetch_one(&mut *conn)
        .await?;
        if n > 0 {
            out.push(format!(
                "sentinel: {n} {t} rows are owned by the world or seed group"
            ));
        }
    }
    Ok(out)
}

/// Every check above, in order.
pub async fn findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let mut out = definer_findings(conn).await?;
    out.extend(row_security_findings(conn).await?);
    out.extend(grant_findings(conn).await?);
    out.extend(sentinel_findings(conn).await?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three privilege classes partition the 14 tables. Kills: a table
    /// dropped from (or listed twice in) the matrix.
    #[test]
    fn the_privilege_classes_partition_the_fourteen_tables() {
        let mut all: Vec<&str> = OWNERSHIP_TABLES
            .iter()
            .chain(APPEND_TABLES.iter())
            .chain(FROZEN_TABLES.iter())
            .copied()
            .collect();
        all.sort_unstable();
        let mut want = EPISCIENCE_TABLES.to_vec();
        want.sort_unstable();
        assert_eq!(all, want);
        let tenancy: BTreeSet<&str> = TENANCY_TABLES.iter().copied().collect();
        let frozen: BTreeSet<&str> = FROZEN_TABLES.iter().copied().collect();
        assert!(tenancy.is_disjoint(&frozen));
        assert_eq!(tenancy.len() + frozen.len(), EPISCIENCE_TABLES.len());
    }

    /// The matrix: maintenance everywhere; the application roles on the
    /// ownership (S/I/U/D) and append (S/I) tables only; the kernel
    /// application role on the kept set only. Kills: a frozen table given to
    /// the application, UPDATE on the queue, the kept set ignored.
    #[test]
    fn the_expected_matrix_follows_the_classes() {
        let m = expected_grants(&["syntheses".to_string()]);
        assert_eq!(m["synthesis_shares"].len(), 1);
        assert_eq!(
            m["synthesis_jobs"]["episcience_rw"],
            ["INSERT", "SELECT"].iter().map(|s| s.to_string()).collect()
        );
        assert_eq!(m["syntheses"]["episcience_rw"].len(), 4);
        assert!(m["syntheses"].contains_key("epigraph_app"));
        assert!(!m["samples"].contains_key("epigraph_app"));
    }
}
