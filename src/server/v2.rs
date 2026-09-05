use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::fs::FileType;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::warn;

use crate::app_state::AppState;
use crate::conversation::{ConversationManifest, ProviderKind};
use crate::error::AppError;
use crate::provider::{
    read_current_version, run_upgrade, CliUpgradeOperation, CliVersionsResponse,
    ConversationPrompt, ManagedCli, PermissionDecision, PromptContentRef, PromptSkillRef,
};
use crate::transport_crypto::TransportCryptoSession;
use crate::workspace_paths::{canonical_workspace_root, validate_workspace_directory_text};
use crate::workspace_store::WorkspaceRecord;

use super::git;
use super::protocol::ServerEvent;
use super::websocket::{self, AuthContext};

/// Maximum WebSocket message size for the unified `/v2/ws` socket (8MB).
/// Matches MAX_LEGACY_WS_MESSAGE_BYTES: chat attachments travel as base64
/// data URLs (up to 8 MiB per image) and the legacy plane has no outbound
/// chunking, so a tighter cap would reject source images the v1 plane accepted.
/// Client v2.ts keeps a stricter 4MB guard for `conversation.*` commands.
const MAX_WS_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_WS_SUBSCRIPTIONS: usize = 128;
const MAX_WS_IN_FLIGHT_OPERATIONS: usize = 16;
/// Keep idle connections alive; mirrors the legacy `/v1/ws` socket so clients
/// without an application-level heartbeat are not reaped. A Ping draws an
/// automatic Pong, which counts as receive activity below.
const WS_PING_INTERVAL_SECS: u64 = 30;
const WS_CLIENT_TIMEOUT_SECS: u64 = 90;
const BROWSER_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const BROWSER_FETCH_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Command types handled natively by the v2 dispatcher. Everything else on the
/// socket is a legacy `ClientMessage` (`terminal.*`, `codex.local.*`,
/// `codex.gateway.control`, `codex.mcp.*`, `codex.cloudTask.*`) dispatched
/// through the shared scoped legacy machinery.
fn is_v2_native_command(command_type: &str) -> bool {
    matches!(
        command_type,
        "conversation.subscribe"
            | "conversation.create"
            | "conversation.prompt"
            | "conversation.followUp"
            | "conversation.retry"
            | "conversation.resume"
            | "conversation.cancel"
            | "conversation.interrupt"
            | "conversation.stop"
            | "conversation.permission.respond"
            | "mcp.list"
            | "mcp.refresh"
            | "mcp.call"
            | "server.ping"
            | "session.resume"
    )
}

fn is_v2_background_command(command_type: &str) -> bool {
    matches!(command_type, "mcp.refresh" | "mcp.call")
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/version", get(version))
        .route("/v2/workspaces", get(workspaces).put(replace_workspaces))
        .route("/v2/workspaces/{workspace_id}", delete(delete_workspace))
        .route(
            "/v2/workspaces/{workspace_id}/trust",
            get(workspace_trust).put(update_workspace_trust),
        )
        .route("/v2/workspace/entries", get(workspace_entries))
        .route("/v2/workspace/directories", get(workspace_directories))
        .route("/v2/workspace/file", get(workspace_file))
        .route("/v2/git/scan", get(git_scan))
        .route("/v2/git/run", post(git_run))
        .route("/v2/browser/fetch", post(browser_fetch))
        .route("/v2/providers", get(providers))
        .route("/v2/providers/versions", get(provider_versions))
        .route(
            "/v2/providers/{provider}/upgrade",
            post(upgrade_provider_cli),
        )
        .route(
            "/v2/providers/upgrades/{operation_id}",
            get(provider_upgrade_operation),
        )
        .route("/v2/providers/models", get(provider_models))
        .route("/v2/providers/image-input", get(provider_image_input))
        .route("/v2/providers/commands", get(provider_commands))
        .route("/v2/catalog/skills", get(skills))
        .route("/v2/catalog/skills/{resource_id}", get(skill_resource))
        .route("/v2/catalog/mcp", get(mcp))
        .route(
            "/v2/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}",
            get(get_conversation)
                .patch(update_conversation)
                .delete(delete_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/events",
            get(replay_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/prompt",
            post(prompt_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/cancel",
            post(cancel_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/interrupt",
            post(cancel_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/permissions/{permission_id}",
            post(resolve_permission),
        )
        .route("/v2/ws", get(ws))
}

async fn skills(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<crate::catalog::SkillCatalog>, AppError> {
    require_auth(&state, &headers)?;
    let workspace = validate_catalog_workspace(&state, &query.workspace)?;
    Ok(Json(state.catalog.skills(query.provider, workspace).await?))
}

/// Daemon self-checks and connection cards poll this without a token, matching
/// the historical `/v1/version` contract. No workspace or file data is exposed.
pub(super) async fn version(State(state): State<AppState>) -> Json<VersionResponse> {
    Json(VersionResponse {
        name: env!("CARGO_PKG_NAME"),
        version: crate::version::APP_VERSION,
        data_dir: state.config.data_dir.display().to_string(),
        workspace_root: state.config.workspace_root.display().to_string(),
    })
}

pub(super) async fn workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<WorkspacesResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let snapshot = state.workspaces.snapshot_owned(&auth.tenant_id).await;
    Ok(Json(WorkspacesResponse {
        workspaces: snapshot.workspaces,
        updated_at: snapshot.updated_at,
    }))
}

pub(super) async fn replace_workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReplaceWorkspacesRequest>,
) -> Result<Json<WorkspacesResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let snapshot = state
        .workspaces
        .merge_owned(&auth.tenant_id, request.workspaces)
        .await?;
    let workspace_paths = snapshot
        .workspaces
        .iter()
        .map(|workspace| PathBuf::from(&workspace.path))
        .collect::<Vec<_>>();
    let trusted = state
        .workspace_trust
        .auto_trust_undecided_owned(&auth.tenant_id, &workspace_paths)
        .await?;
    if !trusted.is_empty() {
        let audit = crate::event::EventRecord::new(
            "workspace.trust.auto_granted",
            None,
            None,
            None,
            json!({
                "principal_id": auth.principal_id,
                "tenant_id": auth.tenant_id,
                "token_id": auth.token_id,
                "workspace_paths": trusted
                    .iter()
                    .map(|status| status.workspace_path.as_str())
                    .collect::<Vec<_>>(),
            }),
        );
        if let Err(error) = websocket::append_audit_event(&state, &audit).await {
            warn!(error = %error, "failed to persist automatic workspace trust audit event");
        }
        state.events.publish(audit).await;
    }
    Ok(Json(WorkspacesResponse {
        workspaces: snapshot.workspaces,
        updated_at: snapshot.updated_at,
    }))
}

pub(super) async fn delete_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(workspace_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace = state
        .workspaces
        .get_owned(&auth.tenant_id, &workspace_id)
        .await?;
    let workspace_path = PathBuf::from(&workspace.path);
    state
        .workspace_trust
        .set_owned(&auth.tenant_id, &workspace_path, false)
        .await?;
    let cancelled = state
        .conversations
        .cancel_workspace_owned(&auth.tenant_id, &workspace_path)
        .await?;
    let deleted = state
        .workspaces
        .delete_owned(&auth.tenant_id, &workspace_id)
        .await?;
    Ok(Json(json!({
        "workspaceId": workspace_id,
        "deleted": deleted,
        "trustRevoked": true,
        "cancelledTurns": cancelled,
    })))
}

pub(super) async fn workspace_trust(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(workspace_id): AxumPath<String>,
) -> Result<Json<crate::workspace_trust::WorkspaceTrustStatus>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace = state
        .workspaces
        .get_owned(&auth.tenant_id, &workspace_id)
        .await?;
    Ok(Json(
        state
            .workspace_trust
            .status_owned(&auth.tenant_id, Path::new(&workspace.path))
            .await?,
    ))
}

pub(super) async fn update_workspace_trust(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(workspace_id): AxumPath<String>,
    Json(request): Json<UpdateWorkspaceTrustRequest>,
) -> Result<Json<crate::workspace_trust::WorkspaceTrustStatus>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace = state
        .workspaces
        .get_owned(&auth.tenant_id, &workspace_id)
        .await?;
    let workspace_path = PathBuf::from(&workspace.path);
    let status = state
        .workspace_trust
        .set_owned(&auth.tenant_id, &workspace_path, request.trusted)
        .await?;
    let cancelled_turns = if request.trusted {
        0
    } else {
        state
            .conversations
            .cancel_workspace_owned(&auth.tenant_id, &workspace_path)
            .await?
    };
    let audit = crate::event::EventRecord::new(
        "workspace.trust.changed",
        None,
        None,
        None,
        json!({
            "principal_id": auth.principal_id,
            "tenant_id": auth.tenant_id,
            "token_id": auth.token_id,
            "workspace_id": workspace_id,
            "workspace_path": status.workspace_path,
            "trusted": status.trusted,
            "cancelled_turns": cancelled_turns,
        }),
    );
    if let Err(error) = websocket::append_audit_event(&state, &audit).await {
        warn!(error = %error, "failed to persist workspace trust audit event");
    }
    state.events.publish(audit).await;
    Ok(Json(status))
}

pub(super) async fn workspace_entries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceEntriesQuery>,
) -> Result<Json<WorkspaceEntriesResponse>, AppError> {
    require_auth(&state, &headers)?;

    let cwd = validate_workspace_directory_text(&state.config.workspace_root, &query.cwd)?;

    let raw_query = query.query.as_deref().unwrap_or("");
    let relative_query = normalize_relative_query(raw_query)?;
    let entries = list_workspace_entries(
        &cwd,
        &relative_query,
        raw_query.trim().ends_with('/'),
        query.limit.unwrap_or(40).clamp(1, 100),
    )
    .await?;

    Ok(Json(WorkspaceEntriesResponse { entries }))
}

