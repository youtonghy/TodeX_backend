use std::collections::BTreeMap;
use std::time::Duration;

use agent_client_protocol::schema::{
    v1::{
        CancelNotification, ClientCapabilities, ClientSessionCapabilities, ContentBlock,
        ImageContent, Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest,
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
use tokio::sync::{mpsc, watch};

use crate::config::{AcpProfileConfig, AgentConfig};
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::process::{
    executable_available, provider_exit_error, redact_sensitive_text, CommandSpec, JsonLineProcess,
};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PendingProviderControl, PermissionOutcome, ProviderCapabilities, ProviderControl,
    ProviderDescriptor, ProviderDriver,
};

pub struct AcpDriver {
    profiles: BTreeMap<String, AcpProfileConfig>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct AcpRuntimeOptions {
    pub authenticate: bool,
    pub auth_method: Option<String>,
    pub auth_meta: Option<Value>,
    pub suppress_load_replay: bool,
    pub allow_cli_config_fallback: bool,
    pub request_ask_mode: bool,
    pub legacy_model_state: bool,
    pub nested_config_values: bool,
    pub allow_unadvertised_images: bool,
    pub snake_case_image_mime: bool,
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

    fn named_profile(&self, profile: Option<&str>) -> Result<&AcpProfileConfig, AppError> {
        let profile = profile
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
            .ok_or_else(|| {
                AppError::InvalidRequest("ACP image capability requires a profile".to_owned())
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
                permission_config: super::types::permission_config_capabilities(ProviderKind::Acp),
                native_fork: false,
                native_compact: false,
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: false,
                model_selection: false,
                image_input: ProviderKind::Acp.supports_image_input(),
                image_input_mode: ImageInputMode::Profile,
            },
            models: Vec::new(),
        }
    }

    async fn discover_image_input(
        &self,
        workspace: &std::path::Path,
        profile: Option<&str>,
    ) -> Result<bool, AppError> {
        let profile = self.named_profile(profile)?;
        let mut spec = CommandSpec::new(&profile.command, workspace);
        spec.args = profile.args.clone();
        spec.env = profile
            .env
            .iter()
            .filter(|(key, _)| !key.starts_with("TODEX_AGENTD_"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut process = JsonLineProcess::spawn(&spec).await?;
        let result = async {
            send_request(
                &mut process,
                "initialize",
                "initialize",
                initialize_request(),
            )
            .await?;
            loop {
                let Some(message) = process.read().await? else {
                    break Err(
                        provider_exit_error(&process, "ACP agent closed during initialize").await,
                    );
                };
                if jsonrpc_id(&message) != Some("initialize") {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    break Err(AppError::ProviderUnavailable(format!(
                        "ACP initialize failed: {}",
                        safe_error_text(error)
                    )));
                }
                let response: InitializeResponse =
                    serde_json::from_value(message.get("result").cloned().unwrap_or(Value::Null))
                        .map_err(|error| {
                        AppError::InvalidRequest(format!(
                            "invalid ACP initialize response: {error}"
                        ))
                    })?;
                break Ok(response.agent_capabilities.prompt_capabilities.image);
            }
        }
        .await;
        process.terminate().await;
        result
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        mut cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
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
        let mut process = JsonLineProcess::spawn_trusted(&spec, launch_permit).await?;
        let result = run_acp_turn(
            &mut process,
            context,
            prompt,
            &sink,
            &mut cancel,
            profile_runtime_options(profile)?,
        )
        .await;
        process.terminate().await;
        result
    }
}

fn profile_runtime_options(profile: &AcpProfileConfig) -> Result<AcpRuntimeOptions, AppError> {
    let api_key = profile
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            std::env::var(name)
                .ok()
                .or_else(|| profile.env.get(name).cloned())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    AppError::ProviderUnavailable(format!(
                        "ACP profile api_key_env '{name}' is not set in the daemon environment or profile env"
                    ))
                })
        })
        .transpose()?;
    Ok(AcpRuntimeOptions {
        authenticate: profile.auth_method.is_some() || api_key.is_some(),
        auth_method: profile.auth_method.clone(),
        auth_meta: api_key.map(|key| json!({ "api_key": key })),
        ..Default::default()
    })
}

fn initialize_request() -> InitializeRequest {
    InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(
            ClientCapabilities::new().session(
                ClientSessionCapabilities::new()
                    .config_options(SessionConfigOptionsCapabilities::new()),
            ),
        )
        .client_info(
            Implementation::new("todex-agentd", crate::version::APP_VERSION).title("TodeX 2.0"),
        )
}

#[derive(Default)]
pub(super) struct AcpConnectionState {
    initialize: Option<Value>,
    session: Option<(String, Value)>,
}

