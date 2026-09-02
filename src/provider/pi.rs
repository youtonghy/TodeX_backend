use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::watch;
use tokio::time::Duration;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, PermissionOutcome,
    ProviderCapabilities, ProviderCommandDescriptor, ProviderDescriptor, ProviderDriver,
};

pub struct PiDriver {
    binary: String,
}

impl PiDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.pi_bin.clone(),
        }
    }
}

#[async_trait]
impl ProviderDriver for PiDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::Pi,
            display_name: "Pi",
            available,
            unavailable_reason: (!available)
                .then(|| format!("executable '{}' was not found", self.binary)),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                native_resume: true,
                cancel: true,
                // Pi RPC exposes extension dialogs, but not a universal pre-tool approval API.
                permissions: false,
                tool_events: true,
                native_skills: true,
                native_mcp: false,
                managed_mcp: true,
                model_selection: true,
            },
            models: Vec::new(),
        }
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<super::types::ProviderModelDescriptor>, AppError> {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec![
            "--mode".to_owned(),
            "rpc".to_owned(),
            "--no-session".to_owned(),
            "--approve".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        process
            .send(&json!({"id":"models","type":"get_available_models"}))
            .await?;
        let response = loop {
            let Some(value) = process.read().await? else {
                return Err(AppError::ProviderUnavailable(
                    "Pi RPC process closed stdout".to_owned(),
                ));
            };
            if value.get("id").and_then(Value::as_str) == Some("models") {
                break value;
            }
        };
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            process.terminate().await;
            return Err(pi_response_error(&response, "get_available_models"));
        }
        process
            .send(&json!({"id":"state","type":"get_state"}))
            .await?;
        let state = loop {
            let Some(value) = process.read().await? else {
                return Err(AppError::ProviderUnavailable(
                    "Pi RPC process closed stdout".to_owned(),
                ));
            };
            if value.get("id").and_then(Value::as_str) == Some("state") {
                break value;
            }
        };
        process.terminate().await;
        if state.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(pi_response_error(&state, "get_state"));
        }
        Ok(parse_pi_models(&response, &state))
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec![
            "--mode".to_owned(),
            "rpc".to_owned(),
            "--no-session".to_owned(),
            "--approve".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        process
            .send(&json!({"id":"commands","type":"get_commands"}))
            .await?;
        let response = loop {
            let Some(value) = process.read().await? else {
                return Err(AppError::ProviderUnavailable(
                    "Pi RPC process closed stdout".to_owned(),
                ));
            };
            if value.get("id").and_then(Value::as_str) == Some("commands") {
                break value;
            }
        };
        process.terminate().await;
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(pi_response_error(&response, "get_commands"));
        }
        Ok(response
            .pointer("/data/commands")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)?
                            .trim()
                            .trim_start_matches('/');
                        if name.is_empty() {
                            return None;
                        }
                        Some(ProviderCommandDescriptor {
                            name: name.to_owned(),
                            description: item
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            source: item
                                .get("source")
                                .and_then(Value::as_str)
                                .unwrap_or("extension")
                                .to_owned(),
                            source_info: item.get("sourceInfo").cloned(),
                            invocation: "prompt".to_owned(),
                            argument_hint: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        let native_session_id = context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| context.manifest.id.clone());
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "--mode".to_owned(),
            "rpc".to_owned(),
            "--session-id".to_owned(),
            native_session_id.clone(),
            "--approve".to_owned(),
        ];
        if let Some(model) = &prompt.model {
            spec.args.push("--model".to_owned());
            spec.args.push(model.clone());
        }
        if let Some(effort) = &prompt.reasoning_effort {
            spec.args.push("--thinking".to_owned());
            spec.args.push(effort.clone());
        }

        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = run_pi_turn(
            &mut process,
            context,
            prompt,
            native_session_id,
            &sink,
            &mut cancel,
        )
        .await;
        process.terminate().await;
        result
    }
}

