//! The request path's database handle (E1g): REST and MCP run on the
//! `episcience_app` application login, and every statement they issue runs on
//! a session STAMPED as the calling principal, so row security, the author
//! binding and the claim guards apply to a request exactly as they apply to
//! that principal's kernel requests.
//!
//! [`EpiscienceDb`] holds two pools on the one application DSN, and exposes
//! neither:
//! - the stamped [`ScopedPool`]: every read ([`EpiscienceDb::read_as`]) and
//!   every write ([`EpiscienceDb::write_as`]) comes from it;
//! - `RESOLVE_POOL` (unstamped): [`Viewer::resolve`] and the operator-link
//!   parity read, exactly as the kernel's bearer middleware runs them on its
//!   plain pool. It never reads or writes an EpiScience table.
//!
//! [`EpiscienceDb::resolve_principal`] is the request's one authority
//! decision (kernel parity, `epigraph-api` `middleware/bearer.rs`): no
//! principal is a 401; a principal with ANY operator-link record (acting or
//! retired: operated agents are stdio-only) or one that cannot be resolved is
//! a 403. [`EpiscienceDb::write_as`] replicates the kernel's write-authority
//! triple: no stamped pool (impossible here by construction: the type holds
//! one), an unresolvable viewer, or an EMPTY writable set refuse BEFORE the
//! transaction begins.
//!
//! [`EpiscienceDb::connect`] carries the boot refusals the worker has (E1f):
//! a privileged or switched session, a session-GUC probe failure, a tenancy
//! contract or schema probe failure. A superuser DSN therefore never serves a
//! request.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use epigraph_db::{
    AgentRepository, ScopedPool, ScopedPoolOptions, ScopedRead, ScopedTx, SessionGucMode, Viewer,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

use crate::tenancy_contract;

/// Why a request was refused before (or instead of) touching EpiScience data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestRefusal {
    /// The token names no principal (`agent_id`). HTTP 401.
    #[error("principal_required: the token carries no agent_id")]
    PrincipalRequired,
    /// The principal has an operator-link record (acting or retired): an
    /// operated agent is stdio-only in the kernel and has no HTTP authority
    /// here either. HTTP 403.
    #[error(
        "agent {0} is an operated agent (it has an operator link): operated agents are \
         stdio-only and carry no HTTP authority"
    )]
    Operated(Uuid),
    /// The principal's authority could not be established (the membership or
    /// the operator-link read failed). Fails closed. HTTP 403.
    #[error("the caller's authority cannot be resolved: {0}")]
    Unresolvable(String),
    /// A write by a principal that may write no group (no `admin` or
    /// `writer` membership). Refused before BEGIN; reads still serve public
    /// rows. HTTP 403.
    #[error("the caller may write no group (it holds no admin or writer membership)")]
    NoWritableGroup,
    /// The stamped session could not be opened. HTTP 500.
    #[error("the caller's database session could not be opened: {0}")]
    Session(String),
}

/// The request path's database handle. Cheap to clone (two pool handles).
#[derive(Clone)]
pub struct EpiscienceDb {
    scoped: Arc<ScopedPool>,
    resolve_pool: PgPool,
}

impl std::fmt::Debug for EpiscienceDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EpiscienceDb")
    }
}

/// Sizing and naming of [`EpiscienceDb::connect`]'s pools.
#[derive(Debug, Clone, Copy)]
pub struct EpiscienceDbOptions {
    /// `application_name` of every session (unless the DSN names one), so the
    /// process is attributable in `pg_stat_activity`.
    pub application_name: &'static str,
    /// Stamped pool size.
    pub max_connections: u32,
    /// Session-GUC transport (`EPIGRAPH_SESSION_GUC_MODE`).
    pub mode: SessionGucMode,
}

/// `url` with `application_name` set, unless the DSN already names one.
#[must_use]
pub fn with_application_name(url: &str, name: &str) -> String {
    if url.contains("application_name=") {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&application_name={name}")
    } else {
        format!("{url}?application_name={name}")
    }
}

