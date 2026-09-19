//! API error type mapping internal failures to HTTP responses.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::flood::parse_flood_wait_secs;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("Unauthorized")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    /// Local rate limiter rejected the request.
    #[error("Rate limit exceeded")]
    RateLimited { retry_after_secs: u64 },
    #[error("{0}")]
    NotImplemented(String),
    /// Errors bubbling up from grammers / Telegram. FLOOD_WAIT is mapped
    /// to 429 with Retry-After so agents can back off safely.
    #[error("{0}")]
    Telegram(String),
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::Internal(e.to_string())
    }

    pub fn telegram(e: impl std::fmt::Display) -> Self {
        Self::Telegram(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, retry_after, message) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, None, m),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, None, "Unauthorized".into()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, None, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, None, m),
            ApiError::RateLimited { retry_after_secs } => (
                StatusCode::TOO_MANY_REQUESTS,
                Some(retry_after_secs),
                "Rate limit exceeded".into(),
            ),
            ApiError::NotImplemented(m) => (StatusCode::NOT_IMPLEMENTED, None, m),
            ApiError::Telegram(m) => {
                if let Some(wait) = parse_flood_wait_secs(&m) {
                    (StatusCode::TOO_MANY_REQUESTS, Some(wait), m)
                } else {
                    (StatusCode::BAD_GATEWAY, None, m)
                }
            }
            ApiError::Internal(m) => {
                tracing::error!(error = %m, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    "Internal server error".into(),
                )
            }
        };

        let body = Json(serde_json::json!({ "error": message }));
        let mut response = (status, body).into_response();
        if let Some(secs) = retry_after {
            if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        response
    }
}