pub(super) async fn workspace_directories(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceDirectoriesQuery>,
) -> Result<Json<WorkspaceDirectoriesResponse>, AppError> {
    require_auth(&state, &headers)?;

    let root = canonical_workspace_root(&state.config.workspace_root)?;
    let current = match query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        Some(path) => validate_workspace_directory_text(&state.config.workspace_root, path)?,
        None => root.clone(),
    };
    let limit = query.limit.unwrap_or(100).clamp(1, 300);
    let entries = list_workspace_directories(&root, &current, limit).await?;
    let parent = current
        .parent()
        .filter(|parent| current != root && parent.starts_with(&root))
        .map(|parent| parent.display().to_string());

    Ok(Json(WorkspaceDirectoriesResponse {
        root: root.display().to_string(),
        current: current.display().to_string(),
        parent,
        entries,
    }))
}

pub(super) async fn workspace_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceFileQuery>,
) -> Result<Json<WorkspaceFileResponse>, AppError> {
    require_auth(&state, &headers)?;
    let path = validate_workspace_file_text(&state.config.workspace_root, &query.path)?;
    let metadata = tokio::fs::metadata(&path).await?;
    if !metadata.is_file() {
        return Err(AppError::InvalidRequest("path must be a file".to_owned()));
    }
    const MAX_PREVIEW_BYTES: u64 = 1024 * 1024;
    if metadata.len() > MAX_PREVIEW_BYTES {
        return Err(AppError::InvalidRequest(
            "file is too large to preview".to_owned(),
        ));
    }
    let bytes = tokio::fs::read(&path).await?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("file")
        .to_owned();
    let mime_type = mime_for_name(&name);
    let text = if mime_type.starts_with("text/") || mime_type == "application/json" {
        Some(String::from_utf8_lossy(&bytes).to_string())
    } else {
        None
    };
    Ok(Json(WorkspaceFileResponse {
        name,
        path: path.display().to_string(),
        mime_type,
        size_bytes: bytes.len() as u64,
        text,
    }))
}

pub(super) async fn git_scan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<super::protocol::GitScanQuery>,
) -> Result<Json<super::protocol::GitScanResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace =
        validate_workspace_directory_text(&state.config.workspace_root, &query.workspace_path)?;
    let result = git::scan(&state.config.workspace_root, &workspace).await;
    let audit = append_git_audit(
        &state,
        &auth,
        "scan",
        &workspace,
        None,
        if result.is_ok() { "allow" } else { "deny" },
        result.as_ref().err().map(AppError::code).unwrap_or("OK"),
        result.as_ref().ok().map(|value| value.repositories.len()),
        None,
    )
    .await;
    combine_git_result(result.map(Json), audit)
}

pub(super) async fn git_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<super::protocol::GitRunResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let request: super::protocol::GitRunRequest =
        serde_json::from_value(payload).map_err(|error| {
            AppError::InvalidRequest(format!(
                "invalid git request: {}",
                truncate_git_error(error.to_string(), 256)
            ))
        })?;
    let workspace =
        validate_workspace_directory_text(&state.config.workspace_root, &request.workspace_path)?;
    state
        .workspace_trust
        .ensure_trusted(&auth.tenant_id, &workspace)
        .await?;
    let result = git::run(
        &state.config.workspace_root,
        &state.config.data_dir,
        &workspace,
        &request,
    )
    .await;
    let decision = if result.is_ok() {
        "allow"
    } else if matches!(&result, Err(AppError::GitPartialSuccess { .. })) {
        "partial"
    } else {
        "deny"
    };
    let repository = match &result {
        Ok(value) => Some(Path::new(value.repository_path.as_str())),
        Err(AppError::GitPartialSuccess {
            repository_path, ..
        }) => Some(Path::new(repository_path.as_str())),
        _ => None,
    };
    let audit = append_git_audit(
        &state,
        &auth,
        request.action.as_str(),
        &workspace,
        repository,
        decision,
        result.as_ref().err().map(AppError::code).unwrap_or("OK"),
        None,
        result.as_ref().ok().map(|value| value.output.len()),
    )
    .await;
    combine_git_result(result.map(Json), audit)
}

fn validate_workspace_file_text(root: &Path, raw: &str) -> Result<PathBuf, AppError> {
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        return Err(AppError::InvalidRequest(
            "workspace file path must be absolute".to_owned(),
        ));
    }
    let root = std::fs::canonicalize(root)?;
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::WorkspacePathNotFound
        } else {
            AppError::Io(error)
        }
    })?;
    if !canonical.starts_with(&root) {
        return Err(AppError::WorkspacePathOutsideRoot);
    }
    Ok(canonical)
}

pub(super) async fn browser_fetch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BrowserFetchRequest>,
) -> Result<Json<BrowserFetchResponse>, AppError> {
    require_auth(&state, &headers)?;
    let url = validate_browser_url(&request.url)?;
    let parsed = reqwest::Url::parse(&url)
        .map_err(|_| AppError::InvalidRequest("browser target is not allowed".to_owned()))?;
    if !is_allowed_browser_target(&parsed) {
        return Err(AppError::InvalidRequest(
            "browser target is not allowed".to_owned(),
        ));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(BROWSER_FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 3 || !is_allowed_browser_target(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| AppError::ProviderUnavailable(error.to_string()))?;
    let mut response = client
        .get(parsed)
        .send()
        .await
        .map_err(|error| AppError::ProviderUnavailable(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::ProviderUnavailable(format!(
            "browser target returned {status}"
        )));
    }
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AppError::ProviderUnavailable(error.to_string()))?
    {
        if body.len().saturating_add(chunk.len()) > BROWSER_FETCH_BODY_LIMIT {
            return Err(AppError::InvalidRequest(format!(
                "browser response exceeds {BROWSER_FETCH_BODY_LIMIT} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Json(BrowserFetchResponse {
        url: final_url,
        status: status.as_u16(),
        content_type,
        body: String::from_utf8_lossy(&body).to_string(),
    }))
}

fn is_allowed_browser_target(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    is_loopback && url.username().is_empty() && url.password().is_none()
}

fn validate_browser_url(raw: &str) -> Result<String, AppError> {
    let value = raw.trim();
    let Ok(mut parsed) = reqwest::Url::parse(value) else {
        return Err(AppError::InvalidRequest(
            "only valid http and https URLs are allowed".to_owned(),
        ));
    };
    if value.len() > 2048
        || !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
    {
        return Err(AppError::InvalidRequest(
            "only valid http and https URLs are allowed".to_owned(),
        ));
    }
    if parsed.path().is_empty() {
        parsed.set_path("/");
    }
    Ok(parsed.to_string())
}

fn mime_for_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".md") {
        "text/markdown"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".html") {
        "text/html"
    } else if lower.ends_with(".css") {
        "text/css"
    } else if lower.ends_with(".js")
        || lower.ends_with(".ts")
        || lower.ends_with(".rs")
        || lower.ends_with(".go")
        || lower.ends_with(".c")
        || lower.ends_with(".cpp")
        || lower.ends_with(".py")
        || lower.ends_with(".toml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".sh")
    {
        "text/plain"
    } else {
        "application/octet-stream"
    }
    .to_owned()
}

async fn list_workspace_entries(
    cwd: &Path,
    relative_query: &Path,
    trailing_slash_query: bool,
    limit: usize,
) -> Result<Vec<WorkspaceEntry>, AppError> {
    let query_text = slash_path(relative_query);
    if query_text.is_empty() {
        return list_direct_workspace_entries(cwd, Path::new(""), "", limit).await;
    }

    let include_hidden = query_includes_hidden_path(&query_text);
    if trailing_slash_query {
        let directory = cwd.join(relative_query);
        if !directory.exists() || !directory.is_dir() {
            return Ok(vec![]);
        }
        return list_direct_workspace_entries(cwd, relative_query, "", limit).await;
    }

    let mut entries = Vec::new();
    let mut queue = VecDeque::from([PathBuf::new()]);
    let query = query_text.to_ascii_lowercase();
    while let Some(directory_query) = queue.pop_front() {
        let directory = cwd.join(&directory_query);
        let mut read_dir = match tokio::fs::read_dir(&directory).await {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(error.into()),
        };

        while let Some(entry) = read_dir.next_entry().await? {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.is_empty() || (!include_hidden && file_name.starts_with('.')) {
                continue;
            }

            let file_type = entry.file_type().await?;
            if !file_type.is_dir() && !file_type.is_file() {
                continue;
            }

            let relative_path = directory_query.join(&file_name);
            let relative_path_text = slash_path(&relative_path);
            if relative_path_text
                .to_ascii_lowercase()
                .contains(query.as_str())
            {
                entries.push(workspace_entry(
                    file_name.clone(),
                    &relative_path,
                    &file_type,
                ));
            }

            if file_type.is_dir()
                && should_descend_workspace_directory(&file_name, include_hidden, query.as_str())
            {
                queue.push_back(relative_path);
            }
        }
    }

    sort_workspace_entries(&mut entries);
    entries.truncate(limit);
    Ok(entries)
}

async fn list_direct_workspace_entries(
    cwd: &Path,
    directory_query: &Path,
    filter: &str,
    limit: usize,
) -> Result<Vec<WorkspaceEntry>, AppError> {
    let directory = cwd.join(directory_query);
    if !directory.exists() || !directory.is_dir() {
        return Ok(vec![]);
    }

    let mut entries = Vec::new();
    let filter = filter.to_ascii_lowercase();
    let include_hidden = filter.starts_with('.');
    let mut read_dir = tokio::fs::read_dir(&directory).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.is_empty() || (!include_hidden && file_name.starts_with('.')) {
            continue;
        }
        if !filter.is_empty() && !file_name.to_ascii_lowercase().contains(&filter) {
            continue;
        }

        let file_type = entry.file_type().await?;
        if !file_type.is_dir() && !file_type.is_file() {
            continue;
        }

        let relative_path = directory_query.join(&file_name);
        entries.push(workspace_entry(file_name, &relative_path, &file_type));
    }

    sort_workspace_entries(&mut entries);
    entries.truncate(limit);
    Ok(entries)
}

async fn list_workspace_directories(
    root: &Path,
    current: &Path,
    limit: usize,
) -> Result<Vec<WorkspaceDirectory>, AppError> {
    let root = tokio::fs::canonicalize(root).await?;
    let current = tokio::fs::canonicalize(current).await?;
    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(&current).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.is_empty() || file_name.starts_with('.') {
            continue;
        }

        let path = entry.path();
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() {
            continue;
        }

        let canonical = match tokio::fs::canonicalize(&path).await {
            Ok(canonical) if canonical.starts_with(&root) => canonical,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        entries.push(WorkspaceDirectory {
            name: file_name,
            path: canonical.display().to_string(),
            kind: WorkspaceEntryKind::Directory,
        });
    }

    entries.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    entries.truncate(limit);
    Ok(entries)
}

fn workspace_entry(name: String, relative_path: &Path, file_type: &FileType) -> WorkspaceEntry {
    let mut path = slash_path(relative_path);
    let kind = if file_type.is_dir() {
        path.push('/');
        WorkspaceEntryKind::Directory
    } else {
        WorkspaceEntryKind::File
    };
    WorkspaceEntry { name, path, kind }
}

fn sort_workspace_entries(entries: &mut [WorkspaceEntry]) {
    entries.sort_by(|left, right| match (&left.kind, &right.kind) {
        (WorkspaceEntryKind::Directory, WorkspaceEntryKind::File) => Ordering::Less,
        (WorkspaceEntryKind::File, WorkspaceEntryKind::Directory) => Ordering::Greater,
        _ => left
            .path
            .to_ascii_lowercase()
            .cmp(&right.path.to_ascii_lowercase()),
    });
}

fn query_includes_hidden_path(query: &str) -> bool {
    query
        .split('/')
        .any(|part| part.starts_with('.') && part.len() > 1)
}

fn should_descend_workspace_directory(name: &str, include_hidden: bool, query: &str) -> bool {
    if name.starts_with('.') && !include_hidden {
        return false;
    }

    const LARGE_DIRECTORY_NAMES: &[&str] = &[
        "node_modules",
        "target",
        "dist",
        "build",
        ".git",
        ".expo",
        ".next",
    ];
    !LARGE_DIRECTORY_NAMES
        .iter()
        .any(|large_name| name == *large_name && !query.contains(large_name))
}

fn normalize_relative_query(raw: &str) -> Result<PathBuf, AppError> {
    let trimmed = raw.trim().trim_start_matches("./");
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(AppError::InvalidRequest(
            "absolute mention paths are not allowed".to_string(),
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::InvalidRequest(
                    "mention path cannot escape the workspace".to_string(),
                ));
            }
        }
    }
    Ok(normalized)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

