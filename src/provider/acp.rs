use std::collections::BTreeMap;

use agent_client_protocol::schema::{
    v1::{
        CancelNotification, ContentBlock, Implementation, InitializeRequest, InitializeResponse,
        LoadSessionRequest, NewSessionRequest, NewSessionResponse, PermissionOptionKind,
        PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, TextContent,
    },
    ProtocolVersion,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::{AcpProfileConfig, AgentConfig};
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, PermissionOutcome,
    ProviderCapabilities, ProviderDescriptor, ProviderDriver,
};

pub struct AcpDriver {
    profiles: BTreeMap<String, AcpProfileConfig>,
}

impl AcpDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            profiles: config.acp_profiles.clone(),
        }
    }

    fn profile<'a>(&'a self, context: &DriverContext) -> Result<&'a AcpProfileConfig, AppError> {
        let profile = context
            .manifest
            .provider_profile
            .as_deref()
            .ok_or_else(|| {
                AppError::InvalidRequest("ACP conversation requires a profile".to_owned())
            })?;
        self.profiles.get(profile).ok_or_else(|| {
            AppError::ProviderUnavailable(format!("ACP profile '{profile}' is not configured"))
        })
    }
}

#[async_trait]
impl ProviderDriver for AcpDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let profiles = self.profiles.keys().cloned().collect::<Vec<_>>();
        let available = self
            .profiles
            .values()
            .any(|profile| executable_available(&profile.command));
        ProviderDescriptor {
            id: ProviderKind::Acp,
            display_name: "ACP",
            available,
            unavailable_reason: (!available).then(|| {
                if self.profiles.is_empty() {
                    "no ACP profiles are configured".to_owned()
                } else {
                    "no configured ACP profile executable was found".to_owned()
                }
            }),
            profiles,
            capabilities: ProviderCapabilities {
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
            },
        }
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<DriverTurnResult, AppError> {
        let profile = self.profile(&context)?;
        let mut spec = CommandSpec::new(&profile.command, &context.manifest.workspace);
        spec.args = profile.args.clone();
        spec.env = profile
            .env
            .iter()
            .filter(|(key, _)| !key.starts_with("TODEX_AGENTD_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut process = JsonLineProcess::spawn(&spec).await?;
        let result = run_acp_turn(&mut process, context, prompt, &sink, &mut cancel).await;
        process.terminate().await;
        result
    }
}

async fn run_acp_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<DriverTurnResult, AppError> {
    let initialize = InitializeRequest::new(ProtocolVersion::V1).client_info(
        Implementation::new("todex-agentd", env!("CARGO_PKG_VERSION")).title("TodeX 2.0"),
    );
    send_request(process, "initialize", "initialize", initialize).await?;
    let initialize: InitializeResponse =
        serde_json::from_value(wait_for_response(process, "initialize", sink, cancel).await?)
            .map_err(|error| {
                AppError::InvalidRequest(format!("invalid ACP initialize response: {error}"))
            })?;
    if initialize.protocol_version != ProtocolVersion::V1 {
        return Err(AppError::Unsupported(format!(
            "ACP agent negotiated unsupported protocol version {}",
            initialize.protocol_version
        )));
    }

    let native_session_id = match context.provider_state.native_session_id.clone() {
        Some(session_id) => {
            if !initialize.agent_capabilities.load_session {
                return Err(AppError::Unsupported(
                    "ACP agent does not support session/load; historical prompts will not be replayed"
                        .to_owned(),
                ));
            }
            send_request(
                process,
                "session",
                "session/load",
                LoadSessionRequest::new(session_id.clone(), context.manifest.workspace.clone()),
            )
            .await?;
            wait_for_response(process, "session", sink, cancel).await?;
            session_id
        }
        None => {
            send_request(
                process,
                "session",
                "session/new",
                NewSessionRequest::new(context.manifest.workspace.clone()),
            )
            .await?;
            let response: NewSessionResponse =
                serde_json::from_value(wait_for_response(process, "session", sink, cancel).await?)
                    .map_err(|error| {
                        AppError::InvalidRequest(format!(
                            "invalid ACP session/new response: {error}"
                        ))
                    })?;
            response.session_id.0.to_string()
        }
    };

    let mut state = context.provider_state;
    state.native_session_id = Some(native_session_id.clone());
    state.recoverable = initialize.agent_capabilities.load_session;
    state.last_error = None;
    sink.save_provider_state(state).await?;

    let request = PromptRequest::new(
        native_session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt.text))],
    );
    send_request(process, &prompt.turn_id, "session/prompt", request).await?;
    loop {
        let message = tokio::select! {
            message = process.read() => message?,
            changed = cancel.changed() => {
                let _ = changed;
                send_notification(
                    process,
                    "session/cancel",
                    CancelNotification::new(native_session_id.clone()),
                ).await?;
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "ACP agent closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        if jsonrpc_id(&message) == Some(prompt.turn_id.as_str()) {
            if let Some(error) = message.get("error") {
                return Err(AppError::ProviderUnavailable(format!(
                    "ACP prompt failed: {}",
                    safe_error_text(error)
                )));
            }
            let response: PromptResponse =
                serde_json::from_value(message.get("result").cloned().unwrap_or(Value::Null))
                    .map_err(|error| {
                        AppError::InvalidRequest(format!("invalid ACP prompt response: {error}"))
                    })?;
            return Ok(DriverTurnResult {
                native_session_id: Some(native_session_id),
                stop_reason: serde_json::to_value(response.stop_reason)?
                    .as_str()
                    .unwrap_or("completed")
                    .to_owned(),
                cancelled: false,
            });
        }
        handle_acp_message(process, message, sink, cancel).await?;
    }
}

