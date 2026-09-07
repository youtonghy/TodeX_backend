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
    resolve_permission_config, DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult,
    ImageInputMode, PermissionOutcome, ProviderCapabilities, ProviderCommandDescriptor,
    ProviderDescriptor, ProviderDriver,
};

mod capabilities;
mod runtime;

pub struct CodexDriver {
    binary: String,
    sessions: runtime::Sessions,
    control_probe: tokio::sync::OnceCell<capabilities::ControlProbe>,
}

impl CodexDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.codex_bin.clone(),
            sessions: runtime::Sessions::new(),
            control_probe: tokio::sync::OnceCell::new(),
        }
    }
}

#[async_trait]
impl ProviderDriver for CodexDriver {
    async fn refresh_control_capabilities(&self) {
        self.control_probe
            .get_or_init(|| capabilities::probe(&self.binary))
            .await;
    }
    fn control_probe(&self) -> Option<Value> {
        Some(
            self.control_probe
                .get()
                .map(|probe| probe.diagnostic.clone())
                .unwrap_or_else(|| json!({"status":"notChecked","source":"installed-schema"})),
        )
    }
    fn supports_live_controls(&self) -> bool {
        self.control_probe.get().is_some_and(|probe| probe.live)
    }
    fn supports_native_queue(&self) -> bool {
        self.control_probe.get().is_some_and(|probe| probe.queue)
    }

    async fn control(
        &self,
        conversation_id: &str,
        expected_turn_id: &str,
        request_id: &str,
        control: super::types::ProviderControl,
    ) -> Result<Value, AppError> {
        self.sessions
            .control(conversation_id, expected_turn_id, request_id, control)
            .await
    }

    async fn shutdown_session(&self, conversation_id: &str) {
        self.sessions.stop(conversation_id).await;
    }
    async fn shutdown(&self) {
        self.sessions.stop_all().await;
    }

    fn supports_native_compact(&self) -> bool {
        true
    }

    async fn compact_session(
        &self,
        context: DriverContext,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<(), AppError> {
        self.sessions.stop(&context.manifest.id).await;
        let native = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| {
                AppError::Unsupported(
                    "conversation has no native Codex session to compact".to_owned(),
                )
            })?;
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "app-server".to_owned(),
            "--listen".to_owned(),
            "stdio://".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = async {
            if *cancel.borrow() { return Err(AppError::TurnCancelled); }
            process.send(&json!({ "id": "initialize", "method": "initialize", "params": { "clientInfo": { "name": "todex-agentd", "version": crate::version::APP_VERSION }, "capabilities": { "experimentalApi": true } } })).await?;
            read_rpc_response_cancellable(&mut process, "initialize", &mut cancel).await?;
            process.send(&json!({ "method": "initialized" })).await?;
            process.send(&json!({ "id": "resume", "method": "thread/resume", "params": { "threadId": native, "cwd": context.manifest.workspace } })).await?;
            read_rpc_response_cancellable(&mut process, "resume", &mut cancel).await?;
            if *cancel.borrow() { return Err(AppError::TurnCancelled); }
            process.send(&json!({ "id": "compact", "method": "thread/compact/start", "params": { "threadId": native } })).await?;
            let deadline = tokio::time::Instant::now() + super::process::compact_timeout()?;
            let acknowledgement_deadline = tokio::time::Instant::now() + super::process::control_timeout()?;
            let mut acknowledged = false;
            let mut completed = false;
            let mut native_turn = None;
            loop {
                let message = tokio::select! {
                    message = process.read_control_until(if acknowledged { deadline } else { acknowledgement_deadline.min(deadline) }) => message?,
                    _ = cancel.changed() => {
                        if let Some(turn) = native_turn.as_deref() {
                            process.send(&json!({ "id": "interrupt-compact", "method": "turn/interrupt", "params": {"threadId":native,"turnId":turn} })).await?;
                        }
                        return Err(AppError::TurnCancelled);
                    }
                }.ok_or_else(|| AppError::ProviderUnavailable("Codex closed during compaction".to_owned()))?;
                if jsonrpc_id_matches(&message, "compact") {
                    if let Some(error) = message.get("error") { return Err(AppError::ProviderUnavailable(format!("Codex compaction failed: {}", safe_error_text(error)))); }
                    acknowledged = message.get("result").is_some();
                }
                if message.pointer("/params/threadId").and_then(Value::as_str) == Some(native) {
                    if message.get("method").and_then(Value::as_str) == Some("turn/started") {
                        native_turn = message.pointer("/params/turn/id").and_then(Value::as_str).map(ToOwned::to_owned);
                    }
                    if message.get("method").and_then(Value::as_str) == Some("item/completed") && message.pointer("/params/item/type").and_then(Value::as_str) == Some("contextCompaction") { completed = true; }
                    if let Some(status) = turn_completion_status(&message, native_turn.as_deref())? {
                        match status {
                            "failed" => return Err(codex_turn_failure(&message)),
                            "interrupted" => return Err(AppError::TurnCancelled),
                            _ => {},
                        }
                    }
                }
                if acknowledged && completed { return Ok(()); }
            }
        }.await;
        process.terminate().await;
        result
    }

    fn supports_native_fork(&self) -> bool {
        true
    }

