//! `/v2/remote/*` — file sessions on SSH hosts (SFTP) and FTP sites.
//!
//! Behind the device-signature middleware like the rest of `/v2`. Every
//! operation runs in its own task holding the session lock, so a client that
//! disconnects mid-request cannot leave an SFTP/FTP exchange half done.

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use zeroize::Zeroizing;

use crate::app_state::AppState;
use crate::error::AppError;
use crate::remote_fs::{
    check_connection, file_name, normalize_path, parent_path, EntryKind, FtpFs, OpenedBy,
    RemoteEntry, RemoteFs, SftpFs, MAX_TRANSFER_BYTES,
};

use super::v2::{mime_for_name, require_auth};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Uploads arrive in chunks so each signed request body stays small.
const MAX_UPLOAD_CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Two 1 MiB strings may each expand sixfold when JSON escaped.
const MAX_SAVE_BODY_BYTES: usize = 12 * MAX_TEXT_BYTES + 64 * 1024;
const MAX_UPLOAD_BODY_BYTES: usize = MAX_UPLOAD_CHUNK_BYTES + 64 * 1024;
/// Download chunks buffered between the remote reader and the client.
const DOWNLOAD_QUEUE: usize = 4;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v2/remote/connections",
            get(list_connections).post(open_connection),
        )
        .route("/v2/remote/connections/{id}", axum::routing::delete(close))
        .route("/v2/remote/connections/{id}/entries", get(entries))
        .route(
            "/v2/remote/connections/{id}/file",
            get(read_file)
                .put(save_file)
                .layer(DefaultBodyLimit::max(MAX_SAVE_BODY_BYTES)),
        )
        .route("/v2/remote/connections/{id}/mkdir", post(mkdir))
        .route("/v2/remote/connections/{id}/rename", post(rename))
        .route("/v2/remote/connections/{id}/delete", post(delete))
        .route("/v2/remote/connections/{id}/download", get(download))
        .route(
            "/v2/remote/connections/{id}/upload",
            put(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES)),
        )
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum OpenRequest {
    Sftp {
        host: String,
        #[serde(default)]
        password: Option<String>,
    },
    Ftp {
        site_id: String,
        #[serde(default)]
        password: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PathRequest {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RenameRequest {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveRequest {
    path: String,
    text: String,
    expected_text: String,
}

#[derive(Debug, Deserialize)]
struct UploadQuery {
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EntriesResponse {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    entries: Vec<RemoteEntry>,
    truncated: bool,
}

/// Same shape as `/v2/workspace/file`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileResponse {
    name: String,
    path: String,
    mime_type: String,
    size_bytes: u64,
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_url: Option<String>,
}

/// Runs `op` with exclusive use of the session in a detached task. A lost
/// connection removes the session so clients see it is gone.
async fn run<T, F>(state: &AppState, id: &str, op: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut dyn RemoteFs) -> BoxFuture<'a, Result<T, AppError>> + Send + 'static,
{
    let mut guard = state.remote_files.lock(id).await?;
    let result = tokio::spawn(async move {
        let result = op(guard.fs()).await;
        result.map_err(|error| check_connection(guard.fs(), error))
    })
    .await
    .map_err(|error| AppError::Anyhow(error.into()))?;
    if matches!(result, Err(AppError::RemoteUnreachable(_))) {
        state.remote_files.close(id);
    }
    result
}

fn non_empty_secret(password: Option<String>) -> Option<Zeroizing<String>> {
    password
        .map(Zeroizing::new)
        .filter(|secret| !secret.is_empty())
}

async fn list_connections(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(json!({ "connections": state.remote_files.list() })))
}

async fn open_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OpenRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.remote_files.ensure_capacity()?;
    let (opened_by, fs): (OpenedBy, Box<dyn RemoteFs>) = match request {
        OpenRequest::Sftp { host, password } => {
            state.ssh.require_host(&host).await?;
            let fs = SftpFs::connect(&state.ssh, &host, non_empty_secret(password)).await?;
            (OpenedBy::Sftp { host }, Box::new(fs))
        }
        OpenRequest::Ftp { site_id, password } => {
            let site = state.ssh.ftp_site(&site_id).await?;
            let fs = FtpFs::connect(&site, non_empty_secret(password)).await?;
            (
                OpenedBy::Ftp {
                    site_id,
                    name: site.name,
                },
                Box::new(fs),
            )
        }
    };
    let connection = state.remote_files.insert(opened_by, fs).await?;
    Ok(Json(json!({ "connection": connection })))
}

async fn close(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    if !state.remote_files.close(&id) {
        return Err(AppError::NotFound(format!("remote connection {id}")));
    }
    Ok(Json(json!({ "closed": true })))
}

async fn entries(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<PathQuery>,
) -> Result<Json<EntriesResponse>, AppError> {
    require_auth(&state, &headers)?;
    let requested = match query.path.filter(|path| !path.is_empty()) {
        Some(path) => Some(normalize_path(&path)?),
        None => None,
    };
    let (path, listing) = run(&state, &id, move |fs| {
        Box::pin(async move {
            let path = match requested {
                Some(path) => path,
                None => fs.home_dir().await?,
            };
            let listing = fs.list(&path).await?;
            Ok((path, listing))
        })
    })
    .await?;
    Ok(Json(EntriesResponse {
        parent: parent_path(&path),
        path,
        entries: listing.entries,
        truncated: listing.truncated,
    }))
}

fn preview_too_large() -> AppError {
    AppError::InvalidRequest("file is too large to preview".to_owned())
}

async fn read_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<PathQuery>,
) -> Result<Json<FileResponse>, AppError> {
    require_auth(&state, &headers)?;
    let path = normalize_path(query.path.as_deref().unwrap_or_default())?;
    let name = file_name(&path).to_owned();
    let mime_type = mime_for_name(&name);
    let is_image = mime_type.starts_with("image/");
    let max = if is_image {
        MAX_IMAGE_BYTES
    } else {
        MAX_TEXT_BYTES as u64
    };
    let target = path.clone();
    let bytes = run(&state, &id, move |fs| {
        Box::pin(async move {
            let stat = fs
                .stat(&target)
                .await?
                .ok_or_else(|| AppError::NotFound(target.clone()))?;
            if stat.kind == EntryKind::Directory {
                return Err(AppError::InvalidRequest("path must be a file".to_owned()));
            }
            if stat.size > max {
                return Err(preview_too_large());
            }
            fs.read(&target, max).await.map_err(|error| match error {
                AppError::InvalidRequest(_) => preview_too_large(),
                other => other,
            })
        })
    })
    .await?;
    let data_url =
        is_image.then(|| format!("data:{mime_type};base64,{}", BASE64_STANDARD.encode(&bytes)));
    let size_bytes = bytes.len() as u64;
    let text = if is_image {
        None
    } else {
        String::from_utf8(bytes).ok()
    };
    Ok(Json(FileResponse {
        name,
        path,
        mime_type,
        size_bytes,
        text,
        data_url,
    }))
}

async fn save_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<SaveRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    if request.text.len() > MAX_TEXT_BYTES || request.expected_text.len() > MAX_TEXT_BYTES {
        return Err(AppError::InvalidRequest(
            "file is too large to edit".to_owned(),
        ));
    }
    let path = normalize_path(&request.path)?;
    run(&state, &id, move |fs| {
        Box::pin(async move {
            let current =
                fs.read(&path, MAX_TEXT_BYTES as u64)
                    .await
                    .map_err(|error| match error {
                        AppError::InvalidRequest(_) => AppError::InvalidRequest(
                            "file is not editable text or is too large".to_owned(),
                        ),
                        other => other,
                    })?;
            if std::str::from_utf8(&current).is_err() {
                return Err(AppError::InvalidRequest(
                    "file is not UTF-8 text".to_owned(),
                ));
            }
            if current != request.expected_text.as_bytes() {
                return Err(AppError::Conflict(
                    "file changed since it was opened; reload before saving".to_owned(),
                ));
            }
            fs.replace(&path, request.text.as_bytes()).await
        })
    })
    .await?;
    Ok(Json(json!({ "saved": true })))
}

