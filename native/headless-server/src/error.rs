//! 错误类型

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// API 错误响应
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_found(resource: &str) -> Self {
        Self::new("NotFound", format!("{} not found", resource))
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new("BadRequest", msg)
    }

    pub fn unauthorized() -> Self {
        Self::new("Unauthorized", "Invalid or missing token")
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new("InternalError", msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code.as_str() {
            "NotFound" => StatusCode::NOT_FOUND,
            "BadRequest" => StatusCode::BAD_REQUEST,
            "Unauthorized" => StatusCode::UNAUTHORIZED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, axum::Json(self)).into_response()
    }
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(err: E) -> Self {
        Self::internal(err.to_string())
    }
}
