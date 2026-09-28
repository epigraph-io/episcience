use std::sync::Arc;

use epigraph_jobs::{JobQueue, JobRunner};
use episcience_api::jobs::{EmptyEdgeProvider, EpiscienceJobQueue, SynthesisJobHandler};
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // ─── Boot refusals (before any database or network I/O) ────────────────
    let jwt_secret = match episcience_api::config::require_jwt_secret(
        std::env::var(episcience_api::config::JWT_SECRET_VAR).ok(),
    ) {
        Ok(secret) => secret,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(2);
        }
    };

    let addr = match episcience_api::config::rest_bind_addr(
        std::env::var(episcience_api::config::BIND_ADDR_VAR)
            .ok()
            .as_deref(),
        std::env::var(episcience_api::config::PORT_VAR)
            .ok()
            .as_deref(),
    ) {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(2);
        }
    };

    let inprocess_worker = match episcience_api::config::inprocess_worker_enabled(
        episcience_api::config::env_value(episcience_api::config::INPROCESS_WORKER_VAR).as_deref(),
    ) {
        Ok(on) => on,
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(2);
        }
    };

    // The retired service client (E1f): nothing reads these any more. Kernel
    // PROV edges and events are written in process on the synthesis owner's
    // transaction. Warn once so a stale unit environment is noticed.
    for retired in [
        "EPIGRAPH_CLIENT_ID",
        "EPIGRAPH_CLIENT_SECRET",
        "EPIGRAPH_SERVICE_TOKEN",
    ] {
        if std::env::var_os(retired).is_some() {
            tracing::warn!(
                "{retired} is set but ignored: the service client is retired (remove it from \
                 the unit environment)"
            );
        }
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    tracing::info!("Connecting to PostgreSQL...");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("Failed to connect to database");
    tracing::info!("PostgreSQL connected");

    // ─── Tenancy contract probe (before any read or write) ─────────────────
    //
    // Refuse to serve on a database whose kernel no longer provides the
    // objects EpiScience relies on (docs/tenancy-contract.md): a missing grant
    // there fails silently at run time.
    match episcience_db::tenancy_contract::probe(&pool).await {
        Ok(report) => tracing::info!(
            checked = ?report.checked,
            asserted_by_migrations = ?report.skipped,
            "tenancy contract v{} probe OK",
            episcience_db::tenancy_contract::CONTRACT_VERSION
        ),
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(2);
        }
    }
    // The EpiScience schema this binary writes (the tenancy columns).
    if let Err(e) = episcience_db::tenancy_contract::probe_schema(&pool).await {
        eprintln!("ERROR: {e}");
        std::process::exit(2);
    }
    tracing::info!("EpiScience schema probe OK (tenancy columns present)");

    tracing::info!("Skipping embedded migrations (applied externally)");

    let blob_dir = std::env::var("EPISCIENCE_BLOB_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/episcience/blobs"));
    tokio::fs::create_dir_all(&blob_dir)
        .await
        .expect("Failed to create blob directory");
    tracing::info!("Blob storage: {}", blob_dir.display());

    let max_upload_bytes: usize = std::env::var("EPISCIENCE_MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(104_857_600); // 100 MB

    let jwt_config = Arc::new(JwtConfig::from_secret(&jwt_secret));

    // ─── Synthesis runner configuration ─────────────────────────────────────
    //
    // Used only by the legacy in-process runner (EPISCIENCE_INPROCESS_WORKER,
    // on by default); `episcience-worker` is the E1f runtime.
    let cost_budget: u32 = std::env::var("EPISCIENCE_COST_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let worker_count: usize = std::env::var("SYNTHESIS_WORKER_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let llm = episcience_api::providers::llm_from_env();
    let embedder = episcience_api::providers::embedder_from_env();
    let embedding_model = episcience_api::providers::embedding_model_from_env();

    // ─── ElnState ─────────────────────────────────────────────────────────────
    //
    // Built here (after the embedder) so the same Arc lands in both the HTTP
    // state (for `POST /syntheses/search`) and the SynthesisJobHandler. Cloning
    // an Arc is cheap; holding a single instance keeps the worker's
    // embedding-at-write and the route's embedding-at-read on the same
    // model/config — otherwise cosine scores would drift between providers.
    let state = ElnState {
        pool: pool.clone(),
        blob_dir,
        jwt_config,
        max_upload_bytes,
        embedder: embedder.clone(),
    };

    // ─── Legacy in-process runner (EPISCIENCE_INPROCESS_WORKER) ───────────────
    //
    // On: the stage-6 startup reconcile (in process, on this pool) and the
    // JobRunner, exactly as before E1f except that kernel PROV edges and
    // events are written in process rather than through the retired service
    // client. Off (the E1f deploy): `episcience-worker` owns the queue, the
    // outbox retries and the staleness rechecks, and this process runs no
    // synthesis stage at all.
    let mut job_runner: Option<JobRunner> = None;
    if inprocess_worker {
        tracing::info!("Running stage-6 reconciliation pass (in process)...");
        match episcience_db::synthesis::publish::reconcile_stage6_inprocess(&pool).await {
            Ok(()) => tracing::info!("Stage-6 reconciliation OK"),
            Err(e) => tracing::error!(
                error = %e,
                "Stage-6 reconciliation failed (continuing — dependent syntheses will retry)",
            ),
        }
        let queue: Arc<dyn JobQueue> = Arc::new(EpiscienceJobQueue::new(pool.clone()));
        let handler = Arc::new(SynthesisJobHandler::new(
            pool.clone(),
            embedder,
            llm,
            Arc::new(EmptyEdgeProvider),
            cost_budget,
            embedding_model,
            true,
        ));
        let mut runner = JobRunner::new(worker_count, queue);
        runner.register_handler(handler);
        runner.start().await;
        tracing::info!(
            worker_count,
            cost_budget,
            "Synthesis job runner started (in process)"
        );
        job_runner = Some(runner);
    } else {
        tracing::info!(
            "{}=0: no in-process synthesis runner (episcience-worker owns the queue)",
            episcience_api::config::INPROCESS_WORKER_VAR
        );
    }

    // ─── HTTP server ──────────────────────────────────────────────────────────
    let app = episcience_api::create_router(state);

    tracing::info!("EpiScience ELN server listening on {}", addr);
    tracing::info!("Health check: http://{}/health", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind");

    // Graceful shutdown: on ctrl_c, drain in-flight synthesis jobs before
    // releasing the listener.
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            if let Some(mut runner) = job_runner {
                tracing::info!("ctrl_c received — draining in-flight synthesis jobs...");
                runner.shutdown().await;
                tracing::info!("Synthesis job runner shut down");
            }
        })
        .await
        .expect("Server error");
}
