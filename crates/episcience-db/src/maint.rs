//! The one-shot maintenance acts EpiScience runs through its maintenance
//! login (`episcience_maint`, a member of `episcience_maint_ops` only): thin
//! calls of the maintenance-owned definers of migration 5034. The login holds
//! no table privilege; the definers are the whole of its authority.

use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::DbError;

/// The manifest kind the backfill writes and the reverse accepts.
pub const MANIFEST_KIND: &str = "episcience.backfill_owners.v1";

/// `episcience_maint_backfill_owners(principal, apply)`: the manifest of the
/// legacy re-own. `apply = false` is a dry run (nothing persists).
pub async fn backfill_owners(
    conn: &mut PgConnection,
    principal: Uuid,
    apply: bool,
) -> Result<serde_json::Value, DbError> {
    Ok(
        sqlx::query_scalar("SELECT manifest FROM public.episcience_maint_backfill_owners($1, $2)")
            .bind(principal)
            .bind(apply)
            .fetch_one(conn)
            .await?,
    )
}

/// `episcience_maint_backfill_reverse(manifest)`: rows restored (only while
/// the pair is still nullable).
pub async fn backfill_reverse(
    conn: &mut PgConnection,
    manifest: &serde_json::Value,
) -> Result<i32, DbError> {
    Ok(
        sqlx::query_scalar("SELECT public.episcience_maint_backfill_reverse($1)")
            .bind(manifest)
            .fetch_one(conn)
            .await?,
    )
}

/// Refuse a session that could do more than the definers allow: a superuser,
/// a BYPASSRLS role, or a member of the kernel maintenance role. The
/// maintenance login is deliberately narrow; a broader DSN in its place (a
/// migration DSN pasted by mistake) is refused rather than used.
pub async fn refuse_privileged_session(conn: &mut PgConnection) -> Result<(), String> {
    let (sup, bypass, maint): (bool, bool, bool) = sqlx::query_as(
        "SELECT r.rolsuper, r.rolbypassrls, \
                pg_has_role(session_user, 'epigraph_maintenance', 'MEMBER') \
           FROM pg_roles r WHERE r.rolname = session_user",
    )
    .fetch_one(conn)
    .await
    .map_err(|e| format!("read the session role: {e}"))?;
    if sup || bypass || maint {
        return Err(
            "refusing: the session role is a superuser, BYPASSRLS or a kernel maintenance member; \
             use the episcience_maint login (EPISCIENCE_MAINT_DATABASE_URL)"
                .into(),
        );
    }
    Ok(())
}
