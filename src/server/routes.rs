use axum::extract::{State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::app_state::AppState;

use super::websocket;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/v1/version", get(version))
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
