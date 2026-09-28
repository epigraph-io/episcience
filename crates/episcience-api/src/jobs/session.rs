//! Where a synthesis stage's writes go.
//!
//! The synthesis handler runs every stage's writes through a [`StageSession`],
//! so ONE handler serves both runtimes:
//!
//! - [`StageSession::Privileged`]: the legacy in-process runner inside the
//!   server (`EPISCIENCE_INPROCESS_WORKER`, on by default until the deploy
//!   flips it), on the server's privileged pool, unstamped. Its behaviour is
//!   the pre-E1f behaviour.
//! - [`StageSession::Owner`]: the `episcience-worker` process, on the
//!   `episcience_worker` application login. Every stage's writes run in their
//!   own transaction stamped (`ScopedPool::begin_as`) as the synthesis'
//!   ACTING principal (`synthesis_jobs.principal_id`, returned by the queue),
//!   so row security, the author binding and the claim guards apply exactly
//!   as they do to that principal's own requests. Before each transaction the
//!   viewer is RE-RESOLVED and the synthesis' owner group must be in its
//!   writable set: a principal who loses write authority mid-job gets
//!   [`SessionError::Authority`], which the worker treats as terminal (the job
//!   ends `failed: authority`, nothing further is written).

use std::sync::Arc;

use epigraph_db::{ScopedPool, ScopedTx, Viewer};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

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

/// Where a stage's writes go. See the module documentation.
#[derive(Clone)]
pub enum StageSession {
    /// The legacy in-process runner's privileged, unstamped pool.
    Privileged(PgPool),
    /// The worker's acting principal.
    Owner(OwnerSession),
}

/// An open stage transaction; derefs to the connection.
pub enum StageTx<'a> {
    /// A plain transaction on the privileged pool.
    Plain(Transaction<'static, Postgres>),
    /// A transaction stamped as the acting principal.
    Scoped(ScopedTx<'a>),
}

impl std::ops::Deref for StageTx<'_> {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        match self {
            Self::Plain(t) => t,
            Self::Scoped(t) => t,
        }
    }
}

impl std::ops::DerefMut for StageTx<'_> {
    fn deref_mut(&mut self) -> &mut PgConnection {
        match self {
            Self::Plain(t) => t,
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
            Self::Plain(t) => t.commit().await.map_err(|e| e.to_string()),
            Self::Scoped(t) => t.commit().await.map_err(|e| e.to_string()),
        }
    }
}

impl StageSession {
    /// The viewer the acting principal reads as (stages 1, 2 and 4).
    ///
    /// # Errors
    /// [`SessionError::Authority`] when the principal cannot be resolved (it
    /// fails the job closed, before any stage runs).
    pub async fn viewer(&self, acting: Uuid) -> Result<Viewer, SessionError> {
        let pool = match self {
            Self::Privileged(pool) => pool,
            Self::Owner(o) => &o.resolve_pool,
        };
        Viewer::resolve(pool, acting).await.map_err(|e| {
            SessionError::Authority(format!("the acting principal cannot be resolved: {e}"))
        })
    }

    /// Open the next stage's transaction.
    ///
    /// On [`StageSession::Owner`]: re-resolve the acting principal, refuse an
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
            Self::Privileged(pool) => pool
                .begin()
                .await
                .map(StageTx::Plain)
                .map_err(|e| SessionError::Db(e.to_string())),
            Self::Owner(o) => {
                let viewer = Viewer::resolve(&o.resolve_pool, o.principal)
                    .await
                    .map_err(|e| {
                        SessionError::Authority(format!(
                            "the acting principal cannot be resolved: {e}"
                        ))
                    })?;
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

    /// The pool the handler's unstamped reads run on: the privileged pool,
    /// or none for the worker (whose engine pool is the handler's own).
    #[must_use]
    pub fn is_privileged(&self) -> bool {
        matches!(self, Self::Privileged(_))
    }
}