fn parse_pi_models(response: &Value, state: &Value) -> Vec<super::types::ProviderModelDescriptor> {
    let default_model = state.pointer("/data/model").and_then(pi_model_id);
    let default_effort = state.pointer("/data/thinkingLevel").and_then(Value::as_str);
    response
        .pointer("/data/models")
        .or_else(|| response.get("data"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = pi_model_id(item)?;
            let is_default = default_model.as_deref() == Some(id.as_str())
                || item
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let efforts = pi_supported_thinking_levels(item);
            Some(super::types::ProviderModelDescriptor {
                id,
                display_name: item
                    .get("name")
                    .or_else(|| item.get("displayName"))
                    .and_then(Value::as_str)
                    .unwrap_or("Pi model")
                    .to_owned(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                is_default,
                supported_reasoning_efforts: efforts.clone(),
                default_reasoning_effort: is_default
                    .then(|| {
                        default_effort.filter(|effort| efforts.iter().any(|item| item == effort))
                    })
                    .flatten()
                    .map(str::to_owned),
                context_window: item.get("contextWindow").and_then(Value::as_u64),
            })
        })
        .collect()
}

fn pi_model_id(item: &Value) -> Option<String> {
    let model_id = item
        .get("id")
        .or_else(|| item.get("modelId"))
        .and_then(Value::as_str)?;
    Some(
        item.get("provider")
            .and_then(Value::as_str)
            .map(|provider| format!("{provider}/{model_id}"))
            .unwrap_or_else(|| model_id.to_owned()),
    )
}

fn pi_supported_thinking_levels(item: &Value) -> Vec<String> {
    if item.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return vec!["off".to_owned()];
    }
    let map = item.get("thinkingLevelMap").and_then(Value::as_object);
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .filter(|level| {
            let mapped = map.and_then(|values| values.get(*level));
            !mapped.is_some_and(Value::is_null)
                && (!matches!(*level, "xhigh" | "max") || mapped.is_some())
        })
        .map(str::to_owned)
        .collect()
}

async fn run_pi_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    native_session_id: String,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<DriverTurnResult, AppError> {
    process
        .send(&json!({ "id": "state", "type": "get_state" }))
        .await?;
    let state = wait_for_response(process, "state", sink, cancel).await?;
    if state.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(pi_response_error(&state, "get_state"));
    }

    let mut provider_state = context.provider_state;
    provider_state.native_session_id = Some(native_session_id.clone());
    provider_state.recoverable = true;
    provider_state.last_error = None;
    sink.save_provider_state(provider_state).await?;

    let mut request = json!({
        "id": prompt.turn_id,
        "type": "prompt",
        "message": prompt.text,
    });
    let images = prompt
        .content
        .iter()
        .filter_map(|content| match content {
            super::types::DriverPromptContent::Image {
                data, mime_type, ..
            } => Some(json!({
                "type": "image",
                "data": data,
                "mimeType": mime_type,
            })),
            super::types::DriverPromptContent::File { .. } => None,
        })
        .collect::<Vec<_>>();
    if !images.is_empty() {
        request["images"] = Value::Array(images);
    }
    process.send(&request).await?;
    let accepted = wait_for_response(process, &prompt.turn_id, sink, cancel).await?;
    if accepted.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(pi_response_error(&accepted, "prompt"));
    }

    let mut stop_reason = "completed".to_owned();
    loop {
        let message = tokio::select! {
            message = process.read() => message?,
            changed = cancel.changed() => {
                let _ = changed;
                wait_for_pi_abort(process, sink).await?;
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "Pi RPC process closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        match message.get("type").and_then(Value::as_str) {
            Some("agent_settled") => {
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason,
                    cancelled: false,
                });
            }
            Some("message_end") => {
                if let Some(reason) = message
                    .pointer("/message/stopReason")
                    .and_then(Value::as_str)
                {
                    stop_reason = reason.to_owned();
                }
                sink.emit(
                    "message.completed",
                    json!({ "provider": "pi", "message": message.get("message") }),
                )
                .await?;
            }
            Some("message_update") => {
                let delta = message
                    .get("assistantMessageEvent")
                    .cloned()
                    .unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                let event_type = if delta_type.starts_with("thinking_") {
                    "thought.delta"
                } else if delta_type.starts_with("toolcall_") {
                    "tool.updated"
                } else {
                    "message.delta"
                };
                sink.emit(
                    event_type,
                    json!({ "provider": "pi", "role": "assistant", "delta": delta, "usage": message.get("usage") }),
                )
                .await?;
            }
            Some("tool_execution_start") => {
                sink.emit("tool.started", pi_tool_payload(&message)).await?;
            }
            Some("tool_execution_update") => {
                sink.emit("tool.updated", pi_tool_payload(&message)).await?;
            }
            Some("tool_execution_end") => {
                sink.emit("tool.completed", pi_tool_payload(&message))
                    .await?;
            }
            Some("extension_ui_request") => {
                handle_extension_ui(process, message, sink, cancel).await?;
            }
            Some("response" | "agent_start" | "agent_end" | "turn_start" | "turn_end") => {}
            Some(event_type) => {
                sink.emit(
                    "provider.event",
                    json!({ "provider": "pi", "providerMethod": event_type, "metadata": message }),
                )
                .await?;
            }
            None => {}
        }
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
                return Err(AppError::TurnCancelled);
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "Pi RPC process closed stdout").await);
        };
        if message.get("id").and_then(Value::as_str) == Some(request_id)
            && message.get("type").and_then(Value::as_str) == Some("response")
        {
            return Ok(message);
        }
        if message.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
            handle_extension_ui(process, message, sink, cancel).await?;
        }
    }
}

