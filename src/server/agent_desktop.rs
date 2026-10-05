//! `/v2/agent-desktop` — the switch for agent desktop tools, Computer Use
//! on this host, plus per-conversation access, screenshots and the live
//! frame. See [`crate::agent_desktop`] and [`crate::computer`].

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;

use super::v2::require_auth;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsRequest {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    computer_enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevokeQuery {
    /// `browser` or `screen`; absent revokes both.
    #[serde(default)]
    capability: Option<String>,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/agent-desktop", get(settings).put(set_settings))
        .route(
            "/v2/agent-desktop/computer/permissions",
            post(request_computer_permissions),
        )
        .route(
            "/v2/conversations/{id}/agent-desktop/frame",
            get(read_frame),
        )
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
    let settings = state.agent_desktop.settings().await;
    json!({
        "enabled": settings.enabled,
        "computerEnabled": settings.computer_enabled,
        "executors": state.agent_desktop.executors().all(),
        "computer": state.agent_desktop.computer().host().status(),
    })
}

async fn journal_screen_end(state: &AppState, conversation_id: &str, reason: &str) {
    if let Err(error) = state
        .conversations
        .append_agent_event(
            conversation_id,
            "desktop.computer.session",
            json!({ "status": "ended", "reason": reason }),
        )
        .await
    {
        tracing::warn!(%error, "failed to journal the end of a Computer Use session");
    }
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
    if request.enabled.is_none() && request.computer_enabled.is_none() {
        return Err(AppError::InvalidRequest(
            "set enabled and/or computerEnabled".to_owned(),
        ));
    }
    let (_, ended) = state
        .agent_desktop
        .update_settings(request.enabled, request.computer_enabled)
        .await?;
    for conversation_id in ended {
        journal_screen_end(&state, &conversation_id, "revoked").await;
    }
    Ok(Json(settings_view(&state).await))
}

/// Shows the OS permission prompts on this host (Screen Recording,
/// Accessibility) for what is missing.
async fn request_computer_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state
        .agent_desktop
        .computer()
        .host()
        .request_permissions()
        .await;
    Ok(Json(settings_view(&state).await))
}

/// The host's screen right now, for a live view of the conversation that
/// controls it. Clients poll while the view is visible.
async fn read_frame(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .get_owned(&auth.tenant_id, &conversation_id)
        .await?;
    let jpeg = state
        .agent_desktop
        .live_frame(&conversation_id)
        .await
        .map_err(|error| match error.code.as_str() {
            "NOT_CONTROLLING" => AppError::NotFound(error.message),
            _ => AppError::InvalidRequest(error.to_string()),
        })?;
    Ok(Json(json!({
        "mimeType": "image/jpeg",
        "dataUrl": format!("data:image/jpeg;base64,{}", BASE64.encode(jpeg.as_slice())),
    })))
}

/// The user stopped the agent's browser and/or Computer Use for this
/// conversation; the next tool call asks again.
async fn revoke_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<RevokeQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .get_owned(&auth.tenant_id, &conversation_id)
        .await?;
    let desktop = &state.agent_desktop;
    let (browser, (computer, screen_ended)) = match query.capability.as_deref() {
        None => {
            let revoked = desktop.revoke(&conversation_id);
            (revoked.browser, (revoked.computer, revoked.screen_ended))
        }
        Some("browser") => (desktop.revoke_browser(&conversation_id), (None, false)),
        Some("screen") => (None, desktop.revoke_computer(&conversation_id)),
        Some(other) => {
            return Err(AppError::InvalidRequest(format!(
                "unknown capability {other}; use browser or screen"
            )))
        }
    };
    if browser.is_some() {
        state
            .conversations
            .append_agent_event(
                &conversation_id,
                "desktop.browser.grant",
                json!({ "status": "revoked", "reason": "user" }),
            )
            .await?;
    }
    if computer.is_some() {
        state
            .conversations
            .append_agent_event(
                &conversation_id,
                "desktop.computer.grant",
                json!({ "status": "revoked", "reason": "user" }),
            )
            .await?;
    }
    if screen_ended {
        journal_screen_end(&state, &conversation_id, "user").await;
    }
    let revoked = browser.is_some() || computer.is_some() || screen_ended;
    Ok(Json(
        json!({ "conversationId": conversation_id, "revoked": revoked }),
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
