use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs::FileType;
use std::path::{Component, Path, PathBuf};

use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::workspace_paths::{canonical_workspace_root, validate_workspace_directory_text};
use crate::workspace_store::WorkspaceRecord;

use super::websocket;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/version", get(version))
        .route("/v1/workspaces", get(workspaces).put(replace_workspaces))
        .route("/v1/workspace/entries", get(workspace_entries))
        .route("/v1/workspace/directories", get(workspace_directories))
        .route("/v1/workspace/file", get(workspace_file))
        .route("/v1/browser/fetch", post(browser_fetch))
        .route("/v1/ws", get(ws))
        .merge(super::v2::routes())
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

async fn workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<WorkspacesResponse>, AppError> {
    authorize_http(&state, &headers)?;
    let snapshot = state.workspaces.snapshot().await;
    Ok(Json(WorkspacesResponse {
        workspaces: snapshot.workspaces,
        updated_at: snapshot.updated_at,
    }))
}

async fn replace_workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ReplaceWorkspacesRequest>,
) -> Result<Json<WorkspacesResponse>, AppError> {
    authorize_http(&state, &headers)?;
    let snapshot = state.workspaces.replace(request.workspaces).await?;
    Ok(Json(WorkspacesResponse {
        workspaces: snapshot.workspaces,
        updated_at: snapshot.updated_at,
    }))
}

async fn workspace_entries(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceEntriesQuery>,
) -> Result<Json<WorkspaceEntriesResponse>, AppError> {
    authorize_http(&state, &headers)?;

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

async fn workspace_directories(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceDirectoriesQuery>,
) -> Result<Json<WorkspaceDirectoriesResponse>, AppError> {
    authorize_http(&state, &headers)?;

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

async fn workspace_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkspaceFileQuery>,
) -> Result<Json<WorkspaceFileResponse>, AppError> {
    authorize_http(&state, &headers)?;
    let path = validate_workspace_directory_text(&state.config.workspace_root, &query.path)?;
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

async fn browser_fetch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<BrowserFetchRequest>,
) -> Result<Json<BrowserFetchResponse>, AppError> {
    authorize_http(&state, &headers)?;
    let url = validate_browser_url(&request.url)?;
    let host = url
        .split('/')
        .nth(2)
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if host.eq_ignore_ascii_case("169.254.169.254")
        || host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
    {
        return Err(AppError::InvalidRequest(
            "browser target is not allowed".to_owned(),
        ));
    }
    let output = tokio::process::Command::new("curl")
        .args([
            "--fail-with-body",
            "--silent",
            "--show-error",
            "--location",
            "--max-redirs",
            "3",
            "--max-time",
            "15",
            "--max-filesize",
            "2097152",
            &url,
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(AppError::ProviderUnavailable(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(Json(BrowserFetchResponse {
        url,
        status: 200,
        content_type: "text/html".to_owned(),
        body,
    }))
}

fn validate_browser_url(raw: &str) -> Result<String, AppError> {
    let url = raw.trim();
    if url.len() > 2048
        || !(url.starts_with("http://") || url.starts_with("https://"))
        || !url[8..].contains('/')
    {
        return Err(AppError::InvalidRequest(
            "only valid http and https URLs are allowed".to_owned(),
        ));
    }
    Ok(url.to_owned())
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
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    let auth = websocket::authenticate_headers(&state, &headers);
    if state.config.security.auth_token.is_some() && auth.is_none() {
        return Err(AppError::Unauthenticated);
    }
    let crypto = websocket::transport_crypto_from_handshake(&state, &headers, uri.query());
    Ok(ws.on_upgrade(move |socket| websocket::handle_socket(state, socket, auth, crypto)))
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
struct ReplaceWorkspacesRequest {
    workspaces: Vec<WorkspaceRecord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspacesResponse {
    workspaces: Vec<WorkspaceRecord>,
    updated_at: u64,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDirectoriesQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceFileQuery {
    path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceFileResponse {
    name: String,
    path: String,
    mime_type: String,
    size_bytes: u64,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserFetchRequest {
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserFetchResponse {
    url: String,
    status: u16,
    content_type: String,
    body: String,
}

#[derive(Debug, Serialize)]
struct WorkspaceEntriesResponse {
    entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceDirectoriesResponse {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn entry_paths(entries: &[WorkspaceEntry]) -> Vec<String> {
        entries.iter().map(|entry| entry.path.clone()).collect()
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