async fn skill_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(resource_id): AxumPath<String>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<crate::catalog::SkillResource>, AppError> {
    require_auth(&state, &headers)?;
    let workspace = validate_catalog_workspace(&state, &query.workspace)?;
    Ok(Json(
        state
            .catalog
            .skill_resource(query.provider, workspace, &resource_id)
            .await?,
    ))
}

async fn mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<crate::catalog::McpCatalog>, AppError> {
    require_auth(&state, &headers)?;
    let workspace = validate_catalog_workspace(&state, &query.workspace)?;
    Ok(Json(state.catalog.mcp(query.provider, workspace).await?))
}

fn validate_catalog_workspace(state: &AppState, workspace: &str) -> Result<PathBuf, AppError> {
    validate_workspace_directory_text(&state.catalog.config().workspace_root, workspace)
}

async fn providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let mut providers = serde_json::to_value(state.conversations.providers())?;
    if let Some(items) = providers.as_array_mut() {
        for provider in items {
            let capabilities = provider
                .get("capabilities")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut actions = Vec::new();
            if capabilities
                .get("cancel")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                actions.push("cancel");
                actions.push("interrupt");
            }
            if let Some(object) = provider.as_object_mut() {
                if let Some(capabilities) = object
                    .get_mut("capabilities")
                    .and_then(Value::as_object_mut)
                {
                    capabilities.insert("controlActions".to_owned(), json!(actions));
                }
            }
        }
    }
    Ok(Json(json!({ "providers": providers })))
}

async fn provider_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CliVersionsResponse>, AppError> {
    require_auth(&state, &headers)?;
    match state.cli_execution_gate.clone().try_read_owned() {
        Ok(_permit) => Ok(Json(state.cli_manager.versions(&state.config).await)),
        Err(_) => state
            .cli_manager
            .cached_versions_while_busy()
            .await
            .map(Json)
            .ok_or_else(|| {
                AppError::Conflict("CLI versions are unavailable during an upgrade".to_owned())
            }),
    }
}

async fn upgrade_provider_cli(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(provider): AxumPath<ManagedCli>,
) -> Result<Json<CliUpgradeOperation>, AppError> {
    let auth = require_auth(&state, &headers)?;
    if state.config.security.auth_token.is_none() {
        return Err(AppError::Unauthorized(
            "CLI upgrades require bearer authentication".to_owned(),
        ));
    }
    let execution_guard = match state.cli_execution_gate.clone().try_write_owned() {
        Ok(guard) => guard,
        Err(_) => {
            append_cli_audit(
                &state,
                &auth,
                provider,
                None,
                "deny",
                Some("CLI_EXECUTION_BUSY"),
            )
            .await?;
            return Err(AppError::Conflict(
                "an Agent or CLI upgrade is starting".to_owned(),
            ));
        }
    };
    if state.conversations.has_active_turns() || state.codex_local_adapters.has_active_adapters() {
        append_cli_audit(&state, &auth, provider, None, "deny", Some("AGENT_ACTIVE")).await?;
        return Err(AppError::Conflict(
            "finish active Agent tasks before upgrading a CLI".to_owned(),
        ));
    }
    let previous_version = match read_current_version(&state.config, provider).await {
        Ok(version) => version,
        Err(error) => {
            append_cli_audit(
                &state,
                &auth,
                provider,
                None,
                "deny",
                Some("CLI_UNAVAILABLE"),
            )
            .await?;
            return Err(error);
        }
    };
    let operation = match state
        .cli_manager
        .begin_upgrade(provider, previous_version)
        .await
    {
        Ok(operation) => operation,
        Err(error) => {
            append_cli_audit(
                &state,
                &auth,
                provider,
                None,
                "deny",
                Some("UPGRADE_IN_PROGRESS"),
            )
            .await?;
            return Err(error);
        }
    };
    if let Err(error) = append_cli_audit(
        &state,
        &auth,
        provider,
        Some(&operation.id),
        "attempt",
        None,
    )
    .await
    {
        state
            .cli_manager
            .complete_upgrade(
                &operation.id,
                Err(AppError::ProviderUnavailable(
                    "CLI upgrade cancelled because the audit record could not be persisted"
                        .to_owned(),
                )),
            )
            .await;
        return Err(error);
    }

    let background_state = state.clone();
    let background_auth = auth.clone();
    let operation_id = operation.id.clone();
    tokio::spawn(async move {
        let _execution_guard = execution_guard;
        let result = run_upgrade(&background_state.config, provider).await;
        let decision = if result.is_ok() { "success" } else { "failure" };
        let reason = result.as_ref().err().map(ToString::to_string);
        if let Some(completed) = background_state
            .cli_manager
            .complete_upgrade(&operation_id, result)
            .await
        {
            append_cli_audit(
                &background_state,
                &background_auth,
                completed.provider,
                Some(&completed.id),
                decision,
                reason.as_ref().map(|_| "CLI_UPGRADE_FAILED"),
            )
            .await
            .unwrap_or_else(|error| {
                warn!(error = %error, "failed to persist final CLI upgrade audit event");
            });
        }
    });
    Ok(Json(operation))
}

async fn provider_upgrade_operation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(operation_id): AxumPath<String>,
) -> Result<Json<CliUpgradeOperation>, AppError> {
    require_auth(&state, &headers)?;
    state
        .cli_manager
        .operation(&operation_id)
        .await
        .map(Json)
        .ok_or_else(|| AppError::NotFound("CLI upgrade operation not found".to_owned()))
}

async fn append_cli_audit(
    state: &AppState,
    auth: &AuthContext,
    provider: ManagedCli,
    operation_id: Option<&str>,
    decision: &str,
    reason: Option<&str>,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "cli.upgrade.audit",
        None,
        None,
        None,
        json!({
            "principal_id": auth.principal_id,
            "tenant_id": auth.tenant_id,
            "token_id": auth.token_id,
            "operation_id": operation_id,
            "provider": provider,
            "decision": decision,
            "reason": reason,
        }),
    );
    websocket::append_audit_event(state, &event).await?;
    state.events.publish(event).await;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderModelsQuery {
    provider: ProviderKind,
    workspace: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderImageInputQuery {
    provider: ProviderKind,
    workspace: String,
    profile: Option<String>,
    model: Option<String>,
}

async fn provider_image_input(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProviderImageInputQuery>,
) -> Result<Json<crate::provider::types::ProviderImageInputCapability>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace =
        validate_workspace_directory_text(&state.config.workspace_root, &query.workspace)?;
    Ok(Json(
        state
            .conversations
            .image_input_live(
                &auth.tenant_id,
                query.provider,
                &workspace,
                query.profile.as_deref(),
                query.model.as_deref(),
            )
            .await?,
    ))
}

async fn provider_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProviderModelsQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace =
        validate_workspace_directory_text(&state.config.workspace_root, &query.workspace)?;
    let models = state
        .conversations
        .models_live(&auth.tenant_id, query.provider, &workspace)
        .await?;
    Ok(Json(
        json!({ "provider": query.provider, "models": models, "source": "provider-discovery", "fetchedAt": chrono::Utc::now().to_rfc3339() }),
    ))
}

async fn provider_commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProviderModelsQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let workspace =
        validate_workspace_directory_text(&state.config.workspace_root, &query.workspace)?;
    let commands = state
        .conversations
        .commands_live(&auth.tenant_id, query.provider, &workspace)
        .await?;
    Ok(Json(json!({
        "provider": query.provider,
        "commands": commands,
        "source": "provider-discovery",
        "fetchedAt": chrono::Utc::now().to_rfc3339(),
    })))
}

