//! `episcience-mcp-server` — MCP server exposing the synthesis pipeline.
//!
//! Phase 3 Tasks 3.6 / 3.7 / 3.8, extended with an optional streamable-HTTP
//! transport (feat/mcp-http-transport). Mirrors the dependency-wiring shape of
//! `bin/server.rs` (the REST server) but lighter: a database pool, an embedder,
//! and an edge writer client.
//!
//! ## Transport selection — `EPISCIENCE_LISTEN`
//!
//! - **unset** (default): stdio transport, unchanged. stdout is reserved for
//!   MCP JSON-RPC; the process boundary is the trust gate.
//! - **`host:port`**: streamable-HTTP over TCP (e.g. loopback so the epigraph
//!   gateway can federate this server).
//! - **`unix:/abs/path`**: streamable-HTTP over a Unix socket (`0o660`, so only
//!   processes with filesystem access can connect).
//!
//! The process refuses to start without `EPIGRAPH_JWT_SECRET` (Bearer auth
//! against the same HMAC secret episcience REST validates with) unless the
//! development opt-out `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP=1` is set; the
//! two are mutually exclusive.
//!
//! Identity: every tool acts as the authenticated caller (the bearer's
//! `agent_id`); see `episcience_api::mcp`. A stdio session, or an HTTP session
//! under the development opt-out, has no caller: it can initialize and list
//! tools, and every `tools/call` is refused.
//!
//! Usage:
//!
//! ```bash
//! # streamable-HTTP over loopback TCP with Bearer auth
//! DATABASE_URL=... EPISCIENCE_LISTEN=127.0.0.1:8093 \
//! EPIGRAPH_JWT_SECRET=<shared HMAC secret> \
//!   ./target/debug/episcience-mcp-server
//! ```

use std::sync::Arc;

use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider, OpenAiProvider};
use episcience_api::clients::epigraph_edges::EpigraphEdgesClient;
use episcience_api::clients::service_token::ServiceToken;
use episcience_api::mcp::http::McpHttpAuth;
use episcience_api::mcp::{EpiscienceServer, DEFAULT_MAX_UPLOAD_BYTES};
use episcience_api::middleware::JwtConfig;
use episcience_db::EdgeWriter;
use rmcp::ServiceExt;

const SYNTHESIS_EMBEDDING_DIM: usize = 1536;