    async fn fork_session(
        &self,
        context: DriverContext,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<crate::conversation::ProviderState, AppError> {
        let original = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| {
                AppError::Unsupported("conversation has no native Codex session to fork".to_owned())
            })?;
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "app-server".to_owned(),
            "--listen".to_owned(),
            "stdio://".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = async {
            process.send(&json!({ "id": "initialize", "method": "initialize", "params": { "clientInfo": { "name": "todex-agentd", "version": crate::version::APP_VERSION }, "capabilities": { "experimentalApi": true } } })).await?;
            read_rpc_response(&mut process, "initialize").await?;
            process.send(&json!({ "method": "initialized" })).await?;
            process.send(&json!({ "id": "fork", "method": "thread/fork", "params": { "threadId": original, "cwd": context.manifest.workspace, "excludeTurns": true, "ephemeral": false } })).await?;
            let response = read_rpc_response(&mut process, "fork").await?;
            let native = response.pointer("/thread/id").and_then(Value::as_str).filter(|id| !id.is_empty() && *id != original)
                .ok_or_else(|| AppError::ProviderUnavailable("Codex fork did not return a new thread id".to_owned()))?;
            let mut state = crate::conversation::ProviderState::new(ProviderKind::Codex);
            state.native_session_id = Some(native.to_owned());
            state.recoverable = true;
            Ok(state)
        }.await;
        process.terminate().await;
        result
    }

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
                permission_config: super::types::permission_config_capabilities(
                    ProviderKind::Codex,
                ),
                native_fork: true,
                native_compact: true,
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: true,
                model_selection: true,
                image_input: ProviderKind::Codex.supports_image_input(),
                image_input_mode: ImageInputMode::Model,
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
            "app-server".to_owned(),
            "--listen".to_owned(),
            "stdio://".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await?;
        process.send(&json!({"id":"initialize","method":"initialize","params":{"clientInfo":{"name":"todex-agentd","version":crate::version::APP_VERSION}}})).await?;
        let _ = read_rpc_response(&mut process, "initialize").await?;
        process.send(&json!({ "method": "initialized" })).await?;
        let result = async {
            let mut items = Vec::new();
            let mut cursor = Value::Null;
            let mut seen = std::collections::HashSet::new();
            loop {
                process
                    .send(&json!({"id":"models","method":"model/list",
                    "params":{"includeHidden":false,"cursor":cursor}}))
                    .await?;
                let response = read_rpc_response(&mut process, "models").await?;
                let page = response
                    .get("data")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        AppError::ProviderUnavailable(
                            "Codex model catalog has no data array".to_owned(),
                        )
                    })?;
                items.extend(page.iter().cloned());
                match response
                    .get("nextCursor")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    None => break,
                    Some(next) if seen.insert(next.to_owned()) => cursor = json!(next),
                    Some(_) => {
                        return Err(AppError::ProviderUnavailable(
                            "Codex model catalog repeated a cursor".to_owned(),
                        ))
                    }
                }
            }
            Ok::<_, AppError>(items)
        }
        .await;
        process.terminate().await;
        Ok(result?
            .iter()
            .filter_map(|item| {
                let id = item
                    .get("model")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)?
                    .to_owned();
                Some(super::types::ProviderModelDescriptor {
                    id,
                    display_name: item
                        .get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex model")
                        .to_owned(),
                    description: item
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    is_default: item
                        .get("isDefault")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    supported_reasoning_efforts: item
                        .get("supportedReasoningEfforts")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|x| {
                                    x.get("reasoningEffort")
                                        .and_then(Value::as_str)
                                        .map(ToOwned::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    default_reasoning_effort: item
                        .get("defaultReasoningEffort")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    context_window: None,
                    image_input: item
                        .get("inputModalities")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().any(|value| value.as_str() == Some("image"))),
                })
            })
            .collect())
    }

    async fn discover_commands(
        &self,
        _workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
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
        Ok(COMMANDS
            .iter()
            .map(|(name, description)| ProviderCommandDescriptor {
                name: (*name).to_owned(),
                description: (*description).to_owned(),
                source: "builtin".to_owned(),
                source_info: None,
                invocation: "desktop".to_owned(),
                argument_hint: None,
            })
            .collect())
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        self.sessions
            .run(
                &self.binary,
                runtime::Run {
                    context,
                    prompt,
                    sink,
                    cancel,
                    permit: launch_permit,
                },
            )
            .await
    }
}

