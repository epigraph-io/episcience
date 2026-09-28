//! Tenancy contract v1: the EpiGraph kernel objects EpiScience depends on.
//!
//! EpiScience sits on the kernel's schema and references a fixed, versioned
//! set of kernel objects (roles, session functions, columns, grants). The set
//! is documented in `docs/tenancy-contract.md` and asserted in three places:
//!
//! 1. migration 5033's inline check and `public.episcience_assert_kernel_contract(1)`,
//!    which every later EpiScience migration calls first;
//! 2. [`probe`], which the server and the MCP binary run at boot and refuse to
//!    start on;
//! 3. the nightly canary, which runs both against kernel `main` HEAD.
//!
//! The boot probe checks what can drift WITHOUT a migration and fail silently
//! at run time: role existence, function signatures and EXECUTE grants, column
//! presence, table / column / sequence privileges, the vector extension's
//! schema. It uses only catalog reads and the `has_*_privilege` inquiry
//! functions, so it passes on a non-superuser application login. It does NOT
//! read table rows (the sentinel groups, the kernel ledger head, the
//! entity-type registration: C6, C7, C8); those are asserted by the migration
//! preamble, which runs as the migration owner, and [`ProbeReport::skipped`]
//! names them.

use sqlx::PgPool;

/// The contract version this build of EpiScience requires.
pub const CONTRACT_VERSION: u32 = 1;

/// Items asserted only by the migration preamble (they read table rows).
pub const PREAMBLE_ONLY: [(&str, &str); 3] = [
    ("C6", "the world and seed sentinel groups (row content)"),
    ("C7", "the kernel ledger head >= 110 (row content)"),
    ("C8", "the synthesis entity-type registration (row content)"),
];

/// One failed contract item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractFailure {
    /// The contract item, e.g. `"C11"`.
    pub item: &'static str,
    /// What was expected and is absent.
    pub detail: String,
}

/// Why [`probe`] refused.
#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("tenancy contract v{CONTRACT_VERSION} probe could not run: {0}")]
    Db(#[from] sqlx::Error),
    #[error("tenancy contract v{CONTRACT_VERSION} probe failed: {}", render(.0))]
    Failed(Vec<ContractFailure>),
}

impl ContractError {
    /// The failed items, in check order (empty for [`ContractError::Db`]).
    #[must_use]
    pub fn items(&self) -> Vec<&'static str> {
        match self {
            ContractError::Failed(f) => f.iter().map(|x| x.item).collect(),
            ContractError::Db(_) => Vec::new(),
        }
    }
}

fn render(f: &[ContractFailure]) -> String {
    f.iter()
        .map(|x| format!("{}: {}", x.item, x.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

/// What a passing probe checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Items checked by the probe, in order (an item appears once).
    pub checked: Vec<&'static str>,
    /// Items left to the migration preamble.
    pub skipped: Vec<&'static str>,
}

/// One boolean catalog check. `sql` returns a single boolean (NULL = false)
/// and takes up to three text binds.
struct Check {
    item: &'static str,
    detail: String,
    sql: &'static str,
    binds: Vec<String>,
}

const ROLE_EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)";

/// Signature, result type and `epigraph_app` EXECUTE of one function.
/// `to_regprocedure` returns NULL for a missing function, and the oid forms
/// of the inquiry functions are strict, so a missing function is `false`,
/// never an error.
const FN_OK: &str = "SELECT coalesce(pg_catalog.pg_get_function_result(p) = $2 \
     AND pg_catalog.has_function_privilege('epigraph_app'::name, p::oid, 'EXECUTE'), false) \
     FROM (SELECT pg_catalog.to_regprocedure($1) AS p) s";

const FN_EXEC_OK: &str = "SELECT coalesce(\
     pg_catalog.has_function_privilege('epigraph_app'::name, pg_catalog.to_regprocedure($1)::oid, 'EXECUTE'), \
     false)";

const COLUMN_EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a \
     WHERE a.attrelid = pg_catalog.to_regclass($1) AND a.attname = $2 \
       AND a.attnum > 0 AND NOT a.attisdropped)";

const TABLE_PRIV: &str = "SELECT coalesce(\
     pg_catalog.has_table_privilege($1::name, pg_catalog.to_regclass($2)::oid, $3), false)";

