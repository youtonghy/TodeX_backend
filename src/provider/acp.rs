use std::collections::BTreeMap;

use agent_client_protocol::schema::{
    v1::{
        CancelNotification, ClientCapabilities, ClientSessionCapabilities, ContentBlock,
        Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
        LoadSessionResponse, NewSessionRequest, NewSessionResponse, PermissionOptionKind,
        PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigOption,
        SessionConfigOptionsCapabilities, SessionNotification, SetSessionConfigOptionResponse,
        TextContent,
    },
    ProtocolVersion,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::config::{AcpProfileConfig, AgentConfig};
use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::process::{
    executable_available, provider_exit_error, redact_sensitive_text, CommandSpec, JsonLineProcess,
};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, PermissionOutcome,
    ProviderCapabilities, ProviderDescriptor, ProviderDriver,
};

pub struct AcpDriver {
    profiles: BTreeMap<String, AcpProfileConfig>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AcpRuntimeOptions {
    pub authenticate: bool,
    pub auth_method: Option<String>,
    pub suppress_load_replay: bool,
    pub allow_cli_config_fallback: bool,
    pub request_ask_mode: bool,
    pub legacy_model_state: bool,
    pub nested_config_values: bool,
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
                managed_mcp: false,
                model_selection: false,
            },
            models: Vec::new(),
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
        let result = run_acp_turn(
            &mut process,
            context,
            prompt,
            &sink,
            &mut cancel,
            AcpRuntimeOptions::default(),
        )
        .await;
        process.terminate().await;
        result
    }
}

pub(super) async fn run_acp_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    options: AcpRuntimeOptions,
) -> Result<DriverTurnResult, AppError> {
    let provider = context.manifest.provider;
    let initialize = InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(
            ClientCapabilities::new().session(
                ClientSessionCapabilities::new()
                    .config_options(SessionConfigOptionsCapabilities::new()),
            ),
        )
        .client_info(
            Implementation::new("todex-agentd", env!("CARGO_PKG_VERSION")).title("TodeX 2.0"),
        );
    send_request(process, "initialize", "initialize", initialize).await?;
    let initialize_value =
        wait_for_response(process, "initialize", sink, cancel, provider, true).await?;
    let initialize: InitializeResponse =
        serde_json::from_value(initialize_value.clone()).map_err(|error| {
            AppError::InvalidRequest(format!("invalid ACP initialize response: {error}"))
        })?;
    if initialize.protocol_version != ProtocolVersion::V1 {
        return Err(AppError::Unsupported(format!(
            "ACP agent negotiated unsupported protocol version {}",
            initialize.protocol_version
        )));
    }
    authenticate_if_requested(process, &initialize_value, sink, cancel, provider, &options).await?;

    let (native_session_id, config_options, legacy_models) = match context
        .provider_state
        .native_session_id
        .clone()
    {
        Some(session_id) => {
            if !initialize.agent_capabilities.load_session {
                return Err(AppError::Unsupported(
                    "ACP agent does not support session/load; historical prompts will not be replayed"
                        .to_owned(),
                ));
            }
            let mut request = serde_json::to_value(LoadSessionRequest::new(
                session_id.clone(),
                context.manifest.workspace.clone(),
            ))?;
            if options.suppress_load_replay {
                request["_meta"] = json!({ "noReplay": true });
            }
            send_request(process, "session", "session/load", request).await?;
            let response_value = wait_for_response(
                process,
                "session",
                sink,
                cancel,
                provider,
                !options.suppress_load_replay,
            )
            .await?;
            let legacy_models = response_value.get("models").cloned();
            let response: LoadSessionResponse =
                serde_json::from_value(response_value).map_err(|error| {
                    AppError::InvalidRequest(format!("invalid ACP session/load response: {error}"))
                })?;
            (session_id, response.config_options, legacy_models)
        }
        None => {
            let mut request =
                serde_json::to_value(NewSessionRequest::new(context.manifest.workspace.clone()))?;
            if options.request_ask_mode {
                request["_meta"] = json!({ "yoloMode": false, "autoMode": false });
            }
            send_request(process, "session", "session/new", request).await?;
            let response_value =
                wait_for_response(process, "session", sink, cancel, provider, true).await?;
            let legacy_models = response_value.get("models").cloned();
            let response: NewSessionResponse =
                serde_json::from_value(response_value).map_err(|error| {
                    AppError::InvalidRequest(format!("invalid ACP session/new response: {error}"))
                })?;
            (
                response.session_id.0.to_string(),
                response.config_options,
                legacy_models,
            )
        }
    };

    let mut state = context.provider_state;
    state.native_session_id = Some(native_session_id.clone());
    state.recoverable = initialize.agent_capabilities.load_session;
    state.last_error = None;
    sink.save_provider_state(state).await?;

    let legacy_models =
        legacy_models.or_else(|| initialize_value.pointer("/_meta/modelState").cloned());
    apply_requested_config(
        process,
        config_options.as_deref(),
        legacy_models.as_ref(),
        &prompt,
        sink,
        cancel,
        SessionConfigContext {
            session_id: &native_session_id,
            provider,
            runtime: &options,
        },
    )
    .await?;

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
        if let Err(error) = handle_acp_message(process, message, sink, cancel, provider, true).await
        {
            if matches!(error, AppError::TurnCancelled) && *cancel.borrow() {
                send_notification(
                    process,
                    "session/cancel",
                    CancelNotification::new(native_session_id.clone()),
                )
                .await?;
                return Ok(DriverTurnResult {
                    native_session_id: Some(native_session_id),
                    stop_reason: "cancelled".to_owned(),
                    cancelled: true,
                });
            }
            return Err(error);
        }
    }
}

