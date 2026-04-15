use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

use crate::middleware::JwtConfig;

/// ELN application state — wraps the shared database pool and blob storage dir.
#[derive(Clone)]
pub struct ElnState {
    pub pool: PgPool,
    pub blob_dir: PathBuf,
    pub jwt_config: Arc<JwtConfig>,
    pub max_upload_bytes: usize,
}
