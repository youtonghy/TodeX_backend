use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::JoinSet;

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::process::{executable_available, provider_exit_error, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PendingProviderControl, PermissionOutcome, ProviderCapabilities, ProviderCommandDescriptor,
    ProviderControl, ProviderDescriptor, ProviderDriver,
};

pub struct PiDriver {
    binary: String,
    sessions: Arc<Mutex<HashMap<String, PiSessionHandle>>>,
}

#[derive(Clone)]
struct PiSessionHandle {
    turns: mpsc::Sender<PiTurnRequest>,
    controls: mpsc::Sender<PendingProviderControl>,
    shutdown: watch::Sender<bool>,
}

struct PiTurnRequest {
    context: DriverContext,
    prompt: DriverPrompt,
    sink: DriverEventSink,
    cancel: watch::Receiver<bool>,
    result: oneshot::Sender<Result<DriverTurnResult, AppError>>,
}

impl PiDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.pi_bin.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    async fn open_existing(
        &self,
        context: &DriverContext,
        permit: WorkspaceTrustPermit,
    ) -> Result<JsonLineProcess, AppError> {
        let native = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| {
                AppError::Unsupported("Pi conversation has no native session".to_owned())
            })?;
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "--mode".to_owned(),
            "rpc".to_owned(),
            "--session".to_owned(),
            native.to_owned(),
            "--approve".to_owned(),
        ];
        JsonLineProcess::spawn_trusted(&spec, permit).await
    }
}

#[async_trait]
impl ProviderDriver for PiDriver {
    fn supports_live_controls(&self) -> bool {
        true
    }
    fn supports_native_queue(&self) -> bool {
        true
    }

    async fn control(
        &self,
        conversation_id: &str,
        expected_turn_id: &str,
        request_id: &str,
        control: ProviderControl,
    ) -> Result<Value, AppError> {
        let handle = self
            .sessions
            .lock()
            .await
            .get(conversation_id)
            .cloned()
            .ok_or_else(|| {
                AppError::Conflict("Pi has no active session for this conversation".to_owned())
            })?;
        let (respond_to, response) = oneshot::channel();
        handle
            .controls
            .send(PendingProviderControl {
                expected_turn_id: expected_turn_id.to_owned(),
                request_id: request_id.to_owned(),
                control,
                respond_to,
            })
            .await
            .map_err(|_| AppError::Conflict("Pi session has closed".to_owned()))?;
        tokio::time::timeout(super::process::control_timeout()? * 4, response).await
            .map_err(|_| AppError::ProviderUnavailable("Pi live control timed out; inspect the effective configuration before retrying".to_owned()))?
            .map_err(|_| AppError::ProviderUnavailable("Pi closed before acknowledging the live control".to_owned()))?
    }

    async fn shutdown_session(&self, conversation_id: &str) {
        if let Some(handle) = self.sessions.lock().await.remove(conversation_id) {
            handle.shutdown.send_replace(true);
            handle.turns.closed().await;
        }
    }

    async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.sessions.lock().await);
        for handle in sessions.values() {
            handle.shutdown.send_replace(true);
        }
        for handle in sessions.values() {
            handle.turns.closed().await;
        }
    }

    fn supports_native_compact(&self) -> bool {
        true
    }

    async fn compact_session(
        &self,
        context: DriverContext,
        mut cancel: watch::Receiver<bool>,
        permit: WorkspaceTrustPermit,
    ) -> Result<(), AppError> {
        self.shutdown_session(&context.manifest.id).await;
        let mut process = self.open_existing(&context, permit).await?;
        let result = async {
            let mut rpc = PiRpc::new(&mut process, None, cancel.clone());
            rpc.process.send(&json!({"id":"state","type":"get_state"})).await?;
            let state = rpc.wait_response("state").await?;
            verify_pi_session(&state, context.provider_state.native_session_id.as_deref())?;
            rpc.process.send(&json!({"id":"compact","type":"compact"})).await?;
            tokio::select! {
                response = rpc.wait_response_for("compact", super::process::compact_timeout()?) => { response?; Ok(()) },
                _ = cancel.changed() => Err(AppError::TurnCancelled),
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
        permit: WorkspaceTrustPermit,
    ) -> Result<crate::conversation::ProviderState, AppError> {
        self.shutdown_session(&context.manifest.id).await;
        let mut process = self.open_existing(&context, permit).await?;
        let result = async {
            let (_cancel, receiver) = watch::channel(false);
            let mut rpc = PiRpc::new(&mut process, None, receiver);
            rpc.process
                .send(&json!({"id":"state","type":"get_state"}))
                .await?;
            let state = rpc.wait_response("state").await?;
            verify_pi_session(&state, context.provider_state.native_session_id.as_deref())?;
            // clone keeps the full current leaf, while fork(entryId) edits from a selected node.
            rpc.process
                .send(&json!({"id":"clone","type":"clone"}))
                .await?;
            let response = rpc.wait_response("clone").await?;
            if response.pointer("/data/cancelled").and_then(Value::as_bool) != Some(false) {
                return Err(AppError::ProviderUnavailable(
                    "Pi clone was cancelled or returned an invalid result".to_owned(),
                ));
            }
            rpc.process
                .send(&json!({"id":"cloned-state","type":"get_state"}))
                .await?;
            let state = rpc.wait_response("cloned-state").await?;
            let native = state
                .pointer("/data/sessionId")
                .and_then(Value::as_str)
                .filter(|id| {
                    !id.is_empty()
                        && Some(*id) != context.provider_state.native_session_id.as_deref()
                })
                .ok_or_else(|| {
                    AppError::ProviderUnavailable(
                        "Pi clone did not create a new session".to_owned(),
                    )
                })?;
            let mut result = crate::conversation::ProviderState::new(ProviderKind::Pi);
            result.native_session_id = Some(native.to_owned());
            result.recoverable = true;
            Ok(result)
        }
        .await;
        process.terminate().await;
        result
    }

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
                permission_config: super::types::permission_config_capabilities(ProviderKind::Pi),
                native_fork: true,
                native_compact: true,
                native_resume: true,
                cancel: true,
                // Pi RPC exposes extension dialogs, but not a universal pre-tool approval API.
                permissions: false,
                tool_events: true,
                native_skills: true,
                native_mcp: false,
                managed_mcp: true,
                model_selection: true,
                image_input: ProviderKind::Pi.supports_image_input(),
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
        cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        super::types::resolve_execution_config(
            context.manifest.provider,
            prompt.permission_mode.as_deref(),
            prompt.work_mode.as_deref(),
            prompt.permission_profile.as_deref(),
            prompt.sandbox_mode.as_deref(),
            prompt.approval_policy.as_deref(),
        )?;
        let native_session_id = context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| context.manifest.id.clone());
        let mut spec = CommandSpec::new(&self.binary, &context.manifest.workspace);
        spec.args = vec![
            "--mode".to_owned(),
            "rpc".to_owned(),
            if context.provider_state.native_session_id.is_some() {
                "--session"
            } else {
                "--session-id"
            }
            .to_owned(),
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

        let conversation_id = context.manifest.id.clone();
        let handle = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, handle| !handle.turns.is_closed());
            if let Some(handle) = sessions.get(&conversation_id) {
                handle.clone()
            } else {
                if sessions.len() >= 32 {
                    return Err(AppError::Conflict("Pi has reached its limit of 32 live sessions; close an idle conversation first".to_owned()));
                }
                let process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
                let (turns, turn_rx) = mpsc::channel(1);
                let (controls, control_rx) = mpsc::channel(32);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let handle = PiSessionHandle {
                    turns,
                    controls,
                    shutdown,
                };
                tokio::spawn(pi_session_worker(process, turn_rx, control_rx, shutdown_rx));
                sessions.insert(conversation_id.clone(), handle.clone());
                handle
            }
        };
        let (result, response) = oneshot::channel();
        handle
            .turns
            .send(PiTurnRequest {
                context,
                prompt,
                sink,
                cancel: cancel.clone(),
                result,
            })
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "Pi session closed before accepting the turn".to_owned(),
                )
            })?;
        response
            .await
            .map_err(|_| AppError::ProviderUnavailable("Pi session worker stopped".to_owned()))?
    }
}

