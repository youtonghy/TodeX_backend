use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::{
    app_state::AppState,
    codex_gateway::{
        CodexGatewayEvent, CodexGatewayEventRecord, CodexLocalAdapterProcessError,
        CodexLocalAdapterRuntime, CodexLocalAdapterStartOptions,
    },
    error::AppError,
    event::EventRecord,
    server::protocol::{
        ClientMessage, ClientMessageKind, CodexCloudTaskApplyRequest, CodexCloudTaskCreateRequest,
        CodexCloudTaskIdRequest, CodexCloudTaskListRequest, CodexCloudTaskSiblingAttemptsRequest,
        CodexGatewayAction, CodexLifecycleRequest, CodexLocalErrorCode, ServerEvent,
    },
    transport_crypto::TransportCryptoSession,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    pub principal_id: String,
    pub tenant_id: String,
    pub token_id: String,
}

pub fn authenticate_headers(state: &AppState, headers: &HeaderMap) -> Option<AuthContext> {
    // CodeX Gateway remote control is never implicitly trusted. The legacy
    // `enable_auth` config flag is kept for broader transport compatibility,
    // but CodeX control requires an explicit configured bearer token so that
    // disabling generic auth cannot accidentally expose remote agent control.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))?;

    let expected = state.config.security.auth_token.as_deref()?;
    if token != expected {
        return None;
    }

    Some(AuthContext {
        principal_id: "gateway-token".to_owned(),
        tenant_id: "local".to_owned(),
        token_id: "configured-token".to_owned(),
    })
}

