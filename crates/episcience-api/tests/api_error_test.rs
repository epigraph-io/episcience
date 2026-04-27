use episcience_api::errors::ApiError;
use axum::response::IntoResponse;
use axum::http::StatusCode;

#[test]
fn service_unavailable_maps_to_503() {
    let resp = ApiError::ServiceUnavailable("epigraph down".into()).into_response();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
