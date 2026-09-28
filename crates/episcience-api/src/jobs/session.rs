//! Where a synthesis stage's writes go.
//!
//! The synthesis handler runs every stage's writes through a [`StageSession`]:
//! the `episcience-worker` process, on the `episcience_worker` application
//! login. Every stage's writes run in their own transaction stamped
//! (`ScopedPool::begin_as`) as the synthesis' ACTING principal
//! (`synthesis_jobs.principal_id`, returned by the queue), so row security,
//! the author binding and the claim guards apply exactly as they do to that
//! principal's own requests. Before each transaction the viewer is
//! RE-RESOLVED and the synthesis' owner group must be in its writable set: a
//! principal who loses write authority mid-job gets
//! [`SessionError::Authority`], which the worker treats as terminal (the job
//! ends `failed: authority`, nothing further is written).
//!
//! (The legacy in-process runner's privileged, unstamped session was deleted
//! in E1h together with the runner.)

use std::sync::Arc;

use epigraph_db::{ScopedPool, ScopedTx, Viewer};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// SQLSTATEs of a failure that says nothing about the request, only about the
/// moment: connection exceptions (class 08), insufficient resources (53),
/// operator intervention (`57P01`-`57P05`, and `57014` query cancelled, e.g. a
/// statement timeout), serialization failure (`40001`), deadlock (`40P01`),
/// lock not available (`55P03`, e.g. a lock timeout).
#[must_use]
pub fn is_transient_sqlstate(code: &str) -> bool {
    code.starts_with("08")
        || code.starts_with("53")
        || code.starts_with("57P")
        || matches!(code, "57014" | "40001" | "40P01" | "55P03")
}

/// Whether a database error is TRANSIENT (retry later) rather than an answer
/// (act on it now): a transport, TLS, protocol or pool failure, or a
/// transient SQLSTATE ([`is_transient_sqlstate`]). Everything else (a missing
/// relation, a permission refusal, a RAISE) is deterministic and terminal.
#[must_use]
pub fn is_transient_sqlx(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(d) => d.code().is_some_and(|c| is_transient_sqlstate(&c)),
        _ => false,
    }
}

/// [`is_transient_sqlx`] for the kernel's repository error.
#[must_use]
pub fn is_transient_db(e: &epigraph_db::DbError) -> bool {
    match e {
        epigraph_db::DbError::QueryFailed { source }
        | epigraph_db::DbError::ConnectionFailed { source } => is_transient_sqlx(source),
        _ => false,
    }
}

/// The session error for a failed `Viewer::resolve`: `Viewer::resolve` never
/// REFUSES (an unknown principal resolves with no groups), so its every error
/// is a database error; a transient one is [`SessionError::Db`] (the job is
/// retried), anything else fails closed as [`SessionError::Authority`].
fn resolve_failure(e: &epigraph_db::DbError) -> SessionError {
    if is_transient_db(e) {
        SessionError::Db(format!(
            "the acting principal could not be resolved now: {e}"
        ))
    } else {
        SessionError::Authority(format!("the acting principal cannot be resolved: {e}"))
    }
}

/// A refusal to open a stage transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The acting principal may no longer write this synthesis (or can no
    /// longer see it). Terminal: never retried.
    Authority(String),
    /// A database failure opening the transaction. Transient.
    Db(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authority(m) => write!(f, "authority: {m}"),
            Self::Db(m) => write!(f, "database: {m}"),
        }
    }
}

/// The worker's side of a stage session: the acting principal on the
/// application login.
#[derive(Clone)]
pub struct OwnerSession {
    /// The stamped pool every stage transaction comes from.
    pub scoped: Arc<ScopedPool>,
    /// `RESOLVE_POOL`: the unstamped application-role pool the viewer is
    /// resolved on (kernel parity: `Viewer::resolve` on a plain pool).
    pub resolve_pool: PgPool,
    /// The acting principal (`synthesis_jobs.principal_id`).
    pub principal: Uuid,
    /// The synthesis whose owner group must stay writable.
    pub synthesis_id: Uuid,
}

/// Where a stage's writes go: the worker's acting principal. See the module
/// documentation.
#[derive(Clone)]
pub enum StageSession {
    /// The worker's acting principal.
    Owner(OwnerSession),
}

/// An open stage transaction, stamped as the acting principal; derefs to the
/// connection.
pub enum StageTx<'a> {
    /// A transaction stamped as the acting principal.
    Scoped(ScopedTx<'a>),
}

