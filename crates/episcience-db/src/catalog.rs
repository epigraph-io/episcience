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
//! - [`policy_findings`]: the policies are exactly 5036's set (table, name,
//!   command, permissive or RESTRICTIVE, for PUBLIC), every expression opens
//!   with the two bypass arms, carries no world arm and calls no function
//!   outside the contract-v1 session helpers, the T-PUB read and write
//!   shapes are the kernel's, the bypass-only policies are bypass-only, and
//!   every owner policy is scoped to the writable groups. (The ratchets R1-R3
//!   hold the same on the template; this holds them on a LIVE database,
//!   where a hand fix or a later script could drop or loosen a policy.)
//! - [`principal_guard_findings`]: each of the 12 tenancy tables carries the
//!   enabled statement-level principal guard of 5036.
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

/// The two bypass arms every policy expression opens with, as Postgres 16
/// renders them (`pg_get_expr`).
pub const BYPASS_ARMS: &str = "(( SELECT epigraph_bypass() AS epigraph_bypass) OR ( SELECT epigraph_definer_bypass() AS epigraph_definer_bypass)";

/// The contract-v1 session helpers (C2): the only functions a policy calls.
pub const POLICY_HELPERS: [&str; 5] = [
    "epigraph_bypass",
    "epigraph_definer_bypass",
    "epigraph_session_groups",
    "epigraph_writable_groups",
    "epigraph_principal_id",
];

/// The tables whose rows cite a claim: each carries a RESTRICTIVE
/// `<t>_claim_visible` policy.
pub const CLAIM_VISIBLE_TABLES: [&str; 4] = [
    "synthesis_claim_membership",
    "sample_claims",
    "countersignatures",
    "synthesis_provo_edges",
];

/// One expected policy: table, name, `polcmd` (`r` `a` `w` `d` `*`),
/// permissive.
pub type ExpectedPolicy = (String, String, char, bool);

/// 5036's policy set, derived from the table classes.
pub fn expected_policies() -> BTreeSet<ExpectedPolicy> {
    let mut out = BTreeSet::new();
    let mut add = |t: &str, suffix: &str, cmd: char, permissive: bool| {
        out.insert((t.to_string(), format!("{t}_{suffix}"), cmd, permissive));
    };
    for t in OWNERSHIP_TABLES {
        add(t, "tenancy", '*', true);
        add(t, "update_owner", 'w', false);
        add(t, "delete_owner", 'd', false);
    }
    for t in APPEND_TABLES {
        add(t, "read", 'r', true);
        add(t, "insert", 'a', true);
        add(t, "bypass_update", 'w', true);
        add(t, "bypass_delete", 'd', true);
    }
    for t in FROZEN_TABLES {
        add(t, "bypass_all", '*', true);
    }
    for t in CLAIM_VISIBLE_TABLES {
        add(t, "claim_visible", '*', false);
    }
    out
}

/// The T-PUB read expression (`<t>_tenancy` USING), as rendered.
pub fn tenancy_read_shape() -> String {
    format!(
        "{BYPASS_ARMS} OR ((visibility)::text = 'public'::text) OR (owner_group_id = ANY (( SELECT epigraph_session_groups() AS epigraph_session_groups)::uuid[])))"
    )
}

/// The T-PUB write expression (`<t>_tenancy` WITH CHECK), as rendered.
pub fn tenancy_write_shape() -> String {
    format!(
        "{BYPASS_ARMS} OR (owner_group_id = ANY (( SELECT epigraph_writable_groups() AS epigraph_writable_groups)::uuid[])))"
    )
}

/// The functions an expression calls: every identifier immediately followed
/// by `(` (the renderer writes a call with no space; `ANY (`, `EXISTS (` and
/// casts carry one).
fn called_functions(e: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut ident = String::new();
    for ch in e.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            ident.push(ch);
        } else {
            if ch == '(' && !ident.is_empty() && !ident.starts_with(|c: char| c.is_ascii_digit()) {
                out.push(ident.trim_start_matches("public.").to_string());
            }
            ident.clear();
        }
    }
    out
}

