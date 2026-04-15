use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub enum ApiError {
    NotFound(String),
    Validation(String),
    Internal(String),
    Unauthorized(String),
    Forbidden(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg),
            ApiError::Internal(msg) => {
                tracing::error!(detail = %msg, "internal server error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
            }
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
        };
        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}

impl From<episcience_db::errors::DbError> for ApiError {
    fn from(e: episcience_db::errors::DbError) -> Self {
        match e {
            episcience_db::errors::DbError::NotFound { entity, id } => {
                ApiError::NotFound(format!("{entity} {id} not found"))
            }
            episcience_db::errors::DbError::Io(msg) => ApiError::Internal(msg),
            episcience_db::errors::DbError::Serialization(msg) => ApiError::Internal(msg),
            other => ApiError::Internal(other.to_string()),
        }
    }
}