/// Bind `router` on the `EPISCIENCE_LISTEN` spec and serve it.
///
/// `unix:/abs/path` → a `UnixListener` chmod'd to `0o660` (a stale socket file
/// from a previous run is removed first); anything else → a `TcpListener`.
/// Mirrors epigraph-mcp's `serve_with_listener`. Inlined in the bin (rather than
/// a lib fn) because there is no unit test exercising the listener split here.
///
/// The TCP branch uses `axum::serve`. The unix branch is hand-rolled with a
/// hyper-util auto accept loop because axum 0.7's `serve` is hardcoded to
/// `TcpListener` (the generic `Listener` trait only arrived in axum 0.8, and
/// bumping the whole crate to 0.8 would cascade through the REST routes,
/// axum-extra, and axum-test — out of scope for adding a transport).
async fn serve_with_listener(listen: &str, router: axum::Router) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(path) = listen.strip_prefix("unix:") {
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use hyper_util::server::conn::auto::Builder;
        use hyper_util::service::TowerToHyperService;
        use std::os::unix::fs::PermissionsExt;

        // Best-effort cleanup of a stale socket from a prior run. AF_UNIX has no
        // SO_REUSEADDR; we rely on a single-instance systemd unit to avoid the
        // concurrent-bind race (see epigraph-mcp lib.rs for the full note).
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
        tracing::info!("episcience-mcp-server listening on unix:{path} (HTTP path: /mcp)");

        loop {
            let (socket, _addr) = listener.accept().await?;
            // Clone the router per-connection; the streamable-HTTP GET stream is
            // long-lived SSE, so each connection needs its own service instance
            // and upgrade-aware serving.
            let service = TowerToHyperService::new(router.clone());
            tokio::spawn(async move {
                if let Err(e) = Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(TokioIo::new(socket), service)
                    .await
                {
                    tracing::warn!("unix MCP connection error: {e}");
                }
            });
        }
    }

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("episcience-mcp-server listening on http://{listen}/mcp");
    axum::serve(listener, router).await
}

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

    // ── Transport selection + auth boot gate ─────────────────────────────────
    //
    // `EPISCIENCE_LISTEN` unset → stdio. Set → streamable HTTP. Either way the
    // process refuses to start without `EPIGRAPH_JWT_SECRET` unless the
    // explicit development opt-out `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` is
    // set (and the two are mutually exclusive). Checked before touching
    // Postgres so a misconfiguration surfaces at boot.
    let listen = std::env::var(episcience_api::config::LISTEN_VAR)
        .ok()
        .filter(|s| !s.is_empty());
    let jwt_secret = std::env::var(episcience_api::config::JWT_SECRET_VAR)
        .ok()
        .filter(|s| !s.is_empty());
    let allow_unauth = matches!(
        std::env::var(episcience_api::config::ALLOW_UNAUTHENTICATED_VAR).as_deref(),
        Ok("1" | "true" | "TRUE")
    );

    match (jwt_secret.is_some(), allow_unauth) {
        (true, false) => {
            // Same strength rule as the REST server (`config::require_jwt_secret`).
            if let Err(e) = episcience_api::config::require_jwt_secret(jwt_secret.clone()) {
                eprintln!("ERROR: {e}");
                std::process::exit(1);
            }
        }
        (false, true) => {} // development opt-out, no token verification
        (true, true) => {
            eprintln!(
                "ERROR: EPIGRAPH_JWT_SECRET and EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP are \
                 mutually exclusive — set exactly one."
            );
            std::process::exit(1);
        }
        (false, false) => {
            eprintln!(
                "ERROR: {} must be set: EpiScience verifies kernel-minted access tokens with \
                 it and has no development fallback (local development only: \
                 EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP=1).",
                episcience_api::config::JWT_SECRET_VAR
            );
            std::process::exit(1);
        }
    }

    // Listener exposure: no wildcard bind, and the development opt-out only on
    // loopback or a unix socket (`config::mcp_listen_guard`).
    if let Some(listen) = listen.as_deref() {
        if let Err(e) = episcience_api::config::mcp_listen_guard(listen, allow_unauth) {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    tracing::info!("Connecting to PostgreSQL...");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;
    tracing::info!("PostgreSQL connected");

    // ── Tenancy contract probe (before any tool can run) ─────────────────────
    //
    // Same refusal as the REST server (docs/tenancy-contract.md).
    match episcience_db::tenancy_contract::probe(&pool).await {
        Ok(report) => tracing::info!(
            checked = ?report.checked,
            asserted_by_migrations = ?report.skipped,
            "tenancy contract v{} probe OK",
            episcience_db::tenancy_contract::CONTRACT_VERSION
        ),
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }

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

    // ── Edge writer (auto-refreshing service credential; mirrors bin/server.rs) ──
    //
    // Preference order, identical to the REST server so a long-lived MCP process
    // keeps a valid token:
    //  1. EPIGRAPH_CLIENT_ID + EPIGRAPH_CLIENT_SECRET → auto-refreshing OAuth
    //     client_credentials token. EpiGraph service tokens are 1h TTL and this
    //     process runs for days, so a static token would 401 an hour after boot;
    //     the provider re-mints transparently ahead of expiry.
    //  2. EPIGRAPH_SERVICE_TOKEN → a fixed bearer with no refresh (legacy; 401s
    //     once it expires).
    //  3. Neither → edge writes 401; warn loudly.
    let epigraph_url =
        std::env::var("EPIGRAPH_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
    let client_id = std::env::var("EPIGRAPH_CLIENT_ID").unwrap_or_default();
    let client_secret = std::env::var("EPIGRAPH_CLIENT_SECRET").unwrap_or_default();
    let static_token = std::env::var("EPIGRAPH_SERVICE_TOKEN").unwrap_or_default();
    let epigraph_token: Arc<ServiceToken> = if !client_id.is_empty() && !client_secret.is_empty() {
        tracing::info!(
            "EpiGraph auth: auto-refreshing OAuth service token (client_credentials, \
             scope 'edges:write edges:read')"
        );
        ServiceToken::oauth(
            epigraph_url.clone(),
            client_id,
            client_secret,
            "edges:write edges:read".to_string(),
        )
    } else if !static_token.is_empty() {
        tracing::warn!(
            "EpiGraph auth: static EPIGRAPH_SERVICE_TOKEN with no auto-refresh — edge writes \
             to {} will 401 once it expires (set EPIGRAPH_CLIENT_ID + EPIGRAPH_CLIENT_SECRET \
             for a self-renewing token)",
            epigraph_url
        );
        ServiceToken::static_token(static_token)
    } else {
        tracing::warn!(
            "EpiGraph auth: no credential (set EPIGRAPH_CLIENT_ID + EPIGRAPH_CLIENT_SECRET, or \
             EPIGRAPH_SERVICE_TOKEN) — edge writes to {} will fail with 401",
            epigraph_url
        );
        ServiceToken::static_token(String::new())
    };
    let edge_writer: Arc<dyn EdgeWriter> = Arc::new(EpigraphEdgesClient::new_with_token(
        epigraph_url.clone(),
        epigraph_token,
    ));

    // ── Retired service identity ─────────────────────────────────────────────
    //
    // Tools act as the authenticated caller; nothing reads a service agent id
    // on the request path any more. Warn once so a stale unit env is noticed.
    if std::env::var_os("EPIGRAPH_SERVICE_AGENT_ID").is_some() {
        tracing::warn!(
            "EPIGRAPH_SERVICE_AGENT_ID is set but ignored: MCP tools act as the \
             authenticated caller (remove it from the unit environment)"
        );
    }

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

    // ── Build server + serve ─────────────────────────────────────────────────
    let server = EpiscienceServer::new(pool, embedder, edge_writer, blob_dir, max_upload_bytes);

    // Captured before the branch: the HTTP arm moves `server` into the
    // per-session factory closure, so it is no longer available at the point
    // the startup line is logged.
    let tool_count = server.tool_count();

    if let Some(listen) = listen.as_deref() {
        // ── Streamable-HTTP transport (TCP or Unix socket) ───────────────────
        // (auth boot gate already enforced above.) The router and its bearer
        // layer come from the library so tests drive this exact stack.
        let auth = match jwt_secret.as_deref() {
            Some(secret) => {
                McpHttpAuth::Bearer(Arc::new(JwtConfig::from_secret(secret.as_bytes())))
            }
            None => McpHttpAuth::Unauthenticated,
        };
        let router = episcience_api::mcp::http::router(server, auth);

        let mode = if jwt_secret.is_some() {
            "Bearer-authenticated"
        } else {
            "UNAUTHENTICATED"
        };
        tracing::info!(
            "episcience-mcp-server starting on {mode} HTTP {listen} ({tool_count} tools)"
        );
        serve_with_listener(listen, router).await?;
    } else {
        // ── Stdio transport (default) ────────────────────────────────────────
        tracing::info!("episcience-mcp-server starting on stdio ({tool_count} tools)");
        let service = server.serve(rmcp::transport::stdio()).await.map_err(|e| {
            tracing::error!("MCP serve error: {e}");
            e
        })?;
        service.waiting().await?;
    }

    Ok(())
}
