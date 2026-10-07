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
    #[error("image input unsupported: {0}")]
    ImageInputUnsupported(String),
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("resource is busy: {0}")]
    Conflict(String),
    #[error("turn was cancelled")]
    TurnCancelled,
    #[error("resource capacity exhausted: {0}")]
    ResourceExhausted(String),
    /// Retired: history has no size limit since storage v3. Kept so the
    /// code stays reserved for clients that still map it.
    #[allow(dead_code)]
    #[error("conversation history is full: {0}")]
    JournalFull(String),
    /// The client cannot read end-to-end encrypted history.
    #[error("client upgrade required: {0}")]
    ClientUpgradeRequired(String),
    /// The request used a retired protocol (transport v1, plaintext from a
    /// non-loopback peer, device pairing v2); the client must move to
    /// transport v2 / pairing v3 (docs/transport-v2.md).
    #[error("protocol upgrade required: {0}")]
    ProtocolUpgradeRequired(String),
    /// A transport v2 envelope could not be opened. Deliberately carries no
    /// detail.
    #[error("transport crypto failure")]
    TransportCryptoFailed,
    /// The calling device is on the history revocation list
    /// (docs/history-encryption.md §3.4); only `history.encryption.get` is
    /// still allowed until another device restores it.
    #[error("history access was revoked for this device; another device must restore it")]
    HistoryAccessRevoked,
    /// No active history recipient: nothing new can be encrypted, so writes
    /// that would add history (new prompts, new conversations) are refused
    /// until a client registers its device key (docs/history-encryption.md
    /// §3.2).
    #[error(
        "conversation history is end-to-end encrypted and no device key is registered; register a device key in the client (history.recipient.register)"
    )]
    HistoryKeyRequired,
    /// The conversation is legacy unencrypted history (`legacyPlaintext`):
    /// it can be read, archived and deleted, never written.
    #[error("this conversation is legacy unencrypted history and is read-only")]
    HistoryReadOnly,
    /// The disk holding the data directory is nearly full; new turns are
    /// refused until space is freed.
    #[error("storage is low: {0}")]
    StorageLow(String),
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("remote authentication failed: {0}")]
    RemoteAuthFailed(String),
    #[error("remote host key is not verified: {0}")]
    RemoteHostKeyUnverified(String),
    #[error("remote host is unreachable: {0}")]
    RemoteUnreachable(String),
    #[error("remote operation failed: {0}")]
    RemoteFailed(String),
    /// The remote server refused a file operation. Distinct from
    /// `Unauthorized`, which clients treat as a TodeX device-auth failure.
    #[error("remote permission denied: {0}")]
    RemotePermissionDenied(String),
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
            Self::ImageInputUnsupported(_) => "IMAGE_INPUT_UNSUPPORTED",
            Self::NotFound(_) => "NOT_FOUND",
            Self::Conflict(_) => "CONFLICT",
            Self::TurnCancelled => "TURN_CANCELLED",
            Self::ResourceExhausted(_) => "RESOURCE_EXHAUSTED",
            Self::JournalFull(_) => "JOURNAL_FULL",
            Self::ClientUpgradeRequired(_) => "CLIENT_UPGRADE_REQUIRED",
            Self::ProtocolUpgradeRequired(_) => "PROTOCOL_UPGRADE_REQUIRED",
            Self::TransportCryptoFailed => "TRANSPORT_CRYPTO_FAILED",
            Self::HistoryAccessRevoked => "HISTORY_ACCESS_REVOKED",
            Self::HistoryKeyRequired => "HISTORY_KEY_REQUIRED",
            Self::HistoryReadOnly => "HISTORY_READ_ONLY",
            Self::StorageLow(_) => "STORAGE_LOW",
            Self::ProviderUnavailable(_) => "PROVIDER_UNAVAILABLE",
            Self::RemoteAuthFailed(_) => "REMOTE_AUTH_FAILED",
            Self::RemoteHostKeyUnverified(_) => "REMOTE_HOST_KEY_UNVERIFIED",
            Self::RemoteUnreachable(_) => "REMOTE_UNREACHABLE",
            Self::RemoteFailed(_) => "REMOTE_OPERATION_FAILED",
            Self::RemotePermissionDenied(_) => "REMOTE_PERMISSION_DENIED",
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
            Self::ImageInputUnsupported(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::TurnCancelled => StatusCode::CONFLICT,
            Self::ResourceExhausted(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::JournalFull(_) | Self::StorageLow(_) => StatusCode::INSUFFICIENT_STORAGE,
            Self::ClientUpgradeRequired(_) | Self::ProtocolUpgradeRequired(_) => {
                StatusCode::UPGRADE_REQUIRED
            }
            Self::TransportCryptoFailed => StatusCode::BAD_REQUEST,
            Self::HistoryAccessRevoked => StatusCode::FORBIDDEN,
            Self::HistoryKeyRequired | Self::HistoryReadOnly => StatusCode::CONFLICT,
            Self::ProviderUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            // Not 401: clients treat that as a failed device signature.
            Self::RemoteAuthFailed(_) | Self::RemoteHostKeyUnverified(_) => StatusCode::FORBIDDEN,
            Self::RemoteUnreachable(_) => StatusCode::BAD_GATEWAY,
            Self::RemoteFailed(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::RemotePermissionDenied(_) => StatusCode::FORBIDDEN,
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
