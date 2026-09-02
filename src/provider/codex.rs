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
    ProviderCapabilities, ProviderCommandDescriptor, ProviderDescriptor, ProviderDriver,
};

pub struct CodexDriver {
    binary: String,
}

impl CodexDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.codex_bin.clone(),
        }
    }
}

#[async_trait]
impl ProviderDriver for CodexDriver {
    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::Codex,
            display_name: "Codex CLI",
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
                model_selection: true,
            },
            models: Vec::new(),
        }
    }

    async fn discover_models(&self, workspace: &Path) -> Result<Vec<super::types::ProviderModelDescriptor>, AppError> {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec!["app-server".to_owned(), "--listen".to_owned(), "stdio://".to_owned()];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        process.send(&json!({"id":"initialize","method":"initialize","params":{"clientInfo":{"name":"todex-agentd","version":env!("CARGO_PKG_VERSION")}}})).await?;
        let _ = read_rpc_response(&mut process, "initialize").await?;
        process.send(&json!({"id":"models","method":"model/list","params":{"includeHidden":false}})).await?;
        let response = read_rpc_response(&mut process, "models").await?;
        process.terminate().await;
        Ok(response.get("data").and_then(Value::as_array).into_iter().flatten().filter_map(|item| {
            let id = item.get("model").or_else(|| item.get("id")).and_then(Value::as_str)?.to_owned();
            Some(super::types::ProviderModelDescriptor { id, display_name: item.get("displayName").and_then(Value::as_str).unwrap_or("Codex model").to_owned(), description: item.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(), is_default: item.get("isDefault").and_then(Value::as_bool).unwrap_or(false), supported_reasoning_efforts: item.get("supportedReasoningEfforts").and_then(Value::as_array).map(|items| items.iter().filter_map(|x| x.get("reasoningEffort").and_then(Value::as_str).map(ToOwned::to_owned)).collect()).unwrap_or_default(), default_reasoning_effort: item.get("defaultReasoningEffort").and_then(Value::as_str).map(ToOwned::to_owned), context_window: None })
        }).collect())
    }

    async fn discover_commands(&self, _workspace: &Path) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        // Codex exposes these as TUI commands rather than an app-server catalog.
        // Keep this adapter aligned with the installed source version; actions
        // are dispatched by the desktop's native Codex control plane.
        const COMMANDS: &[(&str, &str)] = &[
            ("model", "choose what model and reasoning effort to use"),
            ("permissions", "choose what Codex is allowed to do"),
            ("skills", "use skills to improve task execution"),
            ("hooks", "view and manage lifecycle hooks"),
            ("review", "review current changes and find issues"),
            ("rename", "rename the current thread"),
            ("new", "start a new chat"),
            ("archive", "archive this session"),
            ("resume", "resume a saved chat"),
            ("fork", "fork the current chat"),
            ("compact", "summarize conversation context"),
            ("plan", "switch to Plan mode"),
            ("goal", "set or view the task goal"),
            ("mcp", "list configured MCP tools"),
            ("apps", "manage apps"),
            ("plugins", "browse plugins"),
            ("status", "show session configuration and usage"),
            ("diff", "show git diff"),
            ("mention", "mention a file"),
            ("logout", "log out of Codex"),
        ];
        Ok(COMMANDS.iter().map(|(name, description)| ProviderCommandDescriptor {
            name: (*name).to_owned(), description: (*description).to_owned(), source: "builtin".to_owned(), invocation: "desktop".to_owned(), argument_hint: None,
        }).collect())
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<DriverTurnResult, AppError> {
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "app-server".to_owned(),
            "--listen".to_owned(),
            "stdio://".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        let result = run_codex_turn(&mut process, context, prompt, &sink, &mut cancel).await;
        process.terminate().await;
        result
    }
}

async fn read_rpc_response(process: &mut JsonLineProcess, id: &str) -> Result<Value, AppError> {
    loop {
        let Some(value) = process.read().await? else { return Err(AppError::ProviderUnavailable("Codex app-server closed stdout".to_owned())); };
        if value.get("id").and_then(Value::as_str) == Some(id) { return Ok(value.get("result").cloned().unwrap_or(value)); }
    }
}

