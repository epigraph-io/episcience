use episcience_api::state::ElnState;
use tracing_subscriber::EnvFilter;

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

    // EpiScience migrations (5000-series) are applied externally via psql
    // to avoid collision with EpiGraph's sqlx migration tracking.
    // See migrations/README.md for the manual apply procedure.
    tracing::info!("Skipping embedded migrations (applied externally)");

    let state = ElnState { pool };
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