async fn handle_extension_ui(
    process: &mut JsonLineProcess,
    request: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let Some(id) = request.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    if !matches!(method, "select" | "confirm" | "input" | "editor") {
        sink.emit(
            "provider.event",
            json!({ "provider": "pi", "providerMethod": "extension_ui_request", "metadata": request }),
        )
        .await?;
        // Pi documents notify/setStatus/setWidget/setTitle/set_editor_text as
        // fire-and-forget requests. Sending a response for these messages is
        // itself a protocol violation and can stall the agent process.
        return Ok(());
    }
    let decision = sink
        .request_permission(
            id.to_owned(),
            "extension_ui",
            request
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Pi requests input"),
            request.clone(),
            json!([
                { "id": "answer", "kind": "answer", "name": "Respond" },
                { "id": "reject_once", "kind": "reject_once", "name": "Cancel" }
            ]),
            cancel,
        )
        .await?;
    let response = match decision.outcome {
        PermissionOutcome::RejectOnce | PermissionOutcome::RejectAlways => {
            json!({ "type": "extension_ui_response", "id": id, "cancelled": true })
        }
        _ if method == "confirm" => json!({
            "type": "extension_ui_response",
            "id": id,
            "confirmed": decision.data.as_ref().and_then(|data| data.get("confirmed")).and_then(Value::as_bool).unwrap_or(true),
        }),
        _ => json!({
            "type": "extension_ui_response",
            "id": id,
            "value": decision.data.as_ref().and_then(|data| data.get("value")).and_then(Value::as_str).unwrap_or_default(),
        }),
    };
    process.send(&response).await
}

async fn wait_for_pi_abort(
    process: &mut JsonLineProcess,
    sink: &DriverEventSink,
) -> Result<(), AppError> {
    process
        .send(&json!({ "id": "abort", "type": "abort" }))
        .await?;
    let (_cancel_tx, mut no_cancel) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Some(message) = process.read().await? else {
                return Err(
                    provider_exit_error(process, "Pi RPC process closed during abort").await,
                );
            };
            if message.get("id").and_then(Value::as_str) == Some("abort")
                && message.get("type").and_then(Value::as_str) == Some("response")
            {
                return if message.get("success").and_then(Value::as_bool) == Some(true) {
                    Ok(())
                } else {
                    Err(pi_response_error(&message, "abort"))
                };
            }
            if message.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
                handle_extension_ui(process, message, sink, &mut no_cancel).await?;
            }
        }
    })
    .await
    .map_err(|_| AppError::ProviderUnavailable("Pi abort timed out".to_owned()))?
}

fn pi_tool_payload(message: &Value) -> Value {
    json!({
        "provider": "pi",
        "toolCallId": message.get("toolCallId"),
        "toolName": message.get("toolName"),
        "arguments": message.get("args"),
        "partialResult": message.get("partialResult"),
        "result": message.get("result"),
        "isError": message.get("isError"),
    })
}

fn pi_response_error(response: &Value, command: &str) -> AppError {
    let error = response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("Pi rejected the command");
    AppError::ProviderUnavailable(format!("Pi {command} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_specific_thinking_levels_and_default() {
        let response = json!({"data":{"models":[
            {"provider":"zai","id":"glm-5.3","reasoning":true,"thinkingLevelMap":{"off":null,"xhigh":"xhigh","max":"max"}},
            {"provider":"retoo","id":"deepseek-v4","reasoning":true,"thinkingLevelMap":{"off":null,"minimal":null,"low":null,"medium":null,"high":null,"xhigh":null,"max":"max"}},
            {"provider":"plain","id":"chat","reasoning":false}
        ]}});
        let state =
            json!({"data":{"model":{"provider":"zai","id":"glm-5.3"},"thinkingLevel":"high"}});
        let models = parse_pi_models(&response, &state);
        assert_eq!(
            models[0].supported_reasoning_efforts,
            ["minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert!(models[0].is_default);
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(models[1].supported_reasoning_efforts, ["max"]);
        assert_eq!(models[2].supported_reasoning_efforts, ["off"]);
    }

    #[test]
    fn omits_an_invalid_default_thinking_level() {
        let response = json!({"data":{"models":[{"provider":"retoo","id":"deepseek-v4","reasoning":true,"thinkingLevelMap":{"off":null,"minimal":null,"low":null,"medium":null,"high":null,"xhigh":null,"max":"max"}}]}});
        let state = json!({"data":{"model":{"provider":"retoo","id":"deepseek-v4"},"thinkingLevel":"high"}});
        assert_eq!(
            parse_pi_models(&response, &state)[0].default_reasoning_effort,
            None
        );
    }
}
