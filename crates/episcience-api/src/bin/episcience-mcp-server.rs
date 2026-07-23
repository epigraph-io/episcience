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
//! When `EPISCIENCE_LISTEN` is set, HTTP removes the stdio process boundary, so
//! the operator must pick a trust model: either supply `EPIGRAPH_JWT_SECRET`
//! (Bearer auth against the same HMAC secret episcience REST validates with) or
//! set `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP=1` (e.g. a unix-socket listener
//! behind filesystem permissions, or local dev). Exactly one is required — the
//! boot gate fails fast otherwise.
//!
//! Usage:
//!
//! ```bash
//! # stdio (default)
//! DATABASE_URL=postgres://epigraph:epigraph@localhost:5432/epigraph_dev_synthesis \
//! EPIGRAPH_SERVICE_AGENT_ID=<uuid> \
//!   ./target/debug/episcience-mcp-server
//!
//! # streamable-HTTP over loopback TCP with Bearer auth
//! DATABASE_URL=... EPIGRAPH_SERVICE_AGENT_ID=<uuid> \
//! EPISCIENCE_LISTEN=127.0.0.1:8093 EPIGRAPH_JWT_SECRET=<shared HMAC secret> \
//!   ./target/debug/episcience-mcp-server
//! ```

use std::sync::Arc;

use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider, OpenAiProvider};
use episcience_api::clients::epigraph_edges::EpigraphEdgesClient;
use episcience_api::mcp::{EpiscienceServer, DEFAULT_MAX_UPLOAD_BYTES};
use episcience_api::middleware::JwtConfig;
use episcience_db::EdgeWriter;
use rmcp::ServiceExt;

const SYNTHESIS_EMBEDDING_DIM: usize = 1536;

/// Shared state for the MCP Bearer-auth layer. Holds the `JwtConfig` built from
/// the operator-supplied `EPIGRAPH_JWT_SECRET`. Mirrors epigraph-mcp's
/// `McpAuthState`, minus the `resource_metadata_url` / `WWW-Authenticate`
/// discovery apparatus (out of scope for v1 — a plain 401 suffices).
#[derive(Clone)]
struct McpAuthState {
    jwt_config: Arc<JwtConfig>,
}

/// Axum middleware validating the `Authorization: Bearer <jwt>` header against
/// the SHARED `EPIGRAPH_JWT_SECRET` — the same secret episcience REST validates
/// with (`bin/server.rs`), so a single token works against both surfaces.
///
/// On a missing or invalid token, returns a plain `401 Unauthorized`. On
/// success the request passes through unchanged: episcience MCP tools take
/// identity from the construction-time `auth_agent_id` (service-level, v1), not
/// from request extensions, so nothing is injected. Per-call agent_id from the
/// validated JWT claims is a documented v2 follow-up.
async fn bearer_auth_middleware(
    axum::extract::State(state): axum::extract::State<McpAuthState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));

    match token {
        Some(token) if state.jwt_config.validate_token(token).is_ok() => next.run(request).await,
        _ => (
            axum::http::StatusCode::UNAUTHORIZED,
            "Unauthorized: missing or invalid Bearer token",
        )
            .into_response(),
    }
}

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

    // ── Transport selection + HTTP auth boot gate ────────────────────────────
    //
    // `EPISCIENCE_LISTEN` unset → stdio (the unchanged default). Set → HTTP,
    // which removes the stdio process boundary, so the operator must pick a
    // trust model: a shared JWT secret (Bearer auth) XOR an explicit opt-out.
    // Validate this before touching Postgres so a misconfiguration surfaces at
    // boot rather than after a slow connect. Read `EPIGRAPH_JWT_SECRET` as an
    // Option here (NOT via the REST dev-secret fallback in bin/server.rs) —
    // otherwise "secret present" would always be true and the opt-out arm would
    // be unreachable.
    let listen = std::env::var("EPISCIENCE_LISTEN")
        .ok()
        .filter(|s| !s.is_empty());
    let jwt_secret = std::env::var("EPIGRAPH_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let allow_unauth = matches!(
        std::env::var("EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP").as_deref(),
        Ok("1" | "true" | "TRUE")
    );

    if listen.is_some() {
        match (jwt_secret.is_some(), allow_unauth) {
            (true, false) | (false, true) => {} // exactly one trust model chosen
            (true, true) => {
                eprintln!(
                    "ERROR: EPIGRAPH_JWT_SECRET and EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP are \
                     mutually exclusive — set exactly one."
                );
                std::process::exit(1);
            }
            (false, false) => {
                eprintln!(
                    "ERROR: EPISCIENCE_LISTEN requires either EPIGRAPH_JWT_SECRET=<shared HMAC \
                     secret> (Bearer auth) or EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP=1 (e.g. a \
                     unix-socket listener behind filesystem permissions, or local dev). HTTP \
                     removes the stdio process boundary, so one trust model must be chosen."
                );
                std::process::exit(1);
            }
        }
    }

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

    // ── Build server + serve ─────────────────────────────────────────────────
    let server = EpiscienceServer::new(
        pool,
        embedder,
        edge_writer,
        auth_agent_id,
        blob_dir,
        max_upload_bytes,
    );

    if let Some(listen) = listen.as_deref() {
        // ── Streamable-HTTP transport (TCP or Unix socket) ───────────────────
        // (auth boot gate already enforced above.)
        use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService,
        };

        // `EpiscienceServer` is `Clone` (all state is Arc/cheap), so the
        // per-session factory just clones the prebuilt server.
        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );

        let router = axum::Router::new().nest_service("/mcp", service);

        let router = if let Some(secret) = jwt_secret.as_deref() {
            // Bearer auth against the SHARED HMAC secret (same as episcience REST).
            let state = McpAuthState {
                jwt_config: Arc::new(JwtConfig::from_secret(secret.as_bytes())),
            };
            router.layer(axum::middleware::from_fn_with_state(
                state,
                bearer_auth_middleware,
            ))
        } else {
            // `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` path: attach NO auth layer.
            // episcience MCP tools read no per-request auth context (unlike
            // epigraph-mcp, which injects a permissive context to satisfy a
            // downstream scope gate) — identity is the construction-time
            // service `auth_agent_id`. So "inject unauthenticated context" is a
            // no-op here; every request simply passes. Per-call identity from a
            // validated JWT is the documented v2 follow-up.
            router
        };

        let mode = if jwt_secret.is_some() {
            "Bearer-authenticated"
        } else {
            "UNAUTHENTICATED"
        };
        tracing::info!("episcience-mcp-server starting on {mode} HTTP {listen} (8 tools)");
        serve_with_listener(listen, router).await?;
    } else {
        // ── Stdio transport (default) ────────────────────────────────────────
        tracing::info!("episcience-mcp-server starting on stdio (8 tools)");
        let service = server.serve(rmcp::transport::stdio()).await.map_err(|e| {
            tracing::error!("MCP serve error: {e}");
            e
        })?;
        service.waiting().await?;
    }

    Ok(())
}
