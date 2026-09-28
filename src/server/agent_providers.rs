//! `/v2/agent-providers` — managed provider accounts per agent (cc-switch
//! model). All routes sit behind the device-signature middleware like the rest
//! of `/v2`.

use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent_providers::{
    self, ActivateInput, AgentProviderInput, AgentProviderTransfer, ImportLiveInput,
    MAX_PROVIDER_TRANSFER_BYTES,
};
use crate::app_state::AppState;
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::v2::require_auth;
use super::websocket::{self, AuthContext};

#[derive(Debug, Deserialize)]
struct AgentProvidersQuery {
    agent: Option<String>,
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/agent-providers", get(list_agent_providers))
        .route("/v2/agent-providers/{agent}/live", get(agent_live))
        .route("/v2/agent-providers/{agent}/import-live", post(import_live))
        .route(
            "/v2/agent-providers/{agent}/export",
            get(export_agent_providers),
        )
        .route(
            "/v2/agent-providers/{agent}/import",
            post(import_agent_providers).layer(DefaultBodyLimit::max(MAX_PROVIDER_TRANSFER_BYTES)),
        )
        .route(
            "/v2/agent-providers/{agent}/{id}",
            put(upsert_agent_provider).delete(delete_agent_provider),
        )
        .route(
            "/v2/agent-providers/{agent}/{id}/activate",
            post(activate_agent_provider),
        )
        .route(
            "/v2/agent-providers/{agent}/{id}/models",
            get(agent_provider_models).post(preview_agent_provider_models),
        )
}

async fn list_agent_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AgentProvidersQuery>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = query
        .agent
        .as_deref()
        .map(agent_providers::supported_agent)
        .transpose()?;
    Ok(Json(state.agent_providers.snapshot(agent).await?))
}

async fn agent_live(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(agent): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    Ok(Json(state.agent_providers.live(agent).await?))
}

async fn upsert_agent_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((agent, id)): AxumPath<(String, String)>,
    Json(input): Json<AgentProviderInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    Ok(Json(state.agent_providers.upsert(agent, &id, input).await?))
}

async fn delete_agent_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((agent, id)): AxumPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    state.agent_providers.delete(agent, &id).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn activate_agent_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((agent, id)): AxumPath<(String, String)>,
    body: Option<Json<ActivateInput>>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    let model_id = body.and_then(|body| body.0.model_id);
    Ok(Json(
        state.agent_providers.activate(agent, &id, model_id).await?,
    ))
}

async fn import_live(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(agent): AxumPath<String>,
    Json(input): Json<ImportLiveInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    Ok(Json(state.agent_providers.import_live(agent, input).await?))
}

/// Exports carry credentials in clear, so like CLI upgrades they need a
/// device-signed request even on loopback deployments, and are audited.
async fn export_agent_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(agent): AxumPath<String>,
) -> Result<Json<AgentProviderTransfer>, AppError> {
    let auth = require_signed_device(&state, &headers, "exporting providers")?;
    let agent = agent_providers::supported_agent(&agent)?;
    let transfer = state.agent_providers.export(agent).await;
    append_transfer_audit(&state, &auth, "export", agent, transfer.providers.len()).await?;
    Ok(Json(transfer))
}

async fn import_agent_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(agent): AxumPath<String>,
    Json(transfer): Json<AgentProviderTransfer>,
) -> Result<Json<Value>, AppError> {
    let auth = require_signed_device(&state, &headers, "importing providers")?;
    let agent = agent_providers::supported_agent(&agent)?;
    let count = transfer.providers.len();
    let block = state.agent_providers.import(agent, transfer).await?;
    append_transfer_audit(&state, &auth, "import", agent, count).await?;
    Ok(Json(block))
}

fn require_signed_device(
    state: &AppState,
    headers: &HeaderMap,
    action: &str,
) -> Result<AuthContext, AppError> {
    let auth = require_auth(state, headers)?;
    if !state.config.security.enable_auth {
        return Err(AppError::Unauthorized(format!(
            "{action} requires device authentication"
        )));
    }
    Ok(auth)
}

async fn append_transfer_audit(
    state: &AppState,
    auth: &AuthContext,
    action: &str,
    agent: ProviderKind,
    provider_count: usize,
) -> Result<(), AppError> {
    let event = crate::event::EventRecord::new(
        "agent_providers.transfer.audit",
        None,
        None,
        None,
        json!({
            "principal_id": auth.principal_id,
            "tenant_id": auth.tenant_id,
            "token_id": auth.token_id,
            "action": action,
            "agent": agent.as_str(),
            "provider_count": provider_count,
        }),
    );
    websocket::append_audit_event(state, &event).await?;
    state.events.publish(event).await;
    Ok(())
}

async fn agent_provider_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((agent, id)): AxumPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    Ok(Json(state.agent_providers.models(agent, &id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewModelsInput {
    #[serde(default)]
    settings_config: Value,
}

async fn preview_agent_provider_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((agent, id)): AxumPath<(String, String)>,
    Json(input): Json<PreviewModelsInput>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    Ok(Json(
        state
            .agent_providers
            .preview_models(agent, &id, input.settings_config)
            .await?,
    ))
}