/// A world arm: a `true` disjunct or a sentinel group literal.
fn has_world_arm(e: &str) -> bool {
    let lower = e.to_ascii_lowercase();
    let word_true = lower.match_indices("true").any(|(i, _)| {
        let before = lower[..i].chars().last();
        let after = lower[i + 4..].chars().next();
        let boundary =
            |c: Option<char>| !matches!(c, Some(c) if c.is_ascii_alphanumeric() || c == '_');
        boundary(before) && boundary(after)
    });
    word_true || SENTINEL_GROUPS.iter().any(|g| e.contains(g))
}

/// A policy as the catalog holds it: table, name, command, permissive,
/// for PUBLIC, USING, WITH CHECK.
type PolicyRow = (
    String,
    String,
    String,
    bool,
    bool,
    Option<String>,
    Option<String>,
);

/// See the module doc.
pub async fn policy_findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<PolicyRow> = sqlx::query_as(
        "SELECT c.relname::text, p.polname::text, p.polcmd::text, p.polpermissive, \
                    p.polroles = ARRAY[0::oid], \
                    pg_catalog.pg_get_expr(p.polqual, p.polrelid), \
                    pg_catalog.pg_get_expr(p.polwithcheck, p.polrelid) \
               FROM pg_catalog.pg_policy p JOIN pg_catalog.pg_class c ON c.oid = p.polrelid \
              WHERE c.relnamespace = 'public'::pg_catalog.regnamespace AND c.relname = ANY($1) \
              ORDER BY 1, 2",
    )
    .bind(EPISCIENCE_TABLES.to_vec())
    .fetch_all(&mut *conn)
    .await?;
    let expected = expected_policies();
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    // `pg_get_expr` qualifies a name that is not on the session's
    // `search_path` (the migrator's is `episcience_meta`): read every
    // expression as it renders under `public`, the form the shapes use.
    let unqualify = |e: &Option<String>| e.as_ref().map(|e| e.replace("public.", ""));
    for (table, name, cmd, permissive, for_public, using, check) in &rows {
        let (using, check) = (&unqualify(using), &unqualify(check));
        let who = format!("{table}.{name}");
        let key = (
            table.clone(),
            name.clone(),
            cmd.chars().next().unwrap_or('?'),
            *permissive,
        );
        if !expected.contains(&key) {
            out.push(format!(
                "policies: {who} ({cmd}, {}) is not in the model",
                if *permissive {
                    "permissive"
                } else {
                    "RESTRICTIVE"
                }
            ));
        }
        seen.insert(key);
        if !for_public {
            out.push(format!("policies: {who} is not for PUBLIC"));
        }
        let exprs: Vec<&String> = using.iter().chain(check.iter()).collect();
        if exprs.is_empty() {
            out.push(format!("policies: {who} has no expression"));
        }
        for e in &exprs {
            if !(e.as_str() == format!("{BYPASS_ARMS})")
                || e.starts_with(&format!("{BYPASS_ARMS} OR ")))
            {
                out.push(format!(
                    "policies: {who} does not open with the bypass arms"
                ));
            }
            if has_world_arm(e) {
                out.push(format!("policies: {who} carries a world arm: {e}"));
            }
            for f in called_functions(e) {
                if !POLICY_HELPERS.contains(&f.as_str()) {
                    out.push(format!(
                        "policies: {who} calls {f}, which is not a contract-v1 helper"
                    ));
                }
            }
        }
        let bypass_only = format!("{BYPASS_ARMS})");
        if name.ends_with("_tenancy") {
            if using.as_deref() != Some(tenancy_read_shape().as_str()) {
                out.push(format!(
                    "policies: {who}: the read shape differs from the kernel's"
                ));
            }
            if check.as_deref() != Some(tenancy_write_shape().as_str()) {
                out.push(format!(
                    "policies: {who}: the write shape differs from the kernel's"
                ));
            }
        }
        if name.contains("_bypass_") && exprs.iter().any(|e| **e != bypass_only) {
            out.push(format!("policies: {who} is not bypass-only"));
        }
        if (name.ends_with("_update_owner") || name.ends_with("_delete_owner"))
            && exprs
                .iter()
                .any(|e| !e.contains("epigraph_writable_groups()"))
        {
            out.push(format!(
                "policies: {who} is not scoped to the writable groups"
            ));
        }
    }
    for (table, name, cmd, permissive) in expected.difference(&seen) {
        out.push(format!(
            "policies: {table}.{name} ({cmd}, {}) is missing",
            if *permissive {
                "permissive"
            } else {
                "RESTRICTIVE"
            }
        ));
    }
    Ok(out)
}