async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    Ok(Json(
        json!({ "conversations": state.conversations.list_owned(&auth.tenant_id).await? }),
    ))
}

async fn create_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateConversationRequest>,
) -> Result<Json<ConversationManifest>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let provider = match request.provider {
        Some(provider) => provider,
        None => state
            .config
            .agent
            .default_agent
            .parse()
            .map_err(AppError::InvalidRequest)?,
    };
    let manifest = state
        .conversations
        .create_owned(
            &auth.tenant_id,
            provider,
            request.workspace,
            request.title,
            request.provider_profile,
        )
        .await?;
    Ok(Json(manifest))
}

async fn get_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<ConversationManifest>, AppError> {
    let auth = require_auth(&state, &headers)?;
    Ok(Json(
        state
            .conversations
            .get_owned(&auth.tenant_id, &conversation_id)
            .await?,
    ))
}

async fn update_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Json(request): Json<UpdateConversationRequest>,
) -> Result<Json<ConversationManifest>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let manifest = state
        .conversations
        .update_metadata_owned(
            &auth.tenant_id,
            &conversation_id,
            request.title.map(Some),
            request.archived,
        )
        .await?;
    Ok(Json(manifest))
}

async fn delete_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let manifest = state
        .conversations
        .delete_owned(&auth.tenant_id, &conversation_id)
        .await?;
    Ok(Json(
        json!({ "conversationId": manifest.id, "deleted": true }),
    ))
}

async fn replay_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<ReplayQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(
        state
            .conversations
            .replay_owned(
                &auth.tenant_id,
                &conversation_id,
                query.after_sequence.unwrap_or(0),
                query.limit.unwrap_or(200),
            )
            .await?,
    )?))
}

async fn prompt_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let turn_id = state
        .conversations
        .prompt_owned(
            &auth.tenant_id,
            &conversation_id,
            ConversationPrompt {
                text: request.text,
                model: request.model,
                reasoning_effort: request.reasoning_effort,
                permission_profile: request.permission_profile,
                sandbox_mode: request.sandbox_mode,
                approval_policy: request.approval_policy,
                skills: prompt_skills(request.skills),
                content: request.content,
            },
        )
        .await?;
    Ok(Json(
        json!({ "conversationId": conversation_id, "turnId": turn_id }),
    ))
}

async fn cancel_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .cancel_owned(&auth.tenant_id, &conversation_id)
        .await?;
    Ok(Json(
        json!({ "conversationId": conversation_id, "accepted": true }),
    ))
}

async fn resolve_permission(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((conversation_id, permission_id)): AxumPath<(String, String)>,
    Json(decision): Json<PermissionDecision>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .resolve_permission_owned(&auth.tenant_id, &conversation_id, &permission_id, decision)
        .await?;
    Ok(Json(json!({
        "conversationId": conversation_id,
        "permissionId": permission_id,
        "accepted": true,
    })))
}

async fn ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    // Parity with the v2 HTTP `require_auth` and the retired `/v1/ws`: a
    // deployment without a configured token accepts anonymous local
    // connections under the synthetic `local` principal.
    let auth = match websocket::authenticate_headers_or_query(&state, &headers, uri.query()) {
        Some(auth) => auth,
        None if state.config.security.auth_token.is_none() => AuthContext {
            principal_id: "local".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "none".to_owned(),
        },
        None => return Err(AppError::Unauthenticated),
    };
    let crypto = websocket::transport_crypto_from_handshake(&state, &headers, uri.query())?;
    Ok(ws.on_upgrade(move |socket| handle_socket(state, socket, crypto, auth)))
}

async fn handle_socket(
    state: AppState,
    socket: WebSocket,
    crypto: Option<TransportCryptoSession>,
    auth: AuthContext,
) {
    let authenticated = state.config.security.auth_token.is_some();
    let active_connections = state.increment_websocket_connections();
    state
        .events
        .publish(crate::event::EventRecord::new(
            "server.websocket.connected",
            None,
            None,
            None,
            json!({
                "active_connections": active_connections,
                "authenticated": authenticated,
                "principal_id": auth.principal_id,
                "encrypted": crypto.is_some(),
                "encryption_protocol": crypto.as_ref().map(|crypto| crypto.protocol().as_str()),
                "plane": "v2",
            }),
        ))
        .await;

    // Legacy-plane authorization parity with `/v1/ws`: without a configured
    // token the legacy dispatcher trusted any tenant, so pass `None` instead
    // of the synthetic local principal the v2 layer always produces.
    let legacy_auth = authenticated.then(|| auth.clone());

    let (mut sender, mut receiver) = socket.split();
    let mut event_rx = state.events.subscribe();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Value>(256);
    let sender_crypto = crypto.clone();
    let send_task = tokio::spawn(async move {
        let mut ping_interval =
            tokio::time::interval(tokio::time::Duration::from_secs(WS_PING_INTERVAL_SECS));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                value = outgoing_rx.recv() => {
                    let Some(value) = value else { break };
                    let text = match serde_json::to_string(&value) {
                        Ok(text) => text,
                        Err(error) => {
                            warn!(error = %error, "failed to serialize v2 websocket event");
                            continue;
                        }
                    };
                    let text = match &sender_crypto {
                        Some(crypto) => match crypto.encrypt_server_text(&text) {
                            Ok(text) => text,
                            Err(error) => {
                                warn!(error = %error, "failed to encrypt v2 websocket event");
                                break;
                            }
                        },
                        None => text,
                    };
                    if sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                _ = ping_interval.tick() => {
                    if sender.send(Message::Ping(Default::default())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let event_scope = Arc::new(tokio::sync::RwLock::new(
        websocket::LegacyEventScope::default(),
    ));
    let bus_event_scope = event_scope.clone();
    let bus_outgoing_tx = outgoing_tx.clone();
    let bus_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let visible = {
                        let scope = bus_event_scope.read().await;
                        websocket::legacy_event_is_visible(&event, &scope)
                    };
                    if !visible {
                        continue;
                    }
                    if let Ok(value) = serde_json::to_value(ServerEvent::from(event)) {
                        if bus_outgoing_tx.send(value).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    if let Ok(value) = serde_json::to_value(websocket::direct_error_event(
                        AppError::StreamLagged(skipped),
                    )) {
                        if bus_outgoing_tx.send(value).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    if let Ok(value) =
                        serde_json::to_value(websocket::direct_error_event(AppError::StreamClosed))
                    {
                        let _ = bus_outgoing_tx.send(value).await;
                    }
                    break;
                }
            }
        }
    });

    let subscriptions = Arc::new(Mutex::new(HashSet::<String>::new()));
    let mut subscription_tasks = Vec::new();
    let operation_limit = Arc::new(Semaphore::new(MAX_WS_IN_FLIGHT_OPERATIONS));
    let mut operation_tasks = Vec::new();
    let client_timeout = tokio::time::Duration::from_secs(WS_CLIENT_TIMEOUT_SECS);
    loop {
        let frame = match tokio::time::timeout(client_timeout, receiver.next()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => {
                warn!(
                    timeout_secs = WS_CLIENT_TIMEOUT_SECS,
                    "v2 websocket client inactive, closing connection"
                );
                break;
            }
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                warn!(error = %error, "v2 websocket receive failed");
                break;
            }
        };
        let Message::Text(text) = frame else {
            if matches!(frame, Message::Close(_)) {
                break;
            }
            continue;
        };
        if text.len() > MAX_WS_MESSAGE_BYTES {
            let _ = outgoing_tx
                .send(error_response(
                    None,
                    AppError::InvalidRequest("websocket message is too large".to_owned()),
                ))
                .await;
            continue;
        }
        let text = match &crypto {
            Some(crypto) => match crypto.decrypt_client_text(&text) {
                Ok(text) => text,
                Err(error) => {
                    let _ = outgoing_tx.send(error_response(None, error)).await;
                    continue;
                }
            },
            None => text.to_string(),
        };
        let command_type = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned));
        match command_type.as_deref() {
            Some(command_type) if is_v2_native_command(command_type) => {
                let command: V2Command = match serde_json::from_str(&text) {
                    Ok(command) => command,
                    Err(error) => {
                        let _ = outgoing_tx
                            .send(error_response(
                                None,
                                AppError::InvalidRequest(format!(
                                    "invalid v2 websocket command: {error}"
                                )),
                            ))
                            .await;
                        continue;
                    }
                };
                if is_v2_background_command(&command.command_type) {
                    let permit = match operation_limit.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            let _ = outgoing_tx
                                .send(error_response(
                                    Some(command.id),
                                    AppError::ResourceExhausted(format!(
                                        "v2 websocket allows at most {MAX_WS_IN_FLIGHT_OPERATIONS} concurrent operations"
                                    )),
                                ))
                                .await;
                            continue;
                        }
                    };
                    operation_tasks
                        .retain(|task: &tokio::task::JoinHandle<()>| !task.is_finished());
                    let operation_state = state.clone();
                    let operation_outgoing = outgoing_tx.clone();
                    let owner_id = auth.tenant_id.clone();
                    operation_tasks.push(tokio::spawn(async move {
                        let _permit = permit;
                        let response =
                            dispatch_mcp_command_response(&operation_state, &owner_id, command)
                                .await;
                        let _ = operation_outgoing.send(response).await;
                    }));
                    continue;
                }
                let response = dispatch_command(
                    &state,
                    &outgoing_tx,
                    &subscriptions,
                    &event_scope,
                    &mut subscription_tasks,
                    &auth.tenant_id,
                    command,
                )
                .await;
                if let Some(response) = response {
                    let _ = outgoing_tx.send(response).await;
                }
            }
            _ => {
                // Legacy command plane: same scoped dispatch, error events and
                // scope semantics as the previous `/v1/ws` socket.
                if let Err(error) = websocket::dispatch_scoped_client_text(
                    &text,
                    &state,
                    legacy_auth.as_ref(),
                    &event_scope,
                )
                .await
                {
                    if let Ok(value) = serde_json::to_value(websocket::direct_error_event(error)) {
                        let _ = outgoing_tx.send(value).await;
                    }
                }
            }
        }
    }

    for task in subscription_tasks {
        task.abort();
    }
    for task in operation_tasks {
        task.abort();
        let _ = task.await;
    }
    bus_task.abort();
    drop(outgoing_tx);
    let _ = send_task.await;

    let active_connections = state.decrement_websocket_connections();
    state
        .events
        .publish(crate::event::EventRecord::new(
            "server.websocket.disconnected",
            None,
            None,
            None,
            json!({
                "active_connections": active_connections,
                "authenticated": authenticated,
                "encrypted": crypto.is_some(),
                "encryption_protocol": crypto.as_ref().map(|crypto| crypto.protocol().as_str()),
                "plane": "v2",
            }),
        ))
        .await;
}

