//! `episcience-worker`: the synthesis worker on its own application login
//! (E1f).
//!
//! Reads ONLY `EPISCIENCE_WORKER_DATABASE_URL` (the `episcience_worker`
//! login: a member of `epigraph_app`, `episcience_rw` and `episcience_queue`)
//! and refuses to start when a retired service-client variable
//! (`config::RETIRED_SERVICE_VARS`) or a privileged or another login's DSN
//! variable (`config::WORKER_FORBIDDEN_VARS`, `DATABASE_URL` included) is
//! set. It never loads a `.env` file (the checkout's holds the superuser
//! DSN).
//!
//! Three pools on that one DSN, all tagged `application_name=episcience-worker`:
//! - the stamped `ScopedPool` every stage transaction comes from;
//! - `RESOLVE_POOL` (unstamped): `Viewer::resolve`, the operator-link parity
//!   check and the queue/worklist definers;
//! - `ENGINE_POOL` (unstamped): the kernel engine's recall and belief lookups
//!   (`V1-engine-takes-pool`, until KE-1). The novelty reads run on the
//!   stamped stage transaction, not here.
//!
//! Boot, in order: the variable refusals; connect; refuse a privileged or
//! switched session (a role switch, or a superuser, BYPASSRLS or
//! kernel-maintenance role reachable from the login); the session-GUC probe;
//! the tenancy contract probe and the schema probe; then the loop.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use epigraph_db::{ScopedPool, ScopedPoolOptions, SessionGucMode};
use episcience_api::jobs::worker::Worker;
use episcience_api::jobs::{EmptyEdgeProvider, SynthesisJobHandler};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tracing_subscriber::EnvFilter;

const APPLICATION_NAME: &str = "episcience-worker";

fn refuse(msg: impl std::fmt::Display) -> ! {
    eprintln!("ERROR: {msg}");
    std::process::exit(2);
}

/// `url` with `application_name` set (unless the DSN already names one).
fn with_application_name(url: &str) -> String {
    if url.contains("application_name=") {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&application_name={APPLICATION_NAME}")
    } else {
        format!("{url}?application_name={APPLICATION_NAME}")
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // ─── Boot refusals (before any database I/O) ────────────────────────────
    if let Err(e) = episcience_api::config::refuse_retired_service_vars(
        "episcience-worker",
        episcience_api::config::env_value,
    ) {
        refuse(e);
    }
    let url = match episcience_api::config::worker_database_url(episcience_api::config::env_value) {
        Ok(u) => with_application_name(&u),
        Err(e) => refuse(e),
    };
    let mode =
        SessionGucMode::from_env(&std::env::var("EPIGRAPH_SESSION_GUC_MODE").unwrap_or_default());
    let opts = match PgConnectOptions::from_str(&url) {
        Ok(o) => o,
        Err(e) => refuse(format!(
            "{} is not a valid DSN: {e}",
            episcience_api::config::WORKER_DATABASE_URL_VAR
        )),
    };

    tracing::info!("Connecting to PostgreSQL (worker login)...");
    let plain = |max: u32| {
        PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts.clone())
    };
    let resolve_pool = plain(3)
        .await
        .unwrap_or_else(|e| refuse(format!("connect RESOLVE_POOL: {e}")));
    if let Err(e) = episcience_db::tenancy_contract::refuse_privileged_session(&resolve_pool).await
    {
        refuse(e);
    }
    let engine_pool = plain(3)
        .await
        .unwrap_or_else(|e| refuse(format!("connect ENGINE_POOL: {e}")));
    let scoped = ScopedPool::connect_with_options(
        &url,
        mode,
        ScopedPoolOptions {
            max_connections: 5,
            ..ScopedPoolOptions::default()
        },
    )
    .await
    .unwrap_or_else(|e| refuse(format!("connect the stamped pool: {e}")));
    if let Err(e) = scoped.probe_session_gucs().await {
        refuse(e);
    }

    match episcience_db::tenancy_contract::probe(&resolve_pool).await {
        Ok(report) => tracing::info!(
            checked = ?report.checked,
            asserted_by_migrations = ?report.skipped,
            "tenancy contract v{} probe OK",
            episcience_db::tenancy_contract::CONTRACT_VERSION
        ),
        Err(e) => refuse(e),
    }
    if let Err(e) = episcience_db::tenancy_contract::probe_schema(&resolve_pool).await {
        refuse(e);
    }
    tracing::info!("EpiScience schema probe OK (tenancy columns present)");

    // ─── The handler (engine reads on ENGINE_POOL) ──────────────────────────
    let cost_budget: u32 = std::env::var("EPISCIENCE_COST_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let handler = SynthesisJobHandler::new(
        engine_pool,
        episcience_api::providers::embedder_from_env(),
        episcience_api::providers::llm_from_env(),
        Arc::new(EmptyEdgeProvider),
        cost_budget,
        episcience_api::providers::embedding_model_from_env(),
        true,
    );
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".into());
    let worker = Worker::new(
        format!("{APPLICATION_NAME}@{host}/{}", std::process::id()),
        Arc::new(scoped),
        resolve_pool,
        handler,
        Duration::from_secs(30),
    );
    tracing::info!(name = %worker.name, cost_budget, "episcience-worker started");

    // Stop BETWEEN jobs on SIGTERM (systemd's stop) or SIGINT: a job in
    // flight runs to one of its ends first (the unit's TimeoutStopSec must
    // cover one synthesis).
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => refuse(format!("install the SIGTERM handler: {e}")),
            };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        tracing::info!("stop requested; finishing the job in flight");
        let _ = stop_tx.send(true);
    });
    worker.run_until(Duration::from_secs(2), stop_rx).await;
}
