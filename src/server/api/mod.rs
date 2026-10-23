//! The external API listener (`[api]`): REST + SSE under `/api/v1`,
//! authenticated with API keys (`Authorization: Bearer tdx_…`). It shares
//! the daemon's `AppState` with the device listener, so conversations run
//! on the same supervisor; each key owns its conversations as
//! `apikey:<id>`. The handlers are thin adapters over the shared call layer
//! [`crate::agent_calls`], which also applies the global agent scope. See
//! docs/API.md ("外部 API").
mod auth;
mod conversations;
mod stream;

use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    routing::get,
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;

use crate::agent_calls::{self, ApiKeyCaller};
use crate::app_state::AppState;
use crate::conversation::{ConversationManifest, ProviderKind};
use crate::error::AppError;
use crate::workspace_paths::validate_workspace_directory_text;

#[cfg(all(test, unix))]
use crate::{
    agent_calls::ApiKeyPermissionPolicy, api_keys::ApprovalPolicy, provider::PermissionPolicy,
};
pub(crate) use auth::AuthFailures;

/// Request bodies are prompt text and small JSON objects.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn api_router(state: AppState) -> Router {
    let authenticated = Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/agents", get(agents))
        .route("/api/v1/agents/{agent}/models", get(agent_models))
        .route("/api/v1/workspaces", get(workspaces))
        .merge(conversations::routes())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));
    Router::new()
        .route("/api/v1/health", get(health))
        .merge(authenticated)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": crate::version::APP_VERSION }))
}

async fn me(Extension(key): Extension<ApiKeyCaller>) -> Json<Value> {
    Json(key.record.summary())
}

async fn agents(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyCaller>,
) -> Json<Value> {
    let agents = agent_calls::list_agents(&state, &key).await;
    Json(json!({ "agents": agents }))
}

#[derive(Deserialize)]
struct WorkspaceQuery {
    workspace: String,
}

async fn agent_models(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyCaller>,
    AxumPath(agent): AxumPath<String>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<Json<Value>, AppError> {
    let agent = parse_agent(&agent)?;
    let models = agent_calls::models(&state, &key, agent, Some(&query.workspace)).await?;
    Ok(Json(json!({ "agent": agent, "models": models })))
}

/// The configured roots narrowed by the key's workspace scope.
async fn workspaces(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyCaller>,
) -> Json<Value> {
    let roots = &state.config.workspace_roots;
    let workspaces = match &key.record.scopes.workspaces {
        None => roots
            .iter()
            .map(|root| json!({ "path": root, "scope": "root" }))
            .collect::<Vec<_>>(),
        Some(scoped) => scoped
            .iter()
            .filter(|path| {
                validate_workspace_directory_text(roots, &path.to_string_lossy()).is_ok()
            })
            .map(|path| json!({ "path": path, "scope": "key" }))
            .collect(),
    };
    Json(json!({ "workspaces": workspaces }))
}

fn parse_agent(agent: &str) -> Result<ProviderKind, AppError> {
    agent.parse().map_err(AppError::InvalidRequest)
}

/// A manifest as the API returns it: the title decrypted for the key.
async fn present_manifest(
    reader: &mut crate::conversation::server_decrypt::HistoryReader,
    manifest: ConversationManifest,
) -> Value {
    let title = reader.title(&manifest).await;
    let mut value = serde_json::to_value(&manifest).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("titleEnc");
        map.remove("ownerId");
        map.insert("title".to_owned(), json!(title));
        map.insert("agent".to_owned(), json!(manifest.provider));
    }
    value
}

/// Cancels the running turns a key may no longer run: every turn of a key
/// that is revoked or expired, and the turns of an active key whose
/// conversation's workspace or agent fell outside its scopes (narrowed by
/// `PATCH /v2/api-keys/{id}` or by another process).
pub(crate) async fn cancel_unauthorized_key_turns(state: &AppState) {
    let Ok(keys) = state.api_keys.list() else {
        return;
    };
    let now = crate::api_keys::unix_ms();
    for key in &keys {
        let active = key.is_active(now);
        let owner = key.owner_id();
        let Ok(manifests) = state.conversations.list_owned(&owner).await else {
            continue;
        };
        for manifest in manifests.iter().filter(|manifest| {
            matches!(
                manifest.status,
                crate::conversation::ConversationStatus::Running
                    | crate::conversation::ConversationStatus::WaitingPermission
            ) && !(active
                && key.scopes.allows_agent(manifest.provider)
                && key.scopes.allows_workspace(&manifest.workspace))
        }) {
            if let Err(error) = state
                .conversations
                .cancel_owned(&owner, &manifest.id, None)
                .await
            {
                tracing::warn!(
                    conversation_id = %manifest.id,
                    error = %error,
                    "could not cancel the turn of an inactive or narrowed API key"
                );
            }
        }
    }
}

/// Every few seconds: keys that expire, or are revoked or narrowed by
/// another process, reach the daemon only through `api-keys.json`.
pub(crate) fn spawn_revocation_watch(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            ticker.tick().await;
            cancel_unauthorized_key_turns(&state).await;
        }
    })
}

// The fixture runs a Python script as the provider executable.
#[cfg(all(test, unix))]
mod scope_tests;
#[cfg(all(test, unix))]
mod tests;
