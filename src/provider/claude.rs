use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use tokio::sync::watch;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, PermissionOutcome,
    ProviderCapabilities, ProviderDescriptor, ProviderDriver,
};

pub struct ClaudeDriver {
    binary: String,
}

fn claude_model_aliases() -> Vec<super::types::ProviderModelDescriptor> {
    ["default", "sonnet", "opus", "haiku"]
        .into_iter()
        .map(|id| super::types::ProviderModelDescriptor {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: "Claude Code model alias".to_owned(),
            is_default: id == "default",
            supported_reasoning_efforts: ["low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            default_reasoning_effort: None,
            context_window: None,
        })
        .collect()
}

impl ClaudeDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.claude_bin.clone(),
        }
    }
}

#[async_trait]
impl ProviderDriver for ClaudeDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::ClaudeCode,
            display_name: "Claude Code",
            available,
            unavailable_reason: (!available)
                .then(|| format!("executable '{}' was not found", self.binary)),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: true,
                model_selection: true,
                image_input: ProviderKind::ClaudeCode.supports_image_input(),
            },
            models: claude_model_aliases(),
        }
    }

    async fn discover_models(
        &self,
        _workspace: &Path,
    ) -> Result<Vec<super::types::ProviderModelDescriptor>, AppError> {
        let Some(base) = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(claude_model_aliases());
        };
        let url = format!("{}/v1/models", base.trim_end_matches('/'));
        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|error| {
                AppError::ProviderUnavailable(format!("Claude model discovery failed: {error}"))
            })?;
        let payload: Value = response.json().await.map_err(|error| {
            AppError::ProviderUnavailable(format!("Claude model catalog invalid: {error}"))
        })?;
        let models = payload
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?.to_owned();
                Some(super::types::ProviderModelDescriptor {
                    display_name: item
                        .get("display_name")
                        .or_else(|| item.get("displayName"))
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned(),
                    id,
                    description: "Claude gateway model".to_owned(),
                    is_default: false,
                    supported_reasoning_efforts: ["low", "medium", "high", "xhigh", "max"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    default_reasoning_effort: None,
                    context_window: item
                        .get("context_window")
                        .or_else(|| item.get("contextWindow"))
                        .and_then(Value::as_u64),
                })
            })
            .collect::<Vec<_>>();
        Ok(if models.is_empty() {
            claude_model_aliases()
        } else {
            models
        })
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        let requested_session_id = context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| context.manifest.id.clone());
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "-p".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--include-partial-messages".to_owned(),
            "--replay-user-messages".to_owned(),
            "--permission-mode".to_owned(),
            "manual".to_owned(),
        ];
        if context.provider_state.native_session_id.is_some() {
            spec.args.push("--resume".to_owned());
        } else {
            spec.args.push("--session-id".to_owned());
        }
        spec.args.push(requested_session_id.clone());
        if let Some(model) = &prompt.model {
            spec.args.push("--model".to_owned());
            spec.args.push(model.clone());
        }
        if let Some(effort) = &prompt.reasoning_effort {
            spec.args.push("--effort".to_owned());
            spec.args.push(effort.clone());
        }

        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = run_claude_turn(
            &mut process,
            context,
            prompt,
            requested_session_id,
            &sink,
            &mut cancel,
        )
        .await;
        process.terminate().await;
        result
    }
}

fn claude_user_content(prompt: &DriverPrompt) -> Value {
    let images = prompt
        .content
        .iter()
        .filter_map(|content| match content {
            super::types::DriverPromptContent::Image {
                data, mime_type, ..
            } => Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data,
                },
            })),
            super::types::DriverPromptContent::File { .. } => None,
        })
        .collect::<Vec<_>>();
    if images.is_empty() {
        return Value::String(prompt.text.clone());
    }

    let mut content = Vec::with_capacity(images.len() + usize::from(!prompt.text.is_empty()));
    if !prompt.text.is_empty() {
        content.push(json!({ "type": "text", "text": prompt.text }));
    }
    content.extend(images);
    Value::Array(content)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{claude_model_aliases, claude_user_content};
    use crate::provider::types::{DriverPrompt, DriverPromptContent};

    #[test]
    fn built_in_model_aliases_are_selectable_without_gateway_discovery() {
        let models = claude_model_aliases();

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "sonnet", "opus", "haiku"]
        );
        assert!(models[0].is_default);
        assert_eq!(
            models[0].supported_reasoning_efforts,
            ["low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn text_only_prompt_keeps_the_existing_string_shape() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
            content: Vec::new(),
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
        };

        assert_eq!(claude_user_content(&prompt), json!("hello"));
    }

    #[test]
    fn image_prompt_uses_claude_streaming_content_blocks() {
        let prompt = DriverPrompt {
            turn_id: "turn-2".to_owned(),
            text: "describe this".to_owned(),
            content: vec![DriverPromptContent::Image {
                path: None,
                data: "cG5n".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
        };

        assert_eq!(
            claude_user_content(&prompt),
            json!([
                { "type": "text", "text": "describe this" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "cG5n",
                    },
                },
            ])
        );
    }
}

