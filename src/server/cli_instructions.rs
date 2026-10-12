//! `/v2/providers/{claude-code|codex|pi}/instructions` and
//! `/v2/instruction-presets` — the CLIs' global instruction files and the
//! preset library they can follow (see [`crate::cli_instructions`]). Writes
//! need a signed device even on loopback: the instructions steer agents that
//! run commands on this host.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::app_state::AppState;
use crate::cli_instructions::{
    self, InstructionPreset, InstructionPresetList, InstructionsDocument, InstructionsStore,
    InstructionsTarget, PresetUpdate,
};
use crate::error::AppError;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v2/providers/{provider}/instructions",
            get(read).put(write_text),
        )
        .route(
            "/v2/providers/{provider}/instructions/source",
            put(set_source),
        )
        .route(
            "/v2/instruction-presets",
            get(list_presets).post(create_preset),
        )
        .route(
            "/v2/instruction-presets/{id}",
            put(update_preset).delete(delete_preset),
        )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteRequest {
    revision: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRequest {
    revision: String,
    preset_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreatePresetRequest {
    name: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdatePresetRequest {
    revision: u64,
    name: String,
    text: String,
}

const EDIT_ACTION: &str = "editing CLI instructions";

fn target(provider: &str) -> Result<InstructionsTarget, AppError> {
    InstructionsTarget::from_provider(provider).ok_or_else(|| {
        AppError::Unsupported(format!("{provider} has no editable instructions file"))
    })
}

fn store(state: &AppState) -> InstructionsStore {
    InstructionsStore::new(&state.config.data_dir)
}

fn body<T: DeserializeOwned>(body: Value) -> Result<T, AppError> {
    serde_json::from_value(body)
        .map_err(|error| AppError::InvalidRequest(format!("invalid instructions request: {error}")))
}

async fn read(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Result<Json<InstructionsDocument>, AppError> {
    require_auth(&state, &headers)?;
    let (store, file) = (store(&state), target(&provider)?.file());
    blocking(move || cli_instructions::read(&store, &file)).await
}

async fn write_text(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Json<InstructionsDocument>, AppError> {
    let file = target(&provider)?.file();
    require_signed_device(&state, &headers, EDIT_ACTION)?;
    let request: WriteRequest = body(request)?;
    let store = store(&state);
    blocking(move || cli_instructions::write_text(&store, &file, &request.revision, &request.text))
        .await
}

async fn set_source(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Json<InstructionsDocument>, AppError> {
    let file = target(&provider)?.file();
    require_signed_device(&state, &headers, EDIT_ACTION)?;
    let request: SourceRequest = body(request)?;
    let store = store(&state);
    blocking(move || {
        cli_instructions::set_source(
            &store,
            &file,
            &request.revision,
            request.preset_id.as_deref(),
        )
    })
    .await
}

async fn list_presets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<InstructionPresetList>, AppError> {
    require_auth(&state, &headers)?;
    let store = store(&state);
    blocking(move || cli_instructions::list_presets(&store)).await
}

async fn create_preset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<(StatusCode, Json<InstructionPreset>), AppError> {
    require_signed_device(&state, &headers, EDIT_ACTION)?;
    let request: CreatePresetRequest = body(request)?;
    let store = store(&state);
    let preset =
        blocking(move || cli_instructions::create_preset(&store, &request.name, &request.text))
            .await?;
    Ok((StatusCode::CREATED, preset))
}

async fn update_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Result<Json<PresetUpdate>, AppError> {
    require_signed_device(&state, &headers, EDIT_ACTION)?;
    let request: UpdatePresetRequest = body(request)?;
    let store = store(&state);
    blocking(move || {
        cli_instructions::update_preset(&store, &id, request.revision, &request.name, &request.text)
    })
    .await
}

async fn delete_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    require_signed_device(&state, &headers, EDIT_ACTION)?;
    let store = store(&state);
    blocking(move || cli_instructions::delete_preset(&store, &id))
        .await
        .map(|_| StatusCode::NO_CONTENT)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<Json<T>, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map(Json)
}
