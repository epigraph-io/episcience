use sqlx::PgPool;
use std::path::PathBuf;

/// ELN application state — wraps the shared database pool and blob storage dir.
///
/// Phase 0: direct pool access. Future phases will wrap EpiGraph's full
/// AppState for auth middleware reuse.
#[derive(Clone)]
pub struct ElnState {
    pub pool: PgPool,
    pub blob_dir: PathBuf,
}
