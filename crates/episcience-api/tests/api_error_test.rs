use axum::http::StatusCode;
use axum::response::IntoResponse;
use episcience_api::errors::ApiError;

#[test]
fn service_unavailable_maps_to_503() {
    let resp = ApiError::ServiceUnavailable("epigraph down".into()).into_response();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
