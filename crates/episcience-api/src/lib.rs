pub mod errors;
pub mod routes;
pub mod state;

use axum::Router;
use state::ElnState;

pub fn create_router(state: ElnState) -> Router {
    Router::new()
        .merge(routes::health::router())
        .merge(routes::samples::router(state.clone()))
        .merge(routes::protocols::router(state.clone()))
        .merge(routes::blobs::router(state.clone()))
        .merge(routes::search::router(state))
}