async fn mkdir(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PathRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let path = normalize_path(&request.path)?;
    run(&state, &id, move |fs| {
        Box::pin(async move {
            if fs.stat(&path).await?.is_some() {
                return Err(AppError::Conflict(format!("{path} already exists")));
            }
            fs.mkdir(&path).await
        })
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<RenameRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let from = normalize_path(&request.from)?;
    let to = normalize_path(&request.to)?;
    if from == "/" || to == "/" {
        return Err(AppError::InvalidRequest(
            "the root directory cannot be renamed".to_owned(),
        ));
    }
    run(&state, &id, move |fs| {
        Box::pin(async move {
            if from == to {
                return Ok(());
            }
            // Servers differ on whether rename overwrites; never let it.
            if fs.stat(&to).await?.is_some() {
                return Err(AppError::Conflict(format!("{to} already exists")));
            }
            fs.rename(&from, &to).await
        })
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PathRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let path = normalize_path(&request.path)?;
    if path == "/" {
        return Err(AppError::InvalidRequest(
            "the root directory cannot be deleted".to_owned(),
        ));
    }
    run(&state, &id, move |fs| {
        Box::pin(async move { fs.remove(&path).await })
    })
    .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let path = normalize_path(&query.path)?;
    if body.len() > MAX_UPLOAD_CHUNK_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "upload chunks must be at most {MAX_UPLOAD_CHUNK_BYTES} bytes"
        )));
    }
    let offset = query.offset;
    let end = offset.saturating_add(body.len() as u64);
    if end > MAX_TRANSFER_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "files may be at most {MAX_TRANSFER_BYTES} bytes"
        )));
    }
    let overwrite = query.overwrite;
    run(&state, &id, move |fs| {
        Box::pin(async move {
            let current = fs.stat(&path).await?;
            if current.is_some_and(|stat| stat.kind == EntryKind::Directory) {
                return Err(AppError::InvalidRequest("path is a directory".to_owned()));
            }
            match (offset, current) {
                (0, Some(_)) if !overwrite => {
                    return Err(AppError::Conflict(format!("{path} already exists")));
                }
                (0, _) => {}
                (_, current) => {
                    let size = current.map_or(0, |stat| stat.size);
                    if size != offset {
                        return Err(AppError::Conflict(format!(
                            "upload offset {offset} does not match the remote size {size}"
                        )));
                    }
                }
            }
            fs.write_at(&path, offset, &body).await
        })
    })
    .await?;
    Ok(Json(json!({ "sizeBytes": end })))
}

