//! `episcience-mcp-server` — stdio MCP server exposing the synthesis pipeline.
//!
//! Phase 3 Tasks 3.6 / 3.7 / 3.8. Mirrors the dependency-wiring shape of
//! `bin/server.rs` (the REST server) but lighter: no JWT middleware, no
//! axum, no JobRunner — just a database pool, an embedder, and an edge
//! writer client, then `serve(stdio)`.
//!
//! Usage:
//!
//! ```bash
//! DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//! EPIGRAPH_SERVICE_AGENT_ID=<uuid> \
//!   ./target/debug/episcience-mcp-server
//! ```

use std::sync::Arc;

use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider, OpenAiProvider};
use episcience_api::clients::epigraph_edges::EpigraphEdgesClient;
use episcience_api::mcp::{EpiscienceServer, DEFAULT_MAX_UPLOAD_BYTES};
use episcience_db::EdgeWriter;
use rmcp::ServiceExt;

const SYNTHESIS_EMBEDDING_DIM: usize = 1536;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Logging to stderr — stdout is reserved for MCP JSON-RPC.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "episcience_api=info,episcience_mcp=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    tracing::info!("Connecting to PostgreSQL...");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    tracing::info!("PostgreSQL connected");

    // ── Embedder ─────────────────────────────────────────────────────────────
    //
    // Same selection logic as `bin/server.rs`: opt-in to OpenAi only with
    // explicit env var + key, else fall back to MockProvider so a dev smoke
    // run never silently fails on a missing API key.
    let embed_mode = std::env::var("EPISCIENCE_EMBED_MODE").unwrap_or_default();
    let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let embedder: Arc<dyn EmbeddingService> = match (embed_mode.as_str(), openai_key.as_str()) {
        ("openai", key) if !key.is_empty() => {
            let cfg = EmbeddingConfig::openai(SYNTHESIS_EMBEDDING_DIM);
            match OpenAiProvider::new(cfg, key.to_string()) {
                Ok(p) => {
                    tracing::info!(
                        dim = SYNTHESIS_EMBEDDING_DIM,
                        "Using OpenAiProvider for synthesis embeddings",
                    );
                    Arc::new(p)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "OpenAiProvider init failed; falling back to Mock");
                    Arc::new(MockProvider::new(EmbeddingConfig::openai(
                        SYNTHESIS_EMBEDDING_DIM,
                    )))
                }
            }
        }
        _ => {
            tracing::info!(
                dim = SYNTHESIS_EMBEDDING_DIM,
                "Using MockProvider for synthesis embeddings (set EPISCIENCE_EMBED_MODE=openai + OPENAI_API_KEY for real embeddings)"
            );
            Arc::new(MockProvider::new(EmbeddingConfig::openai(
                SYNTHESIS_EMBEDDING_DIM,
            )))
        }
    };

    // ── Edge writer ──────────────────────────────────────────────────────────
    let epigraph_url =
        std::env::var("EPIGRAPH_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
    let service_token = std::env::var("EPIGRAPH_SERVICE_TOKEN").unwrap_or_default();
    if service_token.is_empty() {
        tracing::warn!(
            "EPIGRAPH_SERVICE_TOKEN not set — edge writes to {} will fail with 401",
            epigraph_url
        );
    }
    let edge_writer: Arc<dyn EdgeWriter> = Arc::new(EpigraphEdgesClient::new(
        epigraph_url.clone(),
        service_token,
    ));

    // ── Auth agent ───────────────────────────────────────────────────────────
    //
    // v1 service-mode: the agent_id is the same for every tool call. Future
    // work pulls this from a per-call MCP auth header.
    let auth_agent_id = std::env::var("EPIGRAPH_SERVICE_AGENT_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            tracing::warn!(
                "EPIGRAPH_SERVICE_AGENT_ID not set — using nil UUID; \
                 syntheses will be created under the nil agent and edge writes will fail"
            );
            uuid::Uuid::nil()
        });
    tracing::info!(%auth_agent_id, "MCP auth agent");

    // ── Blob storage + upload cap (mirror bin/server.rs) ────────────────────
    let blob_dir = std::env::var("EPISCIENCE_BLOB_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/episcience/blobs"));
    tokio::fs::create_dir_all(&blob_dir).await?;
    tracing::info!("Blob storage: {}", blob_dir.display());

    let max_upload_bytes: usize = std::env::var("EPISCIENCE_MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_UPLOAD_BYTES);
    tracing::info!(max_upload_bytes, "attach_blob payload cap");

    // ── Build server + serve over stdio ──────────────────────────────────────
    let server = EpiscienceServer::new(
        pool,
        embedder,
        edge_writer,
        auth_agent_id,
        blob_dir,
        max_upload_bytes,
    );
    tracing::info!("episcience-mcp-server starting on stdio (8 tools)");
    let service = server.serve(rmcp::transport::stdio()).await.map_err(|e| {
        tracing::error!("MCP serve error: {e}");
        e
    })?;
    service.waiting().await?;
    Ok(())
}
