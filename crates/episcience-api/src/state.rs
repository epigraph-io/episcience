use epigraph_embeddings::EmbeddingService;
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

use crate::middleware::JwtConfig;

/// ELN application state — wraps the shared database pool and blob storage dir.
///
/// `embedder` is shared with the synthesis worker (`SynthesisJobHandler`) so
/// query embeddings produced by REST routes (e.g. `POST /syntheses/search`)
/// use the same provider/model as the embeddings stored at synthesis time.
#[derive(Clone)]
pub struct ElnState {
    pub pool: PgPool,
    pub blob_dir: PathBuf,
    pub jwt_config: Arc<JwtConfig>,
    pub max_upload_bytes: usize,
    pub embedder: Arc<dyn EmbeddingService>,
}