pub(super) async fn run_acp_turn(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    options: AcpRuntimeOptions,
) -> Result<DriverTurnResult, AppError> {
    run_acp_turn_controlled(
        process,
        context,
        prompt,
        sink,
        cancel,
        options,
        &mut AcpConnectionState::default(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_acp_turn_controlled(
    process: &mut JsonLineProcess,
    context: DriverContext,
    prompt: DriverPrompt,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
    options: AcpRuntimeOptions,
    connection: &mut AcpConnectionState,
    mut controls: Option<&mut mpsc::Receiver<PendingProviderControl>>,
) -> Result<DriverTurnResult, AppError> {
    let provider = context.manifest.provider;
    super::types::resolve_execution_config(
        provider,
        prompt.permission_mode.as_deref(),
        prompt.work_mode.as_deref(),
        prompt.permission_profile.as_deref(),
        prompt.sandbox_mode.as_deref(),
        prompt.approval_policy.as_deref(),
    )?;
    let initialize_value = if let Some(initialize) = &connection.initialize {
        initialize.clone()
    } else {
        send_request(process, "initialize", "initialize", initialize_request()).await?;
        let initialize =
            wait_for_response(process, "initialize", sink, cancel, provider, true).await?;
        initialize
    };
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
    ensure_acp_images_supported(
        &prompt,
        initialize.agent_capabilities.prompt_capabilities.image,
        options.allow_unadvertised_images,
        context.manifest.provider,
    )?;

    if connection.initialize.is_none() {
        authenticate_if_requested(process, &initialize_value, sink, cancel, provider, &options)
            .await?;
        connection.initialize = Some(initialize_value.clone());
    }

    let (native_session_id, config_options, legacy_models) = if let Some((id, response)) =
        &connection.session
    {
        let options = response
            .get("configOptions")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        (id.clone(), options, response.get("models").cloned())
    } else {
        match context.provider_state.native_session_id.clone() {
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
                apply_session_metadata(&mut request, &options, true);
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
                connection.session = Some((session_id.clone(), response_value.clone()));
                let legacy_models = response_value.get("models").cloned();
                let response: LoadSessionResponse = serde_json::from_value(response_value)
                    .map_err(|error| {
                        AppError::InvalidRequest(format!(
                            "invalid ACP session/load response: {error}"
                        ))
                    })?;
                (session_id, response.config_options, legacy_models)
            }
            None => {
                let mut request = serde_json::to_value(NewSessionRequest::new(
                    context.manifest.workspace.clone(),
                ))?;
                apply_session_metadata(&mut request, &options, false);
                send_request(process, "session", "session/new", request).await?;
                let response_value =
                    wait_for_response(process, "session", sink, cancel, provider, true).await?;
                let legacy_models = response_value.get("models").cloned();
                let response: NewSessionResponse = serde_json::from_value(response_value.clone())
                    .map_err(|error| {
                    AppError::InvalidRequest(format!("invalid ACP session/new response: {error}"))
                })?;
                connection.session = Some((response.session_id.0.to_string(), response_value));
                (
                    response.session_id.0.to_string(),
                    response.config_options,
                    legacy_models,
                )
            }
        }
    };

    let mut state = context.provider_state;
    state.native_session_id = Some(native_session_id.clone());
    state.recoverable = initialize.agent_capabilities.load_session;
    state.last_error = None;
    sink.save_provider_state(state).await?;

    let legacy_models =
        legacy_models.or_else(|| initialize_value.pointer("/_meta/modelState").cloned());
    let applied_options = apply_requested_config(
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
    if let (Some((_, response)), Some(options)) = (&mut connection.session, applied_options) {
        response["configOptions"] = serde_json::to_value(options)?;
    }

    let content = acp_prompt_content(&prompt);
    let mut request = serde_json::to_value(PromptRequest::new(native_session_id.clone(), content))?;
    // ACP v1 agents in the field (including Claude Code/Grok Build) still
    // expect the historical snake_case MIME key even though the Rust schema
    // serializes it as camelCase. Keep the wire compatibility at this edge.
    if options.snake_case_image_mime {
        normalize_acp_image_wire(&mut request);
    }
    send_request(process, &prompt.turn_id, "session/prompt", request).await?;
    let mut pending: BTreeMap<String, LiveAcpControl> = BTreeMap::new();
    let mut client_requests: tokio::task::JoinSet<(Vec<Value>, Result<(), AppError>)> =
        tokio::task::JoinSet::new();
    let mut terminal_response = None;
    loop {
        if terminal_response.is_some() && pending.is_empty() {
            return finish_prompt(terminal_response.take().unwrap(), native_session_id);
        }
        let control_deadline = pending.values().map(|entry| entry.deadline).min();
        let message = tokio::select! {
            message = process.read() => message?,
            response = client_requests.join_next(), if !client_requests.is_empty() => {
                if let Some(Ok((responses, outcome))) = response {
                    for response in responses { process.send(&response).await?; }
                    if let Err(error) = outcome {
                        if !matches!(error, AppError::TurnCancelled) { return Err(error); }
                    }
                }
                continue;
            }
            request = receive_control(&mut controls), if terminal_response.is_none() => {
                if let Some(request) = request {
                    if request.expected_turn_id != prompt.turn_id {
                        let _ = request.respond_to.send(Err(AppError::InvalidRequest("control targets a stale turn".to_owned())));
                    } else {
                        start_live_control(process, request, &native_session_id, &mut pending, provider).await?;
                    }
                } else { controls = None; }
                continue;
            }
            _ = async { if let Some(deadline) = control_deadline { tokio::time::sleep_until(deadline).await } else { std::future::pending::<()>().await } } => {
                let now = tokio::time::Instant::now();
                let expired: Vec<_> = pending.iter().filter(|(_,value)| value.deadline <= now).map(|(id,_)| id.clone()).collect();
                for id in expired {
                    if let Some(request) = pending.remove(&id) {
                        let _ = request.request.respond_to.send(Err(AppError::ProviderUnavailable(format!("{} control timed out; effective configuration is unknown", provider.as_str()))));
                    }
                }
                // A timed-out mutation must never remain in a reusable process.
                return Err(AppError::ProviderUnavailable(format!("{} control acknowledgement timed out", provider.as_str())));
            }
            changed = cancel.changed() => {
                let _ = changed;
                send_notification(
                    process,
                    "session/cancel",
                    CancelNotification::new(native_session_id.clone()),
                ).await?;
                return drain_cancelled_turn(process, &native_session_id, &prompt.turn_id, sink, provider, &mut client_requests, terminal_response.is_some()).await;
            }
        };
        let Some(message) = message else {
            return Err(provider_exit_error(process, "ACP agent closed stdout").await);
        };
        if message.is_null() {
            continue;
        }
        if let Some(id) = jsonrpc_id(&message) {
            if let Some(request) = pending.remove(id) {
                complete_live_control(
                    process,
                    message,
                    request,
                    sink,
                    &mut pending,
                    connection,
                    provider,
                )
                .await?;
                continue;
            }
        }
        if jsonrpc_id(&message) == Some(prompt.turn_id.as_str()) {
            emit_prompt_metadata(&message, sink, provider).await?;
            terminal_response = Some(message);
            continue;
        }
        observe_config_options(connection, &message);
        if is_client_request(&message) {
            if client_requests.len() >= 32 {
                process.send(&json!({"jsonrpc":"2.0","id":message.get("id"),"error":{"code":-32000,"message":"too many pending client requests"}})).await?;
                continue;
            }
            let sink = sink.clone();
            let mut cancel = cancel.clone();
            client_requests.spawn(async move {
                let mut writer = BufferedAcpWriter::default();
                let result = handle_client_request(&mut writer, &message, &sink, &mut cancel).await;
                (writer.responses, result)
            });
            continue;
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
                return drain_cancelled_turn(
                    process,
                    &native_session_id,
                    &prompt.turn_id,
                    sink,
                    provider,
                    &mut client_requests,
                    terminal_response.is_some(),
                )
                .await;
            }
            return Err(error);
        }
    }
}

fn finish_prompt(message: Value, session_id: String) -> Result<DriverTurnResult, AppError> {
    if let Some(error) = message.get("error") {
        return Err(AppError::ProviderUnavailable(format!(
            "ACP prompt failed: {}",
            safe_error_text(error)
        )));
    }
    let response: PromptResponse = serde_json::from_value(
        message.get("result").cloned().unwrap_or(Value::Null),
    )
    .map_err(|error| AppError::InvalidRequest(format!("invalid ACP prompt response: {error}")))?;
    Ok(DriverTurnResult {
        native_session_id: Some(session_id),
        stop_reason: serde_json::to_value(response.stop_reason)?
            .as_str()
            .unwrap_or("completed")
            .to_owned(),
        cancelled: response.stop_reason == agent_client_protocol::schema::v1::StopReason::Cancelled,
    })
}

struct LiveAcpControl {
    request: PendingProviderControl,
    remaining: Vec<(String, Value)>,
    deadline: tokio::time::Instant,
}

async fn receive_control(
    controls: &mut Option<&mut mpsc::Receiver<PendingProviderControl>>,
) -> Option<PendingProviderControl> {
    match controls {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn start_live_control(
    process: &mut JsonLineProcess,
    request: PendingProviderControl,
    session_id: &str,
    pending: &mut BTreeMap<String, LiveAcpControl>,
    provider: ProviderKind,
) -> Result<(), AppError> {
    if request.respond_to.is_closed() {
        return Ok(());
    }
    if pending.len() >= 16 {
        let _ = request.respond_to.send(Err(AppError::Conflict(format!(
            "too many pending {} controls",
            provider.as_str()
        ))));
        return Ok(());
    }
    if matches!(request.control, ProviderControl::Configure { .. })
        && pending
            .values()
            .any(|control| matches!(control.request.control, ProviderControl::Configure { .. }))
    {
        let _ = request.respond_to.send(Err(AppError::Conflict(format!(
            "a {} configuration change is already pending",
            provider.as_str()
        ))));
        return Ok(());
    }
    let mut commands = match control_commands(provider, &request, session_id) {
        Ok(commands) => commands,
        Err(error) => {
            let _ = request.respond_to.send(Err(error));
            return Ok(());
        }
    };
    if commands.is_empty() {
        let _ = request.respond_to.send(Err(AppError::InvalidRequest(
            "configuration change is empty".to_owned(),
        )));
        return Ok(());
    }
    let id = format!("live:{}", request.request_id);
    if pending.contains_key(&id) {
        let _ = request.respond_to.send(Err(AppError::InvalidRequest(
            "duplicate live control request".to_owned(),
        )));
        return Ok(());
    }
    let (method, params) = commands.remove(0);
    send_request(process, &id, &method, params).await?;
    pending.insert(
        id,
        LiveAcpControl {
            request,
            remaining: commands,
            deadline: tokio::time::Instant::now() + Duration::from_secs(8),
        },
    );
    Ok(())
}

fn control_commands(
    provider: ProviderKind,
    request: &PendingProviderControl,
    session_id: &str,
) -> Result<Vec<(String, Value)>, AppError> {
    if provider == ProviderKind::Devin {
        return devin_control_commands(&request.control, session_id);
    }
    match &request.control {
        ProviderControl::Steer { text } => Ok(vec![(
            "_x.ai/interject".to_owned(),
            json!({"sessionId":session_id,"text":text,"interjectionId":request.request_id}),
        )]),
        ProviderControl::Configure {
            model,
            reasoning_effort,
        } => Ok([("model", model), ("reasoning_effort", reasoning_effort)]
            .into_iter()
            .filter_map(|(id, value)| {
                value.as_ref().map(|value| {
                    (
                        "session/set_config_option".to_owned(),
                        json!({"sessionId":session_id,"configId":id,"value":{"value":value}}),
                    )
                })
            })
            .collect()),
        _ => Err(AppError::Unsupported(
            "Grok does not expose native prompt queue controls".to_owned(),
        )),
    }
}

fn devin_control_commands(
    control: &ProviderControl,
    session_id: &str,
) -> Result<Vec<(String, Value)>, AppError> {
    match control {
        ProviderControl::Configure {
            model,
            reasoning_effort,
        } => {
            if reasoning_effort.is_some() {
                return Err(AppError::Unsupported(
                    "Devin does not expose a separate reasoning effort control; choose a model variant"
                        .to_owned(),
                ));
            }
            Ok(model
                .as_ref()
                .map(|value| {
                    vec![(
                        "session/set_config_option".to_owned(),
                        json!({"sessionId":session_id,"configId":"model","value":value}),
                    )]
                })
                .unwrap_or_default())
        }
        ProviderControl::Steer { .. } => Err(AppError::Unsupported(
            "Devin does not expose mid-turn steering over ACP".to_owned(),
        )),
        _ => Err(AppError::Unsupported(
            "Devin does not expose native prompt queue controls".to_owned(),
        )),
    }
}

async fn complete_live_control(
    process: &mut JsonLineProcess,
    message: Value,
    mut control: LiveAcpControl,
    sink: &DriverEventSink,
    pending: &mut BTreeMap<String, LiveAcpControl>,
    connection: &mut AcpConnectionState,
    provider: ProviderKind,
) -> Result<(), AppError> {
    if let Some(error) = message.get("error") {
        let _ = control
            .request
            .respond_to
            .send(Err(AppError::InvalidRequest(format!(
                "{} control rejected: {}",
                provider.as_str(),
                safe_error_text(error)
            ))));
        return Ok(());
    }
    let Some(result) = message.get("result") else {
        let _ = control
            .request
            .respond_to
            .send(Err(AppError::ProviderUnavailable(format!(
                "{} control response has no result; outcome is unknown",
                provider.as_str()
            ))));
        return Err(AppError::ProviderUnavailable(format!(
            "{} control response has no result",
            provider.as_str()
        )));
    };
    if let Some(config) = result.get("configOptions") {
        if let Some((_, response)) = &mut connection.session {
            response["configOptions"] = config.clone();
        }
    }
    if let Some(effective) = result.get("configOptions").and_then(config_effective) {
        sink.emit("turn.configuration", json!({"provider":provider.as_str(),"source":"provider-confirmed","effectiveConfig":effective,"effectiveFrom":"next-safe-point"})).await?;
    }
    if !control.remaining.is_empty() {
        let (method, params) = control.remaining.remove(0);
        let id = format!("live:{}", control.request.request_id);
        send_request(process, &id, &method, params).await?;
        control.deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        pending.insert(id, control);
        return Ok(());
    }
    let effective = result.get("configOptions").and_then(config_effective);
    let missing_effective = match &control.request.control {
        ProviderControl::Configure {
            model,
            reasoning_effort,
        } => {
            let has_value = |key: &str| {
                effective
                    .as_ref()
                    .and_then(|config| config.get(key))
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            };
            (model.is_some() && !has_value("model"))
                || (reasoning_effort.is_some() && !has_value("reasoningEffort"))
        }
        _ => false,
    };
    let unknown_steer = matches!(control.request.control, ProviderControl::Steer { .. })
        && result.get("status").and_then(Value::as_str) != Some("queued");
    if missing_effective || unknown_steer {
        let message = format!(
            "{} control acknowledgement does not confirm its effective result",
            provider.as_str()
        );
        let _ = control
            .request
            .respond_to
            .send(Err(AppError::ProviderUnavailable(message.to_owned())));
        return Err(AppError::ProviderUnavailable(message.to_owned()));
    }
    let _ = control.request.respond_to.send(Ok(json!({"source":"provider-confirmed","effectiveConfig":effective,"effectiveFrom":"next-safe-point","result":result})));
    Ok(())
}

pub(super) fn observe_config_options(connection: &mut AcpConnectionState, message: &Value) {
    if message.get("method").and_then(Value::as_str) != Some("session/update")
        || message
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
            != Some("config_option_update")
    {
        return;
    }
    if let Some((id, response)) = &mut connection.session {
        if message.pointer("/params/sessionId").and_then(Value::as_str) == Some(id.as_str()) {
            if let Some(options) = message
                .pointer("/params/update/configOptions")
                .filter(|value| value.is_array())
            {
                response["configOptions"] = options.clone();
            }
        }
    }
}

fn config_effective(options: &Value) -> Option<Value> {
    let mut config = serde_json::Map::new();
    for option in options.as_array()? {
        let key = match option.get("id").and_then(Value::as_str) {
            Some("model") => "model",
            Some("reasoning_effort") => "reasoningEffort",
            Some("mode") => "mode",
            _ => continue,
        };
        if let Some(value) = option
            .get("currentValue")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            config.insert(key.to_owned(), json!(value));
        }
    }
    if config.is_empty() {
        return None;
    }
    config.insert("source".to_owned(), json!("provider-confirmed"));
    Some(Value::Object(config))
}

fn apply_session_metadata(request: &mut Value, options: &AcpRuntimeOptions, loading: bool) {
    if options.request_ask_mode {
        request["_meta"]["yoloMode"] = json!(false);
        request["_meta"]["autoMode"] = json!(false);
    }
    if loading && options.suppress_load_replay {
        request["_meta"]["noReplay"] = json!(true);
    }
}

fn is_grok_update_method(method: &str) -> bool {
    matches!(
        extension_method(method),
        "session/update" | "x.ai/session_notification" | "x.ai/session/update"
    )
}

fn prompt_metadata(message: &Value) -> Option<Value> {
    message
        .pointer("/result/_meta")
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| {
            message
                .pointer("/error/data")
                .filter(|value| value.is_object())
                .cloned()
        })
}

async fn emit_prompt_metadata(
    message: &Value,
    sink: &DriverEventSink,
    provider: ProviderKind,
) -> Result<(), AppError> {
    let Some(metadata) = prompt_metadata(message) else {
        return Ok(());
    };
    sink.emit("provider.event", json!({
        "provider": provider.as_str(), "providerMethod": "session/prompt/result", "metadata": metadata,
    })).await?;
    if provider == ProviderKind::Devin {
        if let Some(usage) = message
            .pointer("/result/usage")
            .filter(|value| value.is_object())
        {
            sink.emit(
                "usage.updated",
                json!({
                    "provider": provider.as_str(), "source": "provider", "scope": "turn",
                    "aggregation": "snapshot", "final": true,
                    "usage": normalize_devin_usage(usage), "metadata": metadata,
                }),
            )
            .await?;
        }
    }
    if provider == ProviderKind::GrokBuild {
        if let Some(usage) = metadata
            .get("usage")
            .or_else(|| metadata.get("promptUsage"))
            .filter(|value| value.is_object())
        {
            sink.emit(
                "usage.updated",
                json!({
                    "provider": provider.as_str(), "source": "provider", "scope": "turn",
                    "aggregation": "snapshot", "final": true,
                    "nativeTurnId": metadata.get("promptId").or_else(|| metadata.get("prompt_id")),
                    "model": metadata.get("modelId").or_else(|| metadata.get("model_id")),
                    "usage": normalize_grok_usage(usage, false), "metadata": metadata,
                }),
            )
            .await?;
        }
    }
    Ok(())
}

/// Give cancellation a bounded protocol drain before the caller terminates the process.
/// Reverse requests during this window are rejected without waiting for another user action.
async fn drain_cancelled_turn(
    process: &mut JsonLineProcess,
    session_id: &str,
    turn_id: &str,
    sink: &DriverEventSink,
    provider: ProviderKind,
    client_requests: &mut tokio::task::JoinSet<(Vec<Value>, Result<(), AppError>)>,
    terminal_seen: bool,
) -> Result<DriverTurnResult, AppError> {
    let drain = async {
        if terminal_seen {
            return Ok(true);
        }
        let (_cancel_tx, mut cancelled) = watch::channel(true);
        loop {
            let message = tokio::select! {
                message = process.read() => message?,
                response = client_requests.join_next(), if !client_requests.is_empty() => {
                    if let Some(Ok((responses, _))) = response { for response in responses { process.send(&response).await?; } }
                    continue;
                }
            };
            let Some(message) = message else {
                return Ok::<bool, AppError>(false);
            };
            if jsonrpc_id(&message) == Some(turn_id) {
                emit_prompt_metadata(&message, sink, provider).await?;
                return Ok(true);
            }
            if let Some(id) = message
                .get("id")
                .filter(|_| message.get("method").is_some())
            {
                if message.get("method").and_then(Value::as_str)
                    == Some("session/request_permission")
                {
                    send_result(
                        process,
                        id,
                        serde_json::to_value(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))?,
                    )
                    .await?;
                } else {
                    process.send(&json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32800,"message":"turn cancelled"}})).await?;
                }
            } else {
                handle_acp_message(process, message, sink, &mut cancelled, provider, true).await?;
            }
        }
    };
    let graceful = matches!(
        tokio::time::timeout(Duration::from_secs(3), drain).await,
        Ok(Ok(true))
    );
    sink.emit("provider.event", json!({"provider":provider.as_str(), "providerMethod":"session/cancel/result", "metadata":{"graceful":graceful}})).await?;
    Ok(DriverTurnResult {
        native_session_id: Some(session_id.to_owned()),
        stop_reason: "cancelled".to_owned(),
        cancelled: true,
    })
}

