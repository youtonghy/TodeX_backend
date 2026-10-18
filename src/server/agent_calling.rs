//! `/v2/agent-calling` — the Agent Calling settings clients manage: the
//! global agent/model scope, the `todex_agents` MCP entry, and the external
//! API listener (`config.toml` `[api]`, applied on the next daemon start).
//! Reads need a paired device; writes need a signed device even on
//! loopback, since they decide which agents other agents and API keys may
//! run and how their permission requests are answered. See
//! [`crate::agent_calls`].

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent_calls::{AgentCallingUpdate, LIMITS};
use crate::app_state::AppState;
use crate::error::AppError;

use super::agent_providers::require_signed_device;
use super::v2::require_auth;
use super::websocket::{self, AuthContext};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/agent-calling", get(read).put(update))
        .route("/v2/agent-calling/api-listener", put(update_api_listener))
}

/// `PUT /v2/agent-calling/api-listener`: absent fields keep their value.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiListenerUpdate {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    port: Option<u64>,
}

fn view(state: &AppState) -> Value {
    let settings = state.agent_calling.settings();
    json!({
        "scope": settings.scope,
        "mcp": {
            "enabled": settings.mcp.enabled,
            "approval": settings.mcp.approval,
            "injected": settings.mcp_injected(),
        },
        "apiListener": state.agent_calling.api_listener(&state.config),
        "limits": LIMITS,
    })
}

async fn read(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(view(&state)))
}

async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let auth = require_signed_device(&state, &headers, "changing agent calling settings")?;
    let update: AgentCallingUpdate = serde_json::from_value(body).map_err(|error| {
        AppError::InvalidRequest(format!("invalid agent calling settings: {error}"))
    })?;
    let settings = state.agent_calling.update(update).await?;
    append_audit(
        &state,
        &auth,
        "settings",
        json!({
            "agents": settings.scope.agents,
            "restrictedModelAgents": settings.scope.models.keys().collect::<Vec<_>>(),
            "mcpEnabled": settings.mcp.enabled,
            "mcpApproval": settings.mcp.approval,
        }),
    )
    .await?;
    Ok(Json(view(&state)))
}

async fn update_api_listener(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let auth = require_signed_device(&state, &headers, "changing the API listener")?;
    let update: ApiListenerUpdate = serde_json::from_value(body).map_err(|error| {
        AppError::InvalidRequest(format!("invalid API listener settings: {error}"))
    })?;
    let listener = state
        .agent_calling
        .save_api_listener(&state.config, update.enabled, update.port)
        .await?;
    append_audit(
        &state,
        &auth,
        "api_listener",
        json!({ "enabled": listener.enabled, "port": listener.port }),
    )
    .await?;
    Ok(Json(view(&state)))
}

async fn append_audit(
    state: &AppState,
    auth: &AuthContext,
    action: &str,
    values: Value,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "agent_calling.settings.audit",
        None,
        None,
        None,
        json!({
            "principal_id": auth.principal_id,
            "tenant_id": auth.tenant_id,
            "token_id": auth.token_id,
            "action": action,
            "values": values,
        }),
    );
    websocket::append_audit_event(state, &event).await?;
    Ok(())
}
