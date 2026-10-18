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
    require_auth(&state, &headers)?;
    let stored = write_chunk(
        &upload_root(),
        query.upload_id.as_deref(),
        &query.name,
        query.offset,
        &body,
    )
    .await?;
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
}