async fn wait_for_response(
    process: &mut JsonLineProcess,
    request_id: &str,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    provider: ProviderKind,
    emit_stream_updates: bool,
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
        handle_acp_message(
            process,
            message,
            sink,
            cancel,
            provider,
            emit_stream_updates,
        )
        .await?;
    }
}

async fn handle_acp_message(
    process: &mut JsonLineProcess,
    message: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    provider: ProviderKind,
    emit_stream_updates: bool,
) -> Result<(), AppError> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(());
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if extension_method(method) == "x.ai/ask_user_question" {
        return handle_ask_user_question(process, &message, params, sink, cancel).await;
    }
    if extension_method(method) == "x.ai/exit_plan_mode" {
        return handle_exit_plan_mode(process, &message, params, sink, cancel).await;
    }
    if extension_method(method) == "x.ai/mcp/elicit" {
        return handle_mcp_elicit(process, &message, params, sink, cancel).await;
    }
    if method == "session/request_permission" {
        let request_id = message.get("id").cloned().ok_or_else(|| {
            AppError::InvalidRequest("ACP permission request is missing an id".to_owned())
        })?;
        let request: RequestPermissionRequest =
            serde_json::from_value(params.clone()).map_err(|error| {
                AppError::InvalidRequest(format!("invalid ACP permission request: {error}"))
            })?;
        let options = serde_json::to_value(&request.options)?;
        let decision = match sink
            .request_permission(
                request_id_text(&request_id),
                "tool",
                "Allow ACP tool call?",
                serde_json::to_value(&request.tool_call)?,
                options,
                cancel,
            )
            .await
        {
            Ok(decision) => decision,
            Err(error @ AppError::TurnCancelled) => {
                let response = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
                process
                    .send(&json!({ "jsonrpc": "2.0", "id": request_id, "result": response }))
                    .await?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
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
        if !emit_stream_updates
            && matches!(
                update_type,
                "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
                    | "user_message_chunk"
            )
        {
            return Ok(());
        }
        let provider_id = provider.as_str();
        let (event_type, payload) = match update_type {
            "agent_message_chunk" => (
                "message.delta",
                json!({ "provider": provider_id, "role": "assistant", "content": update.get("content") }),
            ),
            "agent_thought_chunk" => (
                "thought.delta",
                json!({ "provider": provider_id, "content": update.get("content") }),
            ),
            "tool_call" => (
                "tool.started",
                json!({ "provider": provider_id, "tool": update }),
            ),
            "tool_call_update" => (
                "tool.updated",
                json!({ "provider": provider_id, "tool": update }),
            ),
            "plan" => (
                "plan.updated",
                json!({ "provider": provider_id, "plan": update }),
            ),
            "user_message_chunk" => return Ok(()),
            _ => (
                "provider.event",
                json!({ "provider": provider_id, "providerMethod": method, "metadata": update }),
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
            json!({ "provider": provider.as_str(), "providerMethod": method, "metadata": params }),
        )
        .await?;
    }
    Ok(())
}

async fn handle_ask_user_question(
    process: &mut JsonLineProcess,
    message: &Value,
    params: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let request_id = required_request_id(message, "Grok Build question")?;
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError::InvalidRequest("Grok Build questions must be an array".to_owned())
        })?;
    if questions.is_empty() {
        return send_invalid_params(process, request_id, "questions must not be empty").await;
    }
    let mut answers = serde_json::Map::new();
    for (question_index, question) in questions.iter().enumerate() {
        let decision = match sink
            .request_permission(
                request_id_text(request_id),
                "question",
                question_text(question).unwrap_or("Grok Build has a question"),
                json!({ "question": question, "questionIndex": question_index }),
                Value::Array(question_permission_options(question, question_index)),
                cancel,
            )
            .await
        {
            Ok(decision) => decision,
            Err(error @ AppError::TurnCancelled) => {
                send_result(process, request_id, json!({ "outcome": "cancelled" })).await?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if matches!(
            decision.outcome,
            PermissionOutcome::RejectOnce
                | PermissionOutcome::RejectAlways
                | PermissionOutcome::AllowAlways
        ) {
            return send_result(process, request_id, json!({ "outcome": "cancelled" })).await;
        }
        if let Some(provided) = decision
            .data
            .as_ref()
            .and_then(|data| data.get("answers").or(Some(data)))
            .and_then(Value::as_object)
        {
            answers.extend(provided.clone());
            continue;
        }
        let Some(Value::Object(answer)) =
            selected_question_answer(questions, decision.option_id.as_deref())
        else {
            return send_result(process, request_id, json!({ "outcome": "cancelled" })).await;
        };
        answers.extend(answer);
    }
    send_result(
        process,
        request_id,
        json!({ "outcome": "accepted", "answers": answers }),
    )
    .await
}

async fn handle_exit_plan_mode(
    process: &mut JsonLineProcess,
    message: &Value,
    params: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let request_id = required_request_id(message, "Grok Build plan approval")?;
    if !params.is_object() {
        return send_invalid_params(process, request_id, "plan parameters must be an object").await;
    }
    let decision = match sink
        .request_permission(
            request_id_text(request_id),
            "plan",
            "Approve Grok Build plan?",
            params.clone(),
            json!([
                { "optionId": "approve", "name": "Approve", "kind": "allow_once" },
                { "optionId": "cancel", "name": "Request changes", "kind": "reject_once" }
            ]),
            cancel,
        )
        .await
    {
        Ok(decision) => decision,
        Err(error @ AppError::TurnCancelled) => {
            send_result(process, request_id, json!({ "outcome": "cancelled" })).await?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let result = match decision.outcome {
        PermissionOutcome::AllowOnce | PermissionOutcome::AllowAlways => {
            json!({ "outcome": "approved" })
        }
        PermissionOutcome::RejectOnce
        | PermissionOutcome::RejectAlways
        | PermissionOutcome::Answer => {
            let feedback = decision
                .data
                .as_ref()
                .and_then(|data| data.get("feedback"))
                .and_then(Value::as_str);
            match feedback {
                Some(feedback) => json!({ "outcome": "cancelled", "feedback": feedback }),
                None => json!({ "outcome": "cancelled" }),
            }
        }
    };
    send_result(process, request_id, result).await
}

async fn handle_mcp_elicit(
    process: &mut JsonLineProcess,
    message: &Value,
    params: Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let request_id = required_request_id(message, "Grok Build MCP elicitation")?;
    if !params.is_object() {
        return send_invalid_params(
            process,
            request_id,
            "elicitation parameters must be an object",
        )
        .await;
    }
    let title = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Grok Build needs additional input")
        .to_owned();
    let decision = match sink
        .request_permission(
            request_id_text(request_id),
            "elicitation",
            title,
            params,
            json!([
                { "optionId": "submit", "name": "Submit", "kind": "answer" },
                { "optionId": "cancel", "name": "Cancel", "kind": "reject_once" }
            ]),
            cancel,
        )
        .await
    {
        Ok(decision) => decision,
        Err(error @ AppError::TurnCancelled) => {
            send_result(process, request_id, json!({ "action": "cancel" })).await?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let result = match decision.outcome {
        PermissionOutcome::Answer | PermissionOutcome::AllowOnce => decision
            .data
            .filter(Value::is_object)
            .map(|content| json!({ "action": "accept", "content": content }))
            .unwrap_or_else(|| json!({ "action": "cancel" })),
        _ => json!({ "action": "cancel" }),
    };
    send_result(process, request_id, result).await
}

fn extension_method(method: &str) -> &str {
    method.strip_prefix('_').unwrap_or(method)
}

fn required_request_id<'a>(message: &'a Value, context: &str) -> Result<&'a Value, AppError> {
    message
        .get("id")
        .ok_or_else(|| AppError::InvalidRequest(format!("{context} request is missing an id")))
}

fn request_id_text(id: &Value) -> String {
    id.as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| id.to_string())
}

async fn send_result(
    process: &mut JsonLineProcess,
    id: &Value,
    result: Value,
) -> Result<(), AppError> {
    process
        .send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .await
}

async fn send_invalid_params(
    process: &mut JsonLineProcess,
    id: &Value,
    message: &str,
) -> Result<(), AppError> {
    process
        .send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": message }
        }))
        .await
}

