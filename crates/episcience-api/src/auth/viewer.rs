//! The caller's read authority for kernel reads EpiScience issues itself.
//!
//! `Viewer::resolve` is the kernel's own resolution (one read of the
//! principal's live group memberships), the same call the kernel API makes
//! for a bearer. The resulting viewer is spliced into every EpiScience
//! statement that reads kernel `claims`.

use epigraph_db::Viewer;
use sqlx::PgPool;

use crate::errors::ApiError;
use crate::middleware::AuthContext;

/// Resolve the authenticated caller's viewer. A resolution failure is a
/// server error: the request is refused rather than served unfiltered.
pub async fn caller_viewer(pool: &PgPool, auth: &AuthContext) -> Result<Viewer, ApiError> {
    Viewer::resolve(pool, auth.agent_id)
        .await
        .map_err(|e| ApiError::Internal(format!("resolve caller read authority: {e}")))
}