async fn wait_for_response(
    process: &mut JsonLineProcess,
    request_id: &str,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Value, AppError> {
    loop {
        let message = tokio::select! {
            message = process.read() => message?,
            changed = cancel.changed() => {
                let _ = changed;
                return Err(AppError::Conflict("turn was cancelled".to_owned()));
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "ACP agent closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        if jsonrpc_id(&message) == Some(request_id) {
            if let Some(error) = message.get("error") {
                return Err(AppError::ProviderUnavailable(format!(
                    "ACP request {request_id} failed: {}",
                    safe_error_text(error)
                )));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
        handle_acp_message(process, message, sink, cancel).await?;
    }
}

async fn handle_acp_message(
    process: &mut JsonLineProcess,
    message: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if method == "session/request_permission" {
        let request_id = message.get("id").cloned().ok_or_else(|| {
            AppError::InvalidRequest("ACP permission request is missing an id".to_owned())
        })?;
        let request: RequestPermissionRequest =
            serde_json::from_value(params.clone()).map_err(|error| {
                AppError::InvalidRequest(format!("invalid ACP permission request: {error}"))
            })?;
        let options = serde_json::to_value(&request.options)?;
        let decision = sink
            .request_permission(
                request_id.to_string(),
                "tool",
                "Allow ACP tool call?",
                serde_json::to_value(&request.tool_call)?,
                options,
                cancel,
            )
            .await?;
        let selected = select_acp_option(&request, &decision)?;
        let response = RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(selected),
        ));
        process
            .send(&json!({ "jsonrpc": "2.0", "id": request_id, "result": response }))
            .await?;
        return Ok(());
    }
    if method == "session/update" {
        let notification: SessionNotification =
            serde_json::from_value(params).map_err(|error| {
                AppError::InvalidRequest(format!("invalid ACP session update: {error}"))
            })?;
        let update = serde_json::to_value(notification.update)?;
        let update_type = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let (event_type, payload) = match update_type {
            "agent_message_chunk" => (
                "message.delta",
                json!({ "provider": "acp", "role": "assistant", "content": update.get("content") }),
            ),
            "agent_thought_chunk" => (
                "thought.delta",
                json!({ "provider": "acp", "content": update.get("content") }),
            ),
            "tool_call" => ("tool.started", json!({ "provider": "acp", "tool": update })),
            "tool_call_update" => ("tool.updated", json!({ "provider": "acp", "tool": update })),
            "plan" => ("plan.updated", json!({ "provider": "acp", "plan": update })),
            "user_message_chunk" => return Ok(()),
            _ => (
                "provider.event",
                json!({ "provider": "acp", "providerMethod": method, "metadata": update }),
            ),
        };
        sink.emit(event_type, payload).await?;
        return Ok(());
    }

    if let Some(request_id) = message.get("id") {
        process
            .send(&json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": { "code": -32601, "message": "client capability is not supported" }
            }))
            .await?;
    } else {
        sink.emit(
            "provider.event",
            json!({ "provider": "acp", "providerMethod": method, "metadata": params }),
        )
        .await?;
    }
    Ok(())
}

