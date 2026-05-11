use std::cmp::Ordering;
use std::path::{Component, Path, PathBuf};

use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::error::AppError;

use super::websocket;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/version", get(version))
        .route("/v1/workspace/entries", get(workspace_entries))
        .route("/v1/ws", get(ws))
}

async fn health() -> &'static str {
    "ok"
}

async fn version(State(state): State<AppState>) -> Json<VersionResponse> {
    Json(VersionResponse {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        data_dir: state.config.data_dir.display().to_string(),
        workspace_root: state.config.workspace_root.display().to_string(),
    })
}

async fn workspace_entries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceEntriesQuery>,
) -> Result<Json<WorkspaceEntriesResponse>, AppError> {
    authorize_http(&state, &headers)?;

    let cwd = PathBuf::from(query.cwd.trim());
    if !cwd.exists() || !cwd.is_dir() {
        return Err(AppError::WorkspacePathNotFound);
    }

    let relative_query = normalize_relative_query(query.query.as_deref().unwrap_or(""))?;
    let (directory_query, filter) = split_query(&relative_query);
    let directory = cwd.join(&directory_query);
    if !directory.exists() || !directory.is_dir() {
        return Ok(Json(WorkspaceEntriesResponse { entries: vec![] }));
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
        let mut path = slash_path(&relative_path);
        let kind = if file_type.is_dir() {
            path.push('/');
            WorkspaceEntryKind::Directory
        } else {
            WorkspaceEntryKind::File
        };
        entries.push(WorkspaceEntry {
            name: file_name,
            path,
            kind,
        });
    }

    entries.sort_by(|left, right| match (&left.kind, &right.kind) {
        (WorkspaceEntryKind::Directory, WorkspaceEntryKind::File) => Ordering::Less,
        (WorkspaceEntryKind::File, WorkspaceEntryKind::Directory) => Ordering::Greater,
        _ => left
            .name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase()),
    });
    entries.truncate(query.limit.unwrap_or(40).clamp(1, 100));

    Ok(Json(WorkspaceEntriesResponse { entries }))
}

fn authorize_http(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    if state.config.security.auth_token.is_none() {
        return Ok(());
    }
    websocket::authenticate_headers(state, headers)
        .map(|_| ())
        .ok_or(AppError::Unauthenticated)
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

fn split_query(query: &Path) -> (PathBuf, String) {
    let raw = slash_path(query);
    if raw.ends_with('/') {
        return (query.to_path_buf(), String::new());
    }
    let directory = query
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    let filter = query
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    (directory, filter)
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

async fn ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let auth = websocket::authenticate_headers(&state, &headers);
    ws.on_upgrade(move |socket| websocket::handle_socket(state, socket, auth))
}

#[derive(Debug, Serialize)]
struct VersionResponse {
    name: &'static str,
    version: &'static str,
    data_dir: String,
    workspace_root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceEntriesQuery {
    cwd: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct WorkspaceEntriesResponse {
    entries: Vec<WorkspaceEntry>,
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
