//! Conversations, turns, events and one-shot runs of the API.

use std::time::Duration;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::stream::{assistant_text, ends_turn, sse, EventPump};
use super::{authorize_workspace, parse_agent, present_manifest, ApiKeyContext};
use crate::app_state::AppState;
use crate::conversation::server_decrypt::HistoryReader;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::provider::{CancelOutcome, ConversationPrompt, PermissionDecision, PromptSkillRef};

const DEFAULT_EVENT_PAGE: usize = 200;
const MAX_EVENT_PAGE: usize = 1000;
const DEFAULT_RUN_TIMEOUT_SECS: u64 = 600;
const MAX_RUN_TIMEOUT_SECS: u64 = 3600;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route(
            "/api/v1/conversations/{id}",
            get(get_conversation).delete(delete_conversation),
        )
        .route("/api/v1/conversations/{id}/turns", post(create_turn))
        .route("/api/v1/conversations/{id}/events", get(events))
        .route(
            "/api/v1/conversations/{id}/events/stream",
            get(events_stream),
        )
        .route("/api/v1/conversations/{id}/cancel", post(cancel))
        .route(
            "/api/v1/conversations/{id}/permissions/{permission_id}",
            post(resolve_permission),
        )
        .route("/api/v1/runs", post(run))
}

fn reader(state: &AppState, key: &ApiKeyContext, conversation_id: &str) -> HistoryReader {
    HistoryReader::new(
        state.history_keys.clone(),
        conversation_id,
        key.seed.clone(),
    )
}

