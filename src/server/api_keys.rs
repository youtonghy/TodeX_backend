//! `/v2/api-keys`: paired devices manage the external API's keys. The plain
//! key is returned once, by `POST`.

use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::v2::require_auth;
use crate::api_keys::{ApiKeyScopes, ApiKeyUpdate, ApprovalPolicy, NewApiKey};
use crate::app_state::AppState;
use crate::error::AppError;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/api-keys", get(list_keys).post(create_key))
        .route("/v2/api-keys/listener", get(listener))
        .route("/v2/api-keys/{id}", patch(update_key).delete(revoke_key))
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let keys = state
        .api_keys
        .list()?
        .iter()
        .map(|key| key.summary())
        .collect::<Vec<_>>();
    Ok(Json(json!({ "keys": keys })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateKeyRequest {
    name: String,
    #[serde(default)]
    scopes: ApiKeyScopes,
    #[serde(default)]
    approval: ApprovalPolicy,
    #[serde(default)]
    expires_at: Option<u64>,
}

async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateKeyRequest>,
) -> Result<Response, AppError> {
    require_auth(&state, &headers)?;
    let (record, key) = state.api_keys.create(NewApiKey {
        name: request.name,
        scopes: request.scopes,
        approval: request.approval,
        expires_at: request.expires_at,
    })?;
    let mut body = record.summary();
    body["key"] = json!(key.as_str());
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

async fn update_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(update): Json<ApiKeyUpdate>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let record = state.api_keys.update(&id, update)?;
    Ok(Json(record.summary()))
}

async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    if state.api_keys.get(&id)?.is_none() {
        return Err(AppError::NotFound("api key".to_owned()));
    }
    let revoked = state.api_keys.revoke(&id)?;
    super::api::cancel_inactive_key_turns(&state).await;
    Ok(Json(json!({ "id": id, "revoked": revoked })))
}

async fn listener(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    let api = &state.config.api;
    Ok(Json(json!({
        "enabled": api.enabled,
        "host": api.host,
        "port": api.port,
    })))
}
