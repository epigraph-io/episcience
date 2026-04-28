pub mod clients;
pub mod errors;
pub mod jobs;
pub mod mcp;
pub mod middleware;
pub mod routes;
pub mod state;

use axum::Router;
use state::ElnState;

pub fn create_router(state: ElnState) -> Router {
    let protected = Router::new()
        .merge(routes::samples::router(state.clone()))
        .merge(routes::protocols::router(state.clone()))
        .merge(routes::blobs::router(state.clone()))
        .merge(routes::countersign::router(state.clone()))
        .merge(routes::export::router(state.clone()))
        .merge(routes::search::router(state.clone()))
        .merge(routes::syntheses::router(state.clone()))
        .merge(routes::synthesis_search::router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::bearer_auth_middleware,
        ));

    Router::new()
        .merge(routes::health::router())
        .merge(protected)
}
