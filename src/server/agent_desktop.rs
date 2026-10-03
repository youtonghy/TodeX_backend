//! `/v2/agent-desktop` — the switch for agent desktop tools, plus
//! per-conversation access and screenshots. See [`crate::agent_desktop`].

use axum::extract::{Path as AxumPath, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent_desktop::executors::CAPABILITY_BROWSER;
use crate::app_state::AppState;
use crate::error::AppError;

use super::v2::require_auth;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsRequest {
    enabled: bool,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/agent-desktop", get(settings).put(set_settings))
        .route(
            "/v2/conversations/{id}/agent-desktop",
            delete(revoke_conversation),
        )
        .route(
            "/v2/conversations/{id}/agent-shots/{shot_id}",
            get(read_shot),
        )
}

async fn settings_view(state: &AppState) -> Value {
    json!({
        "enabled": state.agent_desktop.enabled().await,
        "executors": state.agent_desktop.executors().online(CAPABILITY_BROWSER),
    })
}

async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(settings_view(&state).await))
}

async fn set_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SettingsRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.agent_desktop.set_enabled(request.enabled).await?;
    Ok(Json(settings_view(&state).await))
}

/// The user stopped the agent's browser for this conversation; the next tool
/// call asks again.
async fn revoke_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .get_owned(&auth.tenant_id, &conversation_id)
        .await?;
    let revoked = state.agent_desktop.revoke(&conversation_id);
    if revoked.is_some() {
        state
            .conversations
            .append_agent_event(
                &conversation_id,
                "desktop.browser.grant",
                json!({ "status": "revoked", "reason": "user" }),
            )
            .await?;
    }
    Ok(Json(
        json!({ "conversationId": conversation_id, "revoked": revoked.is_some() }),
    ))
}

async fn read_shot(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((conversation_id, shot_id)): AxumPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .get_owned(&auth.tenant_id, &conversation_id)
        .await?;
    let jpeg = state
        .agent_desktop
        .shots()
        .read(&conversation_id, &shot_id)
        .await?;
    Ok(Json(json!({
        "shotId": shot_id,
        "mimeType": "image/jpeg",
        "dataUrl": format!("data:image/jpeg;base64,{}", BASE64.encode(jpeg)),
    })))
}