fn question_text(question: &Value) -> Option<&str> {
    question
        .get("question")
        .or_else(|| question.get("prompt"))
        .and_then(Value::as_str)
}

fn question_permission_options(question: &Value, question_index: usize) -> Vec<Value> {
    let mut options = question
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, option)| {
            let label = option
                .as_str()
                .or_else(|| option.get("label").and_then(Value::as_str))
                .or_else(|| option.get("name").and_then(Value::as_str))
                .or_else(|| option.get("value").and_then(Value::as_str))?;
            Some(json!({
                "optionId": format!("answer:{question_index}:{index}"),
                "name": label,
                "kind": "answer"
            }))
        })
        .collect::<Vec<_>>();
    options.push(json!({
        "optionId": "cancel",
        "name": "Cancel",
        "kind": "reject_once"
    }));
    options
}

fn selected_question_answer(questions: &[Value], option_id: Option<&str>) -> Option<Value> {
    let (kind, question_index, option_index) =
        option_id?.split_once(':').and_then(|(kind, rest)| {
            let (question, option) = rest.split_once(':')?;
            Some((
                kind,
                question.parse::<usize>().ok()?,
                option.parse::<usize>().ok()?,
            ))
        })?;
    if kind != "answer" {
        return None;
    }
    let question = questions.get(question_index)?;
    let question_text = question
        .get("question")
        .or_else(|| question.get("prompt"))
        .and_then(Value::as_str)?;
    let option = question.get("options")?.as_array()?.get(option_index)?;
    let answer = option
        .as_str()
        .or_else(|| option.get("label").and_then(Value::as_str))
        .or_else(|| option.get("name").and_then(Value::as_str))
        .or_else(|| option.get("value").and_then(Value::as_str))?;
    let mut answers = serde_json::Map::new();
    answers.insert(question_text.to_owned(), json!([answer]));
    Some(Value::Object(answers))
}