fn ensure_acp_images_supported(
    prompt: &DriverPrompt,
    advertised: bool,
    allow_unadvertised: bool,
    provider: ProviderKind,
) -> Result<(), AppError> {
    let has_images = prompt
        .content
        .iter()
        .any(|content| matches!(content, super::types::DriverPromptContent::Image { .. }));
    if has_images && !advertised && !allow_unadvertised {
        Err(AppError::ImageInputUnsupported(format!(
            "{} does not advertise ACP image prompt support",
            provider.as_str()
        )))
    } else {
        Ok(())
    }
}

fn acp_prompt_content(prompt: &DriverPrompt) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    if !prompt.text.is_empty() {
        content.push(ContentBlock::Text(TextContent::new(prompt.text.clone())));
    }
    content.extend(prompt.content.iter().filter_map(|item| match item {
        super::types::DriverPromptContent::Image {
            data, mime_type, ..
        } => Some(ContentBlock::Image(ImageContent::new(data, mime_type))),
        super::types::DriverPromptContent::File { .. } => None,
    }));
    content
}

fn normalize_acp_image_wire(request: &mut Value) {
    if let Some(items) = request.get_mut("prompt").and_then(Value::as_array_mut) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("image") {
                if let Some(mime_type) = item.get("mimeType").cloned() {
                    let object = item.as_object_mut().expect("image content object");
                    object.remove("mimeType");
                    object.insert("mime_type".to_owned(), mime_type);
                }
            }
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
    let mut deadline = tokio::time::Instant::now() + super::process::control_timeout()?;
    loop {
        let message = tokio::select! {
            message = process.read_control_until(deadline) => message?,
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
        let waits_for_user = message.get("id").is_some()
            && message
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| {
                    matches!(
                        extension_method(method),
                        "session/request_permission"
                            | "x.ai/ask_user_question"
                            | "x.ai/exit_plan_mode"
                            | "x.ai/mcp/elicit"
                    )
                });
        let handler_started = tokio::time::Instant::now();
        handle_acp_message(
            process,
            message,
            sink,
            cancel,
            provider,
            emit_stream_updates,
        )
        .await?;
        if waits_for_user {
            deadline += handler_started.elapsed();
        }
    }
}