async fn read_rpc_response_cancellable(
    process: &mut JsonLineProcess,
    id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Value, AppError> {
    if *cancel.borrow() {
        return Err(AppError::TurnCancelled);
    }
    tokio::select! {
        response = read_rpc_response(process, id) => response,
        _ = cancel.changed() => Err(AppError::TurnCancelled),
    }
}

async fn read_rpc_response(process: &mut JsonLineProcess, id: &str) -> Result<Value, AppError> {
    let deadline = tokio::time::Instant::now() + super::process::control_timeout()?;
    loop {
        let Some(value) = process.read_control_until(deadline).await? else {
            return Err(provider_exit_error(process, "Codex app-server closed stdout").await);
        };
        if jsonrpc_id_matches(&value, id) {
            if let Some(error) = value.get("error") {
                return Err(AppError::ProviderUnavailable(format!(
                    "Codex app-server request failed: {}",
                    safe_error_text(error)
                )));
            }
            return value.get("result").cloned().ok_or_else(|| {
                AppError::ProviderUnavailable(
                    "Codex app-server response did not include a result".to_owned(),
                )
            });
        }
    }
}

async fn prepare_codex_thread(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: &DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<String, AppError> {
    process
        .send(&json!({
            "id": "initialize",
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "todex-agentd",
                    "title": "TodeX 2.0",
                    "version": crate::version::APP_VERSION
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

    let controls = resolve_permission_config(
        context.manifest.provider,
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?;
    let native_session_id = match context.provider_state.native_session_id.as_deref() {
        Some(thread_id) => {
            let mut resume_params = json!({
                "threadId": thread_id,
                "cwd": context.manifest.workspace,
                "approvalPolicy": controls.approval_policy,
                "sandbox": controls.sandbox_mode,
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
            let response = wait_for_response(process, "thread", sink, cancel).await?;
            if let Some(configuration) = codex_configuration_event(&response, &prompt) {
                sink.emit("turn.configuration", configuration).await?;
            }
            thread_id.to_owned()
        }
        None => {
            let mut params = json!({
                "cwd": context.manifest.workspace,
                "approvalPolicy": controls.approval_policy,
                "sandbox": controls.sandbox_mode,
            });
            if let Some(model) = &prompt.model {
                params["model"] = Value::String(model.clone());
            }
            if let Some(effort) = &prompt.reasoning_effort {
                params["config"] = json!({ "model_reasoning_effort": effort });
            }
            process
                .send(&json!({
                    "id": "thread",
                    "method": "thread/start",
                    "params": params,
                }))
                .await?;
            let response = wait_for_response(process, "thread", sink, cancel).await?;
            if let Some(configuration) = codex_configuration_event(&response, &prompt) {
                sink.emit("turn.configuration", configuration).await?;
            }
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

    Ok(native_session_id)
}

fn codex_configuration_event(response: &Value, prompt: &DriverPrompt) -> Option<Value> {
    let mut effective = serde_json::Map::new();
    for key in ["model", "reasoningEffort", "approvalPolicy"] {
        if let Some(value) = response.get(key).filter(|value| !value.is_null()) {
            effective.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(kind) = response.pointer("/sandbox/type").and_then(Value::as_str) {
        let mode = match kind {
            "readOnly" => "read-only",
            "workspaceWrite" => "workspace-write",
            "dangerFullAccess" => "danger-full-access",
            "externalSandbox" => "external-sandbox",
            _ => kind,
        };
        effective.insert("sandboxMode".to_owned(), json!(mode));
    }
    if effective.is_empty() {
        return None;
    }
    effective.insert("source".to_owned(), json!("provider-confirmed"));
    Some(json!({
        "provider": "codex", "turnId": prompt.turn_id,
        "requested": { "model": prompt.model, "reasoningEffort": prompt.reasoning_effort, "permissionProfile": prompt.permission_profile, "sandboxMode": prompt.sandbox_mode, "approvalPolicy": prompt.approval_policy },
        "effective": effective,
    }))
}

fn codex_sandbox_policy(mode: Option<&str>) -> Value {
    match mode {
        Some("read-only") => json!({ "type": "readOnly", "networkAccess": false }),
        Some("danger-full-access") => json!({ "type": "dangerFullAccess" }),
        _ => json!({ "type": "workspaceWrite", "networkAccess": false }),
    }
}

fn codex_prompt_input(prompt: &DriverPrompt) -> Vec<Value> {
    let mut input = Vec::new();
    if !prompt.text.is_empty() {
        input.push(json!({ "type": "text", "text": prompt.text }));
    }
    for content in &prompt.content {
        match content {
            super::types::DriverPromptContent::Image {
                path: Some(path), ..
            } => input.push(json!({ "type": "localImage", "path": path })),
            super::types::DriverPromptContent::Image {
                path: None,
                data,
                mime_type,
            } => input.push(json!({
                "type": "image",
                "url": format!("data:{mime_type};base64,{data}"),
            })),
            super::types::DriverPromptContent::File { path, name } => input.push(json!({
                "type": "mention",
                "name": name,
                "path": path,
            })),
        }
    }
    for skill in &prompt.skills {
        input.push(json!({
            "type": "skill",
            "name": skill.name,
            "path": skill.path,
        }));
    }
    input
}

async fn wait_for_response(
    process: &mut JsonLineProcess,
    request_id: &str,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Value, AppError> {
    let mut deadline = tokio::time::Instant::now() + super::process::control_timeout()?;
    let mut response = None;
    let mut permissions = tokio::task::JoinSet::<(
        Value,
        String,
        Value,
        Result<super::types::PermissionDecision, AppError>,
    )>::new();
    loop {
        if permissions.is_empty() {
            if let Some(response) = response.take() {
                return Ok(response);
            }
        }
        tokio::select! {
            _ = cancel.changed() => return Err(AppError::TurnCancelled),
            _ = tokio::time::sleep_until(deadline), if permissions.is_empty() => {
                return Err(AppError::ProviderUnavailable(format!("Codex request {request_id} timed out")));
            }
            answer = permissions.join_next(), if !permissions.is_empty() => {
                if let Some(Ok((id, method, params, decision))) = answer {
                    let result = codex_permission_response(&method, &params, decision?);
                    process.send(&json!({"id":id,"result":result})).await?;
                    deadline = tokio::time::Instant::now() + super::process::control_timeout()?;
                }
            }
            message = process.read() => {
                let Some(message) = message? else { return Err(provider_exit_error(process, "Codex app-server closed stdout").await); };
                if message.is_null() { continue; }
                if jsonrpc_id_matches(&message, request_id) {
                    if let Some(error) = message.get("error") { return Err(AppError::ProviderUnavailable(format!("Codex request {request_id} failed: {}",safe_error_text(error)))); }
                    response = Some(message.get("result").cloned().ok_or_else(||AppError::InvalidRequest(format!("Codex response {request_id} did not contain a result")))?);
                    continue;
                }
                if let (Some(id),Some(method)) = (message.get("id"),message.get("method").and_then(Value::as_str)) {
                    if is_codex_permission_method(method) {
                        let id = id.clone(); let method = method.to_owned(); let params = message.get("params").cloned().unwrap_or(Value::Null);
                        let sink = sink.clone(); let mut cancel = cancel.clone();
                        permissions.spawn(async move {
                            let decision = sink.request_permission(jsonrpc_id_text(&id).unwrap_or_default(), codex_permission_kind(&method),
                                codex_permission_title(&method,&params),params.clone(),codex_permission_options(&method),&mut cancel).await;
                            (id,method,params,decision)
                        });
                        continue;
                    }
                }
                handle_codex_message(process, message, sink, cancel, request_id).await?;
            }
        }
    }
}

async fn handle_codex_message(
    process: &mut JsonLineProcess,
    message: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    turn_id: &str,
) -> Result<(), AppError> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if let Some(request_id) = message.get("id").cloned() {
        if is_codex_permission_method(method) {
            let decision = sink
                .request_permission(
                    jsonrpc_id_text(&request_id)?,
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
        for (event_type, payload) in codex_activity_events(method, &params, turn_id) {
            sink.emit(event_type, payload).await?;
        }
        for payload in codex_completed_reasoning(method, &params, turn_id) {
            sink.emit("thought.completed", payload).await?;
        }
        if let Some((event_type, mut payload)) = codex_item_event(method, &params, turn_id) {
            payload["nativeTurnId"] = params.get("turnId").cloned().unwrap_or(Value::Null);
            payload["nativeSessionId"] = params.get("threadId").cloned().unwrap_or(Value::Null);
            sink.emit(event_type, payload).await?;
        } else {
            // New upstream item variants must survive even before a typed UI exists.
            sink.emit(
                "provider.event",
                json!({"provider":"codex", "providerMethod":method,
                "metadata":params, "turnId":turn_id}),
            )
            .await?;
        }
        return Ok(());
    }

    if method == "thread/tokenUsage/updated" {
        if let Some(mut payload) = codex_usage_event(&params) {
            payload["turnId"] = json!(turn_id);
            payload["source"] = json!("provider");
            sink.emit("usage.updated", payload).await?;
        } else {
            sink.emit(
                "provider.event",
                json!({
                    "provider": "codex",
                    "providerMethod": method,
                    "metadata": params,
                }),
            )
            .await?;
        }
        return Ok(());
    }

    let (event_type, mut payload) = match method {
        "item/agentMessage/delta" => (
            "message.delta",
            json!({
                "role": "assistant",
                "delta": params.get("delta").cloned().unwrap_or(Value::Null),
                "provider": "codex",
                "block": codex_block(&params, "assistant_final", "delta", turn_id),
            }),
        ),
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => (
            "thought.delta",
            json!({
                "delta": params.get("delta").cloned().unwrap_or(Value::Null),
                "provider": "codex",
                "thought": params.get("delta"),
                "block": codex_reasoning_block(&params, method, "delta", turn_id),
            }),
        ),
        "thread/settings/updated" => (
            "turn.configuration",
            json!({"provider":"codex", "effective":{
                "model":params.pointer("/threadSettings/model"),
                "reasoningEffort":params.pointer("/threadSettings/effort"),
                "approvalPolicy":params.pointer("/threadSettings/approvalPolicy"),
                "source":"provider-confirmed","scope":"subsequent-turns"
            }}),
        ),
        "model/rerouted" => (
            "turn.configuration",
            json!({"provider":"codex","effective":{
                "model":params.get("toModel"),"source":"provider-confirmed","scope":"active-turn"
            }}),
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
    payload["nativeTurnId"] = params.get("turnId").cloned().unwrap_or(Value::Null);
    payload["nativeSessionId"] = params.get("threadId").cloned().unwrap_or(Value::Null);
    sink.emit(event_type, payload).await?;
    Ok(())
}

fn codex_usage_event(params: &Value) -> Option<Value> {
    let usage = params.get("tokenUsage")?.as_object()?;
    let total = codex_usage_breakdown(usage.get("total")?)?;
    let last = codex_usage_breakdown(usage.get("last")?)?;
    Some(json!({
        "provider": "codex",
        "turnId": params.get("turnId"),
        "usage": {
            "cumulative": total,
            "last": last,
        },
        "contextWindow": usage.get("modelContextWindow"),
    }))
}

fn codex_usage_breakdown(value: &Value) -> Option<Value> {
    let usage = value.as_object()?;
    Some(json!({
        "total": usage.get("totalTokens"),
        "input": usage.get("inputTokens"),
        "cacheRead": usage.get("cachedInputTokens"),
        "cacheWrite": usage.get("cacheWriteInputTokens"),
        "output": usage.get("outputTokens"),
        "reasoningOutput": usage.get("reasoningOutputTokens"),
    }))
}

fn codex_activity_events(
    method: &str,
    params: &Value,
    turn_id: &str,
) -> Vec<(&'static str, Value)> {
    let Some(item) = params.get("item") else {
        return Vec::new();
    };
    match item.get("type").and_then(Value::as_str) {
        Some("subAgentActivity") => {
            let event = match item.get("kind").and_then(Value::as_str) {
                Some("started") => "subagent.started",
                Some("completed") => "subagent.completed",
                Some("interrupted") => "subagent.cancelled",
                _ => "subagent.updated",
            };
            vec![(event, json!({"provider":"codex","turnId":turn_id,
                "subagentId":item.get("agentThreadId"), "title":item.get("agentPath"),
                "source":"provider", "providerItemId":item.get("id")}))]
        }
        Some("contextCompaction") => vec![(if method == "item/started" { "compaction.started" } else { "compaction.completed" },
            json!({ "provider": "codex", "turnId": turn_id, "compactionId": item.get("id"), "source": "provider" }))],
        Some("collabAgentToolCall") => item.get("receiverThreadIds").and_then(Value::as_array).into_iter().flatten().filter_map(|id| {
            let id = id.as_str()?;
            let state = item.get("agentsStates").and_then(|states| states.get(id));
            let status = state.and_then(|state| state.get("status")).and_then(Value::as_str);
            let event = match status {
                Some("completed") => "subagent.completed",
                Some("errored" | "notFound") => "subagent.failed",
                Some("interrupted" | "shutdown") => "subagent.cancelled",
                Some("pendingInit") => "subagent.started",
                _ => "subagent.updated",
            };
            Some((event, json!({ "provider": "codex", "turnId": turn_id, "subagentId": id, "parentId": item.get("senderThreadId"), "title": item.get("tool"), "task": item.get("prompt"), "status": status, "result": state.and_then(|state| state.get("message")), "source": "provider", "providerItemId": item.get("id") })))
        }).collect(),
        _ => Vec::new(),
    }
}

fn codex_item_event(method: &str, params: &Value, turn_id: &str) -> Option<(&'static str, Value)> {
    let item = params.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if matches!(item_type, "agentMessage" | "agent_message") {
        let completed = method == "item/completed";
        return Some((
            if completed {
                "message.completed"
            } else {
                "message.created"
            },
            json!({
                "provider":"codex", "role":"assistant", "message":item,
                "block": codex_block(params, if item.get("phase").and_then(Value::as_str) == Some("commentary") {
                    "assistant_progress"
                } else { "assistant_final" }, if completed { "completed" } else { "started" }, turn_id),
            }),
        ));
    }
    if matches!(item_type, "reasoning" | "reasoningItem" | "reasoning_item") {
        // Preserve authoritative summary/content without merging the two streams.
        return Some((
            "provider.event",
            json!({"provider":"codex", "providerMethod":method,
            "metadata":params, "turnId":turn_id}),
        ));
    }
    if !is_codex_tool_item(item_type) {
        return None;
    }
    let (event_type, phase) = if method == "item/completed" {
        ("tool.completed", "completed")
    } else {
        ("tool.started", "started")
    };
    Some((
        event_type,
        json!({
            "provider": "codex",
            "item": item,
            "block": codex_block(params, "tool", phase, turn_id),
        }),
    ))
}

fn is_codex_tool_item(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "command_execution"
            | "fileChange"
            | "file_change"
            | "mcpToolCall"
            | "mcp_tool_call"
            | "dynamicToolCall"
            | "dynamic_tool_call"
            | "webSearch"
            | "web_search"
            | "imageView"
            | "image_view"
            | "collabAgentToolCall"
            | "collab_agent_tool_call"
            | "imageGeneration"
            | "functionCallOutput"
            | "sleep"
            | "subAgentActivity"
            | "enteredReviewMode"
            | "exitedReviewMode"
    )
}

fn codex_block(params: &Value, category: &str, phase: &str, fallback_turn_id: &str) -> Value {
    let item = params.get("item").unwrap_or(&Value::Null);
    let id = item
        .get("id")
        .or_else(|| params.get("itemId"))
        .or_else(|| params.get("item_id"))
        .and_then(Value::as_str)
        .unwrap_or("current");
    json!({
        "category": category,
        "id": id,
        "turnId": fallback_turn_id,
        "phase": phase,
    })
}

fn codex_completed_reasoning(method: &str, params: &Value, turn_id: &str) -> Vec<Value> {
    if method != "item/completed"
        || params.pointer("/item/type").and_then(Value::as_str) != Some("reasoning")
    {
        return Vec::new();
    }
    let mut events = Vec::new();
    for (field, index_key, native_method) in [
        ("summary", "summaryIndex", "item/reasoning/summaryTextDelta"),
        ("content", "contentIndex", "item/reasoning/textDelta"),
    ] {
        for (index, text) in params
            .get("item")
            .and_then(|item| item.get(field))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(text) = text.as_str() {
                let mut params = params.clone();
                params[index_key] = json!(index);
                events.push(json!({"provider":"codex", "thought":text,
                    "block":codex_reasoning_block(&params,native_method,"completed",turn_id)}));
            }
        }
    }
    events
}

fn codex_reasoning_block(params: &Value, method: &str, phase: &str, turn_id: &str) -> Value {
    let mut block = codex_block(params, "reasoning", phase, turn_id);
    let (source, index) = if method == "item/reasoning/summaryTextDelta" {
        ("summary", params.get("summaryIndex"))
    } else {
        ("content", params.get("contentIndex"))
    };
    block["id"] = json!(format!(
        "{}:{source}:{}",
        block["id"].as_str().unwrap_or("current"),
        index.and_then(Value::as_i64).unwrap_or(0)
    ));
    block["contentIndex"] = index.cloned().unwrap_or(json!(0));
    block["source"] = json!(source);
    block
}

fn is_codex_permission_method(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
    )
}

fn codex_permission_kind(method: &str) -> &'static str {
    match method {
        "item/commandExecution/requestApproval" => "command",
        "item/fileChange/requestApproval" => "file_change",
        "item/permissions/requestApproval" => "permissions",
        "item/tool/requestUserInput" => "user_input",
        "mcpServer/elicitation/request" => "elicitation",
        _ => "unknown",
    }
}

fn codex_permission_title(method: &str, params: &Value) -> String {
    params
        .get("reason")
        .or_else(|| params.get("message"))
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
    if method == "mcpServer/elicitation/request" {
        return json!([
            {"id":"answer", "kind":"answer", "name":"Submit"},
            {"id":"reject_once", "kind":"reject_once", "name":"Decline"}
        ]);
    }
    if method == "item/tool/requestUserInput" {
        return json!([{ "id": "answer", "kind": "answer", "name": "Answer" }]);
    }
    let mut options = json!([
        { "id": "allow_once", "kind": "allow_once", "name": "Allow once" },
        { "id": "allow_always", "kind": "allow_always", "name": "Allow for session" },
        { "id": "reject_once", "kind": "reject_once", "name": "Reject" }
    ]);
    if matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    ) {
        options
            .as_array_mut()
            .expect("permission options array")
            .push(
                json!({ "id": "abort_turn", "kind": "abort_turn", "name": "Reject and stop turn" }),
            );
    }
    options
}

fn codex_permission_response(
    method: &str,
    params: &Value,
    decision: super::types::PermissionDecision,
) -> Value {
    if method == "mcpServer/elicitation/request" {
        return match decision.outcome {
            PermissionOutcome::Answer => {
                let content = if params.get("mode").and_then(Value::as_str) == Some("url") {
                    Value::Null
                } else {
                    decision.data.unwrap_or(Value::Null)
                };
                json!({"action":"accept", "content":content, "_meta":null})
            }
            PermissionOutcome::AbortTurn => {
                json!({"action":"cancel", "content":null, "_meta":null})
            }
            _ => json!({"action":"decline", "content":null, "_meta":null}),
        };
    }
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
        PermissionOutcome::AllowOnce => "accept",
        PermissionOutcome::Answer => "decline",
        PermissionOutcome::AbortTurn => "cancel",
        PermissionOutcome::RejectOnce | PermissionOutcome::RejectAlways => "decline",
    };
    json!({ "decision": decision })
}

fn turn_completion_status<'a>(
    message: &'a Value,
    native_turn_id: Option<&str>,
) -> Result<Option<&'a str>, AppError> {
    if message.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return Ok(None);
    }
    if native_turn_id.is_some_and(|expected| {
        message.pointer("/params/turn/id").and_then(Value::as_str) != Some(expected)
    }) {
        return Ok(None);
    }
    let status = message
        .pointer("/params/turn/status")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::ProviderUnavailable(
                "Codex turn/completed notification did not include a status".to_owned(),
            )
        })?;
    if !matches!(status, "completed" | "interrupted" | "failed") {
        return Err(AppError::ProviderUnavailable(format!(
            "Codex turn/completed notification used unknown status '{status}'"
        )));
    }
    Ok(Some(status))
}

fn codex_turn_failure(message: &Value) -> AppError {
    let detail = message
        .pointer("/params/turn/error/message")
        .or_else(|| message.pointer("/params/turn/error"))
        .map(safe_error_text)
        .unwrap_or_else(|| "Codex turn failed".to_owned());
    AppError::ProviderUnavailable(detail)
}

fn jsonrpc_id_matches(message: &Value, expected: &str) -> bool {
    message.get("id").is_some_and(|id| match id {
        Value::String(value) => value == expected,
        Value::Number(value) => value.to_string() == expected,
        _ => false,
    })
}

fn jsonrpc_id_text(id: &Value) -> Result<String, AppError> {
    match id {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(AppError::InvalidRequest(
            "Codex request id must be a string or number".to_owned(),
        )),
    }
}

fn safe_error_text(error: &Value) -> String {
    error
        .as_str()
        .or_else(|| error.get("message").and_then(Value::as_str))
        .unwrap_or("provider returned an error")
        .chars()
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::conversation::redact_secrets;
    use crate::provider::types::{DriverPromptContent, DriverSkill};

    #[test]
    fn codex_items_keep_phase_metadata_and_unknown_payloads() {
        let commentary = json!({"item":{"id":"m1","type":"agentMessage","phase":"commentary","text":"working","questions":[{"id":"q"}]}});
        let (_, event) = codex_item_event("item/completed", &commentary, "turn").unwrap();
        assert_eq!(event["block"]["category"], "assistant_progress");
        assert_eq!(event["message"]["questions"][0]["id"], "q");
        let image = json!({"item":{"id":"img","type":"imageGeneration","status":"completed","result":"/tmp/image.png"}});
        assert_eq!(
            codex_item_event("item/completed", &image, "turn")
                .unwrap()
                .1["item"],
            image["item"]
        );
        let summary = codex_reasoning_block(
            &json!({"itemId":"r","summaryIndex":1}),
            "item/reasoning/summaryTextDelta",
            "delta",
            "turn",
        );
        let raw = codex_reasoning_block(
            &json!({"itemId":"r","contentIndex":1}),
            "item/reasoning/textDelta",
            "delta",
            "turn",
        );
        assert_ne!(summary["id"], raw["id"]);
        assert_eq!(summary["contentIndex"], 1);
    }

    #[test]
    fn elicitation_maps_form_answers_and_rejection_to_upstream_shape() {
        assert!(is_codex_permission_method("mcpServer/elicitation/request"));
        assert_eq!(
            codex_permission_kind("mcpServer/elicitation/request"),
            "elicitation"
        );
        let response = codex_permission_response(
            "mcpServer/elicitation/request",
            &json!({"mode":"form","requestedSchema":{"type":"object"}}),
            super::super::types::PermissionDecision {
                outcome: PermissionOutcome::Answer,
                option_id: Some("answer".into()),
                data: Some(json!({"name":"Ada"})),
            },
        );
        assert_eq!(
            response,
            json!({"action":"accept","content":{"name":"Ada"},"_meta":null})
        );
        let rejected = codex_permission_response(
            "mcpServer/elicitation/request",
            &Value::Null,
            super::super::types::PermissionDecision {
                outcome: PermissionOutcome::RejectOnce,
                option_id: Some("reject_once".into()),
                data: None,
            },
        );
        assert_eq!(rejected["action"], "decline");
        assert!(rejected["content"].is_null());
    }

    #[test]
    fn agent_message_completion_is_not_emitted_as_a_tool() {
        let params = json!({ "item": { "type": "agentMessage", "text": "done" } });
        assert_eq!(
            codex_item_event("item/started", &params, "turn-1")
                .unwrap()
                .0,
            "message.created"
        );
        let (event_type, payload) = codex_item_event("item/completed", &params, "turn-1").unwrap();
        assert_eq!(event_type, "message.completed");
        assert_eq!(payload["message"]["text"], "done");
    }

    #[test]
    fn token_usage_notification_is_normalized_without_secret_like_keys() {
        let mut payload = codex_usage_event(&json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "tokenUsage": {
                "total": {
                    "totalTokens": 210,
                    "inputTokens": 120,
                    "cachedInputTokens": 30,
                    "cacheWriteInputTokens": 10,
                    "outputTokens": 40,
                    "reasoningOutputTokens": 8
                },
                "last": {
                    "totalTokens": 70,
                    "inputTokens": 40,
                    "cachedInputTokens": 10,
                    "cacheWriteInputTokens": 0,
                    "outputTokens": 20,
                    "reasoningOutputTokens": 5
                },
                "modelContextWindow": 200000
            }
        }))
        .unwrap();
        redact_secrets(&mut payload);

        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["turnId"], "turn-1");
        assert_eq!(payload["usage"]["cumulative"]["total"], 210);
        assert_eq!(payload["usage"]["last"]["input"], 40);
        assert_eq!(payload["usage"]["last"]["cacheRead"], 10);
        assert_eq!(payload["usage"]["last"]["output"], 20);
        assert_eq!(payload["usage"]["last"]["reasoningOutput"], 5);
        assert_eq!(payload["contextWindow"], 200000);
        assert!(!payload.to_string().to_ascii_lowercase().contains("token"));
    }

    #[test]
    fn command_item_keeps_tool_lifecycle() {
        let params = json!({ "item": { "type": "commandExecution", "command": "pwd" } });
        assert_eq!(
            codex_item_event("item/started", &params, "turn-1")
                .unwrap()
                .0,
            "tool.started"
        );
        assert_eq!(
            codex_item_event("item/completed", &params, "turn-1")
                .unwrap()
                .0,
            "tool.completed"
        );
    }

    #[test]
    fn user_message_item_is_not_emitted_as_a_tool() {
        let params = json!({ "item": { "type": "userMessage", "content": [] } });
        assert!(codex_item_event("item/started", &params, "turn-1").is_none());
        assert!(codex_item_event("item/completed", &params, "turn-1").is_none());
    }

    #[test]
    fn block_metadata_keeps_provider_item_identity() {
        let block = codex_block(
            &json!({ "turnId": "turn-1", "item": { "id": "item-2" } }),
            "tool",
            "completed",
            "fallback-turn",
        );
        assert_eq!(block["category"], "tool");
        assert_eq!(block["id"], "item-2");
        assert_eq!(block["turnId"], "fallback-turn");
        assert_eq!(block["phase"], "completed");
    }

    #[test]
    fn request_ids_accept_strings_and_numbers_without_quoting() {
        assert!(jsonrpc_id_matches(&json!({ "id": "42" }), "42"));
        assert!(jsonrpc_id_matches(&json!({ "id": 42 }), "42"));
        assert_eq!(jsonrpc_id_text(&json!("request-1")).unwrap(), "request-1");
        assert_eq!(jsonrpc_id_text(&json!(42)).unwrap(), "42");
        assert!(jsonrpc_id_text(&Value::Null).is_err());
    }

    #[test]
    fn turn_terminal_status_is_authoritative_and_turn_scoped() {
        let failed = json!({
            "method": "turn/completed",
            "params": { "turn": { "id": "turn-1", "status": "failed", "error": "boom" } }
        });
        assert_eq!(
            turn_completion_status(&failed, Some("turn-1")).unwrap(),
            Some("failed")
        );
        assert!(turn_completion_status(&failed, Some("turn-2"))
            .unwrap()
            .is_none());
        assert!(codex_turn_failure(&failed).to_string().contains("boom"));

        let interrupted = json!({
            "method": "turn/completed",
            "params": { "turn": { "id": "turn-1", "status": "interrupted" } }
        });
        assert_eq!(
            turn_completion_status(&interrupted, Some("turn-1")).unwrap(),
            Some("interrupted")
        );

        let missing_status = json!({
            "method": "turn/completed",
            "params": { "turn": { "id": "turn-1" } }
        });
        assert!(turn_completion_status(&missing_status, Some("turn-1")).is_err());
    }

    #[test]
    fn typed_prompt_content_maps_to_codex_input_items() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "inspect these".to_owned(),
            content: vec![
                DriverPromptContent::Image {
                    path: Some(PathBuf::from("/workspace/image.png")),
                    data: String::new(),
                    mime_type: "image/png".to_owned(),
                },
                DriverPromptContent::File {
                    path: PathBuf::from("/workspace/readme.md"),
                    name: "readme.md".to_owned(),
                },
            ],
            skills: vec![DriverSkill {
                name: "review".to_owned(),
                path: PathBuf::from("/skills/review/SKILL.md"),
                content: "unused by the native item".to_owned(),
            }],
            model: None,
            reasoning_effort: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        let input = codex_prompt_input(&prompt);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[1]["type"], "localImage");
        assert_eq!(input[2]["type"], "mention");
        assert_eq!(input[3]["type"], "skill");
    }
    #[test]
    fn sandbox_wire_and_answer_rejection_match_protocol() {
        assert_eq!(
            super::codex_sandbox_policy(Some("read-only"))["type"],
            "readOnly"
        );
        assert_eq!(
            super::codex_sandbox_policy(Some("danger-full-access"))["type"],
            "dangerFullAccess"
        );
        let response = super::codex_permission_response(
            "item/commandExecution/requestApproval",
            &json!({}),
            super::super::types::PermissionDecision {
                outcome: super::PermissionOutcome::Answer,
                option_id: None,
                data: None,
            },
        );
        assert_eq!(response["decision"], "decline");
    }

    #[test]
    fn native_activity_keeps_child_identity_and_independent_status() {
        let events = super::codex_activity_events(
            "item/completed",
            &json!({"item": {"type":"collabAgentToolCall", "id":"tool", "senderThreadId":"parent", "receiverThreadIds":["child"], "status":"completed", "agentsStates":{"child":{"status":"running"}}}}),
            "turn",
        );
        assert_eq!(events[0].0, "subagent.updated");
        assert_eq!(events[0].1["subagentId"], "child");
        assert_eq!(events[0].1["parentId"], "parent");
        assert_eq!(
            super::codex_activity_events(
                "item/completed",
                &json!({"item":{"type":"contextCompaction","id":"compact"}}),
                "turn"
            )[0]
            .0,
            "compaction.completed"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn model_catalog_follows_pages_and_respects_input_modalities() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("todex-codex-models-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let script = root.join("fake-codex");
        tokio::fs::write(&script, r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":"initialize","result":{}}\n' ;;
    *'"cursor":null'*) printf '{"id":"models","result":{"data":[{"model":"text-model","displayName":"Text","inputModalities":["text"],"supportedReasoningEfforts":[{"reasoningEffort":"low"}],"defaultReasoningEffort":"low"}],"nextCursor":"next-page"}}\n' ;;
    *'"cursor":"next-page"'*) printf '{"id":"models","result":{"data":[{"model":"image-model","displayName":"Vision","inputModalities":["text","image"],"supportedReasoningEfforts":[{"reasoningEffort":"high"}],"defaultReasoningEffort":"high"}],"nextCursor":null}}\n' ;;
  esac
done
"#).await.unwrap();
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let driver = CodexDriver {
            binary: script.display().to_string(),
            sessions: runtime::Sessions::new(),
            control_probe: tokio::sync::OnceCell::new(),
        };
        let catalog = driver.discover_models(&root).await.unwrap();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].image_input, Some(false));
        assert_eq!(catalog[1].image_input, Some(true));
        assert_eq!(catalog[1].default_reasoning_effort.as_deref(), Some("high"));
        assert!(matches!(
            driver.descriptor().capabilities.image_input_mode,
            ImageInputMode::Model
        ));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_wire_preserves_controls_resume_fork_and_compact() {
        use crate::conversation::{
            ConversationEventHub, ConversationManifest, ConversationStore, ProviderState,
        };
        use crate::provider::types::PermissionBroker;
        use crate::workspace_trust::WorkspaceTrustStore;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("todex-codex-wire-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let root = tokio::fs::canonicalize(root).await.unwrap();
        let script = root.join("fake-codex");
        tokio::fs::write(&script, r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> wire.jsonl
  id=$(printf '%s' "$line" | /usr/bin/sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":"%s","result":{}}\n' "$id" ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) printf '{"id":"%s","result":{"thread":{"id":"native"}}}\n' "$id" ;;
    *'"method":"turn/start"'*)
      printf '{"id":"%s","result":{"turn":{"id":"native-turn"}}}\n' "$id"
      printf '{"method":"turn/completed","params":{"threadId":"native","turn":{"id":"native-turn","status":"completed"}}}\n' ;;
    *'"method":"thread/fork"'*) printf '{"id":"%s","result":{"thread":{"id":"forked"}}}\n' "$id" ;;
    *'"method":"thread/compact/start"'*)
      printf '{"method":"item/completed","params":{"threadId":"native","item":{"type":"contextCompaction","id":"compact-item"}}}\n'
      printf '{"id":"%s","result":{}}\n' "$id" ;;
  esac
done
"#).await.unwrap();
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
        let store = ConversationStore::new(root.join("data")).await.unwrap();
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        let sink = DriverEventSink::new(
            store,
            ConversationEventHub::default(),
            PermissionBroker::default(),
            manifest.id.clone(),
        );
        let trust = WorkspaceTrustStore::new(root.join("trust"), root.clone())
            .await
            .unwrap();
        trust.set_owned("local", &root, true).await.unwrap();
        let driver = CodexDriver {
            binary: script.display().to_string(),
            sessions: runtime::Sessions::new(),
            control_probe: tokio::sync::OnceCell::new(),
        };
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let mut provider_state = ProviderState::new(ProviderKind::Codex);
        for sandbox in ["read-only", "danger-full-access"] {
            driver
                .run_turn(
                    DriverContext {
                        manifest: manifest.clone(),
                        provider_state: provider_state.clone(),
                    },
                    DriverPrompt {
                        turn_id: "turn".to_owned(),
                        text: "hello".to_owned(),
                        content: vec![],
                        skills: vec![],
                        model: None,
                        reasoning_effort: None,
                        permission_profile: None,
                        sandbox_mode: Some(sandbox.to_owned()),
                        approval_policy: Some("never".to_owned()),
                    },
                    sink.clone(),
                    cancel_rx.clone(),
                    trust.acquire_owned("local", &root).await.unwrap(),
                )
                .await
                .unwrap();
            provider_state.native_session_id = Some("native".to_owned());
        }
        let context = DriverContext {
            manifest,
            provider_state,
        };
        let fork = driver
            .fork_session(
                context.clone(),
                trust.acquire_owned("local", &root).await.unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fork.native_session_id.as_deref(), Some("forked"));
        driver
            .compact_session(
                context,
                cancel_rx,
                trust.acquire_owned("local", &root).await.unwrap(),
            )
            .await
            .unwrap();
        let wire = tokio::fs::read_to_string(root.join("wire.jsonl"))
            .await
            .unwrap();
        let messages: Vec<Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let requests = |method: &str| {
            messages
                .iter()
                .filter(|message| message["method"] == method)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            requests("thread/start")[0]["params"]["sandbox"],
            "read-only"
        );
        assert_eq!(
            requests("thread/start")[0]["params"]["approvalPolicy"],
            "never"
        );
        assert!(requests("thread/resume")
            .iter()
            .all(|request| request["params"]["threadId"] == "native"));
        assert_eq!(
            requests("turn/start")[0]["params"]["sandboxPolicy"]["type"],
            "readOnly"
        );
        assert_eq!(
            requests("turn/start")[1]["params"]["sandboxPolicy"]["type"],
            "dangerFullAccess"
        );
        assert_eq!(requests("thread/fork")[0]["params"]["threadId"], "native");
        assert_eq!(requests("thread/compact/start").len(), 1);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
    #[test]
    fn abort_turn_is_only_advertised_for_supported_codex_requests() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            assert!(codex_permission_options(method)
                .as_array()
                .unwrap()
                .iter()
                .any(|option| option["kind"] == "abort_turn"));
            assert_eq!(
                codex_permission_response(
                    method,
                    &json!({}),
                    crate::provider::types::PermissionDecision {
                        outcome: PermissionOutcome::AbortTurn,
                        option_id: Some("abort_turn".to_owned()),
                        data: None
                    }
                )["decision"],
                "cancel"
            );
        }
        assert!(
            !codex_permission_options("item/permissions/requestApproval")
                .as_array()
                .unwrap()
                .iter()
                .any(|option| option["kind"] == "abort_turn")
        );
    }
}