/// Column SELECT. The CASE evaluates the privilege only for a column that
/// exists (by name, `has_column_privilege` errors on a missing column).
const COLUMN_PRIV: &str = "SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a \
       WHERE a.attrelid = pg_catalog.to_regclass($1) AND a.attname = $2 \
         AND a.attnum > 0 AND NOT a.attisdropped) \
     THEN coalesce(pg_catalog.has_column_privilege('epigraph_app'::name, \
            pg_catalog.to_regclass($1)::oid, $2, 'SELECT'), false) \
     ELSE false END";

const SEQUENCE_USAGE: &str =
    "SELECT coalesce(pg_catalog.has_sequence_privilege('epigraph_app'::name, \
     pg_catalog.to_regclass($1)::oid, 'USAGE'), false)";

const VECTOR_IN_PUBLIC: &str = "SELECT coalesce((SELECT n.nspname = 'public' \
     FROM pg_catalog.pg_extension x JOIN pg_catalog.pg_namespace n ON n.oid = x.extnamespace \
     WHERE x.extname = 'vector'), false)";

const FN_EXISTS: &str = "SELECT pg_catalog.to_regprocedure($1) IS NOT NULL";

fn check(item: &'static str, detail: String, sql: &'static str, binds: &[&str]) -> Check {
    Check {
        item,
        detail,
        sql,
        binds: binds.iter().map(|s| s.to_string()).collect(),
    }
}

/// The checks after C1, in contract order.
fn checks() -> Vec<Check> {
    let mut v = Vec::new();
    for (f, ret) in [
        ("epigraph_bypass", "boolean"),
        ("epigraph_definer_bypass", "boolean"),
        ("epigraph_session_groups", "uuid[]"),
        ("epigraph_writable_groups", "uuid[]"),
        ("epigraph_principal_id", "uuid"),
    ] {
        v.push(check(
            "C2",
            format!("public.{f}() returning {ret}, EXECUTE-able by epigraph_app"),
            FN_OK,
            &[&format!("public.{f}()"), ret],
        ));
    }
    for (item, table, col) in [
        ("C3", "groups", "id"),
        ("C3", "groups", "kind"),
        ("C3", "groups", "did_key"),
        ("C3", "groups", "created_by_agent_id"),
        ("C3", "group_memberships", "group_id"),
        ("C3", "group_memberships", "agent_id"),
        ("C3", "group_memberships", "role"),
        ("C3", "group_memberships", "revoked_at"),
        ("C4", "claims", "id"),
        ("C4", "claims", "visibility"),
        ("C4", "claims", "owner_group_id"),
        ("C5", "security_events", "event_type"),
        ("C5", "security_events", "agent_id"),
        ("C5", "security_events", "success"),
        ("C5", "security_events", "details"),
    ] {
        v.push(check(
            item,
            format!("column public.{table}.{col}"),
            COLUMN_EXISTS,
            &[&format!("public.{table}"), col],
        ));
    }
    v.push(check(
        "C5",
        "epigraph_maintenance INSERT on public.security_events".into(),
        TABLE_PRIV,
        &["epigraph_maintenance", "public.security_events", "INSERT"],
    ));
    v.push(check(
        "C9",
        "extension vector in schema public".into(),
        VECTOR_IN_PUBLIC,
        &[],
    ));
    for t in ["claims", "groups", "group_memberships"] {
        v.push(check(
            "C10",
            format!("epigraph_maintenance SELECT on public.{t}"),
            TABLE_PRIV,
            &["epigraph_maintenance", &format!("public.{t}"), "SELECT"],
        ));
    }
    for t in ["claims", "edges", "events"] {
        v.push(check(
            "C11",
            format!("epigraph_app INSERT on public.{t}"),
            TABLE_PRIV,
            &["epigraph_app", &format!("public.{t}"), "INSERT"],
        ));
    }
    for f in ["epigraph_live_memberships", "epigraph_operator_of_author"] {
        v.push(check(
            "C12",
            format!("public.{f}(uuid) EXECUTE-able by epigraph_app"),
            FN_EXEC_OK,
            &[&format!("public.{f}(uuid)")],
        ));
    }
    for col in ["id", "public_key", "display_name"] {
        v.push(check(
            "C13",
            format!("epigraph_app SELECT on column public.agents.{col}"),
            COLUMN_PRIV,
            &["public.agents", col],
        ));
    }
    v.push(check(
        "C14",
        "epigraph_app USAGE on public.events_graph_version_seq".into(),
        SEQUENCE_USAGE,
        &["public.events_graph_version_seq"],
    ));
    v.push(check(
        "L1",
        "column public.syntheses.autonomy_level (the EpiScience legacy head)".into(),
        COLUMN_EXISTS,
        &["public.syntheses", "autonomy_level"],
    ));
    v.push(check(
        "S1",
        "public.episcience_assert_kernel_contract(integer) (EpiScience migration 5033 applied)"
            .into(),
        FN_EXISTS,
        &["public.episcience_assert_kernel_contract(integer)"],
    ));
    v
}