async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<PathQuery>,
) -> Result<Response, AppError> {
    require_auth(&state, &headers)?;
    let path = normalize_path(query.path.as_deref().unwrap_or_default())?;
    let name = file_name(&path).to_owned();
    let mut guard = state.remote_files.lock(&id).await?;
    let sessions = state.remote_files.clone();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<u64, AppError>>();
    let (chunks_tx, chunks_rx) = mpsc::channel(DOWNLOAD_QUEUE);
    // The task owns the session lock until the transfer ends or the client
    // goes away; memory stays bounded by the chunk queue.
    tokio::spawn(async move {
        let fs = guard.fs();
        let size = match fs.stat(&path).await {
            Ok(Some(stat)) if stat.kind == EntryKind::Directory => {
                Err(AppError::InvalidRequest("path must be a file".to_owned()))
            }
            Ok(Some(stat)) if stat.size > MAX_TRANSFER_BYTES => Err(AppError::InvalidRequest(
                format!("files may be at most {MAX_TRANSFER_BYTES} bytes"),
            )),
            Ok(Some(stat)) => Ok(stat.size),
            Ok(None) => Err(AppError::NotFound(path.clone())),
            Err(error) => Err(check_connection(fs, error)),
        };
        let size = match size {
            Ok(size) => size,
            Err(error) => {
                if matches!(error, AppError::RemoteUnreachable(_)) {
                    sessions.close(&id);
                }
                let _ = ready_tx.send(Err(error));
                return;
            }
        };
        if ready_tx.send(Ok(size)).is_err() {
            return;
        }
        if let Err(error) = fs.download(&path, size, &chunks_tx).await {
            let error = check_connection(fs, error);
            if matches!(error, AppError::RemoteUnreachable(_)) {
                sessions.close(&id);
            }
            // Ends the response body with an error so clients see a failed
            // transfer rather than a short file.
            let _ = chunks_tx
                .send(Err(std::io::Error::other(error.to_string())))
                .await;
        }
    });
    let size = ready_rx
        .await
        .map_err(|_| AppError::RemoteFailed("download task ended unexpectedly".to_owned()))??;
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, content_disposition(&name))
        .body(Body::from_stream(ReceiverStream::new(chunks_rx)))
        .map_err(|error| AppError::Anyhow(error.into()))
}

