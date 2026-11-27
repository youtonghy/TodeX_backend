//! `/v2/agent-desktop` — the switch for agent desktop tools, Computer Use
//! and the agent camera on this host, plus per-conversation access,
//! screenshots and live frames. See [`crate::agent_desktop`],
//! [`crate::computer`] and [`crate::camera`].

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent_browser::{hosts::HostApproval, BrowserEngineKind};
use crate::app_state::AppState;
use crate::computer::{agent_desktop_engine::EngineKind, platform::Permission};
use crate::error::AppError;

use super::v2::require_auth;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsRequest {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    computer_enabled: Option<bool>,
    /// `native` or `agentDesktop`.
    #[serde(default)]
    computer_engine: Option<EngineKind>,
    /// `chromium` or `kitesurf`.
    #[serde(default)]
    browser_engine: Option<BrowserEngineKind>,
    /// `ask` (each host beyond this computer) or `auto` (every host).
    #[serde(default)]
    browser_host_approval: Option<HostApproval>,
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
struct CameraQuery {
    /// The camera to show; default: the conversation's session (or the
    /// first camera its approval card offers).
    #[serde(default)]
    device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevokeQuery {
    /// `browser`, `screen` or `camera`; absent revokes all.
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
        .route("/v2/agent-browser/hosts", get(list_hosts))
        .route("/v2/agent-browser/hosts/{host}", delete(remove_host))
        .route("/v2/agent-browser/install", post(install_browser))
        .route("/v2/agent-desktop", get(settings).put(set_settings))
        .route(
            "/v2/agent-desktop/computer/permissions",
            post(request_computer_permissions),
        )
        .route("/v2/agent-desktop/computer/apps", get(list_computer_apps))
        .route("/v2/agent-desktop/camera", put(set_camera_settings))
        .route(
            "/v2/agent-desktop/camera/permission",
            post(request_camera_permission),
        )
        .route("/v2/agent-desktop/camera/preview", get(read_camera_preview))
        .route(
            "/v2/conversations/{id}/agent-desktop/camera",
            get(read_camera_frame),
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
        "cameraEnabled": state.agent_desktop.camera().settings().await.enabled,
        "camera": state.agent_desktop.camera().status_view().await,
        // Retired: browsers no longer run on desktops.
        "executors": [],
        "browser": state.agent_desktop.browser().status(),
        "computer": state.agent_desktop.computer().host().status(),
        "computerEngine": state.agent_desktop.computer_engine(),
        "computerEngines": {
            "agentDesktop": state.agent_desktop.agent_desktop_engine(),
        },
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
    if request.enabled.is_none()
        && request.computer_enabled.is_none()
        && request.computer_engine.is_none()
        && request.browser_engine.is_none()
        && request.browser_host_approval.is_none()
    {
        return Err(AppError::InvalidRequest(
            "set enabled, computerEnabled, computerEngine, browserEngine and/or browserHostApproval"
                .to_owned(),
        ));
    }
    if let Some(mode) = request.browser_host_approval {
        state.agent_desktop.browser().hosts().set_mode(mode)?;
    }
    if let Some(engine) = request.browser_engine {
        state.agent_desktop.browser().set_engine(engine).await?;
    }
    if let Some(engine) = request.computer_engine {
        if let Some(conversation_id) = state.agent_desktop.set_computer_engine(engine).await? {
            journal_screen_end(&state, &conversation_id, "engine").await;
        }
    }
    if request.enabled.is_some() || request.computer_enabled.is_some() {
        let change = state
            .agent_mcp
            .update_desktop_settings(request.enabled, request.computer_enabled)
            .await?;
        for conversation_id in change.screen_ended {
            journal_screen_end(&state, &conversation_id, "revoked").await;
        }
        // Each journals its own end once its last frame is saved.
        drop(change.camera_ended);
    }
    Ok(Json(settings_view(&state).await))
}

/// The agent camera's switch, allowed cameras and defaults. Turning it off,
/// or taking a camera off the list, ends the sessions and grants it covered.
async fn set_camera_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(update): Json<crate::camera::SettingsUpdate>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let (_, _ended, revoked) = state.agent_desktop.camera().update_settings(update).await?;
    for conversation_id in revoked {
        journal_camera_revoked(&state, &conversation_id, "settings").await;
    }
    Ok(Json(settings_view(&state).await))
}

async fn journal_camera_revoked(state: &AppState, conversation_id: &str, reason: &str) {
    if let Err(error) = state
        .conversations
        .append_agent_event(
            conversation_id,
            "desktop.camera.grant",
            json!({ "status": "revoked", "reason": reason }),
        )
        .await
    {
        tracing::warn!(%error, "failed to journal a camera revoke");
    }
}

/// Shows macOS's camera prompt on this host (when never answered); the
/// settings view reports `camera.permissionRequest` while it is up.
async fn request_camera_permission(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    state.agent_desktop.camera().request_permission();
    Ok(Json(settings_view(&state).await))
}

fn camera_error(error: crate::camera::CameraError) -> AppError {
    match error.code {
        "INVALID_ARGUMENT" => AppError::InvalidRequest(error.message),
        _ => AppError::Conflict(error.to_string()),
    }
}

fn jpeg_view(jpeg: &[u8]) -> Value {
    json!({
        "mimeType": "image/jpeg",
        "dataUrl": format!("data:image/jpeg;base64,{}", BASE64.encode(jpeg)),
    })
}

/// What a connected camera sees now, for choosing cameras in settings.
/// Opens the camera; it closes a few seconds after the last poll.
async fn read_camera_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CameraQuery>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let camera = state.agent_desktop.camera();
    let Some(device_id) = query.device_id else {
        return Err(AppError::InvalidRequest("deviceId is required".to_owned()));
    };
    let connected = camera.host().devices().await.map_err(camera_error)?;
    if !connected.iter().any(|device| device.id == device_id) {
        return Err(AppError::NotFound(format!(
            "no camera {device_id} is connected"
        )));
    }
    let jpeg = camera.preview(&device_id).await.map_err(camera_error)?;
    let mut view = jpeg_view(&jpeg);
    view["deviceId"] = Value::from(device_id);
    Ok(Json(view))
}

/// The conversation's camera now: its recording session (with progress),
/// or a camera its open approval card offers. Clients poll while visible.
async fn read_camera_frame(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<CameraQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .get_owned(&auth.tenant_id, &conversation_id)
        .await?;
    let camera = state.agent_desktop.camera();
    let Some(device_id) = camera.viewable(&conversation_id, query.device_id.as_deref()) else {
        return Err(AppError::NotFound(
            "this conversation is not using or asking for that camera".to_owned(),
        ));
    };
    let jpeg = camera.preview(&device_id).await.map_err(camera_error)?;
    let mut view = jpeg_view(&jpeg);
    view["deviceId"] = Value::from(device_id);
    if let Some(session) = camera.running_for(&conversation_id) {
        view["session"] = json!({
            "sessionId": session.id,
            "deviceName": session.device.name,
            "capture": session.capture,
            "maxSeconds": session.max_seconds,
            "maxFrames": session.max_frames,
            "progress": session.progress(),
        });
    }
    Ok(Json(view))
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

/// Apps on this host for the composer's `@app:` mentions: running ones
/// first, then installed ones, without TodeX and the apps agents may never
/// control.
async fn list_computer_apps(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    if !state.agent_desktop.computer_enabled().await {
        return Err(AppError::Conflict("Computer Use is off".to_owned()));
    }
    let apps = state
        .agent_desktop
        .computer()
        .host()
        .apps()
        .await
        .map_err(|error| AppError::Conflict(error.to_string()))?;
    Ok(Json(json!({ "apps": apps })))
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
    let camera = desktop.camera();
    let (browser, (computer, screen_ended), (camera_grant, camera_session)) =
        match query.capability.as_deref() {
            None => (
                desktop.revoke_browser(&conversation_id),
                state.agent_mcp.revoke_computer(&conversation_id),
                camera.revoke(&conversation_id, crate::camera::SessionEnd::User),
            ),
            Some("browser") => (
                desktop.revoke_browser(&conversation_id),
                (None, false),
                (None, None),
            ),
            Some("screen") => (
                None,
                state.agent_mcp.revoke_computer(&conversation_id),
                (None, None),
            ),
            Some("camera") => (
                None,
                (None, false),
                camera.revoke(&conversation_id, crate::camera::SessionEnd::User),
            ),
            Some(other) => {
                return Err(AppError::InvalidRequest(format!(
                    "unknown capability {other}; use browser, screen or camera"
                )))
            }
        };
    // A stopped session journals its own end once saved.
    if camera_grant.is_some() {
        journal_camera_revoked(&state, &conversation_id, "user").await;
    }
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
    let revoked = browser.is_some()
        || computer.is_some()
        || screen_ended
        || camera_grant.is_some()
        || camera_session.is_some();
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

/// Hosts beyond this computer the person always allows the agent's tab.
async fn list_hosts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(
        json!({ "hosts": state.agent_desktop.browser().hosts().always() }),
    ))
}

/// Takes a host off the always-allowed list; the agent asks again next time.
async fn remove_host(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(host): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let hosts = state.agent_desktop.browser().hosts();
    let removed = hosts.remove(&host.to_lowercase())?;
    if !removed {
        return Err(AppError::NotFound(format!("{host} is not an allowed host")));
    }
    Ok(Json(json!({ "hosts": hosts.always() })))
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
