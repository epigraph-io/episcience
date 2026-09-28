//! Reads of KERNEL `claims` rows that EpiScience issues itself.
//!
//! The process still connects as a role that bypasses row-level security, so
//! the kernel's visibility rules do not apply by themselves. Every statement
//! here therefore carries the kernel's own `/* {VISIBILITY:c} */` marker and is
//! rendered with `epigraph_db::Viewer::splice`: a caller reads exactly the
//! claims the kernel would show it (public, or owned by one of its groups).
//! A claim the viewer cannot read is indistinguishable from an absent one.

use epigraph_db::Viewer;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::DbError;

pub struct KernelClaimRepository;

impl KernelClaimRepository {
    /// The content of claim `id` if `viewer` can read it; `None` when the
    /// claim is absent OR invisible to `viewer`.
    pub async fn content_as(
        pool: &PgPool,
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
        Ok(q.fetch_optional(pool).await?)
    }
}