async fn pi_session_worker(
    mut process: JsonLineProcess,
    mut turns: mpsc::Receiver<PiTurnRequest>,
    mut controls: mpsc::Receiver<PendingProviderControl>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut has_run = false;
    let mut idle_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        if *shutdown.borrow() {
            break;
        }
        let turn = tokio::select! {
            turn = turns.recv() => match turn { Some(turn) => turn, None => break },
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep_until(idle_deadline) => break,
            control = controls.recv() => {
                if let Some(control) = control { let _ = control.respond_to.send(Err(AppError::Conflict("Pi turn is no longer active".to_owned()))); }
                continue;
            }
            frame = process.read(), if has_run => {
                match frame {
                    Ok(Some(frame)) => {
                        if frame.get("type").and_then(Value::as_str) == Some("extension_ui_request") && matches!(frame.get("method").and_then(Value::as_str), Some("select" | "confirm" | "input" | "editor")) {
                            if process.send(&json!({"type":"extension_ui_response","id":frame.get("id"),"cancelled":true})).await.is_err() { break; }
                        }
                    }
                    _ => break,
                }
                continue;
            }
        };
        has_run = true;
        let native = turn
            .context
            .provider_state
            .native_session_id
            .clone()
            .unwrap_or_else(|| turn.context.manifest.id.clone());
        let mut cancel = turn.cancel;
        let mut queue = Vec::new();
        let result = tokio::select! {
            result = run_pi_turn(&mut process, turn.context, turn.prompt, native, &turn.sink, &mut cancel, &mut controls, &mut queue) => result,
            _ = shutdown.changed() => Err(AppError::TurnCancelled),
        };
        let failed = result.is_err() || result.as_ref().is_ok_and(|result| result.cancelled);
        if failed {
            process.terminate().await;
            if let Err(error) = pause_pi_queue(&turn.sink, &mut queue).await {
                tracing::error!(error = %error, "failed to persist Pi queue closure");
            }
            turns.close();
            controls.close();
        }
        idle_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        let _ = turn.result.send(result);
        if failed {
            break;
        }
    }
    process.terminate().await;
    turns.close();
    controls.close();
    while let Ok(control) = controls.try_recv() {
        let _ = control
            .respond_to
            .send(Err(AppError::Conflict("Pi session closed".to_owned())));
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
                image_input: Some(item.get("input").and_then(Value::as_array).is_some_and(
                    |inputs| inputs.iter().any(|input| input.as_str() == Some("image")),
                )),
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

fn pi_model_supports_images(item: &Value) -> bool {
    item.get("input")
        .and_then(Value::as_array)
        .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")))
}

async fn run_pi_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    native_session_id: String,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    controls: &mut mpsc::Receiver<PendingProviderControl>,
    queue: &mut Vec<PiQueueItem>,
) -> Result<DriverTurnResult, AppError> {
    let mut rpc = PiRpc::new(process, Some(sink.clone()), cancel.clone());
    rpc.process
        .send(&json!({ "id": "state", "type": "get_state" }))
        .await?;
    let mut state = rpc.wait_response("state").await?;
    verify_pi_session(&state, context.provider_state.native_session_id.as_deref())?;
    if prompt.model.is_some() || prompt.reasoning_effort.is_some() {
        pi_configure(
            &mut rpc,
            "turn-config",
            prompt.model.as_deref(),
            prompt.reasoning_effort.as_deref(),
        )
        .await?;
        rpc.process
            .send(&json!({"id":"configured-state","type":"get_state"}))
            .await?;
        state = rpc.wait_response("configured-state").await?;
    }
    sink.emit("turn.configuration", pi_configuration(&state, &prompt))
        .await?;
    let has_images = prompt
        .content
        .iter()
        .any(|content| matches!(content, super::types::DriverPromptContent::Image { .. }));
    if has_images
        && !state
            .pointer("/data/model")
            .is_some_and(pi_model_supports_images)
    {
        return Err(AppError::ImageInputUnsupported(
            "the selected Pi model does not support image input".to_owned(),
        ));
    }

    let mut provider_state = context.provider_state;
    provider_state.native_session_id = Some(native_session_id.clone());
    provider_state.recoverable = true;
    provider_state.last_error = None;
    sink.save_provider_state(provider_state.clone()).await?;

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
    rpc.process.send(&request).await?;
    let accepted = rpc.wait_response(&prompt.turn_id).await?;
    if accepted.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(pi_response_error(&accepted, "prompt"));
    }

    // This is a barrier after the authoritative prompt acknowledgement, not idle polling.
    // Pi's normal prompt calls _runAgentPrompt -> agent.prompt -> runWithLifecycle
    // synchronously after preflightResult(true), setting isStreaming before stdin can
    // process this command. A handled extension/input command instead returns idle.
    rpc.process
        .send(&json!({"id":"accepted-state","type":"get_state"}))
        .await?;
    let accepted_state = rpc.wait_response("accepted-state").await?;
    let handled_without_run = pi_is_idle(&accepted_state)
        && !rpc.events.iter().any(|event| {
            matches!(
                event.get("type").and_then(Value::as_str),
                Some("agent_start" | "agent_end")
            )
        });
    let mut stop_reason = "completed".to_owned();
    let mut terminal_error = None;
    let mut extension_error = None;
    let mut message_sequence = 0u64;
    let mut message_open = false;
    let mut active_message_id = format!("{}-message-1", prompt.turn_id);
    loop {
        if handled_without_run && rpc.events.is_empty() {
            rpc.close_dialogs().await;
            save_pi_state(sink, &mut provider_state, &accepted_state, &prompt).await?;
            return pi_turn_result(
                provider_state.native_session_id.clone(),
                stop_reason.clone(),
                if matches!(stop_reason.as_str(), "stop" | "length" | "aborted") {
                    terminal_error
                } else {
                    terminal_error.or(extension_error)
                },
            );
        }
        let next = tokio::select! {
            biased;
            event = rpc.next_event() => event,
            control = controls.recv() => {
                if let Some(control) = control {
                    if control.expected_turn_id != prompt.turn_id || control.respond_to.is_closed() || rpc.events.iter().any(|event| event.get("type").and_then(Value::as_str) == Some("agent_settled")) {
                        let _ = control.respond_to.send(Err(AppError::Conflict("Pi live control targets a stale turn".to_owned())));
                    } else {
                        let result = pi_control(&mut rpc, &prompt, sink, queue, &control.request_id, control.control).await;
                        let uncertain = matches!(&result, Err(AppError::ProviderUnavailable(_) | AppError::Io(_) | AppError::StreamClosed | AppError::Anyhow(_)));
                        let _ = control.respond_to.send(result);
                        if uncertain { return Err(AppError::ProviderUnavailable("Pi live control outcome is unknown; the resident process was closed".to_owned())); }
                    }
                }
                continue;
            }
        };
        let message = match next {
            Err(AppError::TurnCancelled) => {
                rpc.close_dialogs().await;
                wait_for_pi_abort(rpc.process, sink).await?;
                return Err(AppError::TurnCancelled);
            }
            result => result?,
        };
        if message.is_null() {
            continue;
        }
        match message.get("type").and_then(Value::as_str) {
            Some("agent_settled") => {
                rpc.close_dialogs().await;
                rpc.process
                    .send(&json!({"id":"settled-state","type":"get_state"}))
                    .await?;
                let state = rpc.wait_response("settled-state").await?;
                save_pi_state(sink, &mut provider_state, &state, &prompt).await?;
                return pi_turn_result(
                    provider_state.native_session_id.clone(),
                    stop_reason.clone(),
                    if matches!(stop_reason.as_str(), "stop" | "length" | "aborted") {
                        terminal_error
                    } else {
                        terminal_error.or(extension_error)
                    },
                );
            }
            Some("queue_update") => {
                reconcile_pi_queue(&message, queue);
                emit_pi_queue(sink, queue).await?;
            }
            Some("message_start") => {
                if message.pointer("/message/role").and_then(Value::as_str) == Some("user") {
                    let text = pi_user_text(&message);
                    if let Some(item) = queue.iter_mut().find(|item| {
                        item.native_text == text
                            && matches!(item.status.as_str(), "queued" | "delivering")
                    }) {
                        item.status = "consumed".to_owned();
                        sink.emit("message.created", json!({"provider":"pi","turnId":prompt.turn_id,"role":"user","text":text,"clientRequestId":item.id})).await?;
                        emit_pi_queue(sink, queue).await?;
                    }
                }
                message_sequence = message_sequence.saturating_add(1);
                message_open = true;
                active_message_id = pi_message_id(&message)
                    .unwrap_or_else(|| format!("{}-message-{message_sequence}", prompt.turn_id));
            }
            Some("extension_error") => {
                extension_error = Some(
                    message
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("Pi extension failed")
                        .to_owned(),
                );
                sink.emit(
                    "provider.event",
                    json!({"provider":"pi","providerMethod":"extension_error","metadata":message}),
                )
                .await?;
            }
            Some("compaction_start" | "compaction_end") => {
                let event_type = if message["type"] == "compaction_start" {
                    "compaction.started"
                } else if message["aborted"] == true {
                    "compaction.cancelled"
                } else if message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .is_some()
                {
                    "compaction.failed"
                } else {
                    "compaction.completed"
                };
                sink.emit(event_type, json!({ "provider": "pi", "turnId": prompt.turn_id, "source": "provider", "reason": message.get("reason"), "result": message.get("result"), "error": message.get("errorMessage") })).await?;
            }
            Some("message_end") => {
                if !message_open {
                    message_sequence = message_sequence.saturating_add(1);
                }
                message_open = false;
                for (event_type, payload) in
                    pi_completed_message_events(&message, &prompt.turn_id, message_sequence)
                {
                    sink.emit(event_type, payload).await?;
                }
                let reason = message
                    .pointer("/message/stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if message.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
                    && !reason.is_empty()
                {
                    stop_reason = reason.to_owned();
                    terminal_error = (reason == "error").then(|| {
                        message
                            .pointer("/message/errorMessage")
                            .and_then(Value::as_str)
                            .unwrap_or("Pi model request failed")
                            .to_owned()
                    });
                }
            }
            Some("message_update") => {
                let delta = message
                    .get("assistantMessageEvent")
                    .cloned()
                    .unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                let (event_type, category) = if delta_type.starts_with("thinking_") {
                    ("thought.delta", "reasoning")
                } else if delta_type.starts_with("toolcall_") {
                    ("tool.updated", "tool")
                } else {
                    ("message.delta", "assistant_progress")
                };
                let phase = if delta_type.ends_with("_start") {
                    "started"
                } else if delta_type.ends_with("_end") {
                    "completed"
                } else {
                    "delta"
                };
                let content_index = delta.get("contentIndex").and_then(Value::as_u64);
                let block_id = pi_delta_block_id(&delta, &active_message_id, category);
                sink.emit(
                    event_type,
                    json!({
                        "provider": "pi",
                        "role": "assistant",
                        "delta": delta,
                        "usage": message.get("usage"),
                        "block": {
                            "category": category,
                            "id": block_id,
                            "turnId": prompt.turn_id,
                            "phase": phase,
                            "contentIndex": content_index,
                        },
                    }),
                )
                .await?;
            }
            Some("tool_execution_start") => {
                sink.emit(
                    "tool.started",
                    pi_tool_payload(&message, &prompt.turn_id, "started"),
                )
                .await?;
            }
            Some("tool_execution_update") => {
                sink.emit(
                    "tool.updated",
                    pi_tool_payload(&message, &prompt.turn_id, "delta"),
                )
                .await?;
            }
            Some("tool_execution_end") => {
                sink.emit(
                    "tool.completed",
                    pi_tool_payload(&message, &prompt.turn_id, "completed"),
                )
                .await?;
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

#[derive(Clone, serde::Serialize)]
struct PiQueueItem {
    id: String,
    text: String,
    status: String,
    #[serde(skip)]
    native_text: String,
}

fn pi_user_text(message: &Value) -> String {
    message
        .pointer("/message/content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn reconcile_pi_queue(event: &Value, queue: &mut Vec<PiQueueItem>) {
    let Some(pending) = event.get("followUp").and_then(Value::as_array) else {
        return;
    };
    let mut pending = pending.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    for item in queue
        .iter_mut()
        .filter(|item| matches!(item.status.as_str(), "queued" | "delivering"))
    {
        if let Some(index) = pending.iter().position(|text| *text == item.native_text) {
            pending.remove(index);
            item.status = "queued".to_owned();
        } else {
            item.status = "delivering".to_owned();
        }
    }
    for text in pending {
        queue.push(PiQueueItem {
            id: format!("pi-native-{}", uuid::Uuid::new_v4().simple()),
            text: text.to_owned(),
            native_text: text.to_owned(),
            status: "queued".to_owned(),
        });
    }
}

fn pi_added_native_text(
    events: &VecDeque<Value>,
    offset: usize,
    queue: &[PiQueueItem],
    text: &str,
) -> Result<String, AppError> {
    // Ordinary follow_up text is unchanged by Pi. Only slash templates/skills
    // need identity reconciliation with their expanded native content.
    if !text.starts_with('/') {
        return Ok(text.to_owned());
    }
    let mut previous = queue
        .iter()
        .filter(|item| item.status == "queued")
        .map(|item| item.native_text.clone())
        .collect::<Vec<_>>();
    let mut appended = Vec::new();
    for event in events.iter().skip(offset) {
        if event.get("type").and_then(Value::as_str) != Some("queue_update") {
            continue;
        }
        let Some(items) = event.get("followUp").and_then(Value::as_array) else {
            continue;
        };
        let current = items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if current.len() == previous.len() + 1 && current[..previous.len()] == previous {
            appended.push(current.last().unwrap().clone());
        }
        previous = current;
    }
    if appended.len() == 1 {
        return Ok(appended.remove(0));
    }
    Err(AppError::ProviderUnavailable("Pi accepted the queue input but its expanded identity is ambiguous; it will not be submitted again".to_owned()))
}

async fn pause_pi_queue(sink: &DriverEventSink, queue: &mut [PiQueueItem]) -> Result<(), AppError> {
    let mut unresolved = false;
    for item in queue
        .iter_mut()
        .filter(|item| matches!(item.status.as_str(), "queued" | "delivering"))
    {
        // Cancellation/error can race the native delivery notification. The
        // process is gone, but we cannot claim this input was never executed.
        item.status = "unknown".to_owned();
        unresolved = true;
    }
    if unresolved {
        sink.emit(
            "queue.updated",
            json!({"provider":"pi","items":queue,"paused":true}),
        )
        .await?;
        sink.emit("queue.paused", json!({"provider":"pi","reason":"native_session_closed","message":"The Pi process closed. Pending input delivery is unknown and will not be replayed automatically; inspect the conversation before submitting again."})).await?;
    }
    Ok(())
}

async fn emit_pi_queue(sink: &DriverEventSink, queue: &[PiQueueItem]) -> Result<(), AppError> {
    sink.emit("queue.updated", json!({"provider":"pi","items":queue}))
        .await?;
    Ok(())
}

async fn pi_configure(
    rpc: &mut PiRpc<'_>,
    id: &str,
    model: Option<&str>,
    thinking: Option<&str>,
) -> Result<(), AppError> {
    // Validate the entire request before applying either part.
    let model = model
        .map(|model| {
            model
                .split_once('/')
                .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "Pi model must include its provider (provider/model)".to_owned(),
                    )
                })
        })
        .transpose()?;
    if thinking.is_some_and(|level| {
        !matches!(
            level,
            "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        )
    }) {
        return Err(AppError::InvalidRequest(
            "Unsupported Pi thinking level".to_owned(),
        ));
    }
    if let Some((provider, model)) = model {
        let request_id = format!("{id}-model");
        rpc.process
            .send(&json!({"id":request_id,"type":"set_model","provider":provider,"modelId":model}))
            .await?;
        rpc.wait_response(&request_id).await?;
    }
    if let Some(level) = thinking {
        let request_id = format!("{id}-thinking");
        rpc.process
            .send(&json!({"id":request_id,"type":"set_thinking_level","level":level}))
            .await?;
        rpc.wait_response(&request_id).await?;
    }
    Ok(())
}

async fn pi_control(
    rpc: &mut PiRpc<'_>,
    prompt: &DriverPrompt,
    sink: &DriverEventSink,
    queue: &mut Vec<PiQueueItem>,
    request_id: &str,
    control: ProviderControl,
) -> Result<Value, AppError> {
    match control {
        ProviderControl::Steer { text } => {
            rpc.process
                .send(&json!({"id":request_id,"type":"steer","message":text}))
                .await?;
            rpc.wait_response(request_id).await?;
            sink.emit("message.created", json!({"provider":"pi","turnId":prompt.turn_id,"role":"user","text":text,"clientRequestId":request_id})).await?;
            Ok(json!({"accepted":true,"effectiveAt":"next-model-request"}))
        }
        ProviderControl::Configure {
            model,
            reasoning_effort,
        } => {
            let result = pi_configure(
                rpc,
                request_id,
                model.as_deref(),
                reasoning_effort.as_deref(),
            )
            .await;
            let state_id = format!("{request_id}-state");
            rpc.process
                .send(&json!({"id":state_id,"type":"get_state"}))
                .await?;
            let state = rpc.wait_response(&state_id).await?;
            let mut requested = prompt.clone();
            requested.model = model;
            requested.reasoning_effort = reasoning_effort;
            let mut config = pi_configuration(&state, &requested);
            config["requestId"] = json!(request_id);
            config["effectiveAt"] = json!("next-model-request");
            sink.emit("turn.configuration", config.clone()).await?;
            result?;
            Ok(config)
        }
        ProviderControl::QueueAdd { item_id, text } => {
            if let Some(item) = queue.iter().find(|item| item.id == item_id) {
                if item.text != text {
                    return Err(AppError::Conflict(
                        "Queue item id already refers to different text".to_owned(),
                    ));
                }
                return Ok(json!({"provider":"pi","items":queue}));
            }
            if queue.len() >= 100 {
                return Err(AppError::Conflict(
                    "Pi turn queue is limited to 100 items".to_owned(),
                ));
            }
            let event_offset = rpc.events.len();
            rpc.process
                .send(&json!({"id":request_id,"type":"follow_up","message":text}))
                .await?;
            rpc.wait_response(request_id).await?;
            let native_text = pi_added_native_text(&rpc.events, event_offset, queue, &text)?;
            queue.push(PiQueueItem {
                id: item_id,
                text,
                native_text,
                status: "queued".to_owned(),
            });
            emit_pi_queue(sink, queue).await?;
            Ok(json!({"provider":"pi","items":queue}))
        }
        ProviderControl::QueueList => Ok(json!({"provider":"pi","items":queue})),
        ProviderControl::QueueClear => {
            rpc.process
                .send(&json!({"id":request_id,"type":"clear_queue"}))
                .await?;
            let response = rpc.wait_response(request_id).await?;
            let mut cleared = response
                .pointer("/data/followUp")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in queue
                .iter_mut()
                .filter(|item| matches!(item.status.as_str(), "queued" | "delivering"))
            {
                if let Some(index) = cleared
                    .iter()
                    .position(|text| text.as_str() == Some(item.native_text.as_str()))
                {
                    cleared.remove(index);
                    item.status = "cleared".to_owned();
                }
            }
            emit_pi_queue(sink, queue).await?;
            Ok(json!({"provider":"pi","items":queue,"cleared":response.get("data")}))
        }
        ProviderControl::QueueRemove { .. } => Err(AppError::Unsupported(
            "Pi cannot atomically remove one queued message; use clear pending inputs instead"
                .to_owned(),
        )),
    }
}

/// One reader owns stdout. Responses are correlated without discarding events;
/// extension dialogs wait independently so timeout/agent events continue to flow.
struct PiRpc<'a> {
    process: &'a mut JsonLineProcess,
    sink: Option<DriverEventSink>,
    cancel: watch::Receiver<bool>,
    events: VecDeque<Value>,
    dialogs: JoinSet<Result<Value, AppError>>,
    dialog_cancel: watch::Sender<bool>,
    dialog_wait: Duration,
    buffered_bytes: usize,
}

impl<'a> PiRpc<'a> {
    fn new(
        process: &'a mut JsonLineProcess,
        sink: Option<DriverEventSink>,
        cancel: watch::Receiver<bool>,
    ) -> Self {
        let (dialog_cancel, _) = watch::channel(false);
        Self {
            process,
            sink,
            cancel,
            events: VecDeque::new(),
            dialogs: JoinSet::new(),
            dialog_cancel,
            dialog_wait: Duration::ZERO,
            buffered_bytes: 0,
        }
    }

    async fn close_dialogs(&mut self) {
        self.dialog_cancel.send_replace(true);
        while self.dialogs.join_next().await.is_some() {}
    }

    async fn read_frame(
        &mut self,
        mut deadline: Option<tokio::time::Instant>,
    ) -> Result<Value, AppError> {
        loop {
            if *self.cancel.borrow() {
                return Err(AppError::TurnCancelled);
            }
            let waiting_for_dialog = !self.dialogs.is_empty();
            let started = tokio::time::Instant::now();
            let read_deadline = deadline.filter(|_| !waiting_for_dialog);
            let message = tokio::select! {
                value = async {
                    match read_deadline {
                        Some(deadline) => self.process.read_control_until(deadline).await,
                        None => self.process.read().await,
                    }
                } => value?,
                _ = self.cancel.changed() => return Err(AppError::TurnCancelled),
                answer = self.dialogs.join_next(), if !self.dialogs.is_empty() => {
                    let answer = answer.expect("nonempty dialog task set").map_err(|error| AppError::ProviderUnavailable(format!("Pi input task failed: {error}")))??;
                    if !answer.is_null() { self.process.send(&answer).await?; }
                    if waiting_for_dialog {
                        let elapsed = started.elapsed();
                        self.dialog_wait += elapsed;
                        deadline = deadline.map(|deadline| deadline + elapsed);
                    }
                    continue;
                }
            };
            let Some(message) = message else {
                return Err(
                    provider_exit_error(self.process, "Pi RPC process closed stdout").await,
                );
            };
            if waiting_for_dialog {
                let elapsed = started.elapsed();
                self.dialog_wait += elapsed;
                deadline = deadline.map(|deadline| deadline + elapsed);
            }
            if message.get("type").and_then(Value::as_str) != Some("extension_ui_request") {
                return Ok(message);
            }
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            if matches!(method, "select" | "confirm" | "input" | "editor") {
                if self.dialogs.len() >= 32 {
                    return Err(AppError::ProviderUnavailable(
                        "Pi exceeded the concurrent input dialog limit".to_owned(),
                    ));
                }
                if let Some(sink) = &self.sink {
                    self.dialogs.spawn(resolve_extension_ui(
                        message,
                        sink.clone(),
                        self.cancel.clone(),
                        self.dialog_cancel.subscribe(),
                    ));
                } else {
                    // Maintenance operations have no interactive sink. Cancel the request
                    // rather than hanging or approving an extension's action implicitly.
                    self.process.send(&json!({"type":"extension_ui_response","id":message.get("id"),"cancelled":true})).await?;
                }
            } else if let Some(sink) = &self.sink {
                sink.emit("provider.event", json!({"provider":"pi","providerMethod":"extension_ui_request","metadata":message})).await?;
            }
        }
    }

    async fn wait_response(&mut self, id: &str) -> Result<Value, AppError> {
        self.wait_response_for(id, super::process::control_timeout()?)
            .await
    }

    async fn wait_response_for(&mut self, id: &str, timeout: Duration) -> Result<Value, AppError> {
        let mut deadline = tokio::time::Instant::now() + timeout;
        loop {
            let dialog_wait = self.dialog_wait;
            let message = self.read_frame(Some(deadline)).await?;
            deadline += self.dialog_wait - dialog_wait;
            if message.get("type").and_then(Value::as_str) == Some("response")
                && message.get("id").and_then(Value::as_str) == Some(id)
            {
                match message.get("success").and_then(Value::as_bool) {
                    Some(true) => return Ok(message),
                    Some(false) => return Err(pi_response_error(&message, id)),
                    None => {
                        return Err(AppError::ProviderUnavailable(
                            "Pi sent a control response without an outcome".to_owned(),
                        ))
                    }
                }
            }
            // Bound unexpected notifications while a provider withholds its response.
            self.buffered_bytes = self
                .buffered_bytes
                .saturating_add(message.to_string().len());
            if self.events.len() >= 1024 || self.buffered_bytes > 16 * 1024 * 1024 {
                return Err(AppError::ProviderUnavailable(
                    "Pi exceeded the event buffer limit before its control response".to_owned(),
                ));
            }
            self.events.push_back(message);
        }
    }

    async fn next_event(&mut self) -> Result<Value, AppError> {
        if *self.cancel.borrow() {
            return Err(AppError::TurnCancelled);
        }
        if let Some(event) = self.events.pop_front() {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(event.to_string().len());
            return Ok(event);
        }
        self.read_frame(None).await
    }
}

async fn resolve_extension_ui(
    request: Value,
    sink: DriverEventSink,
    mut parent_cancel: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<Value, AppError> {
    let Some(id) = request.get("id").and_then(Value::as_str) else {
        return Ok(Value::Null);
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let (cancel_tx, mut cancel) = watch::channel(false);
    let decision = sink.request_permission(
        id.to_owned(),
        "extension_ui",
        request
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Pi requests input"),
        request.clone(),
        json!([
            {"id":"answer","kind":"answer","name":"Respond"},
            {"id":"reject_once","kind":"reject_once","name":"Cancel"}
        ]),
        &mut cancel,
    );
    tokio::pin!(decision);
    let timeout = request
        .get("timeout")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(10 * 60 * 1000);
    let response = tokio::select! {
        result = &mut decision => match result {
            Ok(decision) => decision,
            Err(AppError::InvalidRequest(_)) => return Ok(json!({"type":"extension_ui_response","id":id,"cancelled":true})),
            Err(error) => return Err(error),
        },
        _ = tokio::time::sleep(Duration::from_millis(timeout)) => {
            cancel_tx.send_replace(true);
            let _ = decision.await;
            return Ok(if request.get("timeout").and_then(Value::as_u64).is_some_and(|value| value > 0) { Value::Null } else { json!({"type":"extension_ui_response","id":id,"cancelled":true}) });
        }
        _ = async { if !*parent_cancel.borrow() { let _ = parent_cancel.changed().await; } } => {
            cancel_tx.send_replace(true);
            let _ = decision.await;
            return Ok(Value::Null);
        }
        _ = async { if !*shutdown.borrow() { let _ = shutdown.changed().await; } } => {
            cancel_tx.send_replace(true);
            let _ = decision.await;
            return Ok(Value::Null);
        }
    };
    Ok(match response.outcome {
        PermissionOutcome::RejectOnce
        | PermissionOutcome::RejectAlways
        | PermissionOutcome::AbortTurn => {
            json!({"type":"extension_ui_response","id":id,"cancelled":true})
        }
        _ if method == "confirm" => {
            json!({"type":"extension_ui_response","id":id,"confirmed":response.data.as_ref().and_then(|data| data.get("confirmed")).and_then(Value::as_bool).unwrap_or(false)})
        }
        _ => {
            json!({"type":"extension_ui_response","id":id,"value":response.data.as_ref().and_then(|data| data.get("value")).and_then(Value::as_str).unwrap_or_default()})
        }
    })
}

fn pi_is_idle(state: &Value) -> bool {
    state.pointer("/data/isStreaming").and_then(Value::as_bool) == Some(false)
        && state.pointer("/data/isCompacting").and_then(Value::as_bool) == Some(false)
        && state
            .pointer("/data/pendingMessageCount")
            .and_then(Value::as_u64)
            == Some(0)
}

fn verify_pi_session(state: &Value, expected: Option<&str>) -> Result<(), AppError> {
    if let Some(expected) = expected {
        if state.pointer("/data/sessionId").and_then(Value::as_str) != Some(expected) {
            return Err(AppError::ProviderUnavailable(
                "Pi resumed a different session; refusing to continue without the original context"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

fn pi_configuration(state: &Value, prompt: &DriverPrompt) -> Value {
    json!({"provider":"pi","turnId":prompt.turn_id,
        "requested":{"model":prompt.model,"reasoningEffort":prompt.reasoning_effort},
        "effective":{"model":state.pointer("/data/model").and_then(pi_model_id),"reasoningEffort":state.pointer("/data/thinkingLevel"),"source":"provider-confirmed"}})
}

async fn save_pi_state(
    sink: &DriverEventSink,
    provider_state: &mut crate::conversation::ProviderState,
    state: &Value,
    prompt: &DriverPrompt,
) -> Result<(), AppError> {
    if let Some(id) = state
        .pointer("/data/sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        provider_state.native_session_id = Some(id.to_owned());
    }
    sink.save_provider_state(provider_state.clone()).await?;
    sink.emit("turn.configuration", pi_configuration(state, prompt))
        .await?;
    Ok(())
}

fn pi_turn_result(
    native_session_id: Option<String>,
    stop_reason: String,
    error: Option<String>,
) -> Result<DriverTurnResult, AppError> {
    if stop_reason == "error" || error.is_some() {
        return Err(AppError::ProviderUnavailable(
            error.unwrap_or_else(|| "Pi model request failed".to_owned()),
        ));
    }
    Ok(DriverTurnResult {
        native_session_id,
        cancelled: stop_reason == "aborted",
        stop_reason,
    })
}

async fn wait_for_pi_abort(
    process: &mut JsonLineProcess,
    _sink: &DriverEventSink,
) -> Result<(), AppError> {
    process
        .send(&json!({ "id": "abort", "type": "abort" }))
        .await?;
    tokio::time::timeout(super::process::cancel_timeout()?, async {
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
                process.send(&json!({"type":"extension_ui_response","id":message.get("id"),"cancelled":true})).await?;
            }
        }
    })
    .await
    .map_err(|_| AppError::ProviderUnavailable("Pi abort timed out".to_owned()))?
}

fn pi_tool_payload(message: &Value, turn_id: &str, phase: &str) -> Value {
    let tool_call_id = message
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{turn_id}-tool-{}",
                message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )
        });
    json!({
        "provider": "pi",
        "toolCallId": message.get("toolCallId"),
        "toolName": message.get("toolName"),
        "arguments": message.get("args"),
        "partialResult": message.get("partialResult"),
        "result": message.get("result"),
        "isError": message.get("isError"),
        "block": {
            "category": "tool",
            "id": tool_call_id,
            "turnId": turn_id,
            "phase": phase,
        },
    })
}

fn is_pi_final_message(message: &Value) -> bool {
    message.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
        && message
            .pointer("/message/stopReason")
            .and_then(Value::as_str)
            .is_some_and(|reason| matches!(reason, "stop" | "length"))
}

fn pi_delta_block_id(delta: &Value, turn_id: &str, category: &str) -> String {
    delta
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| delta.pointer("/toolCall/id").and_then(Value::as_str))
        .or_else(|| {
            delta
                .get("contentIndex")
                .and_then(Value::as_u64)
                .and_then(|index| {
                    delta
                        .pointer("/partial/content")
                        .and_then(Value::as_array)?
                        .get(index as usize)
                })
                .and_then(|part| part.get("id"))
                .and_then(Value::as_str)
        })
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            let index = delta
                .get("contentIndex")
                .and_then(Value::as_u64)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "current".to_owned());
            format!("{turn_id}-{category}-{index}")
        })
}

