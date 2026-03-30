use sqlx::PgPool;

/// ELN application state — wraps the shared database pool.
///
/// Phase 0: direct pool access. Future phases will wrap EpiGraph's full
/// AppState for auth middleware reuse.
#[derive(Clone)]
pub struct ElnState {
    pub pool: PgPool,
}
