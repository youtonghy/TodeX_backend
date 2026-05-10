use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum AppError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("authentication required")]
    Unauthenticated,
    #[error("access denied: {0}")]
    Unauthorized(String),
    #[error("workspace path does not exist")]
    WorkspacePathNotFound,
    #[error("workspace path escapes configured workspace root")]
    WorkspacePathOutsideRoot,
    #[error("codex binary not found in PATH")]
    CodexNotFound,
    #[error("unsupported capability: {0}")]
    Unsupported(String),
    #[error("event stream lagged by {0} messages")]
    StreamLagged(u64),
    #[error("event stream closed")]
    StreamClosed,
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Unauthorized(_) => "UNAUTHORIZED",
            Self::WorkspacePathNotFound => "WORKSPACE_PATH_NOT_FOUND",
            Self::WorkspacePathOutsideRoot => "WORKSPACE_PATH_OUTSIDE_ROOT",
            Self::CodexNotFound => "CODEX_NOT_FOUND",
            Self::Unsupported(_) => "UNSUPPORTED",
            Self::StreamLagged(_) => "EVENT_STREAM_LAGGED",
            Self::StreamClosed => "EVENT_STREAM_CLOSED",
            Self::Serialization(_) => "SERIALIZATION_FAILED",
            Self::Io(_) => "IO_ERROR",
            Self::Anyhow(_) => "INTERNAL_ERROR",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Unauthorized(_) => StatusCode::FORBIDDEN,
            Self::WorkspacePathNotFound | Self::WorkspacePathOutsideRoot => StatusCode::NOT_FOUND,
            Self::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            Self::StreamLagged(_) | Self::StreamClosed => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (
            status,
            Json(json!({
                "code": self.code(),
                "message": self.to_string(),
            })),
        )
            .into_response()
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