/// `attachment` with an RFC 5987 UTF-8 file name.
fn content_disposition(name: &str) -> HeaderValue {
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{encoded}"))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::device_auth::test_support::TestDevice;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Raw upload chunks go through the device-signature middleware as-is:
    /// the signature covers the exact bytes, and the route accepts chunks
    /// above axum's 2 MiB default but not above the chunk limit.
    #[tokio::test]
    async fn signed_raw_upload_bodies_pass_device_auth() {
        let root =
            std::env::temp_dir().join(format!("todex-remote-routes-{}", uuid::Uuid::new_v4()));
        let state = AppState::new(Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        })
        .await
        .unwrap();
        let app = crate::server::loopback_test_router(state);
        let device = TestDevice::new(23);
        device.enroll(&root.join("data"));
        let uri = "/v2/remote/connections/missing/upload?path=%2Ftmp%2Fx.bin&offset=0";
        let request = |body: Vec<u8>, signed_body: &[u8]| {
            let mut builder = Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/octet-stream");
            for (name, value) in device.sign("PUT", uri, signed_body) {
                builder = builder.header(name, value);
            }
            builder.body(Body::from(body)).unwrap()
        };

        let chunk = vec![7_u8; 3 * 1024 * 1024];
        let response = app
            .clone()
            .oneshot(request(chunk.clone(), &chunk))
            .await
            .unwrap();
        // Authenticated and parsed; only the session is unknown.
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let mut tampered = chunk.clone();
        tampered[0] = 8;
        let response = app
            .clone()
            .oneshot(request(tampered, &chunk))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let oversized = vec![0_u8; MAX_UPLOAD_BODY_BYTES + 1];
        let response = app
            .oneshot(request(oversized.clone(), &oversized))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parses_open_requests() {
        let request: OpenRequest =
            serde_json::from_str(r#"{"kind":"sftp","host":"db","password":"pw"}"#).unwrap();
        assert!(
            matches!(request, OpenRequest::Sftp { ref host, password: Some(_) } if host == "db")
        );
        let request: OpenRequest =
            serde_json::from_str(r#"{"kind":"ftp","siteId":"abc"}"#).unwrap();
        assert!(
            matches!(request, OpenRequest::Ftp { ref site_id, password: None } if site_id == "abc")
        );
        assert!(
            serde_json::from_str::<OpenRequest>(r#"{"kind":"sftp","host":"db","x":1}"#).is_err()
        );
        assert!(serde_json::from_str::<OpenRequest>(r#"{"kind":"scp","host":"db"}"#).is_err());
        assert!(non_empty_secret(Some(String::new())).is_none());
    }

    #[test]
    fn encodes_download_file_names() {
        assert_eq!(
            content_disposition("a b€.txt"),
            "attachment; filename*=UTF-8''a%20b%E2%82%AC.txt"
        );
        assert_eq!(
            content_disposition("x\"y"),
            "attachment; filename*=UTF-8''x%22y"
        );
    }
}