async fn authenticate_if_requested(
    process: &mut JsonLineProcess,
    initialize: &Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    provider: ProviderKind,
    options: &AcpRuntimeOptions,
) -> Result<(), AppError> {
    if !options.authenticate {
        return Ok(());
    }
    let advertised = initialize
        .get("authMethods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|method| method.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if advertised.is_empty() {
        return Ok(());
    }
    let selected = options
        .auth_method
        .as_deref()
        .or_else(|| {
            initialize
                .pointer("/_meta/defaultAuthMethodId")
                .and_then(Value::as_str)
        })
        .or_else(|| advertised.contains(&"cached_token").then_some("cached_token"))
        .or_else(|| (advertised.len() == 1).then_some(advertised[0]))
        .ok_or_else(|| {
            AppError::ProviderUnavailable(format!(
                "{} advertised multiple authentication methods but no default; configure agent.grok_auth_method",
                provider.as_str()
            ))
        })?;
    if !advertised.contains(&selected) {
        return Err(AppError::ProviderUnavailable(format!(
            "authentication method '{selected}' is not advertised by {}",
            provider.as_str()
        )));
    }
    if provider == ProviderKind::GrokBuild && !is_grok_headless_auth_method(selected) {
        return Err(AppError::ProviderUnavailable(format!(
            "authentication method '{selected}' requires interaction; run `grok login` first or configure XAI_API_KEY"
        )));
    }
    send_request(
        process,
        "authenticate",
        "authenticate",
        json!({ "methodId": selected, "_meta": { "headless": true } }),
    )
    .await?;
    wait_for_response(process, "authenticate", sink, cancel, provider, true).await?;
    Ok(())
}