pub fn transport_crypto_from_handshake(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<Option<TransportCryptoSession>, AppError> {
    TransportCryptoSession::from_headers_and_query(&state.pairing_keys, headers, query)
}

pub async fn handle_socket(
    state: AppState,
    socket: WebSocket,
    auth: Option<AuthContext>,
    crypto: Result<Option<TransportCryptoSession>, AppError>,
) {
    let crypto = match crypto {
        Ok(crypto) => crypto,
        Err(error) => {
            let mut socket = socket;
            let json =
                serde_json::to_string(&ServerEvent::from(error_event(error, None, None, None)))
                    .unwrap_or_else(|_| {
                        r#"{"type":"server.error","payload":{"code":"INVALID_REQUEST"}}"#.to_owned()
                    });
            let _ = socket.send(Message::Text(json.into())).await;
            let _ = socket.close().await;
            return;
        }
    };

    let active_connections = state.increment_websocket_connections();
    state
        .events
        .publish(EventRecord::new(
            "server.websocket.connected",
            None,
            None,
            None,
            json!({
                "active_connections": active_connections,
                "authenticated": auth.is_some(),
                "principal_id": auth.as_ref().map(|auth| auth.principal_id.as_str()),
                "encrypted": crypto.is_some(),
                "encryption_protocol": crypto.as_ref().map(|crypto| crypto.protocol().as_str()),
            }),
        ))
        .await;

    let (mut sender, mut receiver) = socket.split();
    let mut event_rx = state.events.subscribe();
    let send_crypto = crypto.clone();

    let send_task = tokio::spawn(async move {
        loop {
            let event = match event_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    if send_serialized_event(
                        &mut sender,
                        &send_crypto,
                        ServerEvent::from(error_event(
                            AppError::StreamLagged(skipped),
                            None,
                            None,
                            None,
                        )),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let _ = send_serialized_event(
                        &mut sender,
                        &send_crypto,
                        ServerEvent::from(error_event(AppError::StreamClosed, None, None, None)),
                    )
                    .await;
                    break;
                }
            };

            if send_serialized_event(&mut sender, &send_crypto, ServerEvent::from(event))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(frame) = receiver.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(err) => {
                warn!(error = %err, "websocket receive failed");
                break;
            }
        };

        match frame {
            Message::Text(text) => {
                let text = match &crypto {
                    Some(crypto) => match crypto.decrypt_client_text(&text) {
                        Ok(text) => text,
                        Err(error) => {
                            state
                                .events
                                .publish(error_event(error, None, None, None))
                                .await;
                            continue;
                        }
                    },
                    None => text.to_string(),
                };
                match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(message) => {
                        if let Err(error) = dispatch(message, &state, auth.as_ref()).await {
                            state
                                .events
                                .publish(error_event(error, None, None, None))
                                .await;
                        }
                    }
                    Err(err) => {
                        state
                            .events
                            .publish(error_event(
                                AppError::InvalidRequest(format!(
                                    "failed to parse client message: {err}"
                                )),
                                None,
                                None,
                                None,
                            ))
                            .await;
                    }
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Binary(_) => debug!("ignored binary websocket frame"),
        }
    }

    send_task.abort();

    let active_connections = state.decrement_websocket_connections();
    state
        .events
        .publish(EventRecord::new(
            "server.websocket.disconnected",
            None,
            None,
            None,
            json!({
                "active_connections": active_connections,
                "authenticated": auth.is_some(),
                "principal_id": auth.as_ref().map(|auth| auth.principal_id.as_str()),
                "encrypted": crypto.is_some(),
                "encryption_protocol": crypto.as_ref().map(|crypto| crypto.protocol().as_str()),
            }),
        ))
        .await;
}

async fn dispatch(
    message: ClientMessage,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    debug!(message_id = %message.id, "client message received");

    match message.kind {
        ClientMessageKind::CodexGatewayControl(payload) => {
            ensure_supported_codex_control_action(payload.action)?;
            authorize_codex_control(&message.id, &payload, state, auth).await?;
            state
                .events
                .publish(EventRecord::new(
                    "codex.gateway.control.accepted",
                    None,
                    None,
                    None,
                    json!({
                        "request_id": message.id,
                        "codex_session_id": payload.codex_session_id,
                        "tenant_id": payload.tenant_id,
                        "action": payload.action,
                    }),
                ))
                .await;
        }
        ClientMessageKind::CodexLocalStart(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.start",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .start(CodexLocalAdapterStartOptions::new(
                    payload.codex_session_id.clone(),
                    message.id.clone(),
                    payload.cwd,
                    state.config.agent.codex_bin.clone(),
                ))
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalStatus(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.status",
                auth,
                state,
            )
            .await?;
            let status =
                local_codex_status_payload(state, &message.id, &payload.codex_session_id).await?;
            let record = state
                .codex_gateway
                .append_event(
                    &payload.codex_session_id,
                    CodexGatewayEvent::new("codex.control.status", status),
                )
                .await?;
            publish_codex_gateway_record(state, record).await;
        }
        ClientMessageKind::CodexLocalStop(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.stop",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .stop(&payload.codex_session_id, &message.id, payload.force)
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalTurn(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.turn",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .run_turn(
                    &payload.codex_session_id,
                    &message.id,
                    &payload.thread_id,
                    payload.input,
                    payload.approval_policy,
                    payload.sandbox_policy,
                    payload.service_tier,
                    payload.collaboration_mode,
                )
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalInput(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.input",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .send_input(
                    &payload.codex_session_id,
                    &message.id,
                    &payload.thread_id,
                    &payload.turn_id,
                    payload.input,
                )
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalSteer(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.steer",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .steer(
                    &payload.codex_session_id,
                    &message.id,
                    &payload.thread_id,
                    &payload.turn_id,
                    payload.expected_turn_id,
                    payload.input,
                )
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalInterrupt(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.interrupt",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .interrupt(
                    &payload.codex_session_id,
                    &message.id,
                    &payload.thread_id,
                    payload.turn_id,
                )
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalApprovalRespond(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.approval.respond",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .respond_to_approval(
                    &payload.codex_session_id,
                    &message.id,
                    &payload.request_id,
                    &payload.response_type,
                    payload.response,
                )
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalRequest(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.request",
                auth,
                state,
            )
            .await?;
            let last_cursor = state
                .codex_gateway
                .recover_session(&payload.codex_session_id)
                .await?
                .last_cursor;
            if let Err(error) = state
                .codex_local_adapters
                .send_request(
                    &payload.codex_session_id,
                    &message.id,
                    &payload.method,
                    payload.params,
                )
                .await
            {
                publish_local_codex_error_after(
                    state,
                    &payload.codex_session_id,
                    last_cursor,
                    error,
                )
                .await?;
                return Ok(());
            }
            publish_codex_gateway_records_after(state, &payload.codex_session_id, last_cursor)
                .await?;
        }
        ClientMessageKind::CodexLocalReplay(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.replay",
                auth,
                state,
            )
            .await?;
            let replay = state
                .codex_gateway
                .replay_events(
                    &payload.codex_session_id,
                    payload.after_cursor,
                    payload.limit,
                )
                .await?;
            for record in replay.events {
                publish_codex_gateway_record(state, record).await;
            }
        }
        ClientMessageKind::CodexLocalAttach(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.attach",
                auth,
                state,
            )
            .await?;
            let replay = state
                .codex_gateway
                .replay_events(
                    &payload.codex_session_id,
                    payload.after_cursor,
                    payload.replay_limit,
                )
                .await?;
            for record in replay.events {
                publish_codex_gateway_record(state, record).await;
            }
        }
        ClientMessageKind::CodexLocalSnapshot(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                "codex.local.snapshot",
                auth,
                state,
            )
            .await?;
            let text = String::new();
            let truncated = truncate_snapshot_text(text, payload.max_bytes);
            let record = state
                .codex_gateway
                .append_event(
                    &payload.codex_session_id,
                    CodexGatewayEvent::new(
                        "codex.local.snapshot",
                        json!({
                            "requestId": message.id,
                            "operation": "codex.local.snapshot",
                            "codexSessionId": payload.codex_session_id,
                            "text": truncated,
                            "maxBytes": payload.max_bytes,
                            "authoritative": false,
                            "source": "codex.local.events",
                        }),
                    ),
                )
                .await?;
            publish_codex_gateway_record(state, record).await;
        }
        ClientMessageKind::CodexLocalUnsupported(payload) => {
            authorize_local_codex_request(
                &message.id,
                &payload.codex_session_id,
                &payload.tenant_id,
                &payload.operation,
                auth,
                state,
            )
            .await?;
            let operation = payload.operation;
            let codex_session_id = payload.codex_session_id;
            let reason = payload.reason.unwrap_or_else(|| {
                "operation is excluded from local Codex CLI control".to_string()
            });
            let record = state
                .codex_gateway
                .append_event(
                    &codex_session_id,
                    CodexGatewayEvent::new(
                        "codex.control.error",
                        json!({
                            "requestId": message.id,
                            "operation": operation,
                            "codexSessionId": codex_session_id,
                            "lifecycleState": "failed",
                            "error": {
                                "code": CodexLocalErrorCode::UnsupportedLocal,
                                "message": reason,
                                "codexSessionId": codex_session_id,
                                "requestId": message.id,
                                "operation": operation,
                            }
                        }),
                    ),
                )
                .await?;
            publish_codex_gateway_record(state, record).await;
        }
        ClientMessageKind::CodexThreadStart(payload) => {
            handle_codex_lifecycle(
                &message.id,
                payload,
                CodexGatewayAction::ThreadStart,
                "codex.thread.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexTurnStart(payload) => {
            handle_codex_lifecycle(
                &message.id,
                payload,
                CodexGatewayAction::TurnStart,
                "codex.turn.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexTurnSteer(payload) => {
            handle_codex_lifecycle(
                &message.id,
                payload,
                CodexGatewayAction::TurnSteer,
                "codex.turn.steerUpdated",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexTurnInterrupt(payload) => {
            handle_codex_lifecycle(
                &message.id,
                payload,
                CodexGatewayAction::TurnInterrupt,
                "codex.turn.interrupted",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexMcpServerListStatus(payload) => {
            handle_codex_mcp_request(
                &message.id,
                payload,
                CodexGatewayAction::McpServerListStatus,
                "codex.mcp.server.listStatus.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexMcpResourceRead(payload) => {
            handle_codex_mcp_request(
                &message.id,
                payload,
                CodexGatewayAction::McpResourceRead,
                "codex.mcp.resource.read.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexMcpToolCall(payload) => {
            handle_codex_mcp_request(
                &message.id,
                payload,
                CodexGatewayAction::McpToolCall,
                "codex.mcp.tool.call.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexMcpServerRefresh(payload) => {
            handle_codex_mcp_request(
                &message.id,
                payload,
                CodexGatewayAction::McpServerRefresh,
                "codex.mcp.server.refresh.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexMcpOauthLogin(payload) => {
            handle_codex_mcp_request(
                &message.id,
                payload,
                CodexGatewayAction::McpOauthLogin,
                "codex.mcp.oauth.login.started",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexMcpElicitationRespond(payload) => {
            handle_codex_mcp_request(
                &message.id,
                payload,
                CodexGatewayAction::McpElicitationRespond,
                "codex.mcp.elicitation.resolved",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexCloudTaskCreate(payload) => {
            handle_codex_cloud_task_create(&message.id, payload, state, auth).await?;
        }
        ClientMessageKind::CodexCloudTaskList(payload) => {
            handle_codex_cloud_task_list(&message.id, payload, state, auth).await?;
        }
        ClientMessageKind::CodexCloudTaskGetSummary(payload) => {
            handle_codex_cloud_task_id(
                &message.id,
                payload,
                CodexGatewayAction::CloudTaskGetSummary,
                "codex.cloudTask.getSummary.result",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexCloudTaskGetDiff(payload) => {
            handle_codex_cloud_task_id(
                &message.id,
                payload,
                CodexGatewayAction::CloudTaskGetDiff,
                "codex.cloudTask.getDiff.result",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexCloudTaskGetMessages(payload) => {
            handle_codex_cloud_task_id(
                &message.id,
                payload,
                CodexGatewayAction::CloudTaskGetMessages,
                "codex.cloudTask.getMessages.result",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexCloudTaskGetText(payload) => {
            handle_codex_cloud_task_id(
                &message.id,
                payload,
                CodexGatewayAction::CloudTaskGetText,
                "codex.cloudTask.getText.result",
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexCloudTaskListSiblingAttempts(payload) => {
            handle_codex_cloud_task_sibling_attempts(&message.id, payload, state, auth).await?;
        }
        ClientMessageKind::CodexCloudTaskApplyPreflight(payload) => {
            handle_codex_cloud_task_apply(
                &message.id,
                payload,
                CodexGatewayAction::CloudTaskApplyPreflight,
                "codex.cloudTask.applyPreflight.result",
                true,
                state,
                auth,
            )
            .await?;
        }
        ClientMessageKind::CodexCloudTaskApply(payload) => {
            handle_codex_cloud_task_apply(
                &message.id,
                payload,
                CodexGatewayAction::CloudTaskApply,
                "codex.cloudTask.apply.result",
                false,
                state,
                auth,
            )
            .await?;
        }
    }

    Ok(())
}

async fn send_serialized_event(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    crypto: &Option<TransportCryptoSession>,
    event: ServerEvent,
) -> Result<(), axum::Error> {
    let json = match serde_json::to_string(&event) {
        Ok(json) => json,
        Err(err) => {
            warn!(error = %err, "failed to serialize server event");
            return Ok(());
        }
    };
    let json = match crypto {
        Some(crypto) => match crypto.encrypt_server_text(&json) {
            Ok(json) => json,
            Err(err) => {
                warn!(error = %err, "failed to encrypt server event");
                return Ok(());
            }
        },
        None => json,
    };
    sender.send(Message::Text(json.into())).await
}

async fn handle_codex_cloud_task_create(
    request_id: &str,
    request: CodexCloudTaskCreateRequest,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        CodexGatewayAction::CloudTaskCreate,
        state,
        auth,
    )
    .await?;

    let _ = request;
    Err(AppError::Unsupported(
        "codex.cloudTask.create requires Codex cloud HTTP adapter invocation proof; local placeholder success is forbidden".to_owned(),
    ))
}

async fn handle_codex_mcp_request(
    request_id: &str,
    request: CodexLifecycleRequest,
    action: CodexGatewayAction,
    event_type: &str,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        action,
        state,
        auth,
    )
    .await?;

    let _ = event_type;
    Err(AppError::Unsupported(format!(
        "{} requires upstream Codex app-server invocation proof; local placeholder success is forbidden",
        codex_action_name(action)
    )))
}

async fn handle_codex_cloud_task_list(
    request_id: &str,
    request: CodexCloudTaskListRequest,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        CodexGatewayAction::CloudTaskList,
        state,
        auth,
    )
    .await?;

    let _ = request;
    Err(AppError::Unsupported(
        "codex.cloudTask.list requires Codex cloud HTTP adapter invocation proof; local placeholder success is forbidden".to_owned(),
    ))
}

async fn handle_codex_cloud_task_id(
    request_id: &str,
    request: CodexCloudTaskIdRequest,
    action: CodexGatewayAction,
    event_type: &str,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        action,
        state,
        auth,
    )
    .await?;

    let _ = (request, event_type);
    Err(AppError::Unsupported(format!(
        "{} requires Codex cloud HTTP adapter invocation proof; local placeholder success is forbidden",
        codex_action_name(action)
    )))
}

async fn handle_codex_cloud_task_sibling_attempts(
    request_id: &str,
    request: CodexCloudTaskSiblingAttemptsRequest,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        CodexGatewayAction::CloudTaskListSiblingAttempts,
        state,
        auth,
    )
    .await?;

    let _ = request;
    Err(AppError::Unsupported(
        "codex.cloudTask.listSiblingAttempts requires Codex cloud HTTP adapter invocation proof; local placeholder success is forbidden".to_owned(),
    ))
}

async fn handle_codex_cloud_task_apply(
    request_id: &str,
    request: CodexCloudTaskApplyRequest,
    action: CodexGatewayAction,
    event_type: &str,
    preflight: bool,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        action,
        state,
        auth,
    )
    .await?;

    let _ = (request, event_type, preflight);
    Err(AppError::Unsupported(format!(
        "{} requires Codex cloud HTTP adapter invocation proof; local placeholder success is forbidden",
        codex_action_name(action)
    )))
}

async fn handle_codex_lifecycle(
    request_id: &str,
    request: CodexLifecycleRequest,
    action: CodexGatewayAction,
    event_type: &str,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&request.tenant_id),
        &request.codex_session_id,
        action,
        state,
        auth,
    )
    .await?;

    let _ = (request, event_type);
    Err(AppError::Unsupported(format!(
        "{} requires upstream Codex app-server invocation proof; local placeholder success is forbidden",
        codex_action_name(action)
    )))
}

async fn authorize_codex_control(
    request_id: &str,
    payload: &crate::server::protocol::CodexGatewayControlRequest,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    authorize_codex_access(
        request_id,
        Some(&payload.tenant_id),
        &payload.codex_session_id,
        payload.action,
        state,
        auth,
    )
    .await
}

fn ensure_supported_codex_control_action(action: CodexGatewayAction) -> Result<(), AppError> {
    match action {
        CodexGatewayAction::Control => Ok(()),
        CodexGatewayAction::Approval => Err(AppError::Unsupported(
            "codex.gateway.control action codex.approval is not implemented".to_owned(),
        )),
        CodexGatewayAction::Replay => Err(AppError::Unsupported(
            "codex.gateway.control action codex.replay is not implemented".to_owned(),
        )),
        _ => Err(AppError::Unsupported(format!(
            "codex.gateway.control action {} is not implemented",
            codex_action_name(action)
        ))),
    }
}

async fn authorize_local_codex_request(
    request_id: &str,
    codex_session_id: &str,
    tenant_id: &str,
    operation: &str,
    auth: Option<&AuthContext>,
    state: &AppState,
) -> Result<(), AppError> {
    let Some(auth) = auth else {
        audit_codex_decision(
            state,
            request_id,
            None,
            Some(tenant_id),
            codex_session_id,
            "allow",
            "NO_AUTH_REQUIRED",
            operation,
        )
        .await?;
        return Ok(());
    };

    if auth.tenant_id != tenant_id {
        audit_codex_decision(
            state,
            request_id,
            Some(auth),
            Some(tenant_id),
            codex_session_id,
            "deny",
            "TENANT_MISMATCH",
            operation,
        )
        .await?;
        return Err(AppError::Unauthorized("tenant mismatch".to_owned()));
    }

    audit_codex_decision(
        state,
        request_id,
        Some(auth),
        Some(tenant_id),
        codex_session_id,
        "allow",
        "AUTHORIZED",
        operation,
    )
    .await
}

async fn authorize_codex_access(
    request_id: &str,
    tenant_id: Option<&str>,
    codex_session_id: &str,
    action: CodexGatewayAction,
    state: &AppState,
    auth: Option<&AuthContext>,
) -> Result<(), AppError> {
    let Some(auth) = auth else {
        audit_codex_decision(
            state,
            request_id,
            None,
            tenant_id,
            codex_session_id,
            "allow",
            "NO_AUTH_REQUIRED",
            codex_action_name(action),
        )
        .await?;
        return Ok(());
    };

    if Some(auth.tenant_id.as_str()) != tenant_id {
        audit_codex_decision(
            state,
            request_id,
            Some(auth),
            tenant_id,
            codex_session_id,
            "deny",
            "TENANT_MISMATCH",
            codex_action_name(action),
        )
        .await?;
        return Err(AppError::Unauthorized("tenant mismatch".to_owned()));
    }

    audit_codex_decision(
        state,
        request_id,
        Some(auth),
        tenant_id,
        codex_session_id,
        "allow",
        "AUTHORIZED",
        codex_action_name(action),
    )
    .await
}

fn codex_action_name(action: crate::server::protocol::CodexGatewayAction) -> &'static str {
    match action {
        crate::server::protocol::CodexGatewayAction::Control => "codex.control",
        crate::server::protocol::CodexGatewayAction::Approval => "codex.approval",
        crate::server::protocol::CodexGatewayAction::Replay => "codex.replay",
        crate::server::protocol::CodexGatewayAction::ThreadStart => "codex.thread.start",
        crate::server::protocol::CodexGatewayAction::TurnStart => "codex.turn.start",
        crate::server::protocol::CodexGatewayAction::TurnSteer => "codex.turn.steer",
        crate::server::protocol::CodexGatewayAction::TurnInterrupt => "codex.turn.interrupt",
        crate::server::protocol::CodexGatewayAction::CloudTaskCreate => "codex.cloudTask.create",
        crate::server::protocol::CodexGatewayAction::CloudTaskList => "codex.cloudTask.list",
        crate::server::protocol::CodexGatewayAction::CloudTaskGetSummary => {
            "codex.cloudTask.getSummary"
        }
        crate::server::protocol::CodexGatewayAction::CloudTaskGetDiff => "codex.cloudTask.getDiff",
        crate::server::protocol::CodexGatewayAction::CloudTaskGetMessages => {
            "codex.cloudTask.getMessages"
        }
        crate::server::protocol::CodexGatewayAction::CloudTaskGetText => "codex.cloudTask.getText",
        crate::server::protocol::CodexGatewayAction::CloudTaskListSiblingAttempts => {
            "codex.cloudTask.listSiblingAttempts"
        }
        crate::server::protocol::CodexGatewayAction::CloudTaskApplyPreflight => {
            "codex.cloudTask.applyPreflight"
        }
        crate::server::protocol::CodexGatewayAction::CloudTaskApply => "codex.cloudTask.apply",
        crate::server::protocol::CodexGatewayAction::McpServerListStatus => {
            "codex.mcp.server.listStatus"
        }
        crate::server::protocol::CodexGatewayAction::McpResourceRead => "codex.mcp.resource.read",
        crate::server::protocol::CodexGatewayAction::McpToolCall => "codex.mcp.tool.call",
        crate::server::protocol::CodexGatewayAction::McpServerRefresh => "codex.mcp.server.refresh",
        crate::server::protocol::CodexGatewayAction::McpOauthLogin => "codex.mcp.oauth.login",
        crate::server::protocol::CodexGatewayAction::McpElicitationRespond => {
            "codex.mcp.elicitation.respond"
        }
    }
}

async fn audit_codex_decision(
    state: &AppState,
    request_id: &str,
    auth: Option<&AuthContext>,
    requested_tenant_id: Option<&str>,
    codex_session_id: &str,
    decision: &str,
    reason_code: &str,
    action: &str,
) -> Result<(), AppError> {
    let event = EventRecord::new(
        "codex.audit",
        None,
        None,
        None,
        json!({
            "request_id": request_id,
            "principal_id": auth.map(|auth| auth.principal_id.as_str()),
            "tenant_id": auth.map(|auth| auth.tenant_id.as_str()),
            "requested_tenant_id": requested_tenant_id,
            "token_id": auth.map(|auth| auth.token_id.as_str()),
            "codex_session_id": codex_session_id,
            "action": action,
            "decision": decision,
            "reason_code": reason_code,
            "target_kind": "session",
            "protocol": "codex-gateway.v1",
        }),
    );
    append_audit_event(state, &event).await?;
    state.events.publish(event).await;
    Ok(())
}

async fn append_audit_event(state: &AppState, event: &EventRecord) -> Result<(), AppError> {
    let dir = state.config.data_dir.join("audit");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("audit.jsonl");
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(serde_json::to_string(event)?.as_bytes())
        .await?;
    file.write_all(b"\n").await?;
    Ok(())
}

fn truncate_snapshot_text(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

async fn publish_codex_gateway_records_after(
    state: &AppState,
    codex_session_id: &str,
    after_cursor: u64,
) -> Result<(), AppError> {
    let replay = state
        .codex_gateway
        .replay_events(codex_session_id, Some(after_cursor), 200)
        .await?;
    for record in replay.events {
        publish_codex_gateway_record(state, record).await;
    }
    Ok(())
}

async fn publish_local_codex_error_after(
    state: &AppState,
    codex_session_id: &str,
    after_cursor: u64,
    error: CodexLocalAdapterProcessError,
) -> Result<(), AppError> {
    let replay = state
        .codex_gateway
        .replay_events(codex_session_id, Some(after_cursor), 200)
        .await?;
    if !replay.events.is_empty() {
        for record in replay.events {
            publish_codex_gateway_record(state, record).await;
        }
        return Ok(());
    }

    let payload = error.payload;
    let record = state
        .codex_gateway
        .append_event(
            &payload.codex_session_id,
            CodexGatewayEvent::new(
                "codex.control.error",
                json!({
                    "requestId": &payload.request_id,
                    "operation": &payload.operation,
                    "codexSessionId": &payload.codex_session_id,
                    "lifecycleState": "failed",
                    "error": payload,
                }),
            ),
        )
        .await?;
    publish_codex_gateway_record(state, record).await;
    Ok(())
}

async fn local_codex_status_payload(
    state: &AppState,
    request_id: &str,
    codex_session_id: &str,
) -> Result<Value, AppError> {
    if let Some(adapter) = state.codex_local_adapters.get(codex_session_id) {
        return Ok(local_codex_status_from_runtime(
            request_id,
            &adapter.runtime().await,
            true,
        ));
    }

    let recovered = state
        .codex_gateway
        .recover_session(codex_session_id)
        .await?;
    if let Some(payload) = recovered.local_adapters.get(codex_session_id) {
        let lifecycle_state = payload
            .get("lifecycleState")
            .cloned()
            .unwrap_or_else(|| json!("idle"));
        let child_process = payload.get("childProcess").cloned().unwrap_or(Value::Null);
        return Ok(json!({
            "requestId": request_id,
            "operation": "codex.local.status",
            "codexSessionId": codex_session_id,
            "lifecycleState": lifecycle_state,
            "childProcess": child_process,
            "running": false,
            "latestCursor": recovered.last_cursor,
        }));
    }

    Ok(json!({
        "requestId": request_id,
        "operation": "codex.local.status",
        "codexSessionId": codex_session_id,
        "lifecycleState": "idle",
        "childProcess": null,
        "running": false,
        "latestCursor": recovered.last_cursor,
    }))
}

fn local_codex_status_from_runtime(
    request_id: &str,
    runtime: &CodexLocalAdapterRuntime,
    running: bool,
) -> Value {
    json!({
        "requestId": request_id,
        "operation": "codex.local.status",
        "codexSessionId": &runtime.codex_session_id,
        "lifecycleState": runtime.state,
        "childProcess": &runtime.child_process,
        "commandLane": &runtime.command_lane,
        "running": running,
    })
}

async fn publish_codex_gateway_record(state: &AppState, record: CodexGatewayEventRecord) {
    state
        .events
        .publish(EventRecord {
            time: record.time,
            event_id: record.event_id,
            event_type: record.event_type,
            workspace_id: None,
            window_id: None,
            pane_id: None,
            payload: json!({
                "cursor": record.cursor,
                "codex_session_id": record.session_id,
                "codex_thread_id": record.codex_thread_id,
                "codex_turn_id": record.codex_turn_id,
                "data": record.payload,
            }),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::{json, Value};
    use tokio::{
        sync::Mutex,
        time::{timeout, Duration},
    };

    use crate::{
        app_state::AppState,
        codex_gateway::{
            CodexGatewayAdapter, CodexGatewayEvent, CodexGatewayRequest, CodexGatewayResponse,
            CodexGatewayStore, CodexInboundMessage, CodexOutboundMessage, CodexReviewDelivery,
            CodexReviewTarget, CodexServerRequest, CodexServerResponse,
        },
        config::{AgentConfig, Config, SecurityConfig},
        event::EventRecord,
        server::protocol::{
            ClientMessage, ClientMessageKind, CodexCloudTaskApplyRequest,
            CodexCloudTaskCreateRequest, CodexGatewayAction, CodexGatewayControlRequest,
            CodexLifecycleRequest, CodexLocalApprovalRespondRequest, CodexLocalAttachRequest,
            CodexLocalInterruptRequest, CodexLocalReplayRequest, CodexLocalRequestRequest,
            CodexLocalSessionRequest, CodexLocalSnapshotRequest, CodexLocalStartRequest,
            CodexLocalSteerRequest, CodexLocalStopRequest, CodexLocalTurnRequest,
            CodexLocalUnsupportedRequest,
        },
    };

    use super::{dispatch, AuthContext};

    #[derive(Default)]
    struct RecordingTransport {
        sent: Mutex<Vec<CodexOutboundMessage>>,
    }

    impl RecordingTransport {
        async fn sent(&self) -> Vec<CodexOutboundMessage> {
            self.sent.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::codex_gateway::CodexGatewayTransport for RecordingTransport {
        async fn send(&self, message: CodexOutboundMessage) -> crate::error::Result<()> {
            self.sent.lock().await.push(message);
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unauthenticated_local_codex_start_is_allowed_without_token() {
        let state = test_state().await;
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "local-status-open".to_owned(),
                kind: ClientMessageKind::CodexLocalStatus(CodexLocalSessionRequest {
                    codex_session_id: "cdxs_local_deny".to_owned(),
                    tenant_id: "local".to_owned(),
                }),
            },
            &state,
            None,
        )
        .await
        .expect("unauthenticated local codex status should be allowed");

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("local-status-open")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("allow"));
        assert_eq!(
            payload_str(&audit.payload, "reason_code"),
            Some("NO_AUTH_REQUIRED")
        );
        let status = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.status"
                && payload_str(&event.payload["data"], "requestId") == Some("local-status-open")
        })
        .await;
        assert_eq!(
            payload_str(&status.payload["data"], "lifecycleState"),
            Some("idle")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_local_codex_start_status_stop_publishes_lifecycle() {
        let mut state = test_state().await;
        let root = unique_tmp_dir("todex-local-ws-lifecycle");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_fake_codex_binary(&root).await;
        Arc::make_mut(&mut state.config).agent.codex_bin = binary.to_string_lossy().to_string();
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "local-start-allow".to_owned(),
                kind: ClientMessageKind::CodexLocalStart(CodexLocalStartRequest {
                    codex_session_id: "cdxs_local_allow".to_owned(),
                    tenant_id: "local".to_owned(),
                    cwd: cwd.to_string_lossy().to_string(),
                    model: None,
                    approval_policy: None,
                    sandbox_mode: None,
                    config_overrides: Value::Object(Default::default()),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .expect("authenticated local codex start should spawn adapter");

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("local-start-allow")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("allow"));
        assert_eq!(
            payload_str(&audit.payload, "reason_code"),
            Some("AUTHORIZED")
        );

        let starting = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.starting"
                && payload_str(&event.payload["data"], "requestId") == Some("local-start-allow")
        })
        .await;
        let ready = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.ready"
                && payload_str(&event.payload["data"], "codexSessionId") == Some("cdxs_local_allow")
        })
        .await;
        assert_eq!(payload_u64(&starting.payload, "cursor"), Some(1));
        assert_eq!(payload_u64(&ready.payload, "cursor"), Some(2));
        assert!(state.codex_local_adapters.get("cdxs_local_allow").is_some());

        dispatch(
            ClientMessage {
                id: "local-status-live".to_owned(),
                kind: ClientMessageKind::CodexLocalStatus(CodexLocalSessionRequest {
                    codex_session_id: "cdxs_local_allow".to_owned(),
                    tenant_id: "local".to_owned(),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .expect("authenticated local codex status should publish live snapshot");
        let status = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.status"
                && payload_str(&event.payload["data"], "requestId") == Some("local-status-live")
        })
        .await;
        assert_eq!(
            payload_str(&status.payload["data"], "lifecycleState"),
            Some("ready")
        );
        assert_eq!(status.payload["data"]["running"], json!(true));

        dispatch(
            ClientMessage {
                id: "local-stop-allow".to_owned(),
                kind: ClientMessageKind::CodexLocalStop(CodexLocalStopRequest {
                    codex_session_id: "cdxs_local_allow".to_owned(),
                    tenant_id: "local".to_owned(),
                    force: true,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .expect("authenticated local codex stop should stop adapter");
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.stopping"
        })
        .await;
        let stopped = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.stopped"
                && payload_str(&event.payload["data"], "requestId") == Some("local-stop-allow")
        })
        .await;
        assert_eq!(
            payload_str(&stopped.payload["data"], "lifecycleState"),
            Some("stopped")
        );
        assert!(state.codex_local_adapters.get("cdxs_local_allow").is_none());

        let replay = state
            .codex_gateway
            .replay_events("cdxs_local_allow", None, 10)
            .await
            .unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "codex.control.starting",
                "codex.control.ready",
                "codex.control.status",
                "codex.control.stopping",
                "codex.control.stopped",
            ]
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_status_before_start_reports_idle_without_process() {
        let state = test_state().await;
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "local-status-idle".to_owned(),
                kind: ClientMessageKind::CodexLocalStatus(CodexLocalSessionRequest {
                    codex_session_id: "cdxs_status_idle".to_owned(),
                    tenant_id: "local".to_owned(),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .expect("status before start should be read-only");

        let status = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.status"
                && payload_str(&event.payload["data"], "requestId") == Some("local-status-idle")
        })
        .await;
        assert_eq!(
            payload_str(&status.payload["data"], "lifecycleState"),
            Some("idle")
        );
        assert_eq!(status.payload["data"]["running"], json!(false));
        assert!(state.codex_local_adapters.get("cdxs_status_idle").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_start_invalid_cwd_publishes_typed_error() {
        let state = test_state().await;
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "local-start-invalid-cwd".to_owned(),
                kind: ClientMessageKind::CodexLocalStart(CodexLocalStartRequest {
                    codex_session_id: "cdxs_invalid_ws_cwd".to_owned(),
                    tenant_id: "local".to_owned(),
                    cwd: unique_tmp_dir("todex-missing-local-codex-cwd")
                        .to_string_lossy()
                        .to_string(),
                    model: None,
                    approval_policy: None,
                    sandbox_mode: None,
                    config_overrides: Value::Object(Default::default()),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .expect("invalid cwd should publish typed local error rather than generic websocket error");

        let error = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.error"
                && payload_str(&event.payload["data"], "requestId")
                    == Some("local-start-invalid-cwd")
        })
        .await;
        assert_eq!(error.payload["data"]["error"]["code"], json!("INVALID_CWD"));
        assert_eq!(
            error.payload["data"]["error"]["codexSessionId"],
            json!("cdxs_invalid_ws_cwd")
        );
        assert!(state
            .codex_local_adapters
            .get("cdxs_invalid_ws_cwd")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_stop_missing_session_publishes_typed_error() {
        let state = test_state().await;
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "local-stop-missing".to_owned(),
                kind: ClientMessageKind::CodexLocalStop(CodexLocalStopRequest {
                    codex_session_id: "cdxs_stop_missing".to_owned(),
                    tenant_id: "local".to_owned(),
                    force: false,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .expect("missing stop target should publish typed local error");

        let error = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.error"
                && payload_str(&event.payload["data"], "requestId") == Some("local-stop-missing")
        })
        .await;
        assert_eq!(
            error.payload["data"]["error"]["code"],
            json!("UNSUPPORTED_ACTION")
        );
        assert_eq!(
            payload_str(&error.payload["data"], "operation"),
            Some("codex.local.stop")
        );
        assert!(state
            .codex_local_adapters
            .get("cdxs_stop_missing")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_local_codex_turn_streams_typed_events() {
        let mut state = test_state().await;
        let root = unique_tmp_dir("todex-local-ws-turn");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_fake_codex_binary(&root).await;
        Arc::make_mut(&mut state.config).agent.codex_bin = binary.to_string_lossy().to_string();
        let mut events = state.events.subscribe();

        dispatch(
            local_start_message("local-turn-start", "cdxs_local_turn", &cwd),
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.ready"
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-turn-1".to_owned(),
                kind: ClientMessageKind::CodexLocalTurn(CodexLocalTurnRequest {
                    codex_session_id: "cdxs_local_turn".to_owned(),
                    tenant_id: "local".to_owned(),
                    thread_id: "thread_1".to_owned(),
                    input: json!([{ "type": "text", "text": "hello" }]),
                    approval_policy: None,
                    sandbox_policy: None,
                    service_tier: None,
                    collaboration_mode: None,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();

        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.request.accepted"
                && payload_str(&event.payload["data"], "requestId") == Some("local-turn-1")
        })
        .await;
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.turn.started"
        })
        .await;
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.item.agentMessage.delta"
        })
        .await;
        let completed = wait_for_event(&mut events, |event| {
            event.event_type == "codex.turn.completed"
                && payload_str(&event.payload["data"], "turnId") == Some("turn_1")
        })
        .await;
        assert_eq!(
            payload_str(&completed.payload["data"], "requestId"),
            Some("local-turn-1")
        );

        let runtime = state
            .codex_local_adapters
            .get("cdxs_local_turn")
            .unwrap()
            .runtime()
            .await;
        assert_eq!(serde_json::to_value(runtime.state).unwrap(), json!("ready"));

        let replay = state
            .codex_gateway
            .replay_events("cdxs_local_turn", None, 20)
            .await
            .unwrap();
        assert!(replay
            .events
            .iter()
            .any(|event| event.event_type == "codex.control.request.accepted"));
        assert!(replay
            .events
            .iter()
            .any(|event| event.event_type == "codex.item.agentMessage.delta"));

        let wire_log = tokio::fs::read_to_string(root.join("wire.log"))
            .await
            .expect("fake Codex should record app-server wire messages");
        assert!(wire_log.contains("\"method\":\"initialize\""));
        assert!(wire_log.contains("\"method\":\"initialized\""));
        assert!(wire_log.contains("\"method\":\"turn/start\""));

        dispatch(
            ClientMessage {
                id: "local-request-1".to_owned(),
                kind: ClientMessageKind::CodexLocalRequest(CodexLocalRequestRequest {
                    codex_session_id: "cdxs_local_turn".to_owned(),
                    tenant_id: "local".to_owned(),
                    method: "thread/start".to_owned(),
                    params: json!({ "threadId": "thread_2" }),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        let wire_log = tokio::fs::read_to_string(root.join("wire.log"))
            .await
            .expect("fake Codex should record passthrough wire messages");
        assert!(wire_log.contains("\"method\":\"thread/start\""));

        let _ = state
            .codex_local_adapters
            .stop("cdxs_local_turn", "cleanup", true)
            .await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_local_codex_permission_and_question_requests_can_be_replied_to() {
        let mut state = test_state().await;
        let root = unique_tmp_dir("todex-local-ws-server-requests");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_requesting_fake_codex_binary(&root).await;
        Arc::make_mut(&mut state.config).agent.codex_bin = binary.to_string_lossy().to_string();
        let mut events = state.events.subscribe();

        dispatch(
            local_start_message("local-server-requests-start", "cdxs_local_requests", &cwd),
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.ready"
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-permission-turn".to_owned(),
                kind: ClientMessageKind::CodexLocalTurn(CodexLocalTurnRequest {
                    codex_session_id: "cdxs_local_requests".to_owned(),
                    tenant_id: "local".to_owned(),
                    thread_id: "thread_1".to_owned(),
                    input: json!([{ "type": "text", "text": "needs permission" }]),
                    approval_policy: None,
                    sandbox_policy: None,
                    service_tier: None,
                    collaboration_mode: None,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        let permission = wait_for_event(&mut events, |event| {
            event.event_type == "codex.approval.permissions.request"
                && payload_str(&event.payload["data"], "requestId") == Some("permission-1")
        })
        .await;
        assert_eq!(
            payload_str(&permission.payload["data"], "outcome"),
            Some("pending")
        );

        dispatch(
            ClientMessage {
                id: "local-permission-response".to_owned(),
                kind: ClientMessageKind::CodexLocalApprovalRespond(
                    CodexLocalApprovalRespondRequest {
                        codex_session_id: "cdxs_local_requests".to_owned(),
                        tenant_id: "local".to_owned(),
                        request_id: "permission-1".to_owned(),
                        response_type: "codex.approval.permissions.respond".to_owned(),
                        response: json!({
                            "permissions": {
                                "network": { "enabled": true },
                            },
                            "scope": "turn",
                            "strictAutoReview": false,
                        }),
                    },
                ),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        let permission_resolved = wait_for_event(&mut events, |event| {
            event.event_type == "codex.serverRequest.resolved"
                && event_field_str(event, "requestId") == Some("permission-1")
        })
        .await;
        assert_eq!(
            event_field_str(&permission_resolved, "outcome"),
            Some("accepted")
        );

        dispatch(
            ClientMessage {
                id: "local-question-request".to_owned(),
                kind: ClientMessageKind::CodexLocalRequest(CodexLocalRequestRequest {
                    codex_session_id: "cdxs_local_requests".to_owned(),
                    tenant_id: "local".to_owned(),
                    method: "thread/start".to_owned(),
                    params: json!({ "threadId": "thread_2" }),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        let question = wait_for_event(&mut events, |event| {
            event.event_type == "codex.tool.requestUserInput.request"
                && payload_str(&event.payload["data"], "requestId") == Some("question-1")
        })
        .await;
        assert_eq!(
            payload_str(&question.payload["data"], "outcome"),
            Some("pending")
        );

        dispatch(
            ClientMessage {
                id: "local-question-response".to_owned(),
                kind: ClientMessageKind::CodexLocalApprovalRespond(
                    CodexLocalApprovalRespondRequest {
                        codex_session_id: "cdxs_local_requests".to_owned(),
                        tenant_id: "local".to_owned(),
                        request_id: "question-1".to_owned(),
                        response_type: "codex.tool.requestUserInput.respond".to_owned(),
                        response: json!({
                            "answers": {
                                "confirm_path": { "answers": ["yes"] }
                            }
                        }),
                    },
                ),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        let question_resolved = wait_for_event(&mut events, |event| {
            event.event_type == "codex.serverRequest.resolved"
                && event_field_str(event, "requestId") == Some("question-1")
        })
        .await;
        assert_eq!(
            event_field_str(&question_resolved, "threadId"),
            Some("thread_2")
        );

        let wire_log = tokio::fs::read_to_string(root.join("wire.log"))
            .await
            .expect("fake Codex should record server request responses");
        assert!(wire_log.contains("\"method\":\"turn/start\""));
        assert!(wire_log.contains("\"method\":\"thread/start\""));
        assert!(wire_log.contains("\"id\":\"permission-1\""));
        assert!(wire_log.contains("\"id\":\"question-1\""));

        let _ = state
            .codex_local_adapters
            .stop("cdxs_local_requests", "cleanup", true)
            .await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_second_turn_is_rejected_when_busy() {
        let mut state = test_state().await;
        let root = unique_tmp_dir("todex-local-ws-turn-busy");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_holding_fake_codex_binary(&root).await;
        Arc::make_mut(&mut state.config).agent.codex_bin = binary.to_string_lossy().to_string();
        let mut events = state.events.subscribe();

        dispatch(
            local_start_message("local-busy-start", "cdxs_busy_turn", &cwd),
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.ready"
        })
        .await;

        for request_id in ["busy-turn-1", "busy-turn-2"] {
            dispatch(
                ClientMessage {
                    id: request_id.to_owned(),
                    kind: ClientMessageKind::CodexLocalTurn(CodexLocalTurnRequest {
                        codex_session_id: "cdxs_busy_turn".to_owned(),
                        tenant_id: "local".to_owned(),
                        thread_id: "thread_busy".to_owned(),
                        input: json!([{ "type": "text", "text": request_id }]),
                        approval_policy: None,
                        sandbox_policy: None,
                        service_tier: None,
                        collaboration_mode: None,
                    }),
                },
                &state,
                Some(&test_auth()),
            )
            .await
            .unwrap();
        }

        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.request.accepted"
                && payload_str(&event.payload["data"], "requestId") == Some("busy-turn-1")
        })
        .await;
        let rejected = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.request.rejected"
                && payload_str(&event.payload["data"], "requestId") == Some("busy-turn-2")
        })
        .await;
        assert_eq!(
            rejected.payload["data"]["error"]["code"],
            json!("SESSION_BUSY")
        );

        let _ = state
            .codex_local_adapters
            .stop("cdxs_busy_turn", "cleanup", true)
            .await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_turn_missing_adapter_publishes_typed_error() {
        let state = test_state().await;
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "missing-turn-1".to_owned(),
                kind: ClientMessageKind::CodexLocalTurn(CodexLocalTurnRequest {
                    codex_session_id: "cdxs_missing_turn".to_owned(),
                    tenant_id: "local".to_owned(),
                    thread_id: "thread_missing".to_owned(),
                    input: json!([{ "type": "text", "text": "hello" }]),
                    approval_policy: None,
                    sandbox_policy: None,
                    service_tier: None,
                    collaboration_mode: None,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();

        let error = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.error"
                && payload_str(&event.payload["data"], "requestId") == Some("missing-turn-1")
        })
        .await;
        assert_eq!(
            error.payload["data"]["error"]["code"],
            json!("UNSUPPORTED_ACTION")
        );
        assert_eq!(
            payload_str(&error.payload["data"], "operation"),
            Some("codex.local.turn")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_local_codex_steer_interrupt_replay_attach_and_snapshot_publish_events() {
        let mut state = test_state().await;
        let root = unique_tmp_dir("todex-local-ws-task-10-12");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_fake_codex_binary(&root).await;
        Arc::make_mut(&mut state.config).agent.codex_bin = binary.to_string_lossy().to_string();
        let mut events = state.events.subscribe();

        dispatch(
            local_start_message("task-10-start", "cdxs_task_10", &cwd),
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.ready"
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-steer-1".to_owned(),
                kind: ClientMessageKind::CodexLocalSteer(CodexLocalSteerRequest {
                    codex_session_id: "cdxs_task_10".to_owned(),
                    tenant_id: "local".to_owned(),
                    thread_id: "thread_1".to_owned(),
                    turn_id: "turn_1".to_owned(),
                    expected_turn_id: Some("turn_1".to_owned()),
                    input: json!([{ "type": "text", "text": "steer" }]),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.request.accepted"
                && payload_str(&event.payload["data"], "operation") == Some("codex.local.steer")
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-interrupt-1".to_owned(),
                kind: ClientMessageKind::CodexLocalInterrupt(CodexLocalInterruptRequest {
                    codex_session_id: "cdxs_task_10".to_owned(),
                    tenant_id: "local".to_owned(),
                    thread_id: "thread_1".to_owned(),
                    turn_id: Some("turn_1".to_owned()),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.request.accepted"
                && payload_str(&event.payload["data"], "operation") == Some("codex.local.interrupt")
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-replay-1".to_owned(),
                kind: ClientMessageKind::CodexLocalReplay(CodexLocalReplayRequest {
                    codex_session_id: "cdxs_task_10".to_owned(),
                    tenant_id: "local".to_owned(),
                    after_cursor: Some(0),
                    limit: 5,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.starting"
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-attach-1".to_owned(),
                kind: ClientMessageKind::CodexLocalAttach(CodexLocalAttachRequest {
                    codex_session_id: "cdxs_task_10".to_owned(),
                    tenant_id: "local".to_owned(),
                    after_cursor: Some(1),
                    replay_limit: 5,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.ready"
        })
        .await;

        dispatch(
            ClientMessage {
                id: "local-snapshot-1".to_owned(),
                kind: ClientMessageKind::CodexLocalSnapshot(CodexLocalSnapshotRequest {
                    codex_session_id: "cdxs_task_10".to_owned(),
                    tenant_id: "local".to_owned(),
                    max_bytes: 8,
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();
        let snapshot = wait_for_event(&mut events, |event| {
            event.event_type == "codex.local.snapshot"
                && payload_str(&event.payload["data"], "requestId") == Some("local-snapshot-1")
        })
        .await;
        assert_eq!(snapshot.payload["data"]["authoritative"], json!(false));

        let _ = state
            .codex_local_adapters
            .stop("cdxs_task_10", "cleanup", true)
            .await;
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_codex_cloud_task_unsupported_publishes_unsupported_local() {
        let state = test_state().await;
        let mut events = state.events.subscribe();

        dispatch(
            ClientMessage {
                id: "local-cloud-unsupported".to_owned(),
                kind: ClientMessageKind::CodexLocalUnsupported(CodexLocalUnsupportedRequest {
                    codex_session_id: "cdxs_cloud_excluded".to_owned(),
                    tenant_id: "local".to_owned(),
                    operation: "codex.cloudTask.create".to_owned(),
                    reason: Some(
                        "cloud tasks are excluded from local Codex CLI control".to_owned(),
                    ),
                }),
            },
            &state,
            Some(&test_auth()),
        )
        .await
        .unwrap();

        let error = wait_for_event(&mut events, |event| {
            event.event_type == "codex.control.error"
                && payload_str(&event.payload["data"], "requestId")
                    == Some("local-cloud-unsupported")
        })
        .await;
        assert_eq!(
            error.payload["data"]["error"]["code"],
            json!("UNSUPPORTED_LOCAL")
        );
        assert_eq!(
            payload_str(&error.payload["data"], "operation"),
            Some("codex.cloudTask.create")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_local_codex_tenant_mismatch_is_denied_before_unsupported() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let auth = AuthContext {
            principal_id: "gateway-token".to_owned(),
            tenant_id: "other".to_owned(),
            token_id: "configured-token".to_owned(),
        };

        let error = dispatch(
            ClientMessage {
                id: "local-unsupported-tenant-mismatch".to_owned(),
                kind: ClientMessageKind::CodexLocalUnsupported(CodexLocalUnsupportedRequest {
                    codex_session_id: "cdxs_local_other".to_owned(),
                    tenant_id: "local".to_owned(),
                    operation: "codex.cloudTask.create".to_owned(),
                    reason: Some(
                        "cloud tasks are excluded from local Codex CLI control".to_owned(),
                    ),
                }),
            },
            &state,
            Some(&auth),
        )
        .await
        .expect_err("tenant mismatch must fail before unsupported");
        assert_eq!(error.code(), "UNAUTHORIZED");

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id")
                    == Some("local-unsupported-tenant-mismatch")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("deny"));
        assert_eq!(
            payload_str(&audit.payload, "reason_code"),
            Some("TENANT_MISMATCH")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn websocket_authentication_rejects_missing_configured_token() {
        let mut state = test_state().await;
        Arc::make_mut(&mut state.config).security.auth_token = None;

        let auth = crate::server::websocket::authenticate_headers(
            &state,
            &axum::http::HeaderMap::from_iter([(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer todex_gw_test"),
            )]),
        );

        assert!(auth.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unauthenticated_codex_control_is_allowed_and_audited() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let message = codex_control_message("deny-1", "cdxs_denied", "local");

        dispatch(message, &state, None)
            .await
            .expect("unauthenticated control should be allowed");

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("deny-1")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("allow"));
        assert_eq!(
            payload_str(&audit.payload, "reason_code"),
            Some("NO_AUTH_REQUIRED")
        );

        let audit_file = tokio::fs::read_to_string(state.config.data_dir.join("audit/audit.jsonl"))
            .await
            .expect("audit log should be persisted");
        assert!(audit_file.contains("NO_AUTH_REQUIRED"));
        assert!(audit_file.contains("cdxs_denied"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authorized_codex_control_is_accepted_and_audited() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let message = codex_control_message("allow-1", "cdxs_allowed", "local");
        let auth = AuthContext {
            principal_id: "gateway-token".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "configured-token".to_owned(),
        };

        dispatch(message, &state, Some(&auth))
            .await
            .expect("authorized control should pass");

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("allow-1")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("allow"));
        assert_eq!(
            payload_str(&audit.payload, "reason_code"),
            Some("AUTHORIZED")
        );

        let accepted = wait_for_event(&mut events, |event| {
            event.event_type == "codex.gateway.control.accepted"
                && payload_str(&event.payload, "request_id") == Some("allow-1")
        })
        .await;
        assert_eq!(
            payload_str(&accepted.payload, "codex_session_id"),
            Some("cdxs_allowed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsupported_codex_gateway_control_fails_closed() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let mut message = codex_control_message("unsupported-1", "cdxs_unsupported", "local");
        if let ClientMessageKind::CodexGatewayControl(payload) = &mut message.kind {
            payload.action = CodexGatewayAction::Approval;
        }

        let error = dispatch(message, &state, Some(&test_auth()))
            .await
            .expect_err("unsupported control action must fail");
        assert_eq!(error.code(), "UNSUPPORTED");
        assert!(error.to_string().contains("codex.approval"));

        let lagged = tokio::time::timeout(Duration::from_millis(200), events.recv()).await;
        assert!(
            lagged.is_err(),
            "unsupported control should not emit codex events"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_only_thread_turn_lifecycle_fails_closed_without_upstream_invocation() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let auth = test_auth();

        let error = dispatch(
            codex_lifecycle_message(
                "thread-start-1",
                ClientMessageKind::CodexThreadStart,
                json!({ "threadId": "thread-1" }),
            ),
            &state,
            Some(&auth),
        )
        .await
        .expect_err("schema-only thread start must fail closed without upstream proof");
        assert_eq!(error.code(), "UNSUPPORTED");
        assert!(error.to_string().contains("invocation proof"));

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("thread-start-1")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("allow"));

        let no_success = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                let event = events.recv().await.expect("receive event");
                if event.event_type.starts_with("codex.") && event.event_type != "codex.audit" {
                    return event.event_type;
                }
            }
        })
        .await;
        assert!(
            no_success.is_err(),
            "schema-only thread start must not emit local success events"
        );

        let replay = state
            .codex_gateway
            .replay_events("cdxs_lifecycle", None, 10)
            .await
            .unwrap();
        assert!(replay.events.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_only_turn_steer_fails_closed_without_upstream_invocation() {
        let state = test_state().await;

        let error = dispatch(
            codex_lifecycle_message(
                "turn-steer-1",
                ClientMessageKind::CodexTurnSteer,
                json!({
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "expectedTurnId": "turn-1",
                    "input": [{ "type": "text", "text": "keep going", "textElements": [] }],
                }),
            ),
            &state,
            Some(&test_auth()),
        )
        .await
        .expect_err("schema-only turn steer must fail closed without upstream proof");
        assert_eq!(error.code(), "UNSUPPORTED");
        assert!(error.to_string().contains("invocation proof"));

        let recovered = state
            .codex_gateway
            .recover_session("cdxs_lifecycle")
            .await
            .unwrap();
        assert_eq!(recovered.last_cursor, 0);
        assert!(recovered.turns.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_remote_gateway_flow_is_authenticated_replayed_and_recovered() {
        let state = test_state().await;
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, mut adapter_events, mut server_requests) =
            CodexGatewayAdapter::new(transport.clone());
        let session_id = "cdxs_full_flow";
        let thread_id = "thread-full-1";
        let turn_id = "turn-full-1";
        let plan_id = "plan-full-1";

        assert!(crate::server::websocket::authenticate_headers(
            &state,
            &axum::http::HeaderMap::from_iter([(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer todex_gw_test"),
            )])
        )
        .is_some());

        state
            .codex_gateway
            .append_event(
                session_id,
                CodexGatewayEvent::new(
                    "codex.thread.started",
                    json!({
                        "threadId": thread_id,
                        "title": "Verify remote gateway flow"
                    }),
                ),
            )
            .await
            .expect("thread event should persist");
        state
            .codex_gateway
            .append_event(
                session_id,
                CodexGatewayEvent::new(
                    "codex.turn.started",
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "collaborationMode": {
                            "mode": "plan",
                            "settings": { "model": "mock-model" }
                        },
                        "input": [{
                            "type": "text",
                            "text": "Plan the remote gateway flow",
                            "textElements": []
                        }]
                    }),
                ),
            )
            .await
            .expect("turn event should persist");

        state
            .codex_gateway
            .append_event(
                session_id,
                CodexGatewayEvent::from_codex_notification(
                    "turn/plan/updated",
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "planId": plan_id,
                        "items": [{
                            "type": "plan",
                            "id": plan_id,
                            "text": "Plan the remote gateway flow",
                            "status": "completed"
                        }],
                        "explanation": "Gateway plan step recorded during the remote flow"
                    }),
                ),
            )
            .await
            .expect("plan update should persist");

        let review_request = CodexGatewayRequest::review_start(
            "review-request-full-1",
            thread_id,
            CodexReviewTarget::Commit {
                sha: "1234567deadbeef".to_string(),
                title: Some("Tidy remote gateway flow".to_string()),
            },
            CodexReviewDelivery::Inline,
        );
        adapter
            .dispatch_request(review_request.clone())
            .await
            .expect("review start should route");
        let review_started = timeout(Duration::from_secs(2), adapter_events.recv())
            .await
            .expect("review started event should arrive")
            .expect("review started event should be present");
        assert_eq!(review_started.event_type, "codex.review.started");
        assert_eq!(
            payload_str(&review_started.payload, "requestId"),
            Some("review-request-full-1")
        );
        state
            .codex_gateway
            .append_event(session_id, review_started)
            .await
            .expect("review started should persist");

        let review_response = CodexGatewayResponse {
            id: review_request.id.clone(),
            response_type: "codex.review.start.result".to_string(),
            payload: json!({
                "threadId": thread_id,
                "reviewThreadId": thread_id,
                "turn": { "id": turn_id, "status": "inProgress" }
            }),
        };
        adapter
            .handle_inbound(CodexInboundMessage::Response(review_response))
            .expect("review response should route");
        let review_result = timeout(Duration::from_secs(2), adapter_events.recv())
            .await
            .expect("review result should arrive")
            .expect("review result should be present");
        assert_eq!(review_result.event_type, "codex.review.start.result");
        state
            .codex_gateway
            .append_event(session_id, review_result)
            .await
            .expect("review result should persist");

        let approval_accept_request = CodexServerRequest {
            id: "approval-accept-full-1".to_string(),
            wire_id: None,
            request_type: "codex.approval.commandExecution.request".to_string(),
            payload: json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "command": "cargo test"
            }),
        };
        adapter
            .handle_inbound(CodexInboundMessage::ServerRequest(
                approval_accept_request.clone(),
            ))
            .expect("approval request should route");
        assert_eq!(
            server_requests
                .recv()
                .await
                .expect("approval request should be queued"),
            approval_accept_request
        );
        let approval_pending = timeout(Duration::from_secs(2), adapter_events.recv())
            .await
            .expect("approval pending event should arrive")
            .expect("approval pending event should be present");
        assert_eq!(
            approval_pending.event_type,
            "codex.approval.commandExecution.request"
        );
        state
            .codex_gateway
            .append_event(session_id, approval_pending)
            .await
            .expect("approval pending should persist");

        let approval_accept_response = CodexServerResponse {
            id: "approval-accept-full-1".to_string(),
            wire_id: None,
            response_type: "codex.approval.commandExecution.respond".to_string(),
            payload: json!({ "decision": "accept" }),
        };
        adapter
            .respond_to_server_request(approval_accept_response.clone())
            .await
            .expect("approval accept should route");
        let approval_accepted = timeout(Duration::from_secs(2), adapter_events.recv())
            .await
            .expect("approval resolved event should arrive")
            .expect("approval resolved event should be present");
        assert_eq!(approval_accepted.event_type, "codex.serverRequest.resolved");
        assert_eq!(
            payload_str(&approval_accepted.payload, "outcome"),
            Some("accepted")
        );
        state
            .codex_gateway
            .append_event(session_id, approval_accepted)
            .await
            .expect("approval resolution should persist");

        let approval_deny_request = CodexServerRequest {
            id: "approval-deny-full-1".to_string(),
            wire_id: None,
            request_type: "codex.approval.fileChange.request".to_string(),
            payload: json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "filePath": "src/lib.rs"
            }),
        };
        adapter
            .handle_inbound(CodexInboundMessage::ServerRequest(
                approval_deny_request.clone(),
            ))
            .expect("deny request should route");
        assert_eq!(
            server_requests
                .recv()
                .await
                .expect("deny request should be queued"),
            approval_deny_request
        );
        let deny_pending = timeout(Duration::from_secs(2), adapter_events.recv())
            .await
            .expect("deny pending event should arrive")
            .expect("deny pending event should be present");
        assert_eq!(
            payload_str(&deny_pending.payload, "outcome"),
            Some("pending")
        );
        state
            .codex_gateway
            .append_event(session_id, deny_pending)
            .await
            .expect("deny pending should persist");

        let approval_deny_response = CodexServerResponse {
            id: "approval-deny-full-1".to_string(),
            wire_id: None,
            response_type: "codex.approval.fileChange.respond".to_string(),
            payload: json!({ "decision": "deny" }),
        };
        adapter
            .respond_to_server_request(approval_deny_response.clone())
            .await
            .expect("approval deny should route");
        let approval_denied = timeout(Duration::from_secs(2), adapter_events.recv())
            .await
            .expect("approval denied event should arrive")
            .expect("approval denied event should be present");
        assert_eq!(
            payload_str(&approval_denied.payload, "outcome"),
            Some("denied")
        );
        state
            .codex_gateway
            .append_event(session_id, approval_denied)
            .await
            .expect("approval denial should persist");

        let replay = state
            .codex_gateway
            .replay_events(session_id, Some(2), 20)
            .await
            .expect("session replay should succeed");
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "codex.turn.planUpdated",
                "codex.review.started",
                "codex.review.start.result",
                "codex.approval.commandExecution.request",
                "codex.serverRequest.resolved",
                "codex.approval.fileChange.request",
                "codex.serverRequest.resolved",
            ]
        );

        let recovered = CodexGatewayStore::new(state.config.data_dir.clone())
            .recover_session(session_id)
            .await
            .expect("session should recover after restart");
        assert_eq!(recovered.last_cursor, 9);
        assert!(recovered.threads.contains_key(thread_id));
        assert!(recovered.turns.contains_key(turn_id));
        assert!(recovered.reviews.contains_key("review-request-full-1"));
        assert_eq!(
            recovered
                .approvals
                .get("approval-accept-full-1")
                .and_then(|value| value.get("outcome"))
                .and_then(Value::as_str),
            Some("accepted")
        );
        assert_eq!(
            recovered
                .approvals
                .get("approval-deny-full-1")
                .and_then(|value| value.get("outcome"))
                .and_then(Value::as_str),
            Some("denied")
        );
        assert!(recovered.plan_states.contains_key(plan_id));
        assert_eq!(
            recovered
                .plan_states
                .get(plan_id)
                .and_then(|plan| plan.turn_id.as_deref()),
            Some(turn_id)
        );
        assert_eq!(
            transport.sent().await,
            vec![
                CodexOutboundMessage::Request(review_request),
                CodexOutboundMessage::ServerResponse(approval_accept_response),
                CodexOutboundMessage::ServerResponse(approval_deny_response),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_recovery_fails_on_corrupted_gateway_cursor_gap() {
        let state = test_state().await;
        let session_id = "cdxs_full_flow_failure";

        state
            .codex_gateway
            .append_event(
                session_id,
                CodexGatewayEvent::new(
                    "codex.thread.started",
                    json!({ "threadId": "thread-failure-1" }),
                ),
            )
            .await
            .expect("thread event should persist before corruption");

        let events_path = state
            .config
            .data_dir
            .join("codex_gateway")
            .join("sessions")
            .join(session_id)
            .join("events.jsonl");
        tokio::fs::create_dir_all(events_path.parent().expect("session dir"))
            .await
            .unwrap();
        tokio::fs::write(
            &events_path,
            concat!(
                "{\"session_id\":\"cdxs_full_flow_failure\",\"event_id\":\"evt_1\",\"cursor\":1,\"time\":\"2026-05-07T00:00:00Z\",\"type\":\"codex.thread.started\",\"codex_thread_id\":\"thread-failure-1\",\"payload\":{\"threadId\":\"thread-failure-1\"}}\n",
                "{\"session_id\":\"cdxs_full_flow_failure\",\"event_id\":\"evt_2\",\"cursor\":3,\"time\":\"2026-05-07T00:00:01Z\",\"type\":\"codex.turn.started\",\"codex_thread_id\":\"thread-failure-1\",\"codex_turn_id\":\"turn-failure-1\",\"payload\":{\"threadId\":\"thread-failure-1\",\"turnId\":\"turn-failure-1\"}}\n"
            ),
        )
        .await
        .expect("corrupted gateway events should be written");

        let error = CodexGatewayStore::new(state.config.data_dir.clone())
            .recover_session(session_id)
            .await
            .expect_err("cursor gap should fail recovery");
        assert_eq!(error.code(), "INVALID_REQUEST");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloud_task_success_events_require_upstream_invocation_record() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let auth = test_auth();

        let create_error = dispatch(
            ClientMessage {
                id: "cloud-create-1".to_owned(),
                kind: ClientMessageKind::CodexCloudTaskCreate(CodexCloudTaskCreateRequest {
                    codex_session_id: "cdxs_cloud".to_owned(),
                    tenant_id: "local".to_owned(),
                    env_id: "env-1".to_owned(),
                    prompt: "Implement cloud task lifecycle".to_owned(),
                    git_ref: "main".to_owned(),
                    qa_mode: true,
                    best_of_n: 3,
                }),
            },
            &state,
            Some(&auth),
        )
        .await
        .expect_err("cloud create must fail closed until HTTP adapter records upstream invocation");
        assert_eq!(create_error.code(), "UNSUPPORTED");
        assert!(create_error
            .to_string()
            .contains("HTTP adapter invocation proof"));

        let apply_error = dispatch(
            ClientMessage {
                id: "cloud-apply-1".to_owned(),
                kind: ClientMessageKind::CodexCloudTaskApply(CodexCloudTaskApplyRequest {
                    codex_session_id: "cdxs_cloud".to_owned(),
                    tenant_id: "local".to_owned(),
                    task_id: "task-1".to_owned(),
                    diff_override: Some("diff --git a/src/lib.rs b/src/lib.rs\n".to_owned()),
                    turn_id: Some("turn-alt".to_owned()),
                    attempt_placement: Some(1),
                }),
            },
            &state,
            Some(&auth),
        )
        .await
        .expect_err("cloud apply must fail closed until HTTP adapter records upstream invocation");
        assert_eq!(apply_error.code(), "UNSUPPORTED");
        assert!(apply_error
            .to_string()
            .contains("HTTP adapter invocation proof"));

        let mut audit_actions = Vec::new();
        timeout(Duration::from_secs(2), async {
            while audit_actions.len() < 2 {
                let event = events.recv().await.expect("receive event");
                if event.event_type == "codex.audit" {
                    audit_actions.push(payload_str(&event.payload, "action").map(str::to_owned));
                } else if event.event_type.starts_with("codex.cloudTask.") {
                    panic!(
                        "cloud placeholder emitted success event {}",
                        event.event_type
                    );
                }
            }
        })
        .await
        .expect("cloud audit events should be published without success events");
        assert_eq!(
            audit_actions,
            vec![
                Some("codex.cloudTask.create".to_owned()),
                Some("codex.cloudTask.apply".to_owned())
            ]
        );

        let replay = state
            .codex_gateway
            .replay_events("cdxs_cloud", None, 10)
            .await
            .unwrap();
        assert!(replay.events.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_success_events_require_upstream_invocation_record() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let auth = test_auth();

        let error = dispatch(
            ClientMessage {
                id: "mcp-tool-1".to_owned(),
                kind: ClientMessageKind::CodexMcpToolCall(CodexLifecycleRequest {
                    codex_session_id: "cdxs_mcp".to_owned(),
                    tenant_id: "local".to_owned(),
                    payload: json!({
                        "threadId": "thread-1",
                        "server": "filesystem",
                        "tool": "read_file",
                        "arguments": { "path": "README.md" }
                    }),
                }),
            },
            &state,
            Some(&auth),
        )
        .await
        .expect_err("MCP tool call must fail closed until app-server invocation is recorded");
        assert_eq!(error.code(), "UNSUPPORTED");
        assert!(error.to_string().contains("invocation proof"));

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("mcp-tool-1")
        })
        .await;
        assert_eq!(
            payload_str(&audit.payload, "action"),
            Some("codex.mcp.tool.call")
        );
        assert_eq!(payload_str(&audit.payload, "decision"), Some("allow"));

        let no_success = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                let event = events.recv().await.expect("receive event");
                if event.event_type.starts_with("codex.mcp.") {
                    return event.event_type;
                }
            }
        })
        .await;
        assert!(
            no_success.is_err(),
            "MCP placeholder must not emit success events"
        );

        let recovered = state
            .codex_gateway
            .recover_session("cdxs_mcp")
            .await
            .unwrap();
        assert_eq!(recovered.last_cursor, 0);
        assert!(recovered.mcp_tool_calls.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tenant_mismatch_is_denied_and_audited() {
        let state = test_state().await;
        let mut events = state.events.subscribe();
        let message = codex_control_message("tenant-1", "cdxs_wrong_tenant", "other");
        let auth = AuthContext {
            principal_id: "gateway-token".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "configured-token".to_owned(),
        };

        let error = dispatch(message, &state, Some(&auth))
            .await
            .expect_err("tenant mismatch must fail");
        assert_eq!(error.code(), "UNAUTHORIZED");

        let audit = wait_for_event(&mut events, |event| {
            event.event_type == "codex.audit"
                && payload_str(&event.payload, "request_id") == Some("tenant-1")
        })
        .await;
        assert_eq!(payload_str(&audit.payload, "decision"), Some("deny"));
        assert_eq!(
            payload_str(&audit.payload, "reason_code"),
            Some("TENANT_MISMATCH")
        );
    }

    fn test_auth() -> AuthContext {
        AuthContext {
            principal_id: "gateway-token".to_owned(),
            tenant_id: "local".to_owned(),
            token_id: "configured-token".to_owned(),
        }
    }

    fn codex_lifecycle_message(
        id: &str,
        variant: fn(CodexLifecycleRequest) -> ClientMessageKind,
        payload: Value,
    ) -> ClientMessage {
        ClientMessage {
            id: id.to_owned(),
            kind: variant(CodexLifecycleRequest {
                codex_session_id: "cdxs_lifecycle".to_owned(),
                tenant_id: "local".to_owned(),
                payload,
            }),
        }
    }

    fn local_start_message(
        id: &str,
        codex_session_id: &str,
        cwd: &std::path::Path,
    ) -> ClientMessage {
        ClientMessage {
            id: id.to_owned(),
            kind: ClientMessageKind::CodexLocalStart(CodexLocalStartRequest {
                codex_session_id: codex_session_id.to_owned(),
                tenant_id: "local".to_owned(),
                cwd: cwd.to_string_lossy().to_string(),
                model: None,
                approval_policy: None,
                sandbox_mode: None,
                config_overrides: Value::Object(Default::default()),
            }),
        }
    }

    fn payload_u64(payload: &Value, key: &str) -> Option<u64> {
        payload.get(key).and_then(Value::as_u64)
    }

    async fn test_state() -> AppState {
        AppState::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: crate::config::PairingEncryption::default(),
            data_dir: unique_tmp_dir("todex-codex-auth-test-data"),
            workspace_root: unique_tmp_dir("todex-codex-auth-test-workspaces"),
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("todex_gw_test".to_owned()),
            },
        })
        .await
        .expect("create app state")
    }

    fn codex_control_message(id: &str, codex_session_id: &str, tenant_id: &str) -> ClientMessage {
        ClientMessage {
            id: id.to_owned(),
            kind: ClientMessageKind::CodexGatewayControl(CodexGatewayControlRequest {
                codex_session_id: codex_session_id.to_owned(),
                tenant_id: tenant_id.to_owned(),
                action: CodexGatewayAction::Control,
            }),
        }
    }

    async fn wait_for_event<F>(
        rx: &mut tokio::sync::broadcast::Receiver<EventRecord>,
        predicate: F,
    ) -> EventRecord
    where
        F: Fn(&EventRecord) -> bool,
    {
        timeout(Duration::from_secs(2), async {
            loop {
                let event = rx.recv().await.expect("receive event");
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .expect("event should be published")
    }

    fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
        payload.get(key).and_then(Value::as_str)
    }

    fn event_field_str<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
        event
            .payload
            .get("data")
            .and_then(|data| data.get(key))
            .and_then(Value::as_str)
            .or_else(|| event.payload.get(key).and_then(Value::as_str))
    }

    fn unique_tmp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{}-{}", prefix, uuid::Uuid::new_v4().simple()))
    }

    async fn write_fake_codex_binary(root: &std::path::Path) -> PathBuf {
        let binary = root.join("codex-fake");
        let wire_log = root.join("wire.log");
        tokio::fs::create_dir_all(root).await.unwrap();
        tokio::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '{{\"type\":\"codex.control.ready\"}}\\n'\nwhile read line; do\nprintf '%s\\n' \"$line\" >> '{}'\ncase \"$line\" in\n*'\"method\":\"turn/start\"'*|*'\"method\":\"turn/steer\"'*)\nprintf '{{\"type\":\"codex.turn.started\",\"payload\":{{\"requestId\":\"local-turn-1\",\"threadId\":\"thread_1\",\"turnId\":\"turn_1\",\"lifecycleState\":\"busy\"}}}}\\n'\nprintf '{{\"type\":\"codex.item.agentMessage.delta\",\"payload\":{{\"requestId\":\"local-turn-1\",\"threadId\":\"thread_1\",\"turnId\":\"turn_1\",\"itemId\":\"item_1\",\"delta\":\"hello\"}}}}\\n'\nprintf '{{\"type\":\"codex.turn.completed\",\"payload\":{{\"requestId\":\"local-turn-1\",\"threadId\":\"thread_1\",\"turnId\":\"turn_1\",\"lifecycleState\":\"ready\"}}}}\\n'\n;;\n*'\"method\":\"turn/interrupt\"'*)\nprintf '{{\"type\":\"codex.turn.interrupted\",\"payload\":{{\"requestId\":\"local-interrupt-1\",\"threadId\":\"thread_1\",\"turnId\":\"turn_1\",\"status\":\"interrupted\"}}}}\\n'\n;;\nesac\ndone\n",
                wire_log.display()
            ),
        )
        .await
        .unwrap();
        let mut permissions = tokio::fs::metadata(&binary).await.unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        tokio::fs::set_permissions(&binary, permissions)
            .await
            .unwrap();
        binary
    }

    async fn write_requesting_fake_codex_binary(root: &std::path::Path) -> PathBuf {
        let binary = root.join("codex-requesting-fake");
        let wire_log = root.join("wire.log");
        tokio::fs::create_dir_all(root).await.unwrap();
        tokio::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '{{\"type\":\"codex.control.ready\"}}\\n'\nwhile read line; do\nprintf '%s\\n' \"$line\" >> '{}'\ncase \"$line\" in\n*'\"method\":\"turn/start\"'*)\nprintf '{{\"id\":\"permission-1\",\"method\":\"item/permissions/requestApproval\",\"params\":{{\"threadId\":\"thread_1\",\"turnId\":\"turn_1\",\"itemId\":\"item_1\",\"command\":\"cargo test\"}}}}\\n'\n;;\n*'\"method\":\"thread/start\"'*)\nprintf '{{\"id\":\"question-1\",\"method\":\"item/tool/requestUserInput\",\"params\":{{\"threadId\":\"thread_2\",\"turnId\":\"turn_2\",\"itemId\":\"item_2\",\"questions\":[{{\"id\":\"confirm_path\",\"header\":\"Path\",\"question\":\"Continue?\",\"isOther\":false,\"isSecret\":false,\"options\":[{{\"id\":\"yes\",\"text\":\"yes\"}}]}}]}}}}\\n'\n;;\nesac\ndone\n",
                wire_log.display()
            ),
        )
        .await
        .unwrap();
        let mut permissions = tokio::fs::metadata(&binary).await.unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        tokio::fs::set_permissions(&binary, permissions)
            .await
            .unwrap();
        binary
    }

    async fn write_holding_fake_codex_binary(root: &std::path::Path) -> PathBuf {
        let binary = root.join("codex-holding-fake");
        tokio::fs::create_dir_all(root).await.unwrap();
        tokio::fs::write(
            &binary,
            "#!/bin/sh\nprintf '{\"type\":\"codex.control.ready\"}\\n'\nwhile read line; do sleep 10; done\n",
        )
        .await
        .unwrap();
        let mut permissions = tokio::fs::metadata(&binary).await.unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        tokio::fs::set_permissions(&binary, permissions)
            .await
            .unwrap();
        binary
    }
}

fn error_event(
    error: AppError,
    workspace_id: Option<String>,
    window_id: Option<String>,
    pane_id: Option<String>,
) -> EventRecord {
    EventRecord::new(
        "error",
        workspace_id,
        window_id,
        pane_id,
        json!({ "code": error.code(), "message": error.to_string() }),
    )
}
