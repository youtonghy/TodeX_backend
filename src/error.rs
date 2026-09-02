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
    #[error("workspace must be trusted before execution: {0}")]
    WorkspaceTrustRequired(String),
    #[error("codex binary not found in PATH")]
    CodexNotFound,
    #[error("git executable not found in PATH")]
    GitUnavailable,
    #[error("no Git repository was found at or above the workspace path")]
    GitRepositoryNotFound,
    #[error("git command failed ({operation}): {detail}")]
    GitCommandFailed { operation: String, detail: String },
    #[error("git operation partially succeeded ({operation}) in {repository_path}: {detail}")]
    GitPartialSuccess {
        repository_path: String,
        operation: String,
        detail: String,
    },
    #[error("git command timed out ({0})")]
    GitCommandTimedOut(String),
    #[error("git command output exceeded the {0} byte limit")]
    GitOutputLimitExceeded(usize),
    #[error("git process could not be started: {0}")]
    GitProcess(String),
    #[error("git scan exceeded the repository candidate limit")]
    GitScanLimitExceeded,
    #[error("unsupported capability: {0}")]
    Unsupported(String),
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("resource is busy: {0}")]
    Conflict(String),
    #[error("turn was cancelled")]
    TurnCancelled,
    #[error("resource capacity exhausted: {0}")]
    ResourceExhausted(String),
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
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
            Self::WorkspaceTrustRequired(_) => "WORKSPACE_TRUST_REQUIRED",
            Self::CodexNotFound => "CODEX_NOT_FOUND",
            Self::GitUnavailable => "GIT_UNAVAILABLE",
            Self::GitRepositoryNotFound => "GIT_REPOSITORY_NOT_FOUND",
            Self::GitCommandFailed { .. } => "GIT_COMMAND_FAILED",
            Self::GitPartialSuccess { .. } => "GIT_PARTIAL_SUCCESS",
            Self::GitCommandTimedOut(_) => "GIT_COMMAND_TIMED_OUT",
            Self::GitOutputLimitExceeded(_) => "GIT_OUTPUT_LIMIT_EXCEEDED",
            Self::GitProcess(_) => "GIT_PROCESS_ERROR",
            Self::GitScanLimitExceeded => "GIT_SCAN_LIMIT_EXCEEDED",
            Self::Unsupported(_) => "UNSUPPORTED",
            Self::NotFound(_) => "NOT_FOUND",
            Self::Conflict(_) => "CONFLICT",
            Self::TurnCancelled => "TURN_CANCELLED",
            Self::ResourceExhausted(_) => "RESOURCE_EXHAUSTED",
            Self::ProviderUnavailable(_) => "PROVIDER_UNAVAILABLE",
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
            Self::WorkspacePathNotFound => StatusCode::NOT_FOUND,
            Self::WorkspacePathOutsideRoot => StatusCode::FORBIDDEN,
            Self::WorkspaceTrustRequired(_) => StatusCode::FORBIDDEN,
            Self::GitUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::GitRepositoryNotFound => StatusCode::NOT_FOUND,
            Self::GitCommandFailed { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::GitPartialSuccess { .. } => StatusCode::BAD_GATEWAY,
            Self::GitCommandTimedOut(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::GitOutputLimitExceeded(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::GitProcess(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::GitScanLimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::TurnCancelled => StatusCode::CONFLICT,
            Self::ResourceExhausted(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::ProviderUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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
