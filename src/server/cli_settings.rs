//! `/v2/providers/{claude-code|codex|pi}/settings` — the CLIs' user settings
//! files for the CLI manager (see [`crate::cli_settings`]). Writes need a
//! signed device even on loopback: the files can define hooks, shells and
//! helper commands that the CLI runs on this host.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::app_state::AppState;
use crate::cli_settings::{self, CliSettingsTarget, SettingsDocument};
use crate::error::AppError;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route(
        "/v2/providers/{provider}/settings",
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

fn target(provider: &str) -> Result<CliSettingsTarget, AppError> {
    CliSettingsTarget::from_provider(provider)
        .ok_or_else(|| AppError::Unsupported(format!("{provider} has no editable settings file")))
}

async fn read(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SettingsDocument>, AppError> {
    require_auth(&state, &headers)?;
    let file = target(&provider)?.file();
    blocking(move || cli_settings::read(&file)).await
}

async fn patch(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<SettingsDocument>, AppError> {
    let target = target(&provider)?;
    require_signed_device(&state, &headers, "editing CLI settings")?;
    let request: PatchRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid settings patch: {error}")))?;
    blocking(move || cli_settings::apply_patch(&target.file(), &request.revision, &request.patch))
        .await
}

async fn replace(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<SettingsDocument>, AppError> {
    let target = target(&provider)?;
    require_signed_device(&state, &headers, "editing CLI settings")?;
    let request: ReplaceRequest = serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid settings text: {error}")))?;
    blocking(move || cli_settings::replace_text(&target.file(), &request.revision, &request.text))
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