async fn dispatch_command(
    state: &AppState,
    outgoing: &mpsc::Sender<Value>,
    subscriptions: &Arc<Mutex<HashSet<String>>>,
    event_scope: &Arc<tokio::sync::RwLock<websocket::LegacyEventScope>>,
    subscription_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    owner_id: &str,
    command: V2Command,
) -> Option<Value> {
    let result = dispatch_command_inner(
        state,
        outgoing,
        subscriptions,
        event_scope,
        subscription_tasks,
        owner_id,
        &command,
    )
    .await;
    Some(match result {
        Ok(payload) => json!({
            "id": command.id,
            "type": "server.result",
            "payload": payload,
        }),
        Err(error) => error_response(Some(command.id), error),
    })
}

async fn dispatch_command_inner(
    state: &AppState,
    outgoing: &mpsc::Sender<Value>,
    subscriptions: &Arc<Mutex<HashSet<String>>>,
    event_scope: &Arc<tokio::sync::RwLock<websocket::LegacyEventScope>>,
    subscription_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    owner_id: &str,
    command: &V2Command,
) -> Result<Value, AppError> {
    match command.command_type.as_str() {
        "conversation.subscribe" => {
            let request: SubscribeRequest = serde_json::from_value(command.payload.clone())?;
            state
                .conversations
                .get_owned(owner_id, &request.conversation_id)
                .await?;
            let subscriptions_guard = subscriptions.lock().await;
            if subscriptions_guard.contains(&request.conversation_id) {
                return Ok(json!({
                    "conversationId": request.conversation_id,
                    "subscribed": true,
                    "alreadySubscribed": true,
                }));
            }
            if subscriptions_guard.len() >= MAX_WS_SUBSCRIPTIONS {
                return Err(AppError::InvalidRequest(
                    "v2 websocket subscription limit reached".to_owned(),
                ));
            }
            drop(subscriptions_guard);

            let mut receiver = state.conversations.subscribe(&request.conversation_id);
            let high_water = state
                .conversations
                .get_owned(owner_id, &request.conversation_id)
                .await?
                .last_sequence;
            let mut replay_cursor = request.after_sequence.unwrap_or(0).min(high_water);
            let page_size = request.limit.unwrap_or(500);
            while replay_cursor < high_water {
                let replay = state
                    .conversations
                    .replay_owned(owner_id, &request.conversation_id, replay_cursor, page_size)
                    .await?;
                let mut advanced = false;
                for event in replay
                    .events
                    .into_iter()
                    .take_while(|event| event.sequence <= high_water)
                {
                    replay_cursor = event.sequence;
                    advanced = true;
                    outgoing
                        .send(json!({ "type": "conversation.event", "payload": event }))
                        .await
                        .map_err(|_| AppError::StreamClosed)?;
                }
                if !advanced {
                    return Err(AppError::Conflict(format!(
                        "conversation {} replay did not reach sequence {high_water}",
                        request.conversation_id
                    )));
                }
            }
            subscriptions
                .lock()
                .await
                .insert(request.conversation_id.clone());
            let outgoing = outgoing.clone();
            let conversation_id = request.conversation_id.clone();
            let owner_id = owner_id.to_owned();
            let conversations = state.conversations.clone();
            let active_subscriptions = subscriptions.clone();
            subscription_tasks.push(tokio::spawn(async move {
                let mut delivered_through = high_water;
                loop {
                    match receiver.recv().await {
                        Ok(event) if event.sequence > delivered_through => {
                            delivered_through = event.sequence;
                            if outgoing
                                .send(json!({ "type": "conversation.event", "payload": event }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            if outgoing
                                .send(json!({
                                    "type": "server.error",
                                    "payload": {
                                        "code": "EVENT_STREAM_LAGGED",
                                        "message": format!("conversation {conversation_id} stream lagged by {skipped} events; replaying persisted events"),
                                    }
                                }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            let recovery = async {
                                let recovery_high_water = conversations
                                    .get_owned(&owner_id, &conversation_id)
                                    .await?
                                    .last_sequence;
                                while delivered_through < recovery_high_water {
                                    let replay = conversations
                                        .replay_owned(
                                            &owner_id,
                                            &conversation_id,
                                            delivered_through,
                                            page_size,
                                        )
                                        .await?;
                                    let mut advanced = false;
                                    for event in replay.events.into_iter().take_while(|event| {
                                        event.sequence <= recovery_high_water
                                    }) {
                                        delivered_through = event.sequence;
                                        advanced = true;
                                        outgoing
                                            .send(json!({
                                                "type": "conversation.event",
                                                "payload": event,
                                            }))
                                            .await
                                            .map_err(|_| AppError::StreamClosed)?;
                                    }
                                    if !advanced {
                                        return Err(AppError::Conflict(format!(
                                            "conversation {conversation_id} replay did not reach sequence {recovery_high_water}"
                                        )));
                                    }
                                }
                                Ok::<(), AppError>(())
                            }
                            .await;
                            if let Err(error) = recovery {
                                let _ = outgoing.send(error_response(None, error)).await;
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                active_subscriptions.lock().await.remove(&conversation_id);
            }));
            Ok(json!({
                "conversationId": request.conversation_id,
                "subscribed": true,
                "nextSequence": high_water,
                "hasMore": false,
            }))
        }
        "conversation.create" => {
            let request: CreateConversationRequest =
                serde_json::from_value(command.payload.clone())?;
            let provider = match request.provider {
                Some(provider) => provider,
                None => state
                    .config
                    .agent
                    .default_agent
                    .parse()
                    .map_err(AppError::InvalidRequest)?,
            };
            Ok(serde_json::to_value(
                state
                    .conversations
                    .create_owned(
                        owner_id,
                        provider,
                        request.workspace,
                        request.title,
                        request.provider_profile,
                    )
                    .await?,
            )?)
        }
        "conversation.prompt" | "conversation.followUp" => {
            let request: WsConversationRequest = serde_json::from_value(command.payload.clone())?;
            let text = request.text.unwrap_or_default();
            if text.trim().is_empty() && request.skills.is_empty() && request.content.is_empty() {
                return Err(AppError::InvalidRequest(
                    "conversation.prompt requires text, content, or skills".to_owned(),
                ));
            }
            let turn_id = state
                .conversations
                .prompt_owned(
                    owner_id,
                    &request.conversation_id,
                    ConversationPrompt {
                        text,
                        model: request.model,
                        reasoning_effort: request.reasoning_effort,
                        permission_profile: request.permission_profile,
                        sandbox_mode: request.sandbox_mode,
                        approval_policy: request.approval_policy,
                        skills: prompt_skills(request.skills),
                        content: request.content,
                    },
                )
                .await?;
            Ok(json!({ "conversationId": request.conversation_id, "turnId": turn_id }))
        }
        "conversation.retry" => {
            let request: WsConversationRequest = serde_json::from_value(command.payload.clone())?;
            let turn_id = state
                .conversations
                .retry_owned(owner_id, &request.conversation_id)
                .await?;
            Ok(
                json!({ "conversationId": request.conversation_id, "turnId": turn_id, "retried": true }),
            )
        }
        "conversation.resume" => {
            let request: WsConversationRequest = serde_json::from_value(command.payload.clone())?;
            let turn_id = state
                .conversations
                .retry_owned(owner_id, &request.conversation_id)
                .await?;
            Ok(
                json!({ "conversationId": request.conversation_id, "turnId": turn_id, "resumed": true }),
            )
        }
        "conversation.cancel" | "conversation.interrupt" | "conversation.stop" => {
            let request: WsConversationRequest = serde_json::from_value(command.payload.clone())?;
            state
                .conversations
                .cancel_owned(owner_id, &request.conversation_id)
                .await?;
            Ok(json!({ "conversationId": request.conversation_id, "accepted": true }))
        }
        "conversation.permission.respond" => {
            let request: WsPermissionRequest = serde_json::from_value(command.payload.clone())?;
            state
                .conversations
                .resolve_permission_owned(
                    owner_id,
                    &request.conversation_id,
                    &request.permission_id,
                    request.decision,
                )
                .await?;
            Ok(json!({
                "conversationId": request.conversation_id,
                "permissionId": request.permission_id,
                "accepted": true,
            }))
        }
        "mcp.list" | "mcp.refresh" | "mcp.call" => {
            dispatch_mcp_command(state, owner_id, command).await
        }
        "server.ping" => Ok(json!({ "pong": true })),
        "session.resume" => {
            // Replaces the transport-hello session cursor replay: grant this
            // connection visibility for still-existing Codex sessions and
            // replay gateway events after each client cursor. Replay events
            // reach the client through the scoped legacy event stream above.
            let request: SessionResumeRequest = serde_json::from_value(command.payload.clone())?;
            let resumed =
                websocket::resume_session_cursors(state, event_scope, &request.session_cursors)
                    .await?;
            Ok(json!({ "resumed": resumed }))
        }
        other => Err(AppError::Unsupported(format!(
            "v2 websocket command {other}"
        ))),
    }
}

async fn dispatch_mcp_command_response(
    state: &AppState,
    owner_id: &str,
    command: V2Command,
) -> Value {
    match dispatch_mcp_command(state, owner_id, &command).await {
        Ok(payload) => json!({
            "id": command.id,
            "type": "server.result",
            "payload": payload,
        }),
        Err(error) => error_response(Some(command.id), error),
    }
}

async fn dispatch_mcp_command(
    state: &AppState,
    owner_id: &str,
    command: &V2Command,
) -> Result<Value, AppError> {
    let request: WsMcpRequest = serde_json::from_value(command.payload.clone())?;
    match command.command_type.as_str() {
        "mcp.list" => Ok(serde_json::to_value(
            state
                .conversations
                .list_mcp_owned(owner_id, &request.conversation_id)
                .await?,
        )?),
        "mcp.refresh" => {
            let resource_id = request.resource_id.ok_or_else(|| {
                AppError::InvalidRequest("mcp.refresh requires resourceId".to_owned())
            })?;
            Ok(serde_json::to_value(
                state
                    .conversations
                    .refresh_mcp_owned(owner_id, &request.conversation_id, &resource_id)
                    .await?,
            )?)
        }
        "mcp.call" => {
            let resource_id = request.resource_id.ok_or_else(|| {
                AppError::InvalidRequest("mcp.call requires resourceId".to_owned())
            })?;
            let tool_name = request
                .tool_name
                .ok_or_else(|| AppError::InvalidRequest("mcp.call requires toolName".to_owned()))?;
            let result = state
                .conversations
                .call_mcp_owned(
                    owner_id,
                    &request.conversation_id,
                    &resource_id,
                    &tool_name,
                    request.arguments.unwrap_or(Value::Null),
                )
                .await?;
            Ok(json!({
                "conversationId": request.conversation_id,
                "resourceId": resource_id,
                "toolName": tool_name,
                "result": result,
            }))
        }
        _ => Err(AppError::Unsupported(format!(
            "v2 websocket command {}",
            command.command_type
        ))),
    }
}

fn prompt_skills(skills: Vec<PromptSkillRequest>) -> Vec<PromptSkillRef> {
    skills
        .into_iter()
        .map(|skill| PromptSkillRef {
            resource_id: skill.resource_id,
            name: skill.name,
        })
        .collect()
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<AuthContext, AppError> {
    if state.config.security.auth_token.is_none() {
        return Ok(AuthContext {
            principal_id: "local".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "none".to_owned(),
        });
    }
    websocket::authenticate_headers(state, headers).ok_or(AppError::Unauthenticated)
}

async fn append_git_audit(
    state: &AppState,
    auth: &AuthContext,
    action: &str,
    workspace: &Path,
    repository: Option<&Path>,
    decision: &str,
    reason_code: &str,
    repository_count: Option<usize>,
    output_bytes: Option<usize>,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "git.audit",
        None,
        None,
        None,
        json!({
            "request_id": format!("git_{}", uuid::Uuid::new_v4().simple()),
            "principal_id": auth.principal_id.as_str(),
            "tenant_id": auth.tenant_id.as_str(),
            "token_id": auth.token_id.as_str(),
            "action": action,
            "decision": decision,
            "reason_code": reason_code,
            "target_kind": "repository",
            "workspace_path": workspace.display().to_string(),
            "repository_path": repository.map(|path| path.display().to_string()),
            "repository_count": repository_count,
            "output_bytes": output_bytes,
            "protocol": "git.v2",
        }),
    );
    websocket::append_audit_event(state, &event).await?;
    state.events.publish(event).await;
    Ok(())
}

fn combine_git_result<T>(
    operation: Result<T, AppError>,
    audit: Result<(), AppError>,
) -> Result<T, AppError> {
    match (operation, audit) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(value), Err(audit_error)) => {
            warn!(audit_error = %audit_error, "Git operation completed but audit persistence failed");
            Ok(value)
        }
        (Err(operation_error), Ok(())) => Err(operation_error),
        (Err(operation_error), Err(audit_error)) => {
            warn!(
                operation_error = %operation_error,
                audit_error = %audit_error,
                "failed to persist Git audit event"
            );
            Err(operation_error)
        }
    }
}

fn truncate_git_error(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn error_response(id: Option<String>, error: AppError) -> Value {
    json!({
        "id": id,
        "type": "server.error",
        "payload": { "code": error.code(), "message": error.to_string() },
    })
}

#[derive(Debug, Serialize)]
pub(super) struct VersionResponse {
    name: &'static str,
    version: &'static str,
    data_dir: String,
    workspace_root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReplaceWorkspacesRequest {
    workspaces: Vec<WorkspaceRecord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspacesResponse {
    workspaces: Vec<WorkspaceRecord>,
    updated_at: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct UpdateWorkspaceTrustRequest {
    trusted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceEntriesQuery {
    cwd: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceDirectoriesQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(super) struct WorkspaceFileQuery {
    path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceFileResponse {
    name: String,
    path: String,
    mime_type: String,
    size_bytes: u64,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct BrowserFetchRequest {
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BrowserFetchResponse {
    url: String,
    status: u16,
    content_type: String,
    body: String,
}

#[derive(Debug, Serialize)]
pub(super) struct WorkspaceEntriesResponse {
    entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceDirectoriesResponse {
    root: String,
    current: String,
    parent: Option<String>,
    entries: Vec<WorkspaceDirectory>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum WorkspaceEntryKind {
    Directory,
    File,
}

#[derive(Debug, Serialize)]
struct WorkspaceEntry {
    name: String,
    path: String,
    kind: WorkspaceEntryKind,
}

#[derive(Debug, Serialize)]
struct WorkspaceDirectory {
    name: String,
    path: String,
    kind: WorkspaceEntryKind,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateConversationRequest {
    #[serde(default)]
    provider: Option<ProviderKind>,
    workspace: PathBuf,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    provider_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateConversationRequest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayQuery {
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptSkillRequest {
    resource_id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptRequest {
    #[serde(default)]
    text: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    skills: Vec<PromptSkillRequest>,
    #[serde(default)]
    content: Vec<PromptContentRef>,
    #[serde(default)]
    permission_profile: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<String>,
    #[serde(default)]
    approval_policy: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogQuery {
    provider: ProviderKind,
    workspace: String,
}

#[derive(Debug, Deserialize)]
struct V2Command {
    id: String,
    #[serde(rename = "type")]
    command_type: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeRequest {
    conversation_id: String,
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionResumeRequest {
    #[serde(default)]
    session_cursors: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsConversationRequest {
    conversation_id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    skills: Vec<PromptSkillRequest>,
    #[serde(default)]
    content: Vec<PromptContentRef>,
    #[serde(default)]
    permission_profile: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<String>,
    #[serde(default)]
    approval_policy: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsMcpRequest {
    conversation_id: String,
    #[serde(default)]
    resource_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsPermissionRequest {
    conversation_id: String,
    permission_id: String,
    decision: PermissionDecision,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::process::Command as StdCommand;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use crate::conversation::{ConversationEventHub, ConversationStore};
    use crate::provider::ConversationSupervisor;

    #[test]
    fn browser_url_allowlist_accepts_only_loopback_hosts() {
        for url in [
            "http://localhost",
            "http://127.0.0.1:5173",
            "http://[::1]:3000",
        ] {
            let parsed = reqwest::Url::parse(&validate_browser_url(url).unwrap()).unwrap();
            assert!(
                is_allowed_browser_target(&parsed),
                "expected allowed URL: {url}"
            );
        }
        for url in [
            "https://example.com",
            "http://192.168.1.2",
            "http://localhost.evil",
        ] {
            let parsed = reqwest::Url::parse(&validate_browser_url(url).unwrap()).unwrap();
            assert!(
                !is_allowed_browser_target(&parsed),
                "expected blocked URL: {url}"
            );
        }
    }

    #[tokio::test]
    async fn v2_http_requires_auth_and_persists_an_owned_conversation() {
        let root = std::env::temp_dir().join(format!("todex-v2-http-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("v2-token".to_owned()),
            },
        })
        .await
        .unwrap();
        let app = crate::server::router(state);

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v2/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let create = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/conversations")
                    .header("authorization", "Bearer v2-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "provider": "codex",
                            "workspace": workspace,
                            "title": "HTTP fixture",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created: ConversationManifest =
            serde_json::from_slice(&to_bytes(create.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(created.owner_id, "local");

        let replay = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v2/conversations/{}/events", created.id))
                    .header("authorization", "Bearer v2-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn v2_subscription_replays_through_high_water_and_clamps_future_cursors() {
        let root = std::env::temp_dir().join(format!("todex-v2-replay-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mut state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("v2-token".to_owned()),
            },
        })
        .await
        .unwrap();
        let store = ConversationStore::new(state.config.data_dir.clone())
            .await
            .unwrap();
        let hub = ConversationEventHub::default();
        state.conversations = ConversationSupervisor::new(
            state.config.clone(),
            store.clone(),
            hub.clone(),
            state.workspace_trust.clone(),
        );
        let manifest = state
            .conversations
            .create_owned(
                "local",
                ProviderKind::Codex,
                workspace,
                Some("Replay fixture".to_owned()),
                None,
            )
            .await
            .unwrap();
        for index in 1..=3 {
            store
                .append(&manifest.id, "fixture.event", json!({ "index": index }))
                .await
                .unwrap();
        }

        let (outgoing, mut events) = mpsc::channel(16);
        let subscriptions = Arc::new(Mutex::new(HashSet::new()));
        let event_scope = Arc::new(tokio::sync::RwLock::new(
            websocket::LegacyEventScope::default(),
        ));
        let mut subscription_tasks = Vec::new();
        let result = dispatch_command_inner(
            &state,
            &outgoing,
            &subscriptions,
            &event_scope,
            &mut subscription_tasks,
            "local",
            &V2Command {
                id: "subscribe-1".to_owned(),
                command_type: "conversation.subscribe".to_owned(),
                payload: json!({
                    "conversationId": manifest.id,
                    "afterSequence": 0,
                    "limit": 1,
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["nextSequence"], 4);
        assert_eq!(result["hasMore"], false);

        let mut sequences = Vec::new();
        for _ in 0..4 {
            let event = events.recv().await.expect("replayed conversation event");
            sequences.push(event["payload"]["sequence"].as_u64().unwrap());
        }
        assert_eq!(sequences, vec![1, 2, 3, 4]);
        for task in subscription_tasks {
            task.abort();
        }

        let (future_outgoing, mut future_events) = mpsc::channel(16);
        let future_subscriptions = Arc::new(Mutex::new(HashSet::new()));
        let future_scope = Arc::new(tokio::sync::RwLock::new(
            websocket::LegacyEventScope::default(),
        ));
        let mut future_tasks = Vec::new();
        let result = dispatch_command_inner(
            &state,
            &future_outgoing,
            &future_subscriptions,
            &future_scope,
            &mut future_tasks,
            "local",
            &V2Command {
                id: "subscribe-future".to_owned(),
                command_type: "conversation.subscribe".to_owned(),
                payload: json!({
                    "conversationId": manifest.id,
                    "afterSequence": 10_000,
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["nextSequence"], 4);
        assert!(matches!(
            future_events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let live = store
            .append(&manifest.id, "fixture.live", json!({}))
            .await
            .unwrap();
        hub.publish(live);
        let received = tokio::time::timeout(Duration::from_secs(1), future_events.recv())
            .await
            .expect("future-cursor subscription should receive a live event")
            .expect("future-cursor subscription channel should remain open");
        assert_eq!(received["payload"]["sequence"], 5);
        for task in future_tasks {
            task.abort();
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recursive_workspace_entries_match_nested_paths() {
        let root = make_temp_workspace("recursive-match");
        fs::create_dir_all(root.join("src/server")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("src/server/routes.rs"), "").unwrap();
        fs::write(root.join("docs/routes.md"), "").unwrap();
        fs::write(root.join("README.md"), "").unwrap();

        let entries = list_workspace_entries(&root, Path::new("routes"), false, 20)
            .await
            .unwrap();
        let paths = entry_paths(&entries);

        assert_eq!(paths, vec!["docs/routes.md", "src/server/routes.rs"]);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn recursive_workspace_entries_hide_hidden_paths_until_requested() {
        let root = make_temp_workspace("hidden-match");
        fs::create_dir_all(root.join(".config")).unwrap();
        fs::write(root.join(".config/settings.json"), "").unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/settings.json"), "").unwrap();

        let visible = list_workspace_entries(&root, Path::new("settings"), false, 20)
            .await
            .unwrap();
        assert_eq!(entry_paths(&visible), vec!["src/settings.json"]);

        let hidden = list_workspace_entries(&root, Path::new(".config"), false, 20)
            .await
            .unwrap();
        assert_eq!(
            entry_paths(&hidden),
            vec![".config/", ".config/settings.json"]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_directories_lists_only_directories_under_root() {
        let root = make_temp_workspace("directory-browser");
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("backend")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join("README.md"), "").unwrap();

        let entries = list_workspace_directories(&root, &root, 20).await.unwrap();
        let paths = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, vec!["app", "backend"]);
        assert!(entries
            .iter()
            .all(|entry| entry.kind == WorkspaceEntryKind::Directory));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_directories_skip_symlink_escape() {
        let root = make_temp_workspace("directory-browser-symlink");
        let outside = make_temp_workspace("directory-browser-outside");
        fs::create_dir_all(root.join("safe")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let entries = list_workspace_directories(&root, &root, 20).await.unwrap();
        let names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["safe"]);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[tokio::test]
    async fn v2_http_resources_cover_version_workspaces_files_and_browser_guards() {
        let root = std::env::temp_dir().join(format!("todex-v2-http-res-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(workspace.join("README.md"), "# readme").unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "nope").unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("v2-token".to_owned()),
            },
        })
        .await
        .unwrap();
        let app = crate::server::router(state);

        // `/v2/version` mirrors the unauthenticated daemon self-check contract.
        let version = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v2/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(version.status(), StatusCode::OK);
        let version_body = to_bytes(version.into_body(), 1024 * 1024).await.unwrap();
        let version_json: serde_json::Value = serde_json::from_slice(&version_body).unwrap();
        assert_eq!(version_json["name"], "todex-agentd");

        // Workspace endpoints require the bearer token.
        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v2/workspaces")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let auth = "Bearer v2-token";
        let workspace_payload = json!({
            "workspaces": [{
                "id": "client-generated",
                "name": "project",
                "path": workspace.display().to_string(),
                "sessionId": "session-project",
                "tenantId": "local",
                "threadId": "",
                "model": "gpt-5",
                "reasoningEffort": null,
                "approvalPolicy": "on-request",
                "sandboxMode": "workspace-write",
                "serviceTier": null,
                "localAdapterState": "idle",
                "createdAt": 1,
                "updatedAt": 1
            }]
        })
        .to_string();
        let replaced = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v2/workspaces")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(workspace_payload.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replaced.status(), StatusCode::OK);
        let replaced_body = to_bytes(replaced.into_body(), 1024 * 1024).await.unwrap();
        let replaced_json: Value = serde_json::from_slice(&replaced_body).unwrap();
        let workspace_id = replaced_json["workspaces"][0]["id"].as_str().unwrap();

        let trust = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v2/workspaces/{workspace_id}/trust"))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(trust.status(), StatusCode::OK);
        let trust_body = to_bytes(trust.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            serde_json::from_slice::<Value>(&trust_body).unwrap()["trusted"]
                .as_bool()
                .unwrap()
        );

        let revoked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/v2/workspaces/{workspace_id}/trust"))
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "trusted": false }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::OK);

        let resynced = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v2/workspaces")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(workspace_payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resynced.status(), StatusCode::OK);
        let trust_after_resync = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v2/workspaces/{workspace_id}/trust"))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let trust_after_resync_body = to_bytes(trust_after_resync.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(
            !serde_json::from_slice::<Value>(&trust_after_resync_body).unwrap()["trusted"]
                .as_bool()
                .unwrap()
        );

        let trusted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/v2/workspaces/{workspace_id}/trust"))
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "trusted": true }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(trusted.status(), StatusCode::OK);
        let trusted_body = to_bytes(trusted.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            serde_json::from_slice::<Value>(&trusted_body).unwrap()["trusted"]
                .as_bool()
                .unwrap()
        );

        let entries = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v2/workspace/entries?cwd={}&query=main",
                        workspace.display()
                    ))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(entries.status(), StatusCode::OK);
        let entries_body = to_bytes(entries.into_body(), 1024 * 1024).await.unwrap();
        let entries_json: serde_json::Value = serde_json::from_slice(&entries_body).unwrap();
        assert_eq!(entries_json["entries"][0]["path"], "src/main.rs");

        let directories = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v2/workspace/directories?path={}",
                        workspace.display()
                    ))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(directories.status(), StatusCode::OK);

        let file = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v2/workspace/file?path={}",
                        workspace.join("README.md").display()
                    ))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(file.status(), StatusCode::OK);
        let file_body = to_bytes(file.into_body(), 1024 * 1024).await.unwrap();
        let file_json: serde_json::Value = serde_json::from_slice(&file_body).unwrap();
        assert_eq!(file_json["mimeType"], "text/markdown");
        assert_eq!(file_json["text"], "# readme");

        let escaped = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v2/workspace/file?path={}",
                        outside.join("secret.txt").display()
                    ))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(escaped.status(), StatusCode::FORBIDDEN);

        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v2/workspace/file?path={}",
                        workspace.join("missing.rs").display()
                    ))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let browser_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let browser_address = browser_listener.local_addr().unwrap();
        let browser_server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let (mut stream, _) = browser_listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let body = b"<h1>loopback</h1>";
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let fetched_browser = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/browser/fetch")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "url": format!("http://{browser_address}/") }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        browser_server.await.unwrap();
        assert_eq!(fetched_browser.status(), StatusCode::OK);
        let browser_body = to_bytes(fetched_browser.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let browser_json: serde_json::Value = serde_json::from_slice(&browser_body).unwrap();
        assert_eq!(browser_json["status"], 200);
        assert_eq!(browser_json["contentType"], "text/html; charset=utf-8");
        assert_eq!(browser_json["body"], "<h1>loopback</h1>");

        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_address = redirect_listener.local_addr().unwrap();
        let redirect_server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let (mut stream, _) = redirect_listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: https://example.com/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let blocked_redirect = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/browser/fetch")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "url": format!("http://{redirect_address}/") }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        redirect_server.await.unwrap();
        assert_eq!(blocked_redirect.status(), StatusCode::SERVICE_UNAVAILABLE);

        let blocked_browser = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/browser/fetch")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "url": "http://169.254.169.254/latest/meta-data" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked_browser.status(), StatusCode::BAD_REQUEST);

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_http_api_scans_runs_fixed_actions_and_enforces_root_boundary() {
        let root = std::env::temp_dir().join(format!("todex-v2-git-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        let outside = root.join("outside");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("git-token".to_owned()),
            },
        })
        .await
        .unwrap();
        state
            .workspace_trust
            .set_owned("local", &workspace, true)
            .await
            .unwrap();
        let app = crate::server::router(state.clone());
        let auth = "Bearer git-token";

        let scan = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v2/git/scan?workspacePath={}",
                        workspace.display()
                    ))
                    .header("authorization", auth)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(scan.status(), StatusCode::OK);
        let scan_body = to_bytes(scan.into_body(), 2 * 1024 * 1024).await.unwrap();
        let scan_json: serde_json::Value = serde_json::from_slice(&scan_body).unwrap();
        assert_eq!(scan_json["repositories"][0]["initialEligible"], true);
        assert_eq!(scan_json["repositories"][0]["branch"], "UNINITIALIZED");

        let init_status = StdCommand::new("git")
            .args(["-C", workspace.to_str().unwrap(), "init"])
            .status()
            .unwrap();
        assert!(init_status.success());
        fs::write(workspace.join("README.md"), "hello\n").unwrap();
        // Configure identity locally so the test does not depend on a user's
        // global Git configuration.
        for (key, value) in [
            ("user.email", "todex-test@example.invalid"),
            ("user.name", "TodeX Test"),
        ] {
            let status = StdCommand::new("git")
                .args(["-C", workspace.to_str().unwrap(), "config", key, value])
                .status()
                .unwrap();
            assert!(status.success());
        }

        let initial = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "workspacePath": workspace,
                            "action": "initial",
                            "message": "Initial API commit",
                            "includeUnstaged": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(initial.status(), StatusCode::OK);
        let initial_body = to_bytes(initial.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        let initial_json: serde_json::Value = serde_json::from_slice(&initial_body).unwrap();
        assert_eq!(initial_json["action"], "initial");
        assert!(initial_json["repositoryPath"]
            .as_str()
            .unwrap()
            .ends_with("/project"));

        fs::write(workspace.join("README.md"), "changed\n").unwrap();
        let marker = outside.join("injected");
        #[cfg(unix)]
        let hook_marker = {
            use std::os::unix::fs::PermissionsExt;
            let hook_marker = outside.join("hook-ran");
            let hook = workspace.join(".git/hooks/pre-commit");
            fs::write(
                &hook,
                format!("#!/bin/sh\ntouch \"{}\"\n", hook_marker.display()),
            )
            .unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
            hook_marker
        };
        let malicious_message = format!("$(touch {})", marker.display());
        let commit = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "workspacePath": workspace,
                            "action": "commit",
                            "message": malicious_message,
                            "includeUnstaged": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(commit.status(), StatusCode::OK);
        assert!(
            !marker.exists(),
            "commit message must never be interpreted by a shell"
        );
        #[cfg(unix)]
        assert!(!hook_marker.exists(), "repository hooks must be disabled");

        let nested = workspace.join("nested");
        fs::create_dir_all(&nested).unwrap();
        state
            .workspace_trust
            .set_owned("local", &nested, true)
            .await
            .unwrap();
        let nested_target = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "workspacePath": nested, "action": "push" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(nested_target.status(), StatusCode::BAD_REQUEST);

        let linked_workspace = state.config.workspace_root.join("linked");
        let external_git_dir = outside.join("linked.git");
        let linked_status = StdCommand::new("git")
            .args([
                "init",
                "--separate-git-dir",
                external_git_dir.to_str().unwrap(),
                linked_workspace.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(linked_status.success());
        state
            .workspace_trust
            .set_owned("local", &linked_workspace, true)
            .await
            .unwrap();
        let external_metadata = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "workspacePath": linked_workspace, "action": "push" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(external_metadata.status(), StatusCode::FORBIDDEN);

        fs::write(workspace.join("README.md"), "partial push\n").unwrap();
        let partial = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "workspacePath": workspace,
                            "action": "commit-push",
                            "message": "Commit before expected push failure",
                            "includeUnstaged": true
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::BAD_GATEWAY);
        let partial_body = to_bytes(partial.into_body(), 1024 * 1024).await.unwrap();
        let partial_json: serde_json::Value = serde_json::from_slice(&partial_body).unwrap();
        assert_eq!(partial_json["code"], "GIT_PARTIAL_SUCCESS");

        let escaped = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "workspacePath": outside,
                            "action": "push"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(escaped.status(), StatusCode::FORBIDDEN);

        let invalid_action = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/git/run")
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "workspacePath": workspace,
                            "action": "reset --hard"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_action.status(), StatusCode::BAD_REQUEST);
        let invalid_body = to_bytes(invalid_action.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let invalid_json: serde_json::Value = serde_json::from_slice(&invalid_body).unwrap();
        assert_eq!(invalid_json["code"], "INVALID_REQUEST");

        let audit = fs::read_to_string(state.config.data_dir.join("audit/audit.jsonl")).unwrap();
        assert!(audit.contains("git.audit"));
        assert!(audit.contains("commit"));
        assert!(audit.contains("partial"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn v1_routes_are_retired_and_return_404() {
        let root = std::env::temp_dir().join(format!("todex-v1-404-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        fs::create_dir_all(&workspace_root).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: false,
                enable_tls: false,
                auth_token: None,
            },
        })
        .await
        .unwrap();
        let app = crate::server::router(state);

        for (method, path) in [
            ("GET", "/v1/version"),
            ("GET", "/v1/workspaces"),
            ("PUT", "/v1/workspaces"),
            ("GET", "/v1/workspace/entries"),
            ("GET", "/v1/workspace/directories"),
            ("GET", "/v1/workspace/file"),
            ("POST", "/v1/browser/fetch"),
            ("GET", "/v1/ws"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} must stay retired"
            );
        }

        let _ = fs::remove_dir_all(root);
    }

    fn entry_paths(entries: &[WorkspaceEntry]) -> Vec<String> {
        entries.iter().map(|entry| entry.path.clone()).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v2_ws_accepts_query_token_and_dispatches_legacy_and_resume_commands() {
        let root = std::env::temp_dir().join(format!("todex-v2-ws-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("v2-ws-token".to_owned()),
            },
        })
        .await
        .unwrap();

        // Seed a gateway session with two events; only the event after the
        // client cursor should come back via session.resume.
        for index in 1..=2u64 {
            state
                .codex_gateway
                .append_event(
                    "cdxs_resume",
                    crate::codex_gateway::CodexGatewayEvent::new(
                        "codex.thread.started",
                        json!({ "index": index, "threadId": format!("thread-{index}") }),
                    ),
                )
                .await
                .unwrap();
        }

        let app = crate::server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Missing token is rejected at handshake.
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/v2/ws"))
                .await
                .is_err()
        );

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/v2/ws?access_token=v2-ws-token"))
                .await
                .expect("connect with query token");

        // v2-native command: server.ping result envelope.
        ws.send(WsMessage::Text(
            json!({ "id": "ping-1", "type": "server.ping", "payload": {} })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let ping = wait_for_ws_message(&mut ws, |message| {
            message["id"] == "ping-1" && message["type"] == "server.result"
        })
        .await;
        assert_eq!(ping["payload"]["pong"], true);

        // Legacy plane: codex.local.status still flows through the scoped
        // legacy dispatcher and answers as a plain ServerEvent.
        ws.send(WsMessage::Text(
            json!({
                "id": "status-1",
                "type": "codex.local.status",
                "payload": { "codexSessionId": "cdxs_probe", "tenantId": "local" }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let status = wait_for_ws_message(&mut ws, |message| {
            message["type"] == "codex.control.status"
                && message["payload"]["data"]["requestId"] == "status-1"
        })
        .await;
        assert_eq!(status["payload"]["data"]["codexSessionId"], "cdxs_probe");

        // session.resume replaces the transport hello: scope + replay after
        // the client cursor, answered by a v2 result envelope.
        ws.send(WsMessage::Text(
            json!({
                "id": "resume-1",
                "type": "session.resume",
                "payload": { "sessionCursors": { "cdxs_resume": 1 } }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
        let result = wait_for_ws_message(&mut ws, |message| {
            message["id"] == "resume-1" && message["type"] == "server.result"
        })
        .await;
        assert_eq!(result["payload"]["resumed"], json!(["cdxs_resume"]));
        let replayed = wait_for_ws_message(&mut ws, |message| {
            message["type"] == "codex.thread.started" && message["payload"]["cursor"] == 2
        })
        .await;
        assert_eq!(replayed["payload"]["codex_session_id"], "cdxs_resume");

        let _ = ws.close(None).await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn v2_ws_auth_matrix_covers_anonymous_wrong_and_encoded_query_tokens() {
        let root = std::env::temp_dir().join(format!("todex-v2-ws-auth-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        fs::create_dir_all(&workspace_root).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let base = Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: false,
                enable_tls: false,
                auth_token: None,
            },
        };

        // Token-less deployments keep the historical local trust model: the
        // handshake succeeds under the synthetic `local` principal.
        let state = AppState::new(base.clone()).await.unwrap();
        let app = crate::server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v2/ws"))
            .await
            .expect("anonymous connect allowed without configured token");
        ws.send(WsMessage::Text(
            json!({ "id": "ping-open", "type": "server.ping", "payload": {} })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let pong = wait_for_ws_message(&mut ws, |message| {
            message["id"] == "ping-open" && message["type"] == "server.result"
        })
        .await;
        assert_eq!(pong["payload"]["pong"], true);
        let _ = ws.close(None).await;

        // Token-secured deployments: wrong tokens fail, and query tokens that
        // need URL encoding (browser/Electron path) succeed.
        let mut secured = base;
        secured.security.auth_token = Some("tok&x=1".to_owned());
        let state = AppState::new(secured).await.unwrap();
        let app = crate::server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/v2/ws?access_token=wrong"))
                .await
                .is_err()
        );
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/v2/ws?access_token=tok%26x%3D1"))
                .await
                .expect("url-encoded query token should authenticate");
        ws.send(WsMessage::Text(
            json!({ "id": "ping-secured", "type": "server.ping", "payload": {} })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let pong = wait_for_ws_message(&mut ws, |message| {
            message["id"] == "ping-secured" && message["type"] == "server.result"
        })
        .await;
        assert_eq!(pong["payload"]["pong"], true);
        let _ = ws.close(None).await;
        let _ = fs::remove_dir_all(root);
    }

    async fn wait_for_ws_message<Filter>(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        filter: Filter,
    ) -> serde_json::Value
    where
        Filter: Fn(&serde_json::Value) -> bool,
    {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("websocket message timeout")
                .expect("websocket stream open")
                .expect("websocket frame ok");
            let WsMessage::Text(text) = frame else {
                continue;
            };
            let message: serde_json::Value = serde_json::from_str(&text).expect("json frame");
            if filter(&message) {
                return message;
            }
        }
    }

    fn make_temp_workspace(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("todex-{name}-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