impl EpiscienceDb {
    /// Connect on the application DSN and run the boot refusals, in order:
    /// refuse a privileged or switched session
    /// ([`tenancy_contract::refuse_privileged_session`]: a superuser, a
    /// BYPASSRLS role or the kernel maintenance role reachable from the
    /// login, or a role switch); the tenancy contract probe; the schema
    /// probe; then the stamped pool and its session-GUC probe.
    ///
    /// # Errors
    /// The refusal, as text (the binary prints it and exits). Never contains
    /// the DSN.
    pub async fn connect(url: &str, options: EpiscienceDbOptions) -> Result<Self, String> {
        let url = with_application_name(url, options.application_name);
        let opts = PgConnectOptions::from_str(&url)
            .map_err(|e| format!("the database URL is not a valid DSN: {e}"))?;
        let resolve_pool = PgPoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts)
            .await
            .map_err(|e| format!("connect RESOLVE_POOL: {e}"))?;
        tenancy_contract::refuse_privileged_session(&resolve_pool).await?;
        tenancy_contract::probe(&resolve_pool)
            .await
            .map_err(|e| e.to_string())?;
        tenancy_contract::probe_schema(&resolve_pool).await?;
        let scoped = ScopedPool::connect_with_options(
            &url,
            options.mode,
            ScopedPoolOptions {
                max_connections: options.max_connections,
                ..ScopedPoolOptions::default()
            },
        )
        .await
        .map_err(|e| format!("connect the stamped pool: {e}"))?;
        scoped
            .probe_session_gucs()
            .await
            .map_err(|e| format!("session-GUC probe: {e}"))?;
        Ok(Self {
            scoped: Arc::new(scoped),
            resolve_pool,
        })
    }

    /// The caller's viewer, or the refusal (see [`RequestRefusal`]). The
    /// membership read and the operator-link read run concurrently on
    /// `RESOLVE_POOL`, as the kernel runs them.
    ///
    /// # Errors
    /// [`RequestRefusal::PrincipalRequired`] for `None`;
    /// [`RequestRefusal::Operated`] for any operator-link record;
    /// [`RequestRefusal::Unresolvable`] when either read fails.
    pub async fn resolve_principal(
        &self,
        principal: Option<Uuid>,
    ) -> Result<Viewer, RequestRefusal> {
        let principal = principal.ok_or(RequestRefusal::PrincipalRequired)?;
        let (viewer, link) = tokio::join!(
            Viewer::resolve(&self.resolve_pool, principal),
            AgentRepository::operator_of_author_pool(&self.resolve_pool, principal),
        );
        match link {
            Ok(None) => {}
            Ok(Some(_)) => return Err(RequestRefusal::Operated(principal)),
            Err(e) => {
                return Err(RequestRefusal::Unresolvable(format!(
                    "the operator-link read failed: {e}"
                )))
            }
        }
        viewer.map_err(|e| RequestRefusal::Unresolvable(format!("the membership read failed: {e}")))
    }

    /// A session stamped as `viewer`, for READS (the kernel's mode dispatch:
    /// a stamped connection in session mode, a stamped transaction in
    /// transaction mode). Row security shows the viewer public rows and the
    /// rows of its groups, nothing else.
    ///
    /// # Errors
    /// [`RequestRefusal::Session`] when the session cannot be opened.
    pub async fn read_as(&self, viewer: &Viewer) -> Result<ScopedRead<'_>, RequestRefusal> {
        self.scoped
            .read_as(viewer)
            .await
            .map_err(|e| RequestRefusal::Session(e.to_string()))
    }

    /// A TRANSACTION stamped as `viewer`, for WRITES (and for a read that
    /// decides a write: the check and the write in one transaction). Refused
    /// before BEGIN when the viewer has no principal or may write no group.
    /// Commit it; dropping it rolls back.
    ///
    /// # Errors
    /// [`RequestRefusal::Unresolvable`] (no principal),
    /// [`RequestRefusal::NoWritableGroup`], or
    /// [`RequestRefusal::Session`] when the transaction cannot be opened.
    pub async fn write_as(&self, viewer: &Viewer) -> Result<ScopedTx<'_>, RequestRefusal> {
        if viewer.principal().is_none() {
            return Err(RequestRefusal::Unresolvable(
                "a write needs a resolved principal".into(),
            ));
        }
        if viewer.writable_groups().is_empty() {
            return Err(RequestRefusal::NoWritableGroup);
        }
        self.scoped
            .begin_as(viewer)
            .await
            .map_err(|e| RequestRefusal::Session(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_name_is_added_once_and_never_overrides_the_dsn() {
        assert_eq!(
            with_application_name("postgres://u@h/d", "episcience-server"),
            "postgres://u@h/d?application_name=episcience-server"
        );
        assert_eq!(
            with_application_name("postgres://u@h/d?sslmode=disable", "x"),
            "postgres://u@h/d?sslmode=disable&application_name=x"
        );
        assert_eq!(
            with_application_name("postgres://u@h/d?application_name=mine", "x"),
            "postgres://u@h/d?application_name=mine"
        );
    }

    /// The refusal words the surfaces map to HTTP / MCP answers. Kills: a
    /// variant whose message loses the token a client keys on.
    #[test]
    fn refusals_name_their_cause() {
        assert!(RequestRefusal::PrincipalRequired
            .to_string()
            .starts_with("principal_required"));
        let p = Uuid::now_v7();
        assert!(RequestRefusal::Operated(p)
            .to_string()
            .contains(&p.to_string()));
        assert!(RequestRefusal::NoWritableGroup
            .to_string()
            .contains("may write no group"));
    }
}