fn is_grok_headless_auth_method(method: &str) -> bool {
    matches!(method, "cached_token" | "xai.api_key")
}

#[derive(Clone, Copy)]
struct SessionConfigContext<'a> {
    session_id: &'a str,
    provider: ProviderKind,
    runtime: &'a AcpRuntimeOptions,
}

async fn apply_requested_config(
    process: &mut JsonLineProcess,
    options: Option<&[SessionConfigOption]>,
    legacy_models: Option<&Value>,
    prompt: &DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    context: SessionConfigContext<'_>,
) -> Result<(), AppError> {
    if options.is_none() && context.runtime.legacy_model_state {
        return apply_legacy_model_config(process, legacy_models, prompt, sink, cancel, context)
            .await;
    }
    let mut current_options = options.map(<[SessionConfigOption]>::to_vec);
    for (config_id, requested) in [
        ("model", prompt.model.as_deref()),
        ("reasoning_effort", prompt.reasoning_effort.as_deref()),
    ] {
        let Some(requested) = requested else {
            continue;
        };
        let supported = current_options.as_deref().is_some_and(|options| {
            options
                .iter()
                .any(|option| option.id.0.as_ref() == config_id)
        });
        if !supported {
            if context.runtime.allow_cli_config_fallback {
                continue;
            }
            return Err(AppError::Unsupported(format!(
                "{} does not expose session config option '{config_id}'",
                context.provider.as_str()
            )));
        }
        let request_id = format!("config:{config_id}");
        let value = config_option_wire_value(requested, context.runtime.nested_config_values);
        send_request(
            process,
            &request_id,
            "session/set_config_option",
            json!({
                "sessionId": context.session_id,
                "configId": config_id,
                "value": value,
            }),
        )
        .await?;
        let response =
            wait_for_response(process, &request_id, sink, cancel, context.provider, true).await?;
        let response: SetSessionConfigOptionResponse =
            serde_json::from_value(response).map_err(|error| {
                AppError::InvalidRequest(format!(
                    "invalid ACP session/set_config_option response: {error}"
                ))
            })?;
        current_options = Some(response.config_options);
    }
    Ok(())
}

fn config_option_wire_value(requested: &str, nested: bool) -> Value {
    if nested {
        json!({ "value": requested })
    } else {
        json!(requested)
    }
}