async fn run_claude_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    requested_session_id: String,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<DriverTurnResult, AppError> {
    let message_content = claude_user_content(&prompt);
    process
        .send(&json!({
            "type": "user",
            "session_id": requested_session_id,
            "parent_tool_use_id": null,
            "message": {
                "role": "user",
                "content": message_content,
            }
        }))
        .await?;

    loop {
        let message = tokio::select! {
            message = process.read() => message?,
                changed = cancel.changed() => {
                    let _ = changed;
                    return Ok(DriverTurnResult {
                    native_session_id: Some(requested_session_id.clone()),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "Claude Code closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        match message.get("type").and_then(Value::as_str) {
            Some("result") => {
                let native_session_id = message
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&requested_session_id)
                    .to_owned();
                let is_error = message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if is_error {
                    return Err(AppError::ProviderUnavailable(
                        message
                            .get("result")
                            .and_then(Value::as_str)
                            .unwrap_or("Claude Code returned an error")
                            .chars()
                            .take(1000)
                            .collect(),
                    ));
                }
                let mut provider_state = context.provider_state;
                provider_state.native_session_id = Some(native_session_id.clone());
                provider_state.recoverable = true;
                provider_state.last_error = None;
                sink.save_provider_state(provider_state).await?;
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason: message
                        .get("subtype")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .to_owned(),
                    cancelled: false,
                });
            }
            Some("stream_event") => handle_stream_event(&message, sink).await?,
            Some("assistant") => {
                sink.emit(
                    "message.completed",
                    json!({ "provider": "claude-code", "message": message.get("message") }),
                )
                .await?;
            }
            Some("tool_progress") => {
                sink.emit(
                    "tool.updated",
                    json!({
                        "provider": "claude-code",
                        "toolUseId": message.get("tool_use_id"),
                        "toolName": message.get("tool_name"),
                        "elapsedSeconds": message.get("elapsed_time_seconds"),
                    }),
                )
                .await?;
            }
            Some("control_request") => {
                handle_control_request(process, message, sink, cancel).await?;
            }
            Some("system") => {
                sink.emit(
                    "provider.event",
                    json!({
                        "provider": "claude-code",
                        "providerMethod": message.get("subtype"),
                        "metadata": {
                            "sessionId": message.get("session_id"),
                            "model": message.get("model"),
                            "permissionMode": message.get("permissionMode"),
                        }
                    }),
                )
                .await?;
            }
            Some("user" | "control_response") => {}
            Some(event_type) => {
                sink.emit(
                    "provider.event",
                    json!({ "provider": "claude-code", "providerMethod": event_type }),
                )
                .await?;
            }
            None => {}
        }
    }
}

async fn handle_stream_event(message: &Value, sink: &DriverEventSink) -> Result<(), AppError> {
    let event = message.get("event").cloned().unwrap_or(Value::Null);
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "content_block_delta" => {
            let delta = event.get("delta").cloned().unwrap_or(Value::Null);
            let event_type = if delta.get("type").and_then(Value::as_str) == Some("thinking_delta")
            {
                "thought.delta"
            } else {
                "message.delta"
            };
            sink.emit(
                event_type,
                json!({ "provider": "claude-code", "role": "assistant", "delta": delta }),
            )
            .await?;
        }
        "content_block_start" => {
            let content = event.get("content_block").cloned().unwrap_or(Value::Null);
            if content.get("type").and_then(Value::as_str) == Some("tool_use") {
                sink.emit(
                    "tool.started",
                    json!({ "provider": "claude-code", "tool": content }),
                )
                .await?;
            }
        }
        "message_stop" | "message_start" | "content_block_stop" => {}
        _ => {
            sink.emit(
                "provider.event",
                json!({ "provider": "claude-code", "providerMethod": event_type }),
            )
            .await?;
        }
    }
    Ok(())
}

async fn handle_control_request(
    process: &mut JsonLineProcess,
    message: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let request_id = message
        .get("request_id")
        .or_else(|| message.pointer("/request/request_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::InvalidRequest("Claude control request is missing request_id".to_owned())
        })?;
    let request = message.get("request").cloned().unwrap_or(Value::Null);
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if subtype != "can_use_tool" {
        process
            .send(&json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": "control request is not supported by TodeX"
                }
            }))
            .await?;
        return Ok(());
    }

    let decision = sink
        .request_permission(
            request_id.to_owned(),
            "tool",
            format!(
                "Allow Claude tool {}?",
                request
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or("operation")
            ),
            request.clone(),
            json!([
                { "id": "allow_once", "kind": "allow_once", "name": "Allow once" },
                { "id": "reject_once", "kind": "reject_once", "name": "Reject" }
            ]),
            cancel,
        )
        .await?;
    let response = match decision.outcome {
        PermissionOutcome::AllowOnce
        | PermissionOutcome::AllowAlways
        | PermissionOutcome::Answer => {
            json!({
                "behavior": "allow",
                "updatedInput": request.get("input").cloned().unwrap_or_else(|| json!({})),
            })
        }
        PermissionOutcome::RejectOnce | PermissionOutcome::RejectAlways => json!({
            "behavior": "deny",
            "message": "User rejected this tool request",
        }),
    };
    process
        .send(&json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response,
            }
        }))
        .await
}
