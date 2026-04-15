use episcience_api::middleware::JwtConfig;
use episcience_api::state::ElnState;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const DEV_JWT_SECRET: &[u8] = b"dev-only-insecure-secret-change-in-production";

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

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
        pool,
        blob_dir,
        jwt_config,
        max_upload_bytes,
    };
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
    axum::serve(listener, app)
        .await
        .expect("Server error");
}