/// The statement-level principal guard of 5036 on each tenancy table:
/// enabled, BEFORE, FOR EACH STATEMENT, on INSERT, UPDATE and DELETE,
/// running `episcience_require_principal`.
pub async fn principal_guard_findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    // `tgtype`: bit 0 FOR EACH ROW, bit 1 BEFORE, bits 2/3/4 INSERT/DELETE/UPDATE.
    let ok: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text \
           FROM pg_catalog.pg_trigger t \
           JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
           JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid \
          WHERE c.relnamespace = 'public'::pg_catalog.regnamespace AND c.relname = ANY($1) \
            AND t.tgname = 'tenancy_05_principal' AND NOT t.tgisinternal \
            AND t.tgenabled = 'O' AND p.proname = 'episcience_require_principal' \
            AND (t.tgtype & 1) = 0 AND (t.tgtype & 2) = 2 AND (t.tgtype & 28) = 28",
    )
    .bind(TENANCY_TABLES.to_vec())
    .fetch_all(&mut *conn)
    .await?;
    Ok(TENANCY_TABLES
        .iter()
        .filter(|t| !ok.iter().any(|o| o == *t))
        .map(|t| format!("guards: {t} lacks the enabled statement-level principal guard"))
        .collect())
}

/// Every check above, in order.
pub async fn findings(conn: &mut PgConnection) -> Result<Vec<String>, sqlx::Error> {
    let mut out = definer_findings(conn).await?;
    out.extend(row_security_findings(conn).await?);
    out.extend(grant_findings(conn).await?);
    out.extend(sentinel_findings(conn).await?);
    out.extend(policy_findings(conn).await?);
    out.extend(principal_guard_findings(conn).await?);
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

    /// 44 policies (brief 7.2): three per ownership table, four per append
    /// table, one per frozen table, one claim-visibility per citing table.
    /// Kills: a class dropped from or duplicated in the expected set.
    #[test]
    fn the_expected_policy_set_is_the_forty_four() {
        let p = expected_policies();
        assert_eq!(p.len(), 44);
        assert!(p.contains(&(
            "synthesis_jobs".into(),
            "synthesis_jobs_bypass_update".into(),
            'w',
            true
        )));
        assert!(p.contains(&(
            "sample_claims".into(),
            "sample_claims_claim_visible".into(),
            '*',
            false
        )));
    }

    /// The expression scanners: calls are identifiers glued to `(`; `true`
    /// counts only as a word. Kills: `ANY (` read as a call, `trueish` or a
    /// column named `is_true` read as a world arm.
    #[test]
    fn the_expression_scanners_read_calls_and_world_arms() {
        assert_eq!(
            called_functions(&tenancy_read_shape()),
            vec![
                "epigraph_bypass",
                "epigraph_definer_bypass",
                "epigraph_session_groups"
            ]
        );
        assert!(has_world_arm("(x OR true)"));
        assert!(has_world_arm(
            "owner_group_id = '00000000-0000-0000-0000-000000000000'"
        ));
        assert!(!has_world_arm("(is_true OR trueish)"));
        assert!(!has_world_arm(&tenancy_write_shape()));
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
