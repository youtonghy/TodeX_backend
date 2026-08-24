use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Uri};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tracing::warn;

use crate::app_state::AppState;
use crate::conversation::{ConversationManifest, ProviderKind};
use crate::error::AppError;
use crate::provider::PermissionDecision;
use crate::transport_crypto::TransportCryptoSession;
use crate::workspace_paths::validate_workspace_directory_text;

use super::websocket::{self, AuthContext};

const MAX_WS_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_WS_SUBSCRIPTIONS: usize = 128;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v2/providers", get(providers))
        .route("/v2/catalog/skills", get(skills))
        .route("/v2/catalog/skills/{resource_id}", get(skill_resource))
        .route("/v2/catalog/mcp", get(mcp))
        .route(
            "/v2/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route("/v2/conversations/{conversation_id}", get(get_conversation))
        .route(
            "/v2/conversations/{conversation_id}/events",
            get(replay_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/prompt",
            post(prompt_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/cancel",
            post(cancel_conversation),
        )
        .route(
            "/v2/conversations/{conversation_id}/permissions/{permission_id}",
            post(resolve_permission),
        )
        .route("/v2/ws", get(ws))
}

async fn skills(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<crate::catalog::SkillCatalog>, AppError> {
    require_auth(&state, &headers)?;
    let workspace = validate_catalog_workspace(&state, &query.workspace)?;
    Ok(Json(state.catalog.skills(query.provider, workspace).await?))
}

async fn skill_resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(resource_id): Path<String>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<crate::catalog::SkillResource>, AppError> {
    require_auth(&state, &headers)?;
    let workspace = validate_catalog_workspace(&state, &query.workspace)?;
    Ok(Json(
        state
            .catalog
            .skill_resource(query.provider, workspace, &resource_id)
            .await?,
    ))
}

async fn mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<crate::catalog::McpCatalog>, AppError> {
    require_auth(&state, &headers)?;
    let workspace = validate_catalog_workspace(&state, &query.workspace)?;
    Ok(Json(state.catalog.mcp(query.provider, workspace).await?))
}

fn validate_catalog_workspace(state: &AppState, workspace: &str) -> Result<PathBuf, AppError> {
    validate_workspace_directory_text(&state.catalog.config().workspace_root, workspace)
}

async fn providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_auth(&state, &headers)?;
    Ok(Json(
        json!({ "providers": state.conversations.providers() }),
    ))
}

async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    Ok(Json(
        json!({ "conversations": state.conversations.list_owned(&auth.tenant_id).await? }),
    ))
}

async fn create_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateConversationRequest>,
) -> Result<Json<ConversationManifest>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let provider = match request.provider {
        Some(provider) => provider,
        None => state
            .config
            .agent
            .default_agent
            .parse()
            .map_err(AppError::InvalidRequest)?,
    };
    let manifest = state
        .conversations
        .create_owned(
            &auth.tenant_id,
            provider,
            request.workspace,
            request.title,
            request.provider_profile,
        )
        .await?;
    Ok(Json(manifest))
}

async fn get_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<Json<ConversationManifest>, AppError> {
    let auth = require_auth(&state, &headers)?;
    Ok(Json(
        state
            .conversations
            .get_owned(&auth.tenant_id, &conversation_id)
            .await?,
    ))
}

async fn replay_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    Query(query): Query<ReplayQuery>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    Ok(Json(serde_json::to_value(
        state
            .conversations
            .replay_owned(
                &auth.tenant_id,
                &conversation_id,
                query.after_sequence.unwrap_or(0),
                query.limit.unwrap_or(200),
            )
            .await?,
    )?))
}

async fn prompt_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    let turn_id = state
        .conversations
        .prompt_owned(
            &auth.tenant_id,
            &conversation_id,
            request.text,
            request.model,
        )
        .await?;
    Ok(Json(
        json!({ "conversationId": conversation_id, "turnId": turn_id }),
    ))
}

async fn cancel_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(conversation_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .cancel_owned(&auth.tenant_id, &conversation_id)
        .await?;
    Ok(Json(
        json!({ "conversationId": conversation_id, "accepted": true }),
    ))
}

async fn resolve_permission(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((conversation_id, permission_id)): Path<(String, String)>,
    Json(decision): Json<PermissionDecision>,
) -> Result<Json<Value>, AppError> {
    let auth = require_auth(&state, &headers)?;
    state
        .conversations
        .resolve_permission_owned(&auth.tenant_id, &conversation_id, &permission_id, decision)
        .await?;
    Ok(Json(json!({
        "conversationId": conversation_id,
        "permissionId": permission_id,
        "accepted": true,
    })))
}

