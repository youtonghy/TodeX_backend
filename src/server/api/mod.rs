//! The external API listener (`[api]`): REST + SSE under `/api/v1`,
//! authenticated with API keys (`Authorization: Bearer tdx_…`). It shares
//! the daemon's `AppState` with the device listener, so conversations run
//! on the same supervisor; each key owns its conversations as
//! `apikey:<id>`. See docs/API.md ("外部 API").
mod auth;
mod conversations;
mod stream;

use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    routing::get,
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::trace::TraceLayer;
use zeroize::Zeroizing;

use crate::api_keys::{ApiKeyRecord, ApprovalPolicy};
use crate::app_state::AppState;
use crate::conversation::{ConversationManifest, ProviderKind};
use crate::error::AppError;
use crate::provider::{PermissionDecision, PermissionOutcome, PermissionPolicy};
use crate::workspace_paths::validate_workspace_directory_text;

pub(crate) use auth::AuthFailures;

/// Request bodies are prompt text and small JSON objects.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// The key a request authenticated with, and the history recipient seed
/// its secret derives; see [`crate::conversation::server_decrypt`].
#[derive(Clone)]
pub(crate) struct ApiKeyContext {
    pub record: ApiKeyRecord,
    pub seed: Arc<Zeroizing<[u8; 32]>>,
}

impl ApiKeyContext {
    pub(crate) fn owner_id(&self) -> String {
        self.record.owner_id()
    }

    fn ensure_agent(&self, agent: ProviderKind) -> Result<(), AppError> {
        if self.record.scopes.allows_agent(agent) {
            Ok(())
        } else {
            Err(AppError::Unauthorized(format!(
                "agent {} is outside this API key's scope",
                agent.as_str()
            )))
        }
    }
}

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

async fn me(Extension(key): Extension<ApiKeyContext>) -> Json<Value> {
    Json(key.record.summary())
}

async fn agents(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
) -> Json<Value> {
    let agents = state
        .conversations
        .providers_snapshot()
        .await
        .into_iter()
        .filter(|snapshot| key.record.scopes.allows_agent(snapshot.id))
        .collect::<Vec<_>>();
    Json(json!({ "agents": agents }))
}

#[derive(Deserialize)]
struct WorkspaceQuery {
    workspace: String,
}

async fn agent_models(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(agent): AxumPath<String>,
    Query(query): Query<WorkspaceQuery>,
) -> Result<Json<Value>, AppError> {
    let agent = parse_agent(&agent)?;
    key.ensure_agent(agent)?;
    let workspace = authorize_workspace(&state, &key, &query.workspace).await?;
    let models = state
        .conversations
        .models_live(&key.owner_id(), agent, &workspace)
        .await?;
    Ok(Json(json!({ "agent": agent, "models": models })))
}

/// The configured roots narrowed by the key's workspace scope.
async fn workspaces(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
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

/// The canonical `workspace` if this key may run agents in it. The path
/// must be inside a workspace root and the key's scope, and trusted: a
/// workspace listed in the key's scope is trusted by issuing the key; any
/// other must already be trusted on a paired device. The key's own trust
/// record is written on first use.
async fn authorize_workspace(
    state: &AppState,
    key: &ApiKeyContext,
    workspace: &str,
) -> Result<std::path::PathBuf, AppError> {
    let workspace = validate_workspace_directory_text(&state.config.workspace_roots, workspace)?;
    if !key.record.scopes.allows_workspace(&workspace) {
        return Err(AppError::Unauthorized(
            "workspace is outside this API key's scope".to_owned(),
        ));
    }
    let owner = key.owner_id();
    let trust = &state.workspace_trust;
    if !trust.status_owned(&owner, &workspace).await?.trusted {
        let granted = key.record.scopes.lists_workspace(&workspace)
            || trust.status_owned(DEVICE_OWNER, &workspace).await?.trusted;
        if !granted {
            return Err(AppError::WorkspaceTrustRequired(
                workspace.display().to_string(),
            ));
        }
        trust.set_owned(&owner, &workspace, true).await?;
    }
    Ok(workspace)
}

/// Paired devices share this owner (`device_auth::verified_context`).
const DEVICE_OWNER: &str = "local";

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

/// Cancels the running turns of every key that is revoked or expired: a
/// key that stops being active must not keep an agent working for it.
pub(crate) async fn cancel_inactive_key_turns(state: &AppState) {
    let Ok(keys) = state.api_keys.list() else {
        return;
    };
    let now = crate::api_keys::unix_ms();
    for key in keys.iter().filter(|key| !key.is_active(now)) {
        let owner = key.owner_id();
        let Ok(manifests) = state.conversations.list_owned(&owner).await else {
            continue;
        };
        for manifest in manifests.iter().filter(|manifest| {
            matches!(
                manifest.status,
                crate::conversation::ConversationStatus::Running
                    | crate::conversation::ConversationStatus::WaitingPermission
            )
        }) {
            if let Err(error) = state
                .conversations
                .cancel_owned(&owner, &manifest.id, None)
                .await
            {
                tracing::warn!(
                    conversation_id = %manifest.id,
                    error = %error,
                    "could not cancel the turn of an inactive API key"
                );
            }
        }
    }
}

/// Every few seconds: revocations made by the CLI or TUI reach the daemon
/// only through `api-keys.json`.
pub(crate) fn spawn_revocation_watch(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            ticker.tick().await;
            cancel_inactive_key_turns(&state).await;
        }
    })
}

/// Answers permission requests of API key conversations whose key is set
/// to `auto-approve` or `reject`.
pub(crate) struct ApiKeyPermissionPolicy {
    pub state_conversations: crate::conversation::ConversationStore,
    pub keys: crate::api_keys::ApiKeyStore,
}

#[async_trait::async_trait]
impl PermissionPolicy for ApiKeyPermissionPolicy {
    async fn decide(
        &self,
        conversation_id: &str,
        options: &Value,
        device_bound: bool,
    ) -> Option<(PermissionDecision, String)> {
        let manifest = self.state_conversations.get(conversation_id).await.ok()?;
        let id = manifest
            .owner_id
            .strip_prefix(crate::api_keys::OWNER_PREFIX)?;
        let record = self.keys.get(id).ok().flatten()?;
        let wanted: &[PermissionOutcome] = match (record.approval, device_bound) {
            (ApprovalPolicy::Ask, _) => return None,
            // A device-bound request is never approved on a key's behalf.
            (ApprovalPolicy::AutoApprove, false) => &[PermissionOutcome::AllowOnce],
            (ApprovalPolicy::AutoApprove, true) | (ApprovalPolicy::Reject, _) => &[
                PermissionOutcome::RejectOnce,
                PermissionOutcome::RejectAlways,
                PermissionOutcome::AbortTurn,
            ],
        };
        let options = options.as_array()?;
        let (outcome, option_id) = wanted.iter().find_map(|outcome| {
            options
                .iter()
                .find(|option| option.get("kind").and_then(Value::as_str) == Some(outcome.as_str()))
                .map(|option| {
                    (
                        *outcome,
                        option
                            .get("optionId")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    )
                })
        })?;
        Some((
            PermissionDecision {
                outcome,
                option_id,
                data: None,
            },
            format!("apikey-policy:{id}"),
        ))
    }
}

#[cfg(test)]
mod tests;
