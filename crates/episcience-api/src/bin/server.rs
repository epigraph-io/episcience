use std::sync::Arc;

use epigraph_cli::enrichment::llm_client::{AnthropicClient, LlmClient, MockLlmClient};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider, OpenAiProvider};
use epigraph_jobs::{JobQueue, JobRunner};
use episcience_api::clients::epigraph_edges::EpigraphEdgesClient;
use episcience_api::jobs::{
    EmptyEdgeProvider, EpiscienceJobQueue, SynthesisJobHandler,
};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_db::EdgeWriter;
use tracing_subscriber::EnvFilter;

const DEV_JWT_SECRET: &[u8] = b"dev-only-insecure-secret-change-in-production";

/// Embedding dimension used by the synthesis pipeline.
///
/// `synthesis_embeddings.embedding` is `vector(1536)` (migration 5013), and the
/// upstream EpiGraph claim embeddings are also 1536 (text-embedding-3-small).
/// Both providers configured here must produce 1536-dim vectors.
const SYNTHESIS_EMBEDDING_DIM: usize = 1536;

/// Embedding model name written to `synthesis_embeddings.embedding_model`.
const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    tracing::info!("Connecting to PostgreSQL...");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("Failed to connect to database");
    tracing::info!("PostgreSQL connected");

    tracing::info!("Skipping embedded migrations (applied externally)");

    let blob_dir = std::env::var("EPISCIENCE_BLOB_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/episcience/blobs"));
    tokio::fs::create_dir_all(&blob_dir)
        .await
        .expect("Failed to create blob directory");
    tracing::info!("Blob storage: {}", blob_dir.display());

    let jwt_secret = std::env::var("EPIGRAPH_JWT_SECRET")
        .map(|s| s.into_bytes())
        .unwrap_or_else(|_| {
            tracing::warn!("EPIGRAPH_JWT_SECRET not set — using insecure dev secret");
            DEV_JWT_SECRET.to_vec()
        });

    let max_upload_bytes: usize = std::env::var("EPISCIENCE_MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(104_857_600); // 100 MB

    let jwt_config = Arc::new(JwtConfig::from_secret(&jwt_secret));

    let state = ElnState {
        pool: pool.clone(),
        blob_dir,
        jwt_config,
        max_upload_bytes,
    };

    // ─── Synthesis worker bootstrap ───────────────────────────────────────────
    //
    // Build dependencies for the SynthesisJobHandler, run the Stage 6
    // reconciliation pass once, then spawn the JobRunner. The worker reads
    // and writes the same `synthesis_jobs` / `syntheses` tables that the API
    // routes will eventually enqueue against (Phase 3).

    let epigraph_url = std::env::var("EPIGRAPH_API_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
    let service_token = std::env::var("EPIGRAPH_SERVICE_TOKEN").unwrap_or_default();
    let cost_budget: u32 = std::env::var("EPISCIENCE_COST_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let worker_count: usize = std::env::var("SYNTHESIS_WORKER_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    if service_token.is_empty() {
        tracing::warn!(
            "EPIGRAPH_SERVICE_TOKEN not set — synthesis edge writes to {} will fail \
             with 401 until configured",
            epigraph_url
        );
    }

    // ─── LLM client ───────────────────────────────────────────────────────────
    //
    // Default to MockLlmClient unless explicitly opted into Anthropic AND an
    // API key is present. Mock errors are loud and deterministic, which beats
    // a misconfigured production client silently rotating retries.
    let llm_mode = std::env::var("EPISCIENCE_LLM_MODE").unwrap_or_default();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let llm: Arc<dyn LlmClient + Send + Sync> = match (llm_mode.as_str(), anthropic_key.as_str()) {
        ("anthropic", key) if !key.is_empty() => {
            let model = std::env::var("ANTHROPIC_MODEL").ok();
            match AnthropicClient::new(key.to_string(), model.clone()) {
                Ok(c) => {
                    tracing::info!(
                        model = %model.unwrap_or_else(|| "<default>".to_string()),
                        "Using AnthropicClient for synthesis LLM",
                    );
                    Arc::new(c)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "AnthropicClient init failed; falling back to MockLlmClient",
                    );
                    Arc::new(MockLlmClient::new())
                }
            }
        }
        _ => {
            tracing::info!(
                "Using MockLlmClient for synthesis LLM \
                 (set EPISCIENCE_LLM_MODE=anthropic + ANTHROPIC_API_KEY for real LLM)"
            );
            Arc::new(MockLlmClient::new())
        }
    };

    // ─── Embedder ─────────────────────────────────────────────────────────────
    //
    // OpenAiProvider only does live API calls when the `openai` feature is
    // enabled in epigraph-embeddings. With the feature off, `generate_query`
    // returns ConfigError on the first call. The handler tolerates that
    // (Stage 2 prunes all neighbours), but for a dev smoke run it's noisy —
    // default to MockProvider unless explicitly opted in AND an API key is
    // present.
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
                    tracing::warn!(
                        error = %e,
                        "OpenAiProvider init failed; falling back to MockProvider",
                    );
                    Arc::new(MockProvider::new(EmbeddingConfig::openai(
                        SYNTHESIS_EMBEDDING_DIM,
                    )))
                }
            }
        }
        _ => {
            tracing::info!(
                dim = SYNTHESIS_EMBEDDING_DIM,
                "Using MockProvider for synthesis embeddings \
                 (set EPISCIENCE_EMBED_MODE=openai + OPENAI_API_KEY for real embeddings)"
            );
            Arc::new(MockProvider::new(EmbeddingConfig::openai(
                SYNTHESIS_EMBEDDING_DIM,
            )))
        }
    };

    let embedding_model =
        std::env::var("EPISCIENCE_EMBEDDING_MODEL").unwrap_or_else(|_| DEFAULT_EMBEDDING_MODEL.to_string());

    // ─── Edge writer ──────────────────────────────────────────────────────────
    //
    // Construct once as `Arc<dyn EdgeWriter>` so:
    //  - `reconcile_stage6_on_startup` gets `&dyn EdgeWriter` via `as_ref()`.
    //  - `SynthesisJobHandler` gets a clonable `Arc<dyn EdgeWriter>`.
    let edges_writer: Arc<dyn EdgeWriter> = Arc::new(EpigraphEdgesClient::new(
        epigraph_url.clone(),
        service_token.clone(),
    ));

    // ─── Edge provider (Phase 2 v1 stub) ──────────────────────────────────────
    let edge_provider = Arc::new(EmptyEdgeProvider);

    // ─── Job queue ────────────────────────────────────────────────────────────
    let queue: Arc<dyn JobQueue> = Arc::new(EpiscienceJobQueue::new(pool.clone()));

    // ─── Reconciliation pass ──────────────────────────────────────────────────
    //
    // Drains any `complete` syntheses that crashed mid-Stage-6 with provo
    // edges still unwritten. Run this before workers start so we don't race
    // an in-flight job against a reconciliation pass for the same synthesis.
    // A failure here is logged but non-fatal — dependent syntheses retry on
    // the next worker poll.
    tracing::info!("Running stage-6 reconciliation pass...");
    match episcience_db::synthesis::publish::reconcile_stage6_on_startup(
        &pool,
        edges_writer.as_ref(),
    )
    .await
    {
        Ok(()) => tracing::info!("Stage-6 reconciliation OK"),
        Err(e) => tracing::error!(
            error = %e,
            "Stage-6 reconciliation failed (continuing — dependent syntheses will retry)",
        ),
    }

    // ─── Build & start the runner ─────────────────────────────────────────────
    //
    // `JobRunner::start(&mut self)` spawns `worker_count` tasks internally
    // and returns immediately. We keep `job_runner` on the main task so we
    // can call `shutdown()` on ctrl_c.
    let handler = Arc::new(SynthesisJobHandler::new(
        pool.clone(),
        embedder,
        llm,
        edges_writer,
        edge_provider,
        cost_budget,
        embedding_model,
    ));

    let mut job_runner = JobRunner::new(worker_count, queue);
    job_runner.register_handler(handler);
    job_runner.start().await;
    tracing::info!(
        worker_count,
        cost_budget,
        "Synthesis job runner started",
    );

    // ─── HTTP server ──────────────────────────────────────────────────────────
    let app = episcience_api::create_router(state);

    let port: u16 = std::env::var("EPISCIENCE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8081);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("EpiScience ELN server listening on {}", addr);
    tracing::info!("Health check: http://127.0.0.1:{}/health", port);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind");

    // Graceful shutdown: on ctrl_c, drain in-flight synthesis jobs before
    // releasing the listener.
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("ctrl_c received — draining in-flight synthesis jobs...");
            job_runner.shutdown().await;
            tracing::info!("Synthesis job runner shut down");
        })
        .await
        .expect("Server error");
}