async fn ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    let auth = require_auth(&state, &headers)?;
    let crypto = websocket::transport_crypto_from_handshake(&state, &headers, uri.query())?;
    Ok(ws.on_upgrade(move |socket| handle_socket(state, socket, crypto, auth)))
}

async fn handle_socket(
    state: AppState,
    socket: WebSocket,
    crypto: Option<TransportCryptoSession>,
    auth: AuthContext,
) {
    let (mut sender, mut receiver) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Value>(256);
    let sender_crypto = crypto.clone();
    let send_task = tokio::spawn(async move {
        while let Some(value) = outgoing_rx.recv().await {
            let text = match serde_json::to_string(&value) {
                Ok(text) => text,
                Err(error) => {
                    warn!(error = %error, "failed to serialize v2 websocket event");
                    continue;
                }
            };
            let text = match &sender_crypto {
                Some(crypto) => match crypto.encrypt_server_text(&text) {
                    Ok(text) => text,
                    Err(error) => {
                        warn!(error = %error, "failed to encrypt v2 websocket event");
                        break;
                    }
                },
                None => text,
            };
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let subscriptions = Arc::new(Mutex::new(HashSet::<String>::new()));
    let mut subscription_tasks = Vec::new();
    while let Some(frame) = receiver.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                warn!(error = %error, "v2 websocket receive failed");
                break;
            }
        };
        let Message::Text(text) = frame else {
            if matches!(frame, Message::Close(_)) {
                break;
            }
            continue;
        };
        if text.len() > MAX_WS_MESSAGE_BYTES {
            let _ = outgoing_tx
                .send(error_response(
                    None,
                    AppError::InvalidRequest("websocket message is too large".to_owned()),
                ))
                .await;
            continue;
        }
        let text = match &crypto {
            Some(crypto) => match crypto.decrypt_client_text(&text) {
                Ok(text) => text,
                Err(error) => {
                    let _ = outgoing_tx.send(error_response(None, error)).await;
                    continue;
                }
            },
            None => text.to_string(),
        };
        let command: V2Command = match serde_json::from_str(&text) {
            Ok(command) => command,
            Err(error) => {
                let _ = outgoing_tx
                    .send(error_response(
                        None,
                        AppError::InvalidRequest(format!("invalid v2 websocket command: {error}")),
                    ))
                    .await;
                continue;
            }
        };
        let response = dispatch_command(
            &state,
            &outgoing_tx,
            &subscriptions,
            &mut subscription_tasks,
            &auth.tenant_id,
            command,
        )
        .await;
        if let Some(response) = response {
            let _ = outgoing_tx.send(response).await;
        }
    }

    for task in subscription_tasks {
        task.abort();
    }
    drop(outgoing_tx);
    let _ = send_task.await;
}

async fn dispatch_command(
    state: &AppState,
    outgoing: &mpsc::Sender<Value>,
    subscriptions: &Arc<Mutex<HashSet<String>>>,
    subscription_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    owner_id: &str,
    command: V2Command,
) -> Option<Value> {
    let result = dispatch_command_inner(
        state,
        outgoing,
        subscriptions,
        subscription_tasks,
        owner_id,
        &command,
    )
    .await;
    Some(match result {
        Ok(payload) => json!({
            "id": command.id,
            "type": "server.result",
            "payload": payload,
        }),
        Err(error) => error_response(Some(command.id), error),
    })
}