fn select_acp_option(
    request: &RequestPermissionRequest,
    decision: &super::types::PermissionDecision,
) -> Result<String, AppError> {
    if let Some(option_id) = &decision.option_id {
        if request
            .options
            .iter()
            .any(|option| option.option_id.0.as_ref() == option_id)
        {
            return Ok(option_id.clone());
        }
        return Err(AppError::InvalidRequest(
            "selected ACP permission option is not available".to_owned(),
        ));
    }
    let expected_kind = match decision.outcome {
        PermissionOutcome::AllowOnce | PermissionOutcome::Answer => PermissionOptionKind::AllowOnce,
        PermissionOutcome::AllowAlways => PermissionOptionKind::AllowAlways,
        PermissionOutcome::RejectOnce => PermissionOptionKind::RejectOnce,
        PermissionOutcome::RejectAlways => PermissionOptionKind::RejectAlways,
    };
    request
        .options
        .iter()
        .find(|option| option.kind == expected_kind)
        .map(|option| option.option_id.0.to_string())
        .ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "ACP permission request has no option matching {}",
                expected_kind_name(expected_kind)
            ))
        })
}

fn expected_kind_name(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "unknown",
    }
}

async fn send_request(
    process: &mut JsonLineProcess,
    id: &str,
    method: &str,
    params: impl serde::Serialize,
) -> Result<(), AppError> {
    process
        .send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(params)?,
        }))
        .await
}

async fn send_notification(
    process: &mut JsonLineProcess,
    method: &str,
    params: impl serde::Serialize,
) -> Result<(), AppError> {
    process
        .send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params)?,
        }))
        .await
}

fn jsonrpc_id(message: &Value) -> Option<&str> {
    message.get("id").and_then(Value::as_str)
}

fn safe_error_text(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("provider returned an error")
        .chars()
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(options: Value) -> RequestPermissionRequest {
        serde_json::from_value(json!({
            "sessionId": "session-1",
            "toolCall": { "toolCallId": "tool-1" },
            "options": options,
        }))
        .expect("fixture ACP permission request")
    }

    fn decision(outcome: PermissionOutcome) -> super::super::types::PermissionDecision {
        super::super::types::PermissionDecision {
            outcome,
            option_id: None,
            data: None,
        }
    }

    #[test]
    fn permission_selection_preserves_reject_semantics_when_options_are_reordered() {
        let request = request(json!([
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
        ]));
        assert_eq!(
            select_acp_option(&request, &decision(PermissionOutcome::RejectOnce)).unwrap(),
            "reject"
        );
    }

    #[test]
    fn permission_selection_rejects_missing_semantic_option() {
        let request = request(json!([
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" }
        ]));
        assert!(matches!(
            select_acp_option(&request, &decision(PermissionOutcome::RejectOnce)),
            Err(AppError::InvalidRequest(message)) if message.contains("reject_once")
        ));
    }
}
