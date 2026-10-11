//! `/v2/providers/claude-code/settings` — Claude Code's user `settings.json`
//! for the CLI manager (see [`crate::cli_settings`]). Writes need a signed
//! device even on loopback: the file can define hooks and helper commands
//! that Claude Code runs on this host.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::app_state::AppState;
use crate::cli_settings::{self, SettingsDocument};
use crate::error::AppError;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route(
        "/v2/providers/claude-code/settings",
        get(read).patch(patch).put(replace),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchRequest {
    revision: String,
    patch: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplaceRequest {
    revision: String,
    text: String,
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SettingsDocument>, AppError> {
    require_auth(&state, &headers)?;
    blocking(|| cli_settings::read(&cli_settings::claude_settings_path())).await
}

async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<SettingsDocument>, AppError> {
    require_signed_device(&state, &headers, "editing Claude Code settings")?;
    let request: PatchRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid settings patch: {error}")))?;
    blocking(move || {
        cli_settings::apply_patch(
            &cli_settings::claude_settings_path(),
            &request.revision,
            &request.patch,
        )
    })
    .await
}

async fn replace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<SettingsDocument>, AppError> {
    require_signed_device(&state, &headers, "editing Claude Code settings")?;
    let request: ReplaceRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid settings text: {error}")))?;
    blocking(move || {
        cli_settings::replace_text(
            &cli_settings::claude_settings_path(),
            &request.revision,
            &request.text,
        )
    })
    .await
}

async fn blocking(
    work: impl FnOnce() -> Result<SettingsDocument, AppError> + Send + 'static,
) -> Result<Json<SettingsDocument>, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map(Json)
}
