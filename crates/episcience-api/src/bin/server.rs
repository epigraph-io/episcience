//! `episcience-server`: the REST API, on the `episcience_app` application
//! login (E1g).
//!
//! Reads `DATABASE_URL` from its unit environment (it loads no `.env` file:
//! the checkout's holds a superuser DSN) and refuses to start when a retired
//! service-client variable is set (`config::RETIRED_SERVICE_VARS`), when a
//! privileged or another login's DSN variable is set
//! (`config::REQUEST_FORBIDDEN_VARS`), or when the connected session is
//! privileged or switched (`EpiscienceDb::connect`). Every request runs on a
//! session stamped as its caller. `episcience-worker` runs the synthesis
//! queue; this server runs no synthesis.

use std::sync::Arc;

use epigraph_db::SessionGucMode;
use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use episcience_db::tenancy::{EpiscienceDb, EpiscienceDbOptions};
use tracing_subscriber::EnvFilter;

fn refuse(msg: impl std::fmt::Display) -> ! {
    eprintln!("ERROR: {msg}");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // ─── Boot refusals (before any database or network I/O) ────────────────
    if let Err(e) = episcience_api::config::refuse_retired_service_vars(
        "episcience-server",
        episcience_api::config::env_value,
    ) {
        refuse(e);
    }
    // The retired in-process runner: a unit that ASKS for it (1/true/on, or
    // an unknown value) is refused; off or unset passes (a set, off value is
    // warned about below as a harmless leftover).
    if let Err(e) = episcience_api::config::inprocess_worker_off(
        episcience_api::config::env_value(episcience_api::config::INPROCESS_WORKER_VAR).as_deref(),
    ) {
        refuse(e);
    }
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

    let database_url = episcience_api::config::request_database_url(
        "episcience-server",
        episcience_api::config::env_value,
    )
    .unwrap_or_else(|e| refuse(e));

    // Harmless leftovers (the in-process runner's switch, set to off, and the
    // retired client's endpoint): acted on by nothing; warn once so a stale
    // unit environment is noticed.
    for retired in
        episcience_api::config::retired_harmless_vars_set(episcience_api::config::env_value)
    {
        tracing::warn!(
            "{retired} is set but ignored: nothing reads it (remove it from the unit environment)"
        );
    }

    // ─── The application login: boot refusals, then the stamped pool ──────────
    //
    // `EpiscienceDb::connect` refuses a privileged or switched session, a
    // database whose kernel no longer provides the tenancy contract, one whose
    // EpiScience schema predates the tenancy columns, and a session-GUC probe
    // failure (docs/tenancy-contract.md).
    tracing::info!("Connecting to PostgreSQL (application login)...");
    let mode =
        SessionGucMode::from_env(&std::env::var("EPIGRAPH_SESSION_GUC_MODE").unwrap_or_default());
    let db = EpiscienceDb::connect(
        &database_url,
        EpiscienceDbOptions {
            application_name: "episcience-server",
            max_connections: 10,
            mode,
        },
    )
    .await
    .unwrap_or_else(|e| refuse(e));
    tracing::info!(
        "tenancy contract v{} probe OK; EpiScience schema probe OK; request sessions are stamped",
        episcience_db::tenancy_contract::CONTRACT_VERSION
    );

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

    // The embedder `POST /syntheses/search` embeds queries with (the worker's
    // provider/model, so scores are comparable).
    let embedder = episcience_api::providers::embedder_from_env();

    // ─── ElnState: the stamped handle is the only database access ────────────
    let state = ElnState {
        db,
        blob_dir,
        jwt_config,
        max_upload_bytes,
        embedder,
    };

    // ─── HTTP server ──────────────────────────────────────────────────────────
    let app = episcience_api::create_router(state);

    tracing::info!("EpiScience ELN server listening on {}", addr);
    tracing::info!("Health check: http://{}/health", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("Server error");
}
