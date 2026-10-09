//! `/v2/agent-desktop` — the switch for agent desktop tools, Computer Use
//! on this host, plus per-conversation access, screenshots and the live
//! frame. See [`crate::agent_desktop`] and [`crate::computer`].

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::computer::platform::Permission;
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
struct PermissionRequest {
    /// `screen` or `accessibility`; absent requests every missing one.
    #[serde(default)]
    permission: Option<Permission>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevokeQuery {
    /// `browser` or `screen`; absent revokes both.
    #[serde(default)]
    capability: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrameQuery {
    /// `screen` (Computer Use, default) or `browser`.
    #[serde(default)]
    capability: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProfileRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignRequest {
    /// Workspace id (or path for workspaces without one).
    workspace: String,
    profile_id: String,
}

fn browser_error(error: crate::agent_browser::BrowserError) -> AppError {
    match error.code.as_str() {
        "INVALID_ARGUMENT" => AppError::InvalidRequest(error.message),
        "NO_TAB" => AppError::NotFound(error.message),
        _ => AppError::Conflict(error.to_string()),
    }
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v2/agent-browser/profiles",
            get(list_profiles).post(create_profile),
        )
        .route(
            "/v2/agent-browser/profiles/{id}",
            put(rename_profile).delete(delete_profile),
        )
        .route("/v2/agent-browser/workspaces", put(assign_profile))
        .route("/v2/agent-browser/install", post(install_browser))
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
        // Retired: browsers no longer run on desktops.
        "executors": [],
        "browser": state.agent_desktop.browser().status(),
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
    let change = state
        .agent_mcp
        .update_desktop_settings(request.enabled, request.computer_enabled)
        .await?;
    for conversation_id in change.screen_ended {
        journal_screen_end(&state, &conversation_id, "revoked").await;
    }
    Ok(Json(settings_view(&state).await))
}

/// Shows the OS permission prompt on this host for one permission (Screen
/// Recording or Accessibility), or for each missing one when none is named.
async fn request_computer_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let which = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<PermissionRequest>(&body)
            .map_err(|error| {
                AppError::InvalidRequest(format!("invalid permission request: {error}"))
            })?
            .permission
    };
    state
        .agent_desktop
        .computer()
        .host()
        .request_permissions(which)
        .await;
    Ok(Json(settings_view(&state).await))
}

/// The host's screen right now, for a live view of the conversation that
/// controls it. Clients poll while the view is visible.
async fn read_frame(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<FrameQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .get_owned(&auth.tenant_id, &conversation_id)
        .await?;
    match query.capability.as_deref() {
        None | Some("screen") => {}
        Some("browser") => {
            let jpeg = state
                .agent_desktop
                .browser()
                .frame(&conversation_id)
                .await
                .map_err(browser_error)?;
            return Ok(Json(json!({
                "mimeType": "image/jpeg",
                "dataUrl": format!("data:image/jpeg;base64,{}", BASE64.encode(jpeg)),
            })));
        }
        Some(other) => {
            return Err(AppError::InvalidRequest(format!(
                "unknown capability {other}; use screen or browser"
            )))
        }
    }
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
        None => (
            desktop.revoke_browser(&conversation_id),
            state.agent_mcp.revoke_computer(&conversation_id),
        ),
        Some("browser") => (desktop.revoke_browser(&conversation_id), (None, false)),
        Some("screen") => (None, state.agent_mcp.revoke_computer(&conversation_id)),
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

// ---- Agent browser profiles ------------------------------------------------

async fn list_profiles(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(
        state.agent_desktop.browser().profiles(),
    )?))
}

async fn create_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ProfileRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let record = state
        .agent_desktop
        .browser()
        .create_profile(&request.name)
        .map_err(browser_error)?;
    Ok(Json(serde_json::to_value(record)?))
}

async fn rename_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ProfileRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let browser = state.agent_desktop.browser();
    browser
        .rename_profile(&id, &request.name)
        .map_err(browser_error)?;
    Ok(Json(serde_json::to_value(browser.profiles())?))
}

/// Deletes the profile and its cookies, storage and cache.
async fn delete_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let browser = state.agent_desktop.browser();
    browser.delete_profile(&id).await.map_err(browser_error)?;
    Ok(Json(serde_json::to_value(browser.profiles())?))
}

async fn assign_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AssignRequest>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let browser = state.agent_desktop.browser();
    browser
        .assign_profile(&request.workspace, &request.profile_id)
        .await
        .map_err(browser_error)?;
    Ok(Json(serde_json::to_value(browser.profiles())?))
}

/// Starts downloading the pinned Chromium (progress shows in settings).
async fn install_browser(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.agent_desktop.browser().start_install();
    Ok(Json(settings_view(&state).await))
}