async fn apply_legacy_model_config(
    process: &mut JsonLineProcess,
    models: Option<&Value>,
    prompt: &DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    context: SessionConfigContext<'_>,
) -> Result<(), AppError> {
    if prompt.model.is_none() && prompt.reasoning_effort.is_none() {
        return Ok(());
    }
    let model_id = prompt.model.as_deref().or_else(|| {
        models
            .and_then(|value| value.get("currentModelId"))
            .and_then(Value::as_str)
    });
    let Some(model_id) = model_id else {
        if context.runtime.allow_cli_config_fallback {
            return Ok(());
        }
        return Err(AppError::Unsupported(
            "Grok Build did not expose a model for session configuration".to_owned(),
        ));
    };
    let model = models
        .and_then(|value| value.get("availableModels"))
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find(|model| {
                model
                    .get("modelId")
                    .or_else(|| model.get("id"))
                    .and_then(Value::as_str)
                    == Some(model_id)
            })
        });
    let mut meta = serde_json::Map::new();
    if let Some(effort) = prompt.reasoning_effort.as_deref() {
        let supports_effort = model
            .and_then(|model| model.pointer("/_meta/supportsReasoningEffort"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !supports_effort {
            if context.runtime.allow_cli_config_fallback {
                return Ok(());
            }
            return Err(AppError::Unsupported(format!(
                "Grok Build model '{model_id}' does not support reasoning effort"
            )));
        }
        meta.insert("reasoningEffort".to_owned(), json!(effort));
    }
    let mut request = json!({ "sessionId": context.session_id, "modelId": model_id });
    if !meta.is_empty() {
        request["_meta"] = Value::Object(meta);
    }
    send_request(process, "config:model", "session/set_model", request).await?;
    wait_for_response(
        process,
        "config:model",
        sink,
        cancel,
        context.provider,
        true,
    )
    .await?;
    Ok(())
}

fn select_acp_option(
    request: &RequestPermissionRequest,
    decision: &super::types::PermissionDecision,
) -> Result<String, AppError> {
    let expected_kind = match decision.outcome {
        PermissionOutcome::AllowOnce => PermissionOptionKind::AllowOnce,
        PermissionOutcome::AllowAlways => PermissionOptionKind::AllowAlways,
        PermissionOutcome::RejectOnce => PermissionOptionKind::RejectOnce,
        PermissionOutcome::RejectAlways => PermissionOptionKind::RejectAlways,
        PermissionOutcome::Answer => {
            return Err(AppError::InvalidRequest(
                "answer outcome cannot authorize an ACP tool permission".to_owned(),
            ));
        }
    };
    if let Some(option_id) = &decision.option_id {
        let matching = request
            .options
            .iter()
            .filter(|option| option.option_id.0.as_ref() == option_id)
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(AppError::InvalidRequest(
                "selected ACP permission option is missing or ambiguous".to_owned(),
            ));
        }
        if matching[0].kind != expected_kind {
            return Err(AppError::InvalidRequest(
                "selected ACP permission option does not match the declared outcome".to_owned(),
            ));
        }
        return Ok(option_id.clone());
    }
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
    redact_sensitive_text(
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider returned an error")
            .chars()
            .take(500)
            .collect::<String>()
            .as_str(),
    )
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

    #[test]
    fn permission_selection_rejects_option_outcome_mismatch() {
        let request = request(json!([
            { "optionId": "reject", "name": "Reject", "kind": "reject_always" }
        ]));
        let mut decision = decision(PermissionOutcome::AllowAlways);
        decision.option_id = Some("reject".to_owned());
        assert!(matches!(
            select_acp_option(&request, &decision),
            Err(AppError::InvalidRequest(message)) if message.contains("does not match")
        ));
    }

    #[test]
    fn answer_outcome_cannot_authorize_an_acp_tool_permission() {
        let request = request(json!([
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" }
        ]));
        assert!(matches!(
            select_acp_option(&request, &decision(PermissionOutcome::Answer)),
            Err(AppError::InvalidRequest(message)) if message.contains("cannot authorize")
        ));
    }

    #[test]
    fn grok_authentication_only_allows_known_headless_methods() {
        assert!(is_grok_headless_auth_method("cached_token"));
        assert!(is_grok_headless_auth_method("xai.api_key"));
        assert!(!is_grok_headless_auth_method("grok.com"));
        assert!(!is_grok_headless_auth_method("oidc"));
        assert!(!is_grok_headless_auth_method("future-browser-flow"));
    }

    #[test]
    fn grok_config_options_use_vendor_nested_values() {
        assert_eq!(
            config_option_wire_value("high", true),
            json!({ "value": "high" })
        );
        assert_eq!(config_option_wire_value("high", false), json!("high"));
    }

    #[test]
    fn grok_questions_keep_question_and_option_indexes() {
        let questions = json!([
            { "question": "First?", "options": [{ "label": "A" }, { "label": "B" }] },
            { "question": "Second?", "options": ["C"] }
        ]);
        let questions = questions.as_array().unwrap();
        assert_eq!(
            question_permission_options(&questions[1], 1)[0]["optionId"],
            "answer:1:0"
        );
        assert_eq!(
            selected_question_answer(questions, Some("answer:1:0")).unwrap(),
            json!({ "Second?": ["C"] })
        );
    }
}