/// The usage and final-message events represent one native message and must
/// share identity even when Pi omitted its optional text signature.
fn pi_completed_message_events(
    message: &Value,
    turn_id: &str,
    sequence: u64,
) -> Vec<(&'static str, Value)> {
    let message_id =
        pi_message_id(message).unwrap_or_else(|| format!("{turn_id}-message-{sequence}"));
    let mut events = Vec::new();
    if let Some(usage) = message
        .pointer("/message/usage")
        .filter(|usage| usage.is_object())
    {
        events.push((
            "usage.updated",
            json!({
                "provider": "pi", "turnId": turn_id, "messageId": message_id,
                "source": "provider", "scope": "message", "usage": usage,
            }),
        ));
    }
    // Tool-use messages carry usage but are not the turn's final answer.
    if is_pi_final_message(message) {
        events.push(("message.completed", json!({
            "provider": "pi", "turnId": turn_id, "messageId": message_id,
            "role": "assistant", "message": message.get("message"),
            "block": { "category": "assistant_final", "id": message_id, "turnId": turn_id, "phase": "completed" },
        })));
    }
    events
}

fn pi_message_id(message: &Value) -> Option<String> {
    if let Some(id) = message
        .pointer("/message/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        return Some(id.to_owned());
    }
    message
        .pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|content| {
            content.iter().find_map(|part| {
                part.get("textSignature")
                    .and_then(Value::as_str)
                    .and_then(|signature| serde_json::from_str::<Value>(signature).ok())
                    .and_then(|signature| {
                        signature
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(ToOwned::to_owned)
                    })
            })
        })
}

