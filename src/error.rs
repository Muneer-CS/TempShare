use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("password required")]
    PasswordRequired,
    #[error("incorrect password")]
    IncorrectPassword,
    #[error("rate limited")]
    RateLimited,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Deliberately generic messages for anything that could leak
        // internal state (file paths, DB errors, stack info) to a network
        // client. Internal errors are logged server-side, not echoed back.
        let (status, msg) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            AppError::PasswordRequired => {
                (StatusCode::UNAUTHORIZED, "password required".to_string())
            }
            AppError::IncorrectPassword => {
                (StatusCode::UNAUTHORIZED, "incorrect password".to_string())
            }
            AppError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "too many requests, slow down".to_string(),
            ),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            AppError::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
