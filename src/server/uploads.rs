//! `PUT /v2/uploads` — files attached to a prompt — and `POST
//! /v2/uploads/link`, which registers a file already on this machine instead
//! of copying it (see [`crate::prompt_uploads`]).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{Extensions, HeaderMap};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::ipc::PeerAddr;
use crate::prompt_uploads::{
    register_link, upload_root, verify_link_candidate, write_chunk, MAX_UPLOAD_CHUNK_BYTES,
};

use super::v2::{mime_for_name, require_auth, ApiJson};

const MAX_UPLOAD_BODY_BYTES: usize = MAX_UPLOAD_CHUNK_BYTES + 64 * 1024;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v2/uploads",
            put(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES)),
        )
        .route("/v2/uploads/link", post(link))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    name: String,
    #[serde(default)]
    upload_id: Option<String>,
    #[serde(default)]
    offset: u64,
    /// A conversation of the caller. A private one stores the upload in its
    /// private folder, deleted with it; any other conversation (or none)
    /// uses the shared upload directory. A conversation that does not exist
    /// (or is not the caller's) is `NOT_FOUND`: a private conversation that
    /// was already deleted leaves nothing to tell it apart, and its files
    /// must not fall back to the shared directory and its retention.
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadResponse {
    upload_id: String,
    path: String,
    name: String,
    mime_type: String,
    size_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LinkRequest {
    /// Absolute path of the file on the client's machine.
    path: String,
    size_bytes: u64,
    /// Lowercase hex SHA-256 of the file's first
    /// [`crate::prompt_uploads::LINK_FINGERPRINT_BYTES`].
    head_sha256: String,
    /// As for uploads: a private conversation keeps the registration in its
    /// private folder.
    #[serde(default)]
    conversation_id: Option<String>,
}

/// The private conversation `conversation_id` names, when it is one of the
/// caller's; `NOT_FOUND` when it is not the caller's or does not exist.
async fn private_conversation(
    state: &AppState,
    tenant_id: &str,
    conversation_id: Option<&str>,
) -> Result<Option<String>, AppError> {
    Ok(match conversation_id {
        Some(id) => Some(state.conversations.get_owned(tenant_id, id).await?)
            .filter(|manifest| manifest.ephemeral)
            .map(|manifest| manifest.id),
        None => None,
    })
}

/// Where uploads and links of `private` (or the shared directory) go.
fn upload_dir(private: Option<&str>) -> Result<std::path::PathBuf, AppError> {
    match private {
        Some(id) => crate::private_dirs::root()
            .ensure_subdir(id, crate::private_dirs::UPLOADS_DIR)
            .map_err(|error| {
                AppError::Io(std::io::Error::other(format!(
                    "the private upload directory is unavailable: {error}"
                )))
            }),
        None => Ok(upload_root()),
    }
}

/// Fails once `private` was deleted while its folder was written to, after
/// removing what was written: the folder must not outlive the conversation.
fn ensure_still_private(state: &AppState, private: Option<&str>) -> Result<(), AppError> {
    if let Some(id) = private {
        if !state.conversations.is_private(id) {
            crate::private_dirs::root().remove_conversation(id);
            return Err(AppError::NotFound(format!("conversation {id}")));
        }
    }
    Ok(())
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Result<Json<UploadResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let private =
        private_conversation(&state, &auth.tenant_id, query.conversation_id.as_deref()).await?;
    let root = upload_dir(private.as_deref())?;
    let stored = write_chunk(
        &root,
        query.upload_id.as_deref(),
        &query.name,
        query.offset,
        &body,
    )
    .await?;
    ensure_still_private(&state, private.as_deref())?;
    Ok(Json(UploadResponse {
        mime_type: mime_for_name(&stored.name),
        path: stored.path.to_string_lossy().into_owned(),
        upload_id: stored.upload_id,
        name: stored.name,
        size_bytes: stored.size_bytes,
    }))
}

/// Registers a file the client has on this machine, so the prompt can name
/// it in place (docs/API.md). Only a client on this machine may: an IPC
/// peer, or a loopback one that no local proxy forwarded. The fingerprint
/// then tells a loopback tunnel from another machine apart.
async fn link(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    ApiJson(request): ApiJson<LinkRequest>,
) -> Result<Json<UploadResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let peer = super::sealed::peer_address(&extensions);
    let local = peer.is_some_and(PeerAddr::is_loopback)
        && crate::listen_addrs::forwarded(&headers, &extensions, peer.and_then(PeerAddr::ip), &[])
            .is_none();
    if !local {
        return Err(AppError::Unauthorized(
            "only clients on this machine may link files".to_owned(),
        ));
    }
    let private =
        private_conversation(&state, &auth.tenant_id, request.conversation_id.as_deref()).await?;
    let canonical = verify_link_candidate(
        std::path::Path::new(&request.path),
        request.size_bytes,
        &request.head_sha256,
    )
    .await?;
    let root = upload_dir(private.as_deref())?;
    let upload_id = register_link(&root, &canonical).await?;
    ensure_still_private(&state, private.as_deref())?;
    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    Ok(Json(UploadResponse {
        mime_type: mime_for_name(&name),
        path: canonical.to_string_lossy().into_owned(),
        upload_id,
        name,
        size_bytes: request.size_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::device_auth::test_support::TestDevice;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    #[tokio::test]
    async fn signed_chunks_build_an_upload() {
        let root =
            std::env::temp_dir().join(format!("todex-upload-routes-{}", uuid::Uuid::new_v4()));
        let state = AppState::new_for_tests(Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        })
        .await
        .unwrap();
        let app = crate::server::loopback_test_router(state);
        let device = TestDevice::new(29);
        device.enroll(&root.join("data"));
        let put = |uri: String, body: &'static [u8]| {
            let mut builder = Request::builder()
                .method("PUT")
                .uri(&uri)
                .header("content-type", "application/octet-stream");
            for (name, value) in device.sign("PUT", &uri, body) {
                builder = builder.header(name, value);
            }
            builder.body(Body::from(body)).unwrap()
        };

        let response = app
            .clone()
            .oneshot(put(
                "/v2/uploads?name=%E7%AC%94%E8%AE%B0.md&offset=0".to_owned(),
                b"# a",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let first: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(first["name"], "笔记.md");
        assert_eq!(first["mimeType"], "text/markdown");
        let id = first["uploadId"].as_str().unwrap().to_owned();

        let response = app
            .clone()
            .oneshot(put(
                format!("/v2/uploads?name=%E7%AC%94%E8%AE%B0.md&uploadId={id}&offset=3"),
                b"\nb",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let second: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(second["sizeBytes"], 5);
        let path = std::path::PathBuf::from(second["path"].as_str().unwrap());
        assert!(path.starts_with(std::fs::canonicalize(upload_root()).unwrap()));
        assert_eq!(std::fs::read(&path).unwrap(), b"# a\nb");

        let unsigned = Request::builder()
            .method("PUT")
            .uri("/v2/uploads?name=x&offset=0")
            .body(Body::from("x"))
            .unwrap();
        let response = app.oneshot(unsigned).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(upload_root().join(&id));
        let _ = std::fs::remove_dir_all(root);
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    fn signed_post(device: &TestDevice, body: &Value) -> axum::http::request::Builder {
        let body = serde_json::to_vec(body).unwrap();
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v2/uploads/link")
            .header("content-type", "application/json");
        for (name, value) in device.sign("POST", "/v2/uploads/link", &body) {
            builder = builder.header(name, value);
        }
        builder
    }

    #[tokio::test]
    async fn local_clients_link_files_in_place() {
        let root = std::env::temp_dir().join(format!("todex-upload-link-{}", uuid::Uuid::new_v4()));
        let state = AppState::new_for_tests(Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        })
        .await
        .unwrap();
        let app = crate::server::loopback_test_router(state);
        let device = TestDevice::new(37);
        device.enroll(&root.join("data"));
        std::fs::create_dir_all(root.join("Downloads")).unwrap();
        let file = root.join("Downloads").join("shot.png");
        std::fs::write(&file, b"png bytes").unwrap();
        let body = serde_json::json!({
            "path": file.display().to_string(),
            "sizeBytes": 9,
            "headSha256": sha256_hex(b"png bytes"),
        });
        let send = |builder: axum::http::request::Builder, body: &Value| {
            let request = builder
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap();
            app.clone().oneshot(request)
        };

        let response = send(signed_post(&device, &body), &body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let linked: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        assert_eq!(linked["path"], canonical.display().to_string());
        assert_eq!(linked["name"], "shot.png");
        assert_eq!(linked["mimeType"], "image/png");
        assert_eq!(linked["sizeBytes"], 9);
        assert!(crate::prompt_uploads::is_linked(&upload_root(), &canonical).await);
        // Nothing was copied: the registration holds only its marker.
        let id = linked["uploadId"].as_str().unwrap().to_owned();
        assert_eq!(
            std::fs::read_dir(upload_root().join(&id)).unwrap().count(),
            1
        );

        // Another machine's file of the same name does not match.
        let stale = serde_json::json!({
            "path": file.display().to_string(),
            "sizeBytes": 9,
            "headSha256": sha256_hex(b"different"),
        });
        let response = send(signed_post(&device, &stale), &stale).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        // A remote peer, or a loopback one a local proxy forwarded, may not
        // link at all.
        // (As a sealed request arrives: transport v2 from a remote address.)
        let remote = signed_post(&device, &body)
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [203, 0, 113, 7],
                4000,
            ))))
            .extension(crate::server::sealed::ArrivedViaTransportV2);
        let response = send(remote, &body).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let proxied = signed_post(&device, &body)
            .header("x-forwarded-for", "203.0.113.7")
            .extension(crate::server::sealed::ArrivedViaTransportV2);
        let response = send(proxied, &body).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let unsigned = Request::builder()
            .method("POST")
            .uri("/v2/uploads/link")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = app.clone().oneshot(unsigned).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(upload_root().join(&id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uploads_of_a_private_conversation_live_and_die_with_it() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("todex-upload-private-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspaces").join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let codex = root.join("codex");
        std::fs::write(&codex, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.agent.codex_bin = codex.display().to_string();
        let state = AppState::new_for_tests(config).await.unwrap();
        let conversation = state
            .conversations
            .create_private_owned(
                "local",
                crate::conversation::ProviderKind::Codex,
                std::fs::canonicalize(&workspace).unwrap(),
                None,
                None,
            )
            .await
            .unwrap();
        let normal = state
            .conversations
            .create_owned(
                "local",
                crate::conversation::ProviderKind::Codex,
                std::fs::canonicalize(&workspace).unwrap(),
                None,
                None,
            )
            .await
            .unwrap();
        let app = crate::server::loopback_test_router(state.clone());
        let device = TestDevice::new(31);
        device.enroll(&root.join("data"));
        let put = |uri: String| {
            let mut builder = Request::builder()
                .method("PUT")
                .uri(&uri)
                .header("content-type", "application/octet-stream");
            for (name, value) in device.sign("PUT", &uri, b"secret") {
                builder = builder.header(name, value);
            }
            builder.body(Body::from(&b"secret"[..])).unwrap()
        };
        let upload = |id: &str| {
            let app = app.clone();
            let uri = format!("/v2/uploads?name=a.txt&offset=0&conversationId={id}");
            let request = put(uri);
            async move {
                let response = app.oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body: Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                        .unwrap();
                std::path::PathBuf::from(body["path"].as_str().unwrap())
            }
        };
        let private = upload(&conversation.id).await;
        let folder = crate::private_dirs::root().conversation_path(&conversation.id);
        assert!(private.starts_with(std::fs::canonicalize(&folder).unwrap()));
        assert_eq!(std::fs::read(&private).unwrap(), b"secret");
        // A normal conversation keeps the shared upload directory.
        let shared = upload(&normal.id).await;
        assert!(shared.starts_with(std::fs::canonicalize(upload_root()).unwrap()));
        // A link for the private conversation is registered in its folder,
        // not in the shared directory.
        let original = std::fs::canonicalize(&workspace)
            .unwrap()
            .join("..")
            .join("own.txt");
        std::fs::write(&original, b"mine").unwrap();
        let original = std::fs::canonicalize(original).unwrap();
        let body = serde_json::json!({
            "path": original.display().to_string(),
            "sizeBytes": 4,
            "headSha256": sha256_hex(b"mine"),
            "conversationId": conversation.id,
        });
        let request = signed_post(&device, &body)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let private_uploads = folder.join(crate::private_dirs::UPLOADS_DIR);
        assert!(crate::prompt_uploads::is_linked(&private_uploads, &original).await);
        assert!(!crate::prompt_uploads::is_linked(&upload_root(), &original).await);
        state
            .conversations
            .delete_owned("local", &conversation.id)
            .await
            .unwrap();
        assert!(!private.exists());
        assert!(!folder.exists());
        assert!(!crate::prompt_uploads::is_linked(&private_uploads, &original).await);
        assert!(original.exists());
        // A first chunk that arrives after the deletion is refused instead
        // of landing in the shared directory; so is an unknown conversation.
        let unknown = uuid::Uuid::new_v4().to_string();
        for id in [conversation.id.as_str(), unknown.as_str()] {
            let uri = format!("/v2/uploads?name=late.txt&offset=0&conversationId={id}");
            let response = app.clone().oneshot(put(uri)).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        assert!(!folder.exists());
        let _ = std::fs::remove_dir_all(shared.parent().unwrap());
        let _ = std::fs::remove_dir_all(root);
    }
}
