use epigraph_embeddings::EmbeddingService;
use episcience_db::tenancy::EpiscienceDb;
use std::path::PathBuf;
use std::sync::Arc;

use crate::middleware::JwtConfig;

/// ELN application state: the request path's database handle and blob
/// storage.
///
/// `db` is the ONLY database access a handler has: stamped reads
/// (`read_as`) and writes (`write_as`) on the `episcience_app` application
/// login; there is no raw pool here.
///
/// `embedder` is the same provider/model the synthesis worker embeds with, so
/// query embeddings produced by REST routes (e.g. `POST /syntheses/search`)
/// score against the stored ones.
#[derive(Clone)]
pub struct ElnState {
    pub db: EpiscienceDb,
    pub blob_dir: PathBuf,
    pub jwt_config: Arc<JwtConfig>,
    pub max_upload_bytes: usize,
    pub embedder: Arc<dyn EmbeddingService>,
}
