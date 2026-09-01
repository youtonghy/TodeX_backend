use async_trait::async_trait;
use std::path::Path;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, PermissionOutcome,
    ProviderCommandDescriptor,
    ProviderCapabilities, ProviderDescriptor, ProviderDriver,
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
                native_mcp: true,
                model_selection: true,
            },
            models: Vec::new(),
        }
    }

    async fn discover_models(&self, workspace: &Path) -> Result<Vec<super::types::ProviderModelDescriptor>, AppError> {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec!["--mode".to_owned(), "rpc".to_owned(), "--no-session".to_owned(), "--approve".to_owned()];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        process.send(&json!({"id":"models","type":"get_available_models"})).await?;
        let response = loop {
            let Some(value) = process.read().await? else { return Err(AppError::ProviderUnavailable("Pi RPC process closed stdout".to_owned())); };
            if value.get("id").and_then(Value::as_str) == Some("models") { break value; }
        };
        process.terminate().await;
        Ok(response.pointer("/data/models").or_else(|| response.get("data")).and_then(Value::as_array).into_iter().flatten().filter_map(|item| {
            let model_id = item.get("id").or_else(|| item.get("modelId")).and_then(Value::as_str)?.to_owned();
            let id = item.get("provider").and_then(Value::as_str).map(|provider| format!("{provider}/{model_id}")).unwrap_or(model_id);
            // Match Pi's getSupportedThinkingLevels semantics: ordinary levels are
            // supported when omitted (the provider default), while xhigh/max must
            // be explicitly mapped. A null mapping explicitly disables a level.
            let efforts = if item.get("reasoning").and_then(Value::as_bool) == Some(false) {
                vec!["off".to_owned()]
            } else {
                let map = item.get("thinkingLevelMap").and_then(Value::as_object);
                ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .filter(|level| {
                        let mapped = map.and_then(|values| values.get(*level));
                        if mapped.is_some_and(Value::is_null) {
                            return false;
                        }
                        if matches!(*level, "xhigh" | "max") {
                            mapped.is_some()
                        } else {
                            true
                        }
                    })
                    .map(str::to_owned)
                    .collect()
            };
            Some(super::types::ProviderModelDescriptor { id, display_name: item.get("name").or_else(|| item.get("displayName")).and_then(Value::as_str).unwrap_or("Pi model").to_owned(), description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(), is_default: item.get("isDefault").and_then(Value::as_bool).unwrap_or(false), supported_reasoning_efforts: efforts, context_window: item.get("contextWindow").and_then(Value::as_u64) })
        }).collect())
    }

    async fn discover_commands(&self, workspace: &Path) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec!["--mode".to_owned(), "rpc".to_owned(), "--no-session".to_owned(), "--approve".to_owned()];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        process.send(&json!({"id":"commands","type":"get_commands"})).await?;
        let response = loop {
            let Some(value) = process.read().await? else { return Err(AppError::ProviderUnavailable("Pi RPC process closed stdout".to_owned())); };
            if value.get("id").and_then(Value::as_str) == Some("commands") { break value; }
        };
        process.terminate().await;
        Ok(response.pointer("/data/commands").and_then(Value::as_array).map(|items| items.iter().filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str)?.trim().trim_start_matches('/');
            if name.is_empty() { return None; }
            Some(ProviderCommandDescriptor {
                name: name.to_owned(),
                description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(),
                source: item.get("source").and_then(Value::as_str).unwrap_or("extension").to_owned(),
                invocation: "prompt".to_owned(),
                argument_hint: None,
            })
        }).collect()).unwrap_or_default())
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
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

        let mut process = JsonLineProcess::spawn(&spec).await?;
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

    process
        .send(&json!({
            "id": prompt.turn_id,
            "type": "prompt",
            "message": prompt.text,
        }))
        .await?;
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
                let _ = process.send(&json!({ "id": "abort", "type": "abort" })).await;
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
                return Err(AppError::Conflict("turn was cancelled".to_owned()));
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
            "value": decision.data.as_ref().and_then(|data| data.get("value")).cloned().unwrap_or(Value::Null),
        }),
    };
    process.send(&response).await
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