#[async_trait]
trait AcpWriter: Send {
    async fn write_response(&mut self, value: Value) -> Result<(), AppError>;
}

#[async_trait]
impl AcpWriter for JsonLineProcess {
    async fn write_response(&mut self, value: Value) -> Result<(), AppError> {
        self.send(&value).await
    }
}

#[derive(Default)]
struct BufferedAcpWriter {
    responses: Vec<Value>,
}
#[async_trait]
impl AcpWriter for BufferedAcpWriter {
    async fn write_response(&mut self, value: Value) -> Result<(), AppError> {
        self.responses.push(value);
        Ok(())
    }
}

fn is_client_request(message: &Value) -> bool {
    message.get("id").is_some()
        && message
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| {
                matches!(
                    extension_method(method),
                    "session/request_permission"
                        | "x.ai/ask_user_question"
                        | "x.ai/exit_plan_mode"
                        | "x.ai/mcp/elicit"
                )
            })
}

pub(super) async fn handle_acp_message(
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
    if is_client_request(&message) {
        return handle_client_request(process, &message, sink, cancel).await;
    }
    if provider == ProviderKind::GrokBuild && is_grok_update_method(method) {
        if !emit_stream_updates {
            return Ok(());
        }
        if let Some((event, payload)) = grok_activity_event(&params) {
            sink.emit(event, payload).await?;
            return Ok(());
        }
        if method != "session/update" {
            sink.emit(
                "provider.event",
                json!({
                    "provider": provider.as_str(), "providerMethod": method, "metadata": params,
                }),
            )
            .await?;
            return Ok(());
        }
    }
    if method == "session/update" {
        let notification = serde_json::from_value::<SessionNotification>(params.clone());
        if let Err(error) = notification {
            if provider == ProviderKind::GrokBuild {
                sink.emit(
                    "provider.event",
                    json!({"provider":provider.as_str(),"providerMethod":method,"metadata":params}),
                )
                .await?;
                return Ok(());
            }
            return Err(AppError::InvalidRequest(format!(
                "invalid ACP session update: {error}"
            )));
        }
        let update = params.get("update").cloned().unwrap_or(Value::Null);
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
            "config_option_update" => (
                "turn.configuration",
                json!({"provider":provider_id,"source":"provider-confirmed","effectiveConfig":update.get("configOptions").and_then(config_effective),"metadata":update}),
            ),
            "available_commands_update"
                if matches!(provider, ProviderKind::GrokBuild | ProviderKind::Devin) =>
            {
                (
                    "provider.commands.updated",
                    json!({ "provider": provider_id, "commands": super::grok::parse_commands(&json!({"_meta":{"availableCommands":update.get("availableCommands")}})), "metadata":update }),
                )
            }
            "usage_update" if provider == ProviderKind::Devin => (
                "usage.updated",
                json!({ "provider": provider_id, "source": "provider", "scope": "turn",
                    "aggregation": "snapshot", "final": false,
                    "usage": normalize_devin_usage(&update), "metadata": update }),
            ),
            "current_mode_update" if provider == ProviderKind::Devin => (
                "turn.configuration",
                json!({ "provider": provider_id, "source": "provider-confirmed",
                    "effectiveConfig": { "mode": update.get("currentModeId"), "source": "provider-confirmed" },
                    "effectiveFrom": "current-turn", "metadata": update }),
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

async fn handle_client_request(
    process: &mut (impl AcpWriter + ?Sized),
    message: &Value,
    sink: &DriverEventSink,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), AppError> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if *cancel.borrow() {
        let id = required_request_id(message, "ACP client request")?;
        let response = match extension_method(method) {
            "session/request_permission" => serde_json::to_value(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))?,
            "x.ai/mcp/elicit" => json!({"action":"cancel"}),
            _ => json!({"outcome":"cancelled"}),
        };
        send_result(process, id, response).await?;
        return Err(AppError::TurnCancelled);
    }
    if extension_method(method) == "x.ai/ask_user_question" {
        return handle_ask_user_question(process, message, params, sink, cancel).await;
    }
    if extension_method(method) == "x.ai/exit_plan_mode" {
        return handle_exit_plan_mode(process, message, params, sink, cancel).await;
    }
    if extension_method(method) == "x.ai/mcp/elicit" {
        return handle_mcp_elicit(process, message, params, sink, cancel).await;
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
                    .write_response(
                        json!({ "jsonrpc": "2.0", "id": request_id, "result": response }),
                    )
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
            .write_response(json!({ "jsonrpc": "2.0", "id": request_id, "result": response }))
            .await?;
        return Ok(());
    }
    Ok(())
}

/// Grok's prompt ledger includes cached input, while individual response usage excludes it.
/// Normalize both to included-cache counts; missing counts remain missing rather than zero.
fn normalize_grok_usage(raw: &Value, response: bool) -> Value {
    let mut last = serde_json::Map::new();
    let names = if response {
        [
            ("input", "input_tokens"),
            ("output", "output_tokens"),
            ("cacheRead", "cache_read_input_tokens"),
            ("cacheWrite", "cache_creation_input_tokens"),
            ("total", "total_tokens"),
        ]
    } else {
        [
            ("input", "inputTokens"),
            ("output", "outputTokens"),
            ("cacheRead", "cachedReadTokens"),
            ("cacheWrite", "cacheCreationTokens"),
            ("total", "totalTokens"),
        ]
    };
    for (target, source) in names {
        if let Some(count) = raw.get(source).and_then(Value::as_u64) {
            last.insert(target.to_owned(), json!(count));
        }
    }
    if response {
        let included_input = last
            .get("input")
            .and_then(Value::as_u64)
            .zip(last.get("cacheRead").and_then(Value::as_u64))
            .and_then(|(input, cache)| input.checked_add(cache))
            .zip(last.get("cacheWrite").and_then(Value::as_u64))
            .and_then(|(input, cache)| input.checked_add(cache));
        match included_input {
            Some(input) => {
                last.insert("input".to_owned(), json!(input));
            }
            None => {
                last.remove("input");
            }
        }
    }
    if !last.contains_key("total") {
        if let Some(total) = last
            .get("input")
            .and_then(Value::as_u64)
            .zip(last.get("output").and_then(Value::as_u64))
            .and_then(|(input, output)| input.checked_add(output))
        {
            last.insert("total".to_owned(), json!(total));
        }
    }
    json!({"last":last, "cacheSemantics":"included", "raw":raw})
}

/// Devin reports turn usage both as `usage_update` notifications (used/size
/// plus `_meta["cognition.ai/inputTokens"]` etc.) and as a flat `usage` object
/// on the prompt response. Normalize both shapes into one summary.
fn normalize_devin_usage(raw: &Value) -> Value {
    let meta = raw.get("_meta").cloned().unwrap_or(Value::Null);
    let pick = |keys: &[&str]| -> Option<Value> {
        keys.iter().find_map(|key| {
            raw.get(*key)
                .or_else(|| meta.get(*key))
                .or_else(|| meta.get(format!("cognition.ai/{key}")))
                .cloned()
        })
    };
    json!({
        "input": pick(&["inputTokens", "input_tokens", "input"]),
        "output": pick(&["outputTokens", "output_tokens", "output"]),
        "total": pick(&["totalTokens", "total_tokens", "total", "used"]),
        "contextWindow": pick(&["size", "contextWindow", "contextTokens"]),
        "raw": raw,
    })
}

fn grok_activity_event(params: &Value) -> Option<(&'static str, Value)> {
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate")?.as_str()?;
    let event = match kind {
        "subagent_spawned" => "subagent.started",
        "subagent_progress" => "subagent.updated",
        "subagent_finished" => match update.get("status")?.as_str()? {
            "completed" => "subagent.completed",
            "failed" => "subagent.failed",
            "cancelled" => "subagent.cancelled",
            _ => return None,
        },
        "auto_compact_started" => "compaction.started",
        "auto_compact_completed" => "compaction.completed",
        "auto_compact_failed" => "compaction.failed",
        "auto_compact_cancelled" => "compaction.cancelled",
        "response_completed" if update.get("usage").is_some_and(Value::is_object) => {
            "usage.updated"
        }
        _ => return None,
    };
    if event.starts_with("subagent.") && !update.get("subagent_id").is_some_and(Value::is_string) {
        return None;
    }
    Some((
        event,
        json!({
            "provider": "grok-build", "source": "provider", "scope": "message",
            "nativeSessionId": params.get("sessionId"), "turnId": update.get("parent_prompt_id"),
            "subagentId": update.get("subagent_id"), "parentId": update.get("parent_session_id"),
            "title": update.get("subagent_type"), "task": update.get("description"),
            "result": update.get("output"), "status": update.get("status"), "error": update.get("error"),
            "messageId": update.get("message_id"), "usage": update.get("usage").map(|usage| normalize_grok_usage(usage, true)), "metadata": update,
        }),
    ))
}

async fn handle_ask_user_question(
    process: &mut (impl AcpWriter + ?Sized),
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
    process: &mut (impl AcpWriter + ?Sized),
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
        | PermissionOutcome::Answer
        | PermissionOutcome::AbortTurn => {
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
    process: &mut (impl AcpWriter + ?Sized),
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
    process: &mut (impl AcpWriter + ?Sized),
    id: &Value,
    result: Value,
) -> Result<(), AppError> {
    process
        .write_response(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .await
}

async fn send_invalid_params(
    process: &mut (impl AcpWriter + ?Sized),
    id: &Value,
    message: &str,
) -> Result<(), AppError> {
    process
        .write_response(json!({
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
    let Some(selected) = select_auth_method(initialize, options.auth_method.as_deref(), provider)?
    else {
        return Ok(());
    };
    let mut params = json!({ "methodId": selected });
    if let Some(meta) = &options.auth_meta {
        params["_meta"] = meta.clone();
    }
    send_request(process, "authenticate", "authenticate", params).await?;
    wait_for_response(process, "authenticate", sink, cancel, provider, true).await?;
    Ok(())
}

pub(super) fn select_auth_method(
    initialize: &Value,
    configured: Option<&str>,
    provider: ProviderKind,
) -> Result<Option<String>, AppError> {
    let advertised = initialize
        .get("authMethods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|method| method.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if advertised.is_empty() {
        return Ok(None);
    }
    let configured = configured
        .map(str::trim)
        .filter(|method| !method.is_empty());
    let selected = configured
        .or_else(|| {
            initialize
                .pointer("/_meta/defaultAuthMethodId")
                .and_then(Value::as_str)
        })
        .or_else(|| advertised.contains(&"cached_token").then_some("cached_token"))
        .or_else(|| (advertised.len() == 1).then_some(advertised[0]))
        .ok_or_else(|| {
            AppError::ProviderUnavailable(format!(
                "{} advertised multiple authentication methods but no default; configure an auth method",
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
    Ok(Some(selected.to_owned()))
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
) -> Result<Option<Vec<SessionConfigOption>>, AppError> {
    if options.is_none() && context.runtime.legacy_model_state {
        apply_legacy_model_config(process, legacy_models, prompt, sink, cancel, context).await?;
        return Ok(None);
    }
    let mut requested_configs = vec![
        ("model", prompt.model.as_deref()),
        ("reasoning_effort", prompt.reasoning_effort.as_deref()),
    ];
    if context.provider == ProviderKind::Devin {
        requested_configs.push(("mode", devin_session_mode(prompt)));
    }
    let mut current_options = options.map(<[SessionConfigOption]>::to_vec);
    for (config_id, requested) in requested_configs {
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
    if let Some(options) = &current_options {
        if let Some(effective) = config_effective(&serde_json::to_value(options)?) {
            sink.emit("turn.configuration", json!({"provider":context.provider.as_str(), "source":"provider-confirmed", "effectiveConfig":effective, "effectiveFrom":"current-turn"})).await?;
        }
    }
    Ok(current_options)
}

/// Devin session modes map the product permission modes onto its native
/// ask / accept-edits / plan / bypass selector. Plan work always wins.
fn devin_session_mode(prompt: &DriverPrompt) -> Option<&'static str> {
    if prompt.work_mode.as_deref() == Some("plan") {
        return Some("plan");
    }
    match prompt.permission_mode.as_deref() {
        Some("ask") => Some("ask"),
        Some("auto") => Some("accept-edits"),
        Some("full-access") => Some("bypass"),
        _ => None,
    }
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
        PermissionOutcome::Answer | PermissionOutcome::AbortTurn => {
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

    #[test]
    fn devin_session_mode_maps_permission_and_work_modes() {
        let mut prompt = image_prompt();
        prompt.permission_mode = Some("ask".to_owned());
        assert_eq!(devin_session_mode(&prompt), Some("ask"));
        prompt.permission_mode = Some("auto".to_owned());
        assert_eq!(devin_session_mode(&prompt), Some("accept-edits"));
        prompt.permission_mode = Some("full-access".to_owned());
        assert_eq!(devin_session_mode(&prompt), Some("bypass"));
        prompt.permission_mode = None;
        assert_eq!(devin_session_mode(&prompt), None);
        prompt.permission_mode = Some("auto".to_owned());
        prompt.work_mode = Some("plan".to_owned());
        assert_eq!(devin_session_mode(&prompt), Some("plan"));
    }

    #[test]
    fn devin_usage_normalizes_notifications_and_prompt_results() {
        let update = json!({
            "used": 12466, "size": 262000,
            "_meta": {
                "cognition.ai/inputTokens": 12422,
                "cognition.ai/outputTokens": 44
            }
        });
        let usage = normalize_devin_usage(&update);
        assert_eq!(usage["input"], 12422);
        assert_eq!(usage["output"], 44);
        assert_eq!(usage["total"], 12466);
        assert_eq!(usage["contextWindow"], 262000);

        let result = json!({"totalTokens": 12466, "inputTokens": 12422, "outputTokens": 44});
        let usage = normalize_devin_usage(&result);
        assert_eq!(usage["input"], 12422);
        assert_eq!(usage["total"], 12466);
        assert!(usage["contextWindow"].is_null());
    }

    #[test]
    fn devin_controls_use_plain_string_config_values() {
        let commands = devin_control_commands(
            &ProviderControl::Configure {
                model: Some("claude-opus-5-low".to_owned()),
                reasoning_effort: None,
            },
            "devin-native",
        )
        .unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "session/set_config_option");
        assert_eq!(commands[0].1["configId"], "model");
        assert_eq!(commands[0].1["value"], "claude-opus-5-low");
        assert!(matches!(
            devin_control_commands(
                &ProviderControl::Steer {
                    text: "redirect".to_owned()
                },
                "devin-native"
            ),
            Err(AppError::Unsupported(_))
        ));
        assert!(matches!(
            devin_control_commands(
                &ProviderControl::Configure {
                    model: None,
                    reasoning_effort: Some("high".to_owned())
                },
                "devin-native"
            ),
            Err(AppError::Unsupported(_))
        ));
        assert!(matches!(
            devin_control_commands(&ProviderControl::QueueList, "devin-native"),
            Err(AppError::Unsupported(_))
        ));
    }

    fn image_prompt() -> DriverPrompt {
        DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "inspect this".to_owned(),
            content: vec![super::super::types::DriverPromptContent::Image {
                path: None,
                data: "aGVsbG8=".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            skills: Vec::new(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        }
    }

    #[test]
    fn acp_prompt_serializes_images_as_protocol_content_blocks() {
        let mut value = serde_json::to_value(PromptRequest::new(
            "session-1".to_owned(),
            acp_prompt_content(&image_prompt()),
        ))
        .unwrap();
        normalize_acp_image_wire(&mut value);
        assert_eq!(value["prompt"][0]["type"], "text");
        assert_eq!(value["prompt"][1]["type"], "image");
        assert_eq!(value["prompt"][1]["data"], "aGVsbG8=");
        assert_eq!(value["prompt"][1]["mime_type"], "image/png");
        assert!(value["prompt"][1].get("mimeType").is_none());
    }

    #[test]
    fn acp_rejects_unadvertised_images_except_for_vendor_override() {
        let prompt = image_prompt();
        assert!(matches!(
            ensure_acp_images_supported(&prompt, false, false, ProviderKind::Acp),
            Err(AppError::ImageInputUnsupported(_))
        ));
        assert!(ensure_acp_images_supported(&prompt, false, true, ProviderKind::GrokBuild).is_ok());
    }

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
    fn acp_profile_auth_method_selects_single_advertised_method() {
        let initialize = json!({ "authMethods": [{ "id": "devin-browser" }] });
        assert_eq!(
            select_auth_method(&initialize, None, ProviderKind::Acp)
                .unwrap()
                .as_deref(),
            Some("devin-browser")
        );
    }

    #[test]
    fn acp_profile_auth_method_honors_configured_choice() {
        let initialize = json!({ "authMethods": [{ "id": "a" }, { "id": "b" }] });
        assert_eq!(
            select_auth_method(&initialize, Some("b"), ProviderKind::Acp)
                .unwrap()
                .as_deref(),
            Some("b")
        );
        assert!(matches!(
            select_auth_method(&initialize, None, ProviderKind::Acp),
            Err(AppError::ProviderUnavailable(message)) if message.contains("multiple authentication methods")
        ));
        assert!(matches!(
            select_auth_method(&initialize, Some("missing"), ProviderKind::Acp),
            Err(AppError::ProviderUnavailable(message)) if message.contains("not advertised")
        ));
        assert_eq!(
            select_auth_method(&json!({}), Some("a"), ProviderKind::Acp).unwrap(),
            None
        );
    }

    #[test]
    fn acp_profile_runtime_options_resolve_api_key_from_profile_env() {
        let profile = AcpProfileConfig {
            command: "devin".to_owned(),
            args: vec!["acp".to_owned()],
            env: BTreeMap::from([("TODEX_TEST_ACP_KEY_3f9b1c".to_owned(), "secret".to_owned())]),
            auth_method: None,
            api_key_env: Some("TODEX_TEST_ACP_KEY_3f9b1c".to_owned()),
        };
        let options = profile_runtime_options(&profile).unwrap();
        assert!(options.authenticate);
        assert_eq!(options.auth_meta, Some(json!({ "api_key": "secret" })));
    }

    #[test]
    fn acp_profile_runtime_options_require_configured_api_key_env() {
        let profile = AcpProfileConfig {
            command: "devin".to_owned(),
            args: vec!["acp".to_owned()],
            env: BTreeMap::new(),
            auth_method: None,
            api_key_env: Some("TODEX_TEST_ACP_MISSING_7c2d4e".to_owned()),
        };
        assert!(matches!(
            profile_runtime_options(&profile),
            Err(AppError::ProviderUnavailable(message)) if message.contains("api_key_env")
        ));
        let profile = AcpProfileConfig {
            api_key_env: None,
            ..profile
        };
        let options = profile_runtime_options(&profile).unwrap();
        assert!(!options.authenticate);
        assert_eq!(options.auth_meta, None);
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
    #[test]
    fn grok_extension_activity_preserves_actual_failure_status() {
        let event = super::grok_activity_event(&json!({"sessionId":"parent", "update":{"sessionUpdate":"subagent_finished", "subagent_id":"child", "status":"failed", "error":"failure"}})).unwrap();
        assert_eq!(event.0, "subagent.failed");
        assert_eq!(event.1["subagentId"], "child");
        assert!(super::grok_activity_event(
            &json!({"update":{"sessionUpdate":"subagent_finished", "status":"unknown"}})
        )
        .is_none());
    }
    #[test]
    fn grok_usage_unifies_response_and_prompt_cache_semantics_without_inventing_missing_counts() {
        let response = normalize_grok_usage(
            &json!({"input_tokens":20,"output_tokens":10,"cache_read_input_tokens":50,"cache_creation_input_tokens":30}),
            true,
        );
        let prompt = normalize_grok_usage(
            &json!({"inputTokens":100,"outputTokens":10,"cachedReadTokens":50,"cacheCreationTokens":30,"totalTokens":110}),
            false,
        );
        assert_eq!(response["last"], prompt["last"]);
        assert_eq!(response["last"]["total"], 110);
        assert_eq!(prompt["cacheSemantics"], "included");
        let absent = normalize_grok_usage(&json!({"usageIsIncomplete":true}), false);
        assert_eq!(absent["last"], json!({}));
        assert_eq!(absent["raw"]["usageIsIncomplete"], true);
        let missing_cache =
            normalize_grok_usage(&json!({"input_tokens":20,"output_tokens":10}), true);
        assert!(missing_cache["last"].get("input").is_none());
        assert!(missing_cache["last"].get("total").is_none());
    }

    #[test]
    fn grok_error_metadata_keeps_partial_prompt_usage() {
        let message = json!({"error":{"message":"failed","data":{"promptUsage":{"inputTokens":8,"usageIsIncomplete":true}}}});
        assert_eq!(
            prompt_metadata(&message).unwrap()["promptUsage"]["inputTokens"],
            8
        );
    }
}
