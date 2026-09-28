//! Reads of KERNEL `claims` rows that EpiScience issues itself.
//!
//! The process still connects as a role that bypasses row-level security, so
//! the kernel's visibility rules do not apply by themselves. Every statement
//! here therefore carries the kernel's own `/* {VISIBILITY:c} */` marker and is
//! rendered with `epigraph_db::Viewer::splice`: a caller reads exactly the
//! claims the kernel would show it (public, or owned by one of its groups).
//! A claim the viewer cannot read is indistinguishable from an absent one.

use epigraph_db::Viewer;
use uuid::Uuid;

use crate::errors::DbError;

pub struct KernelClaimRepository;

impl KernelClaimRepository {
    /// The content of claim `id` if `viewer` can read it; `None` when the
    /// claim is absent OR invisible to `viewer`.
    pub async fn content_as<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        viewer: &Viewer,
        id: Uuid,
    ) -> Result<Option<String>, DbError> {
        let sql = viewer.splice(
            "SELECT c.content FROM claims c WHERE c.id = $1 /* {VISIBILITY:c} */",
            2,
        );
        let mut q = sqlx::query_scalar::<_, String>(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_optional(executor).await?)
    }

    /// Claim `id`'s content and ownership pair `(content, visibility,
    /// owner_group_id)` if `viewer` can read it; `None` when absent OR
    /// invisible.
    pub async fn content_and_pair_as<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        viewer: &Viewer,
        id: Uuid,
    ) -> Result<Option<(String, String, Uuid)>, DbError> {
        let sql = viewer.splice(
            "SELECT c.content, c.visibility::text, c.owner_group_id FROM claims c \
              WHERE c.id = $1 /* {VISIBILITY:c} */",
            2,
        );
        let mut q = sqlx::query_as::<_, (String, String, Uuid)>(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_optional(executor).await?)
    }

    /// The registered Ed25519 SIGNING key of agent `id` (kernel
    /// `agents.public_key`), or `None` when there is no such agent or its key
    /// is not a signing key. The kernel's rule for every signature path
    /// (`agents.key_kind` comment; `AgentRepository::public_key_if_signer`):
    /// only `key_kind = 'ed25519'` is a verifier; a `derived` key is a
    /// placeholder for a keyless OAuth principal that no one holds. A
    /// countersignature's `signer_id` is proven by a signature that verifies
    /// against THIS key, never a key the request supplies.
    pub async fn agent_public_key<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
    ) -> Result<Option<Vec<u8>>, DbError> {
        Ok(sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT public_key FROM agents WHERE id = $1 AND key_kind = 'ed25519'",
        )
        .bind(id)
        .fetch_optional(executor)
        .await?)
    }
}
