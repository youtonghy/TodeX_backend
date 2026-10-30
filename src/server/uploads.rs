//! `PUT /v2/uploads` — files attached to a prompt (see [`crate::prompt_uploads`]).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::HeaderMap;
use axum::routing::put;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::error::AppError;
use crate::prompt_uploads::{upload_root, write_chunk, MAX_UPLOAD_CHUNK_BYTES};

use super::v2::{mime_for_name, require_auth};

const MAX_UPLOAD_BODY_BYTES: usize = MAX_UPLOAD_CHUNK_BYTES + 64 * 1024;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route(
        "/v2/uploads",
        put(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES)),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadQuery {
    name: String,
    #[serde(default)]
    upload_id: Option<String>,
    #[serde(default)]
    offset: u64,
    /// A private conversation of the caller: the upload is stored in its
    /// private folder and deleted with it. Any other conversation (or none)
    /// uses the shared upload directory.
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

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Result<Json<UploadResponse>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let private = match query.conversation_id.as_deref() {
        Some(id) => state
            .conversations
            .get_owned(&auth.tenant_id, id)
            .await
            .ok()
            .filter(|manifest| manifest.ephemeral)
            .map(|manifest| manifest.id),
        None => None,
    };
    let root = match &private {
        Some(id) => crate::private_dirs::root()
            .ensure_subdir(id, crate::private_dirs::UPLOADS_DIR)
            .map_err(|error| {
                AppError::Io(std::io::Error::other(format!(
                    "the private upload directory is unavailable: {error}"
                )))
            })?,
        None => upload_root(),
    };
    let stored = write_chunk(
        &root,
        query.upload_id.as_deref(),
        &query.name,
        query.offset,
        &body,
    )
    .await?;
    if let Some(id) = &private {
        // Deleted while the chunk was written: the folder must not outlive it.
        if !state.conversations.is_private(id) {
            crate::private_dirs::root().remove_conversation(id);
            return Err(AppError::NotFound(format!("conversation {id}")));
        }
    }
    Ok(Json(UploadResponse {
        mime_type: mime_for_name(&stored.name),
        path: stored.path.to_string_lossy().into_owned(),
        upload_id: stored.upload_id,
        name: stored.name,
        size_bytes: stored.size_bytes,
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
        state
            .conversations
            .delete_owned("local", &conversation.id)
            .await
            .unwrap();
        assert!(!private.exists());
        assert!(!folder.exists());
        let _ = std::fs::remove_dir_all(shared.parent().unwrap());
        let _ = std::fs::remove_dir_all(root);
    }
}