fn pi_response_error(response: &Value, command: &str) -> AppError {
    let error = response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("Pi rejected the command");
    AppError::InvalidRequest(format!("Pi {command} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_messages_share_usage_identity_with_final_output() {
        let message = json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "stop", "content": [{ "type": "text", "text": "answer" }], "usage": { "input": 10, "output": 4 } } });
        let events = pi_completed_message_events(&message, "turn-local", 1);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "usage.updated");
        assert_eq!(events[1].0, "message.completed");
        assert_eq!(events[0].1["messageId"], "turn-local-message-1");
        assert_eq!(events[0].1["messageId"], events[1].1["messageId"]);
        assert_eq!(events[0].1["messageId"], events[1].1["block"]["id"]);
        assert_ne!(
            events[0].1["messageId"],
            pi_completed_message_events(&message, "turn-local", 2)[0].1["messageId"]
        );
        let mut tool_message = message;
        tool_message["message"]["stopReason"] = json!("toolUse");
        tool_message["message"]["id"] = json!("native-message");
        let events = pi_completed_message_events(&tool_message, "turn-local", 3);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1["messageId"], "native-message");
    }

    #[test]
    fn parses_model_specific_thinking_levels_and_default() {
        let response = json!({"data":{"models":[
            {"provider":"zai","id":"glm-5.3","input":["text","image"],"reasoning":true,"thinkingLevelMap":{"off":null,"xhigh":"xhigh","max":"max"}},
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
        assert_eq!(models[0].image_input, Some(true));
        assert_eq!(models[1].image_input, Some(false));
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

    #[test]
    fn only_terminal_assistant_messages_are_final_answers() {
        assert!(!is_pi_final_message(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "stopReason": "toolUse" }
        })));
        assert!(!is_pi_final_message(&json!({
            "type": "message_end",
            "message": { "role": "toolResult" }
        })));
        assert!(is_pi_final_message(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "stopReason": "stop" }
        })));
    }

    #[test]
    fn delta_block_ids_separate_content_indexes_and_keep_tool_ids() {
        assert_eq!(
            pi_delta_block_id(&json!({ "contentIndex": 2 }), "turn-1", "reasoning"),
            "turn-1-reasoning-2"
        );
        assert_eq!(
            pi_delta_block_id(
                &json!({ "id": "call-1", "contentIndex": 2 }),
                "turn-1",
                "tool"
            ),
            "call-1"
        );
    }
    #[cfg(unix)]
    struct Fixture {
        root: std::path::PathBuf,
        driver: Arc<PiDriver>,
        store: crate::conversation::ConversationStore,
        manifest: crate::conversation::ConversationManifest,
        trust: crate::workspace_trust::WorkspaceTrustStore,
    }

    #[cfg(unix)]
    impl Fixture {
        async fn new() -> Self {
            use crate::conversation::{ConversationManifest, ConversationStore};
            use crate::workspace_trust::WorkspaceTrustStore;
            use std::os::unix::fs::PermissionsExt;
            let root = std::env::temp_dir().join(format!(
                "todex-pi-contract-{}",
                uuid::Uuid::new_v4().simple()
            ));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(root).await.unwrap();
            let script = root.join("pi.py");
            tokio::fs::write(
                &script,
                include_str!("../../tests/fixtures/pi_rpc_audit.py"),
            )
            .await
            .unwrap();
            tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .await
                .unwrap();
            let store = ConversationStore::new(root.join("data")).await.unwrap();
            let manifest = store
                .create(ConversationManifest::new(
                    ProviderKind::Pi,
                    root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap();
            let trust = WorkspaceTrustStore::new(root.join("trust"), root.clone())
                .await
                .unwrap();
            trust.set_owned("local", &root, true).await.unwrap();
            Self {
                root,
                driver: Arc::new(PiDriver {
                    binary: script.display().to_string(),
                    sessions: Arc::new(Mutex::new(HashMap::new())),
                }),
                store,
                manifest,
                trust,
            }
        }

        async fn start(
            &self,
            scenario: &str,
            turn: &str,
        ) -> tokio::task::JoinHandle<Result<DriverTurnResult, AppError>> {
            use super::super::types::PermissionBroker;
            use crate::conversation::ConversationEventHub;
            let driver = self.driver.clone();
            let context = DriverContext {
                manifest: self.manifest.clone(),
                provider_state: self.store.provider_state(&self.manifest.id).await.unwrap(),
            };
            let prompt = DriverPrompt {
                turn_id: turn.to_owned(),
                text: scenario.to_owned(),
                content: vec![],
                skills: vec![],
                model: None,
                reasoning_effort: None,
                permission_mode: None,
                work_mode: None,
                permission_profile: None,
                sandbox_mode: None,
                approval_policy: None,
            };
            let sink = DriverEventSink::new(
                self.store.clone(),
                ConversationEventHub::default(),
                PermissionBroker::default(),
                &self.manifest.id,
            )
            .with_turn_id(turn);
            let permit = self.trust.acquire_owned("local", &self.root).await.unwrap();
            tokio::spawn(async move {
                let (_cancel, receiver) = watch::channel(false);
                driver
                    .run_turn(context, prompt, sink, receiver, permit)
                    .await
            })
        }

        async fn run(&self, scenario: &str, turn: &str) -> Result<DriverTurnResult, AppError> {
            tokio::time::timeout(Duration::from_secs(3), self.start(scenario, turn).await)
                .await
                .expect("Pi turn hung")
                .unwrap()
        }

        async fn running(&self) {
            for _ in 0..200 {
                if self
                    .store
                    .complete_history(&self.manifest.id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| event.event_type == "tool.started")
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("fixture never started");
        }

        async fn finish(self) {
            self.driver.shutdown().await;
            tokio::fs::remove_dir_all(self.root).await.unwrap();
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_handles_pure_commands_preack_events_and_reuses_process() {
        let fixture = Fixture::new().await;
        assert!(!fixture.run("pure", "one").await.unwrap().cancelled);
        assert!(!fixture.run("preack", "two").await.unwrap().cancelled);
        assert!(!fixture.run("normal", "three").await.unwrap().cancelled);
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "message.completed"
                && event.payload.to_string().contains("before ack")));
        assert!(events
            .iter()
            .any(|event| event.event_type == "message.completed"
                && event.payload.to_string().contains("answer")));
        assert_eq!(
            tokio::fs::read_to_string(fixture.root.join("pi-launches"))
                .await
                .unwrap()
                .lines()
                .count(),
            1
        );
        fixture.finish().await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_distinguishes_final_error_and_successful_retry() {
        let fixture = Fixture::new().await;
        assert!(fixture
            .run("error", "failed")
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture model failure"));
        assert_eq!(
            fixture.run("retry", "retried").await.unwrap().stop_reason,
            "stop"
        );
        assert_eq!(
            fixture
                .run("extension-warning", "warned")
                .await
                .unwrap()
                .stop_reason,
            "stop"
        );
        assert!(fixture
            .run("extension-error", "command-failed")
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture extension failed"));
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(!events
            .iter()
            .any(|event| event.event_type == "message.completed"
                && event
                    .payload
                    .pointer("/message/stopReason")
                    .and_then(Value::as_str)
                    == Some("error")));
        fixture.finish().await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_separates_reasoning_messages_and_does_not_block_on_dialogs() {
        let fixture = Fixture::new().await;
        fixture.run("thoughts", "thought-turn").await.unwrap();
        fixture.run("dialog", "dialog-turn").await.unwrap();
        fixture
            .run("preack-dialog", "expired-dialog-turn")
            .await
            .unwrap();
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        let thoughts = events
            .iter()
            .filter(|event| event.event_type == "thought.delta")
            .collect::<Vec<_>>();
        assert_eq!(thoughts.len(), 2);
        assert_ne!(
            thoughts[0].payload.pointer("/block/id"),
            thoughts[1].payload.pointer("/block/id")
        );
        let requested = events
            .iter()
            .filter(|event| event.event_type == "permission.requested")
            .count();
        let resolved = events
            .iter()
            .filter(|event| event.event_type == "permission.resolved")
            .count();
        assert!(requested >= 3);
        assert_eq!(requested, resolved);
        fixture.finish().await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_controls_read_back_config_and_queue_consumption() {
        let fixture = Fixture::new().await;
        let running = fixture.start("hold", "active").await;
        fixture.running().await;
        let control = |id: &'static str, command| {
            fixture
                .driver
                .control(&fixture.manifest.id, "active", id, command)
        };
        let config = control(
            "config",
            ProviderControl::Configure {
                model: Some("fixture/other".to_owned()),
                reasoning_effort: Some("max".to_owned()),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            config.pointer("/effective/model"),
            Some(&json!("fixture/other"))
        );
        assert_eq!(
            config.pointer("/effective/reasoningEffort"),
            Some(&json!("high"))
        );
        assert!(fixture
            .driver
            .control(
                &fixture.manifest.id,
                "stale",
                "bad",
                ProviderControl::Steer {
                    text: "must not send".to_owned()
                }
            )
            .await
            .is_err());
        control(
            "queue-1",
            ProviderControl::QueueAdd {
                item_id: "q1".to_owned(),
                text: "next".to_owned(),
            },
        )
        .await
        .unwrap();
        control(
            "queue-2",
            ProviderControl::QueueAdd {
                item_id: "q2".to_owned(),
                text: "later".to_owned(),
            },
        )
        .await
        .unwrap();
        assert!(control(
            "remove",
            ProviderControl::QueueRemove {
                item_id: "q1".to_owned()
            }
        )
        .await
        .is_err());
        control("clear", ProviderControl::QueueClear).await.unwrap();
        control(
            "queue-3",
            ProviderControl::QueueAdd {
                item_id: "q3".to_owned(),
                text: "consume".to_owned(),
            },
        )
        .await
        .unwrap();
        control(
            "finish",
            ProviderControl::Steer {
                text: "finish".to_owned(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "message.created"
                && event.payload.get("clientRequestId") == Some(&json!("q3"))));
        assert!(!events
            .iter()
            .any(|event| event.event_type == "message.created"
                && event.payload.get("clientRequestId") == Some(&json!("q1"))));
        assert!(events
            .iter()
            .any(|event| event.event_type == "queue.updated"
                && event.payload.to_string().contains("consumed")));
        fixture.finish().await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_compact_clone_and_missing_resume_use_strict_session() {
        let fixture = Fixture::new().await;
        fixture.run("normal", "original").await.unwrap();
        let context = DriverContext {
            manifest: fixture.manifest.clone(),
            provider_state: fixture
                .store
                .provider_state(&fixture.manifest.id)
                .await
                .unwrap(),
        };
        let (_cancel, cancel) = watch::channel(false);
        fixture
            .driver
            .compact_session(
                context.clone(),
                cancel,
                fixture
                    .trust
                    .acquire_owned("local", &fixture.root)
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        let cloned = fixture
            .driver
            .fork_session(
                context,
                fixture
                    .trust
                    .acquire_owned("local", &fixture.root)
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cloned.native_session_id.as_deref(), Some("cloned-session"));
        let mut missing = fixture
            .store
            .provider_state(&fixture.manifest.id)
            .await
            .unwrap();
        missing.native_session_id = Some("missing".to_owned());
        fixture
            .store
            .save_provider_state(&fixture.manifest.id, missing)
            .await
            .unwrap();
        assert!(fixture.run("normal", "must-fail").await.is_err());
        let launches = tokio::fs::read_to_string(fixture.root.join("pi-launches"))
            .await
            .unwrap();
        assert!(launches
            .lines()
            .skip(1)
            .all(|line| line.contains("\"--session\"") && !line.contains("--session-id")));
        fixture.finish().await;
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_queue_consumption_before_add_ack_keeps_the_new_item_identity() {
        let fixture = Fixture::new().await;
        let running = fixture.start("hold", "race").await;
        fixture.running().await;
        for (id, text) in [
            ("old1", "first"),
            ("old2", "second"),
            ("expanded", "/expanded"),
        ] {
            fixture
                .driver
                .control(
                    &fixture.manifest.id,
                    "race",
                    id,
                    ProviderControl::QueueAdd {
                        item_id: id.to_owned(),
                        text: text.to_owned(),
                    },
                )
                .await
                .unwrap();
        }
        fixture
            .driver
            .control(
                &fixture.manifest.id,
                "race",
                "finish",
                ProviderControl::Steer {
                    text: "finish".to_owned(),
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        let delivery = events
            .iter()
            .find(|event| {
                event.event_type == "message.created"
                    && event.payload.get("clientRequestId") == Some(&json!("expanded"))
            })
            .unwrap();
        assert_eq!(
            delivery.payload.get("text"),
            Some(&json!("expanded queued input"))
        );
        fixture.finish().await;
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn rpc_shutdown_marks_pending_native_queue_unknown_and_paused() {
        let fixture = Fixture::new().await;
        let running = fixture.start("hold", "shutdown").await;
        fixture.running().await;
        fixture
            .driver
            .control(
                &fixture.manifest.id,
                "shutdown",
                "queued",
                ProviderControl::QueueAdd {
                    item_id: "pending".to_owned(),
                    text: "do not replay".to_owned(),
                },
            )
            .await
            .unwrap();
        fixture.driver.shutdown_session(&fixture.manifest.id).await;
        assert!(running.await.unwrap().is_err());
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        let snapshot = events
            .iter()
            .rev()
            .find(|event| event.event_type == "queue.updated")
            .unwrap();
        assert_eq!(snapshot.payload.get("paused"), Some(&json!(true)));
        assert_eq!(
            snapshot.payload.pointer("/items/0/status"),
            Some(&json!("unknown"))
        );
        assert!(events
            .iter()
            .any(|event| event.event_type == "queue.paused"));
        fixture.finish().await;
    }
}
