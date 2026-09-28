//! Thin alias + helpers around `rmcp::model::ErrorData` mirroring the
//! `epigraph-mcp/src/errors.rs` shape. Keeps the tool handlers terse.

use std::borrow::Cow;

use rmcp::model::{ErrorCode, ErrorData};

pub type McpError = ErrorData;

pub fn invalid_params(msg: impl Into<String>) -> McpError {
    McpError {
        code: ErrorCode::INVALID_PARAMS,
        message: Cow::from(msg.into()),
        data: None,
    }
}

pub fn invalid_request(msg: impl Into<String>) -> McpError {
    McpError {
        code: ErrorCode::INVALID_REQUEST,
        message: Cow::from(msg.into()),
        data: None,
    }
}

pub fn internal_error(e: impl std::fmt::Display) -> McpError {
    McpError {
        code: ErrorCode::INTERNAL_ERROR,
        message: Cow::from(e.to_string()),
        data: None,
    }
}

/// The authenticated caller's read authority (the kernel's `Viewer::resolve`).
/// A resolution failure refuses the call rather than serving it unfiltered.
pub async fn caller_viewer(
    pool: &sqlx::PgPool,
    auth: &crate::middleware::AuthContext,
) -> Result<epigraph_db::Viewer, McpError> {
    epigraph_db::Viewer::resolve(pool, auth.agent_id)
        .await
        .map_err(|e| internal_error(format!("resolve caller read authority: {e}")))
}

/// An API-layer refusal as an MCP error: a caller mistake (403 / 404 / 410 /
/// 422) is `invalid_params`, anything else is internal.
pub fn from_api(e: crate::errors::ApiError) -> McpError {
    use crate::errors::ApiError;
    match e {
        ApiError::NotFound(m)
        | ApiError::Validation(m)
        | ApiError::Forbidden(m)
        | ApiError::Gone(m)
        | ApiError::Unauthorized(m) => invalid_params(m),
        ApiError::Internal(m) | ApiError::ServiceUnavailable(m) => internal_error(m),
    }
}