async fn run_codex_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<DriverTurnResult, AppError> {
    process
        .send(&json!({
            "id": "initialize",
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "todex-agentd",
                    "title": "TodeX 2.0",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true,
                    "optOutNotificationMethods": null
                }
            }
        }))
        .await?;
    wait_for_response(process, "initialize", sink, cancel).await?;
    process.send(&json!({ "method": "initialized" })).await?;

    let native_session_id = match context.provider_state.native_session_id.as_deref() {
        Some(thread_id) => {
            let mut resume_params = json!({
                "threadId": thread_id,
                "cwd": context.manifest.workspace,
            });
            if let Some(model) = &prompt.model {
                resume_params["model"] = Value::String(model.clone());
            }
            if let Some(effort) = &prompt.reasoning_effort {
                resume_params["config"] = json!({ "model_reasoning_effort": effort });
            }
            process
                .send(&json!({
                    "id": "thread",
                    "method": "thread/resume",
                    "params": resume_params,
                }))
                .await?;
            wait_for_response(process, "thread", sink, cancel).await?;
            thread_id.to_owned()
        }
        None => {
            let mut params = json!({
                "cwd": context.manifest.workspace,
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write",
            });
            if let Some(model) = &prompt.model {
                params["model"] = Value::String(model.clone());
            }
            process
                .send(&json!({
                    "id": "thread",
                    "method": "thread/start",
                    "params": params,
                }))
                .await?;
            let response = wait_for_response(process, "thread", sink, cancel).await?;
            response
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "Codex thread/start response did not include a thread id".to_owned(),
                    )
                })?
                .to_owned()
        }
    };

    let mut provider_state = context.provider_state;
    provider_state.native_session_id = Some(native_session_id.clone());
    provider_state.recoverable = true;
    provider_state.last_error = None;
    sink.save_provider_state(provider_state).await?;

    process
        .send(&json!({
            "id": prompt.turn_id,
            "method": "turn/start",
            "params": {
                "threadId": native_session_id,
                "input": [{ "type": "text", "text": prompt.text }],
                "model": prompt.model.clone(),
                "effort": prompt.reasoning_effort.clone(),
            }
        }))
        .await?;
    let response = wait_for_response(process, &prompt.turn_id, sink, cancel).await?;
    let native_turn_id = response
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    loop {
        let message = tokio::select! {
            message = process.read() => message?,
            changed = cancel.changed() => {
                let _ = changed;
                if let Some(turn_id) = native_turn_id.as_deref() {
                    let _ = process.send(&json!({
                        "id": format!("cancel_{}", prompt.turn_id),
                        "method": "turn/interrupt",
                        "params": { "threadId": native_session_id, "turnId": turn_id },
                    })).await;
                }
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "Codex app-server closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        if is_turn_completed(&message, native_turn_id.as_deref()) {
            let stop_reason = message
                .pointer("/params/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_owned();
            return Ok(DriverTurnResult {
                native_session_id: Some(native_session_id),
                stop_reason,
                cancelled: false,
            });
        }
        handle_codex_message(process, message, sink, cancel).await?;
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
            return Err(provider_exit_error(process, "Codex app-server closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        if jsonrpc_id(&message) == Some(request_id) {
            if let Some(error) = message.get("error") {
                return Err(AppError::ProviderUnavailable(format!(
                    "Codex request {request_id} failed: {}",
                    safe_error_text(error)
                )));
            }
            return message.get("result").cloned().ok_or_else(|| {
                AppError::InvalidRequest(format!(
                    "Codex response {request_id} did not contain a result"
                ))
            });
        }
        handle_codex_message(process, message, sink, cancel).await?;
    }
}

async fn handle_codex_message(
    process: &mut JsonLineProcess,
    message: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if let Some(request_id) = message.get("id").cloned() {
        if is_codex_permission_method(method) {
            let decision = sink
                .request_permission(
                    request_id.to_string(),
                    codex_permission_kind(method),
                    codex_permission_title(method, &params),
                    params.clone(),
                    codex_permission_options(method),
                    cancel,
                )
                .await?;
            let result = codex_permission_response(method, &params, decision);
            process
                .send(&json!({ "id": request_id, "result": result }))
                .await?;
            return Ok(());
        }

        process
            .send(&json!({
                "id": request_id,
                "error": { "code": -32601, "message": "request is not supported by TodeX" }
            }))
            .await?;
        return Ok(());
    }

    if matches!(method, "item/started" | "item/completed") {
        if let Some((event_type, payload)) = codex_item_event(method, &params) {
            sink.emit(event_type, payload).await?;
        }
        return Ok(());
    }

    let (event_type, payload) = match method {
        "item/agentMessage/delta" => (
            "message.delta",
            json!({
                "role": "assistant",
                "delta": params.get("delta").cloned().unwrap_or(Value::Null),
                "provider": "codex",
            }),
        ),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => (
            "thought.delta",
            json!({
                "delta": params.get("delta").cloned().unwrap_or(Value::Null),
                "provider": "codex",
            }),
        ),
        "turn/plan/updated" => (
            "plan.updated",
            json!({ "provider": "codex", "plan": params.get("plan") }),
        ),
        "error" => (
            "provider.error",
            json!({ "provider": "codex", "error": params }),
        ),
        "turn/started" | "turn/completed" | "thread/started" => return Ok(()),
        _ => (
            "provider.event",
            json!({
                "provider": "codex",
                "providerMethod": method,
                "metadata": params,
            }),
        ),
    };
    sink.emit(event_type, payload).await?;
    Ok(())
}

fn codex_item_event(method: &str, params: &Value) -> Option<(&'static str, Value)> {
    let item = params.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if matches!(item_type, "agentMessage" | "agent_message") {
        return (method == "item/completed").then(|| (
            "message.completed",
            json!({ "provider": "codex", "role": "assistant", "message": item }),
        ));
    }
    if matches!(item_type, "reasoning" | "reasoningItem" | "reasoning_item") {
        return None;
    }
    Some((
        if method == "item/completed" { "tool.completed" } else { "tool.started" },
        json!({ "provider": "codex", "item": item }),
    ))
}

fn is_codex_permission_method(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
    )
}

fn codex_permission_kind(method: &str) -> &'static str {
    match method {
        "item/commandExecution/requestApproval" => "command",
        "item/fileChange/requestApproval" => "file_change",
        "item/permissions/requestApproval" => "permissions",
        "item/tool/requestUserInput" => "user_input",
        _ => "unknown",
    }
}