async fn list_conversations(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
) -> Result<Json<Value>, AppError> {
    let manifests = state.conversations.list_owned(&key.owner_id()).await?;
    let mut conversations = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        let mut reader = reader(&state, &key, &manifest.id);
        conversations.push(present_manifest(&mut reader, manifest).await);
    }
    Ok(Json(json!({ "conversations": conversations })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateConversationRequest {
    agent: String,
    workspace: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

async fn create(
    state: &AppState,
    key: &ApiKeyContext,
    agent: ProviderKind,
    workspace: &str,
    title: Option<String>,
    profile: Option<String>,
) -> Result<crate::conversation::ConversationManifest, AppError> {
    key.ensure_agent(agent)?;
    let workspace = authorize_workspace(state, key, workspace).await?;
    state
        .conversations
        .create_owned(&key.owner_id(), agent, workspace, title, profile)
        .await
}

async fn create_conversation(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    Json(request): Json<CreateConversationRequest>,
) -> Result<Response, AppError> {
    let agent = parse_agent(&request.agent)?;
    let manifest = create(
        &state,
        &key,
        agent,
        &request.workspace,
        request.title,
        request.profile,
    )
    .await?;
    let mut reader = reader(&state, &key, &manifest.id);
    Ok((
        StatusCode::CREATED,
        Json(present_manifest(&mut reader, manifest).await),
    )
        .into_response())
}

async fn get_conversation(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let manifest = state
        .conversations
        .get_owned(&key.owner_id(), &conversation_id)
        .await?;
    let mut reader = reader(&state, &key, &conversation_id);
    Ok(Json(present_manifest(&mut reader, manifest).await))
}

async fn delete_conversation(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(conversation_id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let manifest = state
        .conversations
        .delete_owned(&key.owner_id(), &conversation_id)
        .await?;
    Ok(Json(
        json!({ "conversationId": manifest.id, "deleted": true }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillRequest {
    resource_id: String,
    #[serde(default)]
    name: Option<String>,
}

/// What a turn may set. Permission, sandbox and approval settings are not
/// accepted: the key's approval policy governs them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TurnRequest {
    text: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    skills: Vec<SkillRequest>,
    #[serde(default)]
    client_request_id: Option<String>,
}

impl TurnRequest {
    fn prompt(self) -> Result<ConversationPrompt, AppError> {
        if self.text.trim().is_empty() {
            return Err(AppError::InvalidRequest("text is required".to_owned()));
        }
        Ok(ConversationPrompt {
            permission_mode: None,
            work_mode: None,
            client_request_id: self.client_request_id,
            text: self.text,
            model: self.model,
            reasoning_effort: self.reasoning_effort,
            skills: self
                .skills
                .into_iter()
                .map(|skill| PromptSkillRef {
                    resource_id: skill.resource_id,
                    name: skill.name,
                })
                .collect(),
            content: Vec::new(),
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        })
    }
}

fn wants_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"))
}

/// Starts a turn. With `Accept: text/event-stream` the response streams
/// the turn's events until it ends; otherwise it is `202` with the ids.
async fn create_turn(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(conversation_id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<TurnRequest>,
) -> Result<Response, AppError> {
    let prompt = request.prompt()?;
    let owner = key.owner_id();
    let manifest = state
        .conversations
        .get_owned(&owner, &conversation_id)
        .await?;
    key.ensure_agent(manifest.provider)?;
    if !wants_event_stream(&headers) {
        let turn_id = state
            .conversations
            .prompt_owned(&owner, &conversation_id, prompt)
            .await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "conversationId": conversation_id, "turnId": turn_id })),
        )
            .into_response());
    }
    // Subscribed before the prompt, from the last event before it, so the
    // stream starts with the turn's first event.
    let pump = EventPump::new(&state, &key, &conversation_id, manifest.last_sequence).await?;
    let turn_id = state
        .conversations
        .prompt_owned(&owner, &conversation_id, prompt)
        .await?;
    Ok(turn_stream_response(pump, &conversation_id, turn_id))
}

fn turn_stream_response(pump: EventPump, conversation_id: &str, turn_id: String) -> Response {
    let mut response = sse(pump, Some(Some(turn_id.clone()))).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = conversation_id.parse() {
        headers.insert("x-todex-conversation-id", value);
    }
    if let Ok(value) = turn_id.parse() {
        headers.insert("x-todex-turn-id", value);
    }
    response
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventsQuery {
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn events(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EVENT_PAGE)
        .clamp(1, MAX_EVENT_PAGE);
    let replay = state
        .conversations
        .replay_owned(
            &key.owner_id(),
            &conversation_id,
            query.after.unwrap_or(0),
            limit,
            crate::conversation::ReplayDetail::Full,
        )
        .await?;
    let mut reader = reader(&state, &key, &conversation_id);
    let mut events = Vec::with_capacity(replay.events.len());
    for event in replay.events {
        events.push(reader.present(event, &replay.frames).await);
    }
    Ok(Json(json!({
        "conversationId": conversation_id,
        "events": events,
        "nextSequence": replay.next_sequence,
        "hasMore": replay.has_more,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StreamQuery {
    #[serde(default)]
    after: Option<u64>,
}

/// SSE of every event after `after` (or `Last-Event-ID`), then live ones.
async fn events_stream(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(conversation_id): AxumPath<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let after = last_event_id.or(query.after).unwrap_or(0);
    let pump = EventPump::new(&state, &key, &conversation_id, after).await?;
    Ok(sse(pump, None).into_response())
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelRequest {
    #[serde(default)]
    turn_id: Option<String>,
}

async fn cancel(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath(conversation_id): AxumPath<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, AppError> {
    let request: CancelRequest = if body.trim_ascii().is_empty() {
        CancelRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|error| AppError::InvalidRequest(format!("invalid cancel request: {error}")))?
    };
    let outcome = state
        .conversations
        .cancel_owned(
            &key.owner_id(),
            &conversation_id,
            request.turn_id.as_deref(),
        )
        .await?;
    Ok(Json(match outcome {
        CancelOutcome::Signalled => json!({
            "conversationId": conversation_id,
            "cancelled": true,
            "turnId": request.turn_id,
        }),
        CancelOutcome::NotActive { active_turn_id } => json!({
            "conversationId": conversation_id,
            "cancelled": false,
            "turnId": request.turn_id,
            "activeTurnId": active_turn_id,
        }),
    }))
}

async fn resolve_permission(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    AxumPath((conversation_id, permission_id)): AxumPath<(String, String)>,
    Json(decision): Json<PermissionDecision>,
) -> Result<Json<Value>, AppError> {
    let owner = key.owner_id();
    state
        .conversations
        .resolve_permission_owned(&owner, &owner, &conversation_id, &permission_id, decision)
        .await?;
    Ok(Json(json!({
        "conversationId": conversation_id,
        "permissionId": permission_id,
        "accepted": true,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunRequest {
    agent: String,
    workspace: String,
    text: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// One call: a new conversation, one turn, and either its event stream or,
/// once the turn ends, its status and assistant text.
async fn run(
    State(state): State<AppState>,
    Extension(key): Extension<ApiKeyContext>,
    Json(request): Json<RunRequest>,
) -> Result<Response, AppError> {
    let agent = parse_agent(&request.agent)?;
    let timeout = Duration::from_secs(
        request
            .timeout_secs
            .unwrap_or(DEFAULT_RUN_TIMEOUT_SECS)
            .clamp(1, MAX_RUN_TIMEOUT_SECS),
    );
    let prompt = TurnRequest {
        text: request.text,
        model: request.model,
        reasoning_effort: request.reasoning_effort,
        skills: Vec::new(),
        client_request_id: None,
    }
    .prompt()?;
    let manifest = create(
        &state,
        &key,
        agent,
        &request.workspace,
        request.title,
        request.profile,
    )
    .await?;
    let conversation_id = manifest.id.clone();
    let mut pump = EventPump::new(&state, &key, &conversation_id, manifest.last_sequence).await?;
    let owner = key.owner_id();
    let turn_id = state
        .conversations
        .prompt_owned(&owner, &conversation_id, prompt)
        .await?;
    if request.stream {
        return Ok(turn_stream_response(pump, &conversation_id, turn_id));
    }

    let mut output = String::new();
    let collected = tokio::time::timeout(timeout, async {
        loop {
            for event in pump.next_batch().await? {
                let ours =
                    event.payload.get("turnId").and_then(Value::as_str) == Some(turn_id.as_str());
                if ours {
                    if let Some(text) = assistant_text(&event) {
                        output.push_str(text);
                    }
                }
                if ends_turn(&event, Some(&turn_id)) {
                    return Ok::<_, AppError>(event);
                }
            }
        }
    })
    .await;
    let terminal = match collected {
        Ok(result) => result?,
        Err(_) => {
            let _ = state
                .conversations
                .cancel_owned(&owner, &conversation_id, Some(&turn_id))
                .await;
            return Ok((
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "code": "RUN_TIMEOUT",
                    "message": format!("the turn did not finish within {} s and was cancelled", timeout.as_secs()),
                    "conversationId": conversation_id,
                    "turnId": turn_id,
                    "output": output,
                })),
            )
                .into_response());
        }
    };
    let status = terminal
        .event_type
        .strip_prefix("turn.")
        .or_else(|| terminal.event_type.strip_prefix("conversation."))
        .unwrap_or(&terminal.event_type)
        .to_owned();
    let error = terminal
        .payload
        .get("error")
        .or_else(|| terminal.payload.get("message"))
        .filter(|_| status != "completed")
        .cloned();
    Ok(Json(json!({
        "conversationId": conversation_id,
        "turnId": turn_id,
        "status": status,
        "output": output,
        "error": error,
    }))
    .into_response())
}
