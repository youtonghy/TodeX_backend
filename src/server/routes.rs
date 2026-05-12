use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs::FileType;
use std::path::{Component, Path, PathBuf};

use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
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
        .route("/v1/pairing", get(pairing))
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

async fn pairing(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<crate::transport_crypto::PairingPayload>, AppError> {
    authorize_http(&state, &headers)?;
    let port = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|host| host.rsplit_once(':'))
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .unwrap_or(state.config.port);
    Ok(Json(
        state.pairing_keys.pairing_payload(&state.config, port),
    ))
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
) -> impl IntoResponse {
    let auth = websocket::authenticate_headers(&state, &headers);
    let crypto = websocket::transport_crypto_from_handshake(&state, &headers, uri.query());
    ws.on_upgrade(move |socket| websocket::handle_socket(state, socket, auth, crypto))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn pairing_requires_auth_and_returns_available_protocols() {
        let state = AppState::new(test_config(true)).await.unwrap();

        assert!(pairing(State(state.clone()), HeaderMap::new())
            .await
            .is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer test-token".parse().unwrap(),
        );
        headers.insert(
            axum::http::header::HOST,
            "phone.local:9191".parse().unwrap(),
        );

        let Json(payload) = pairing(State(state), headers).await.unwrap();
        let value = serde_json::to_value(payload).unwrap();

        assert_eq!(value["kind"], "todex-pairing");
        assert_eq!(value["version"], 1);
        assert_eq!(value["serverUrl"], "http://127.0.0.1:9191");
        assert_eq!(value["wsUrl"], "ws://127.0.0.1:9191/v1/ws");
        assert_eq!(value["authToken"], "test-token");
        assert_eq!(value["protocols"][0]["id"], "x25519");
        assert_eq!(value["protocols"][1]["id"], "ml-kem-768");
        assert!(value["protocols"][0]["publicKey"].as_str().unwrap().len() > 16);
        assert!(value["protocols"][1]["publicKey"].as_str().unwrap().len() > 16);
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

    fn test_config(enable_auth: bool) -> Config {
        let root = make_temp_workspace("config");
        Config {
            host: "127.0.0.1".to_owned(),
            port: 7345,
            pairing_encryption: PairingEncryption::default(),
            data_dir: root.join("data"),
            workspace_root: root.join("workspace"),
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
            },
            security: SecurityConfig {
                enable_auth,
                enable_tls: false,
                auth_token: Some("test-token".to_owned()),
            },
        }
    }
}