async fn dispatch_command_inner(
    state: &AppState,
    outgoing: &mpsc::Sender<Value>,
    subscriptions: &Arc<Mutex<HashSet<String>>>,
    subscription_tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    owner_id: &str,
    command: &V2Command,
) -> Result<Value, AppError> {
    match command.command_type.as_str() {
        "conversation.subscribe" => {
            let request: SubscribeRequest = serde_json::from_value(command.payload.clone())?;
            state
                .conversations
                .get_owned(owner_id, &request.conversation_id)
                .await?;
            let subscriptions_guard = subscriptions.lock().await;
            if subscriptions_guard.contains(&request.conversation_id) {
                return Ok(json!({
                    "conversationId": request.conversation_id,
                    "subscribed": true,
                    "alreadySubscribed": true,
                }));
            }
            if subscriptions_guard.len() >= MAX_WS_SUBSCRIPTIONS {
                return Err(AppError::InvalidRequest(
                    "v2 websocket subscription limit reached".to_owned(),
                ));
            }
            drop(subscriptions_guard);

            let mut receiver = state.conversations.subscribe(&request.conversation_id);
            let high_water = state
                .conversations
                .get_owned(owner_id, &request.conversation_id)
                .await?
                .last_sequence;
            let mut replay_cursor = request.after_sequence.unwrap_or(0).min(high_water);
            let page_size = request.limit.unwrap_or(500);
            while replay_cursor < high_water {
                let replay = state
                    .conversations
                    .replay_owned(owner_id, &request.conversation_id, replay_cursor, page_size)
                    .await?;
                let mut advanced = false;
                for event in replay
                    .events
                    .into_iter()
                    .take_while(|event| event.sequence <= high_water)
                {
                    replay_cursor = event.sequence;
                    advanced = true;
                    outgoing
                        .send(json!({ "type": "conversation.event", "payload": event }))
                        .await
                        .map_err(|_| AppError::StreamClosed)?;
                }
                if !advanced {
                    return Err(AppError::Conflict(format!(
                        "conversation {} replay did not reach sequence {high_water}",
                        request.conversation_id
                    )));
                }
            }
            subscriptions
                .lock()
                .await
                .insert(request.conversation_id.clone());
            let skip_through = high_water;
            let outgoing = outgoing.clone();
            let conversation_id = request.conversation_id.clone();
            subscription_tasks.push(tokio::spawn(async move {
                loop {
                    match receiver.recv().await {
                        Ok(event) if event.sequence > skip_through => {
                            if outgoing
                                .send(json!({ "type": "conversation.event", "payload": event }))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            let _ = outgoing
                                .send(json!({
                                    "type": "server.error",
                                    "payload": {
                                        "code": "EVENT_STREAM_LAGGED",
                                        "message": format!("conversation {conversation_id} stream lagged by {skipped} events"),
                                    }
                                }))
                                .await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));
            Ok(json!({
                "conversationId": request.conversation_id,
                "subscribed": true,
                "nextSequence": high_water,
                "hasMore": false,
            }))
        }
        "conversation.create" => {
            let request: CreateConversationRequest =
                serde_json::from_value(command.payload.clone())?;
            let provider = match request.provider {
                Some(provider) => provider,
                None => state
                    .config
                    .agent
                    .default_agent
                    .parse()
                    .map_err(AppError::InvalidRequest)?,
            };
            Ok(serde_json::to_value(
                state
                    .conversations
                    .create_owned(
                        owner_id,
                        provider,
                        request.workspace,
                        request.title,
                        request.provider_profile,
                    )
                    .await?,
            )?)
        }
        "conversation.prompt" => {
            let request: WsConversationRequest = serde_json::from_value(command.payload.clone())?;
            let text = request.text.ok_or_else(|| {
                AppError::InvalidRequest("conversation.prompt requires text".to_owned())
            })?;
            let turn_id = state
                .conversations
                .prompt_owned(owner_id, &request.conversation_id, text, request.model)
                .await?;
            Ok(json!({ "conversationId": request.conversation_id, "turnId": turn_id }))
        }
        "conversation.cancel" | "conversation.stop" => {
            let request: WsConversationRequest = serde_json::from_value(command.payload.clone())?;
            state
                .conversations
                .cancel_owned(owner_id, &request.conversation_id)
                .await?;
            Ok(json!({ "conversationId": request.conversation_id, "accepted": true }))
        }
        "conversation.permission.respond" => {
            let request: WsPermissionRequest = serde_json::from_value(command.payload.clone())?;
            state
                .conversations
                .resolve_permission_owned(
                    owner_id,
                    &request.conversation_id,
                    &request.permission_id,
                    request.decision,
                )
                .await?;
            Ok(json!({
                "conversationId": request.conversation_id,
                "permissionId": request.permission_id,
                "accepted": true,
            }))
        }
        "server.ping" => Ok(json!({ "pong": true })),
        other => Err(AppError::Unsupported(format!(
            "v2 websocket command {other}"
        ))),
    }
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<AuthContext, AppError> {
    if state.config.security.auth_token.is_none() {
        return Ok(AuthContext {
            principal_id: "local".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "none".to_owned(),
        });
    }
    websocket::authenticate_headers(state, headers).ok_or(AppError::Unauthenticated)
}

fn error_response(id: Option<String>, error: AppError) -> Value {
    json!({
        "id": id,
        "type": "server.error",
        "payload": { "code": error.code(), "message": error.to_string() },
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateConversationRequest {
    #[serde(default)]
    provider: Option<ProviderKind>,
    workspace: PathBuf,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    provider_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayQuery {
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptRequest {
    text: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogQuery {
    provider: ProviderKind,
    workspace: String,
}

#[derive(Debug, Deserialize)]
struct V2Command {
    id: String,
    #[serde(rename = "type")]
    command_type: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeRequest {
    conversation_id: String,
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsConversationRequest {
    conversation_id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WsPermissionRequest {
    conversation_id: String,
    permission_id: String,
    decision: PermissionDecision,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::Duration;

    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use crate::conversation::{ConversationEventHub, ConversationStore};
    use crate::provider::ConversationSupervisor;

    #[tokio::test]
    async fn v2_http_requires_auth_and_persists_an_owned_conversation() {
        let root = std::env::temp_dir().join(format!("todex-v2-http-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable,
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("v2-token".to_owned()),
            },
        })
        .await
        .unwrap();
        let app = crate::server::router(state);

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v2/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let create = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/conversations")
                    .header("authorization", "Bearer v2-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "provider": "codex",
                            "workspace": workspace,
                            "title": "HTTP fixture",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created: ConversationManifest =
            serde_json::from_slice(&to_bytes(create.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(created.owner_id, "local");

        let replay = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v2/conversations/{}/events", created.id))
                    .header("authorization", "Bearer v2-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn v2_subscription_replays_through_high_water_and_clamps_future_cursors() {
        let root = std::env::temp_dir().join(format!("todex-v2-replay-{}", Uuid::new_v4()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let mut state = AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable,
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("v2-token".to_owned()),
            },
        })
        .await
        .unwrap();
        let store = ConversationStore::new(state.config.data_dir.clone())
            .await
            .unwrap();
        let hub = ConversationEventHub::default();
        state.conversations =
            ConversationSupervisor::new(state.config.clone(), store.clone(), hub.clone());
        let manifest = state
            .conversations
            .create_owned(
                "local",
                ProviderKind::Codex,
                workspace,
                Some("Replay fixture".to_owned()),
                None,
            )
            .await
            .unwrap();
        for index in 1..=3 {
            store
                .append(&manifest.id, "fixture.event", json!({ "index": index }))
                .await
                .unwrap();
        }

        let (outgoing, mut events) = mpsc::channel(16);
        let subscriptions = Arc::new(Mutex::new(HashSet::new()));
        let mut subscription_tasks = Vec::new();
        let result = dispatch_command_inner(
            &state,
            &outgoing,
            &subscriptions,
            &mut subscription_tasks,
            "local",
            &V2Command {
                id: "subscribe-1".to_owned(),
                command_type: "conversation.subscribe".to_owned(),
                payload: json!({
                    "conversationId": manifest.id,
                    "afterSequence": 0,
                    "limit": 1,
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["nextSequence"], 4);
        assert_eq!(result["hasMore"], false);

        let mut sequences = Vec::new();
        for _ in 0..4 {
            let event = events.recv().await.expect("replayed conversation event");
            sequences.push(event["payload"]["sequence"].as_u64().unwrap());
        }
        assert_eq!(sequences, vec![1, 2, 3, 4]);
        for task in subscription_tasks {
            task.abort();
        }

        let (future_outgoing, mut future_events) = mpsc::channel(16);
        let future_subscriptions = Arc::new(Mutex::new(HashSet::new()));
        let mut future_tasks = Vec::new();
        let result = dispatch_command_inner(
            &state,
            &future_outgoing,
            &future_subscriptions,
            &mut future_tasks,
            "local",
            &V2Command {
                id: "subscribe-future".to_owned(),
                command_type: "conversation.subscribe".to_owned(),
                payload: json!({
                    "conversationId": manifest.id,
                    "afterSequence": 10_000,
                }),
            },
        )
        .await
        .unwrap();
        assert_eq!(result["nextSequence"], 4);
        assert!(matches!(
            future_events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let live = store
            .append(&manifest.id, "fixture.live", json!({}))
            .await
            .unwrap();
        hub.publish(live);
        let received = tokio::time::timeout(Duration::from_secs(1), future_events.recv())
            .await
            .expect("future-cursor subscription should receive a live event")
            .expect("future-cursor subscription channel should remain open");
        assert_eq!(received["payload"]["sequence"], 5);
        for task in future_tasks {
            task.abort();
        }
        let _ = fs::remove_dir_all(root);
    }
}