fn codex_permission_title(method: &str, params: &Value) -> String {
    params
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or(match method {
            "item/commandExecution/requestApproval" => "Allow command execution?",
            "item/fileChange/requestApproval" => "Allow file changes?",
            "item/permissions/requestApproval" => "Grant additional permissions?",
            "item/tool/requestUserInput" => "Codex needs input",
            _ => "Provider permission request",
        })
        .to_owned()
}

fn codex_permission_options(method: &str) -> Value {
    if method == "item/tool/requestUserInput" {
        return json!([{ "id": "answer", "kind": "answer", "name": "Answer" }]);
    }
    json!([
        { "id": "allow_once", "kind": "allow_once", "name": "Allow once" },
        { "id": "allow_always", "kind": "allow_always", "name": "Allow for session" },
        { "id": "reject_once", "kind": "reject_once", "name": "Reject" }
    ])
}

fn codex_permission_response(
    method: &str,
    params: &Value,
    decision: super::types::PermissionDecision,
) -> Value {
    if method == "item/tool/requestUserInput" {
        return decision.data.unwrap_or_else(|| json!({ "answers": {} }));
    }
    if method == "item/permissions/requestApproval" {
        return match decision.outcome {
            PermissionOutcome::AllowOnce | PermissionOutcome::AllowAlways => json!({
                "permissions": params.get("permissions").cloned().unwrap_or_else(|| json!({})),
                "scope": if matches!(decision.outcome, PermissionOutcome::AllowAlways) { "session" } else { "turn" },
            }),
            _ => json!({ "permissions": {}, "scope": "turn" }),
        };
    }
    let decision = match decision.outcome {
        PermissionOutcome::AllowAlways => "acceptForSession",
        PermissionOutcome::AllowOnce | PermissionOutcome::Answer => "accept",
        PermissionOutcome::RejectOnce | PermissionOutcome::RejectAlways => "decline",
    };
    json!({ "decision": decision })
}

fn is_turn_completed(message: &Value, native_turn_id: Option<&str>) -> bool {
    if message.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return false;
    }
    native_turn_id.is_none_or(|expected| {
        message.pointer("/params/turn/id").and_then(Value::as_str) == Some(expected)
    })
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

    #[test]
    fn agent_message_completion_is_not_emitted_as_a_tool() {
        let params = json!({ "item": { "type": "agentMessage", "text": "done" } });
        assert!(codex_item_event("item/started", &params).is_none());
        let (event_type, payload) = codex_item_event("item/completed", &params).unwrap();
        assert_eq!(event_type, "message.completed");
        assert_eq!(payload["message"]["text"], "done");
    }

    #[test]
    fn command_item_keeps_tool_lifecycle() {
        let params = json!({ "item": { "type": "commandExecution", "command": "pwd" } });
        assert_eq!(codex_item_event("item/started", &params).unwrap().0, "tool.started");
        assert_eq!(codex_item_event("item/completed", &params).unwrap().0, "tool.completed");
    }
}