fn push_once(v: &mut Vec<&'static str>, item: &'static str) {
    if !v.contains(&item) {
        v.push(item);
    }
}

/// Run the contract v1 catalog checks on `pool`.
///
/// # Errors
/// [`ContractError::Failed`] listing every failed item (C1 alone when a kernel
/// role is missing, since every later privilege check names those roles);
/// [`ContractError::Db`] when a check cannot run at all.
pub async fn probe(pool: &PgPool) -> Result<ProbeReport, ContractError> {
    let mut checked = Vec::new();
    let mut failed = Vec::new();

    push_once(&mut checked, "C1");
    for role in ["epigraph_app", "epigraph_maintenance"] {
        let ok: bool = sqlx::query_scalar(ROLE_EXISTS)
            .bind(role)
            .fetch_one(pool)
            .await?;
        if !ok {
            failed.push(ContractFailure {
                item: "C1",
                detail: format!("role {role} is missing"),
            });
        }
    }
    if !failed.is_empty() {
        return Err(ContractError::Failed(failed));
    }

    for c in checks() {
        push_once(&mut checked, c.item);
        let mut q = sqlx::query_scalar::<_, Option<bool>>(c.sql);
        for b in &c.binds {
            q = q.bind(b);
        }
        let ok = q.fetch_one(pool).await?.unwrap_or(false);
        if !ok {
            failed.push(ContractFailure {
                item: c.item,
                detail: format!("expected {}", c.detail),
            });
        }
    }

    if failed.is_empty() {
        Ok(ProbeReport {
            checked,
            skipped: PREAMBLE_ONLY.iter().map(|(i, _)| *i).collect(),
        })
    } else {
        Err(ContractError::Failed(failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every contract item of v1 is either probed at boot or explicitly left
    /// to the preamble, and no item is both. Kills: a check silently dropped
    /// from [`checks`] (its item would be neither probed nor listed skipped).
    #[test]
    fn every_v1_item_is_probed_or_explicitly_skipped() {
        let mut probed: Vec<&str> = vec!["C1"];
        for c in checks() {
            if !probed.contains(&c.item) {
                probed.push(c.item);
            }
        }
        let skipped: Vec<&str> = PREAMBLE_ONLY.iter().map(|(i, _)| *i).collect();
        for s in &skipped {
            assert!(!probed.contains(s), "{s} is both probed and skipped");
        }
        let mut all: Vec<&str> = probed.iter().chain(skipped.iter()).copied().collect();
        all.sort_by_key(|i| {
            (
                i.chars().next().unwrap(),
                i[1..].parse::<u32>().unwrap_or(0),
            )
        });
        assert_eq!(
            all,
            vec![
                "C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8", "C9", "C10", "C11", "C12", "C13",
                "C14", "L1", "S1"
            ]
        );
    }

    #[test]
    fn a_failure_renders_every_item_and_detail() {
        let e = ContractError::Failed(vec![
            ContractFailure {
                item: "C11",
                detail: "x".into(),
            },
            ContractFailure {
                item: "C14",
                detail: "y".into(),
            },
        ]);
        let s = e.to_string();
        assert!(s.contains("contract v1 probe failed"), "{s}");
        assert!(s.contains("C11: x") && s.contains("C14: y"), "{s}");
        assert_eq!(e.items(), vec!["C11", "C14"]);
    }
}
