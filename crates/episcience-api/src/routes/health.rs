use axum::{routing::get, Json, Router};
use serde_json::json;

async fn check() -> Json<serde_json::Value> {
    Json(json!({
        "status": "healthy",
        "service": "episcience-eln",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub fn router() -> Router {
    Router::new().route("/health", get(check))
}
