//! `/v2/agent-providers` — managed provider accounts per agent (cc-switch
//! model). All routes sit behind the device-signature middleware like the rest
//! of `/v2`.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::agent_providers::{self, ActivateInput, AgentProviderInput, ImportLiveInput};
use crate::app_state::AppState;
use crate::error::AppError;

use super::v2::require_auth;

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
            "/v2/agent-providers/{agent}/{id}",
            put(upsert_agent_provider).delete(delete_agent_provider),
        )
        .route(
            "/v2/agent-providers/{agent}/{id}/activate",
            post(activate_agent_provider),
        )
        .route(
            "/v2/agent-providers/{agent}/{id}/models",
            get(agent_provider_models),
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

async fn agent_provider_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((agent, id)): AxumPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let agent = agent_providers::supported_agent(&agent)?;
    Ok(Json(state.agent_providers.models(agent, &id).await?))
}