impl std::ops::Deref for StageTx<'_> {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        match self {
            Self::Scoped(t) => t,
        }
    }
}

impl std::ops::DerefMut for StageTx<'_> {
    fn deref_mut(&mut self) -> &mut PgConnection {
        match self {
            Self::Scoped(t) => t,
        }
    }
}

impl StageTx<'_> {
    /// Commit the stage's writes.
    ///
    /// # Errors
    /// The commit failure, as text.
    pub async fn commit(self) -> Result<(), String> {
        match self {
            Self::Scoped(t) => t.commit().await.map_err(|e| e.to_string()),
        }
    }
}

impl StageSession {
    /// The viewer the acting principal reads as (stages 1, 2 and 4).
    ///
    /// # Errors
    /// [`SessionError::Authority`] when the principal cannot be resolved (it
    /// fails the job closed, before any stage runs); [`SessionError::Db`] when
    /// the resolution failed transiently (the job is retried).
    pub async fn viewer(&self, acting: Uuid) -> Result<Viewer, SessionError> {
        let Self::Owner(o) = self;
        Viewer::resolve(&o.resolve_pool, acting)
            .await
            .map_err(|e| resolve_failure(&e))
    }

    /// Open the next stage's transaction.
    ///
    /// Re-resolve the acting principal, refuse an
    /// empty writable set, open a transaction stamped as that viewer, and
    /// refuse unless the synthesis is visible on it and its owner group is
    /// one the viewer may write. The check runs INSIDE the stamped
    /// transaction, so the owner group read is the row the stage then writes.
    ///
    /// # Errors
    /// [`SessionError::Authority`] as above; [`SessionError::Db`] when the
    /// transaction cannot be opened or the check cannot be read.
    pub async fn begin(&self) -> Result<StageTx<'_>, SessionError> {
        match self {
            Self::Owner(o) => {
                let viewer = Viewer::resolve(&o.resolve_pool, o.principal)
                    .await
                    .map_err(|e| resolve_failure(&e))?;
                if viewer.writable_groups().is_empty() {
                    return Err(SessionError::Authority(
                        "the acting principal may write no group".into(),
                    ));
                }
                let mut tx = o
                    .scoped
                    .begin_as(&viewer)
                    .await
                    .map_err(|e| SessionError::Db(e.to_string()))?;
                let owner: Option<Uuid> =
                    sqlx::query_scalar("SELECT owner_group_id FROM syntheses WHERE id = $1")
                        .bind(o.synthesis_id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(|e| SessionError::Db(e.to_string()))?;
                match owner {
                    None => Err(SessionError::Authority(
                        "the synthesis is not visible to the acting principal".into(),
                    )),
                    Some(g) if !viewer.writable_groups().contains(&g) => {
                        Err(SessionError::Authority(
                            "the acting principal may not write the synthesis' owner group".into(),
                        ))
                    }
                    Some(_) => Ok(StageTx::Scoped(tx)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transient classes retry; an answer does not. Kills: widening the
    /// transient set to every database error (a missing relation or a
    /// permission refusal would then be retried until the attempts run out
    /// instead of failing closed), and narrowing it to transport errors only.
    #[test]
    fn transient_sqlstates_and_errors_are_told_from_answers() {
        for code in [
            "08006", "08P01", "53300", "57P01", "57014", "40001", "40P01", "55P03",
        ] {
            assert!(is_transient_sqlstate(code), "{code}");
        }
        for code in [
            "42P01", "42501", "23503", "22023", "55000", "P0001", "XX000",
        ] {
            assert!(!is_transient_sqlstate(code), "{code}");
        }
        assert!(is_transient_sqlx(&sqlx::Error::PoolClosed));
        assert!(is_transient_sqlx(&sqlx::Error::PoolTimedOut));
        assert!(is_transient_sqlx(&sqlx::Error::Io(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        ))));
        assert!(!is_transient_sqlx(&sqlx::Error::RowNotFound));
        assert!(!is_transient_sqlx(&sqlx::Error::ColumnNotFound("x".into())));
        assert!(is_transient_db(&epigraph_db::DbError::QueryFailed {
            source: sqlx::Error::PoolClosed
        }));
        assert!(!is_transient_db(&epigraph_db::DbError::QueryFailed {
            source: sqlx::Error::RowNotFound
        }));
    }
}
