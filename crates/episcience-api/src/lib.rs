pub mod auth;
pub mod clients;
pub mod config;
pub mod errors;
pub mod jobs;
pub mod mcp;
pub mod middleware;
pub mod providers;
pub mod routes;
pub mod state;

use axum::Router;
use state::ElnState;

/// The REST router with out-of-band upload off (`PUT
/// /api/v1/eln/blobs/upload` answers 503).
pub fn create_router(state: ElnState) -> Router {
    create_router_with_uploads(state, None)
}

/// The REST router; with `uploads`, `PUT /api/v1/eln/blobs/upload` accepts
/// upload tokens signed with that key (`auth::upload_token`), and without it
/// refuses every upload (503).
pub fn create_router_with_uploads(
    state: ElnState,
    uploads: Option<auth::upload_token::UploadTokenKey>,
) -> Router {
    let mut protected = Router::new()
        .merge(routes::samples::router(state.clone()))
        .merge(routes::protocols::router(state.clone()))
        .merge(routes::blobs::router(state.clone()))
        .merge(routes::countersign::router(state.clone()))
        .merge(routes::export::router(state.clone()))
        .merge(routes::search::router(state.clone()))
        .merge(routes::syntheses::router(state.clone()))
        .merge(routes::synthesis_search::router(state.clone()))
        .merge(routes::workflow_runs::router(state.clone()))
        .merge(routes::wiki::router(state.clone()));
    if let Some(key) = uploads {
        protected = protected.layer(axum::Extension(key));
    }
    let protected = protected.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::bearer_auth_middleware,
    ));

    Router::new()
        .merge(routes::health::router())
        .merge(protected)
}
