use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub enum ApiError {
    NotFound(String),
    Validation(String),
    Internal(String),
    Unauthorized(String),
    Forbidden(String),
    ServiceUnavailable(String),
    /// 410: a retired surface (synthesis shares).
    Gone(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg),
            ApiError::Internal(msg) => {
                tracing::error!(detail = %msg, "internal server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ApiError::Gone(msg) => (StatusCode::GONE, msg),
        };
        let body = axum::Json(json!({ "error": message }));
        (status, body).into_response()
    }
}

/// The request path's refusals (`EpiscienceDb`): no principal is 401; an
/// operated or unresolvable principal, or a write by one who may write no
/// group, is 403; a session that cannot be opened is 500.
impl From<episcience_db::tenancy::RequestRefusal> for ApiError {
    fn from(r: episcience_db::tenancy::RequestRefusal) -> Self {
        use episcience_db::tenancy::RequestRefusal as R;
        match r {
            R::PrincipalRequired => ApiError::Unauthorized(r.to_string()),
            R::Operated(_) | R::Unresolvable(_) | R::NoWritableGroup => {
                ApiError::Forbidden(r.to_string())
            }
            R::Session(_) => ApiError::Internal(r.to_string()),
        }
    }
}

/// A commit failure of a request transaction.
impl From<epigraph_db::DbError> for ApiError {
    fn from(e: epigraph_db::DbError) -> Self {
        ApiError::Internal(e.to_string())
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
            episcience_db::errors::DbError::TenancyRefused(msg) => {
                ApiError::Forbidden(format!("refused by the tenancy guard: {msg}"))
            }
            // The tenancy row guards (migration 5035) refuse with SQLSTATE:
            // 42501 = not the caller's to write, 23503 = a parent the caller
            // cannot see (reported like a missing one).
            episcience_db::errors::DbError::Sqlx(sqlx::Error::Database(d))
                if d.code().as_deref() == Some("42501") =>
            {
                ApiError::Forbidden(format!("refused by the tenancy guard: {}", d.message()))
            }
            episcience_db::errors::DbError::Sqlx(sqlx::Error::Database(d))
                if d.code().as_deref() == Some("23503") =>
            {
                ApiError::NotFound("a referenced row was not found".into())
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}
