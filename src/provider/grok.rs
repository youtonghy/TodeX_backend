use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::config::AgentConfig;
use crate::conversation::{ProviderKind, ProviderState};
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::acp::{run_acp_turn_controlled, AcpConnectionState, AcpRuntimeOptions};
use super::process::{
    executable_available, redact_sensitive_text, run_bounded_command, CommandSpec, JsonLineProcess,
};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PendingProviderControl, ProviderCapabilities, ProviderCommandDescriptor, ProviderControl,
    ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
};

const INSPECT_MAX_BYTES: usize = 4 * 1024 * 1024;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(8);

pub struct GrokBuildDriver {
    binary: String,
    auth_method: Option<String>,
    env_allowlist: Vec<String>,
    sessions: Mutex<HashMap<String, GrokSessionHandle>>,
}

#[derive(Clone)]
struct GrokSessionHandle {
    workspace: PathBuf,
    turns: mpsc::Sender<GrokTurn>,
    controls: mpsc::Sender<PendingProviderControl>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}

struct GrokTurn {
    context: DriverContext,
    prompt: DriverPrompt,
    sink: DriverEventSink,
    cancel: watch::Receiver<bool>,
    launch_permit: WorkspaceTrustPermit,
    respond_to: oneshot::Sender<Result<DriverTurnResult, AppError>>,
}

impl GrokBuildDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.grok_bin.clone(),
            auth_method: config.grok_auth_method.clone(),
            env_allowlist: config.grok_env_allowlist.clone(),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn command_spec(&self, workspace: &Path, prompt: Option<&DriverPrompt>) -> CommandSpec {
        grok_command_spec(&self.binary, &self.env_allowlist, workspace, prompt)
    }

    async fn initialize(&self, workspace: &Path) -> Result<Value, AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace, None)).await?;
        let result = initialize_process(&mut process).await;
        process.terminate().await;
        result
    }

    async fn authenticate(
        &self,
        process: &mut JsonLineProcess,
        initialize: &Value,
    ) -> Result<(), AppError> {
        if let Some(method) = super::acp::select_auth_method(
            initialize,
            self.auth_method.as_deref(),
            ProviderKind::GrokBuild,
        )? {
            control_request(
                process,
                "authenticate",
                "authenticate",
                json!({"methodId":method,"_meta":{"headless":true}}),
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl ProviderDriver for GrokBuildDriver {
    fn supports_live_controls(&self) -> bool {
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
            .ok_or_else(|| AppError::InvalidRequest("Grok session is not active".to_owned()))?;
        let (respond_to, response) = oneshot::channel();
        handle
            .controls
            .try_send(PendingProviderControl {
                expected_turn_id: expected_turn_id.to_owned(),
                request_id: request_id.to_owned(),
                control,
                respond_to,
            })
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "Grok control channel is unavailable or full".to_owned(),
                )
            })?;
        tokio::time::timeout(Duration::from_secs(20), response)
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable("Grok control acknowledgement timed out".to_owned())
            })?
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "Grok turn ended before control acknowledgement".to_owned(),
                )
            })?
    }

    async fn shutdown_session(&self, conversation_id: &str) {
        let handle = self.sessions.lock().await.remove(conversation_id);
        if let Some(handle) = handle {
            stop_session(handle).await;
        }
    }

    async fn shutdown(&self) {
        let handles: Vec<_> = self
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in &handles {
            let _ = handle.shutdown.send(true);
        }
        for handle in handles {
            stop_session(handle).await;
        }
    }

    fn supports_native_fork(&self) -> bool {
        true
    }

    async fn fork_session(
        &self,
        context: DriverContext,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<ProviderState, AppError> {
        let source = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| AppError::InvalidRequest("Grok session has not started".to_owned()))?;
        let mut process = JsonLineProcess::spawn_trusted(
            &self.command_spec(&context.manifest.workspace, None),
            launch_permit,
        )
        .await?;
        let result = async {
            let initialize = initialize_process(&mut process).await?;
            self.authenticate(&mut process, &initialize).await?;
            let response = control_request(
                &mut process,
                "fork",
                "_x.ai/session/fork",
                json!({
                    "sourceSessionId": source, "sourceCwd": context.manifest.workspace,
                    "newCwd": context.manifest.workspace,
                }),
            )
            .await?;
            let id = response
                .get("newSessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != source)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "invalid Grok fork response: missing distinct newSessionId".to_owned(),
                    )
                })?;
            let mut state = context.provider_state.clone();
            state.native_session_id = Some(id.to_owned());
            state.recoverable = true;
            state.last_error = None;
            Ok(state)
        }
        .await;
        process.terminate().await;
        result
    }

    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::GrokBuild,
            display_name: "Grok Build",
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install Grok Build from https://x.ai/cli",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                permission_config: super::types::permission_config_capabilities(
                    ProviderKind::GrokBuild,
                ),
                native_fork: true,
                native_compact: false,
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: false,
                model_selection: true,
                image_input: ProviderKind::GrokBuild.supports_image_input(),
                image_input_mode: ImageInputMode::Always,
            },
            models: Vec::new(),
        }
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        Ok(parse_models(&self.initialize(workspace).await?))
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace, None)).await?;
        let result = async {
            let initialize = initialize_process(&mut process).await?;
            self.authenticate(&mut process, &initialize).await?;
            let response = control_request(
                &mut process,
                "commands",
                "_x.ai/commands/list",
                json!({"cwd":workspace}),
            )
            .await?;
            let commands = response
                .get("commands")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    AppError::InvalidRequest("invalid Grok commands response".to_owned())
                })?;
            Ok(parse_commands(
                &json!({"_meta":{"availableCommands":commands}}),
            ))
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
        cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError> {
        let conversation_id = context.manifest.id.clone();
        let handle = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, handle| !handle.turns.is_closed());
            if let Some(handle) = sessions.get(&conversation_id) {
                if handle.workspace != context.manifest.workspace {
                    return Err(AppError::InvalidRequest(
                        "Grok resident session workspace changed".to_owned(),
                    ));
                }
                handle.clone()
            } else {
                if sessions.len() >= 32 {
                    return Err(AppError::ProviderUnavailable(
                        "Grok resident session limit reached (32); close an idle session"
                            .to_owned(),
                    ));
                }
                let (turns, turn_rx) = mpsc::channel(1);
                let (controls, control_rx) = mpsc::channel(16);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let (stopped_tx, stopped) = watch::channel(false);
                let handle = GrokSessionHandle {
                    workspace: context.manifest.workspace.clone(),
                    turns,
                    controls,
                    shutdown,
                    stopped,
                };
                let spec = self.command_spec(&context.manifest.workspace, Some(&prompt));
                let runtime = AcpRuntimeOptions {
                    authenticate: true,
                    auth_method: self.auth_method.clone(),
                    auth_meta: Some(json!({ "headless": true })),
                    auth_timeout: None,
                    suppress_load_replay: true,
                    allow_cli_config_fallback: true,
                    request_ask_mode: true,
                    legacy_model_state: true,
                    nested_config_values: true,
                    allow_unadvertised_images: true,
                    snake_case_image_mime: false,
                };
                tokio::spawn(run_session_actor(
                    spec,
                    runtime,
                    turn_rx,
                    control_rx,
                    shutdown_rx,
                    stopped_tx,
                ));
                sessions.insert(conversation_id, handle.clone());
                handle
            }
        };
        let (respond_to, response) = oneshot::channel();
        handle
            .turns
            .send(GrokTurn {
                context,
                prompt,
                sink,
                cancel,
                launch_permit,
                respond_to,
            })
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable("Grok session closed before turn started".to_owned())
            })?;
        response.await.map_err(|_| {
            AppError::ProviderUnavailable("Grok session stopped before turn completed".to_owned())
        })?
    }
}

async fn stop_session(mut handle: GrokSessionHandle) {
    let _ = handle.shutdown.send(true);
    if !*handle.stopped.borrow() {
        let _ = tokio::time::timeout(Duration::from_secs(6), handle.stopped.changed()).await;
    }
}

async fn run_session_actor(
    spec: CommandSpec,
    runtime: AcpRuntimeOptions,
    mut turns: mpsc::Receiver<GrokTurn>,
    mut controls: mpsc::Receiver<PendingProviderControl>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
) {
    let mut process: Option<JsonLineProcess> = None;
    let mut connection = AcpConnectionState::default();
    let mut last_sink: Option<DriverEventSink> = None;
    let mut idle_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let request = tokio::select! {
            request = turns.recv() => match request { Some(request) => request, None => break },
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep_until(idle_deadline) => break,
            control = controls.recv() => {
                if let Some(control) = control { let _ = control.respond_to.send(Err(AppError::InvalidRequest("Grok has no active turn".to_owned()))); }
                continue;
            }
            notification = async { match process.as_mut() { Some(process) => process.read().await, None => std::future::pending().await } } => {
                match notification {
                    Ok(Some(message)) => {
                        if let Some(process) = process.as_mut() {
                            if let Some(id) = message.get("id").filter(|_| message.get("method").is_some()) {
                                if process.send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32800,"message":"no active turn"}})).await.is_err() { break; }
                            } else if let Some(sink) = &last_sink {
                                super::acp::observe_config_options(&mut connection, &message);
                                let (_tx, mut cancel) = watch::channel(false);
                                if super::acp::handle_acp_message(process, message, sink, &mut cancel, ProviderKind::GrokBuild, true, super::acp::AutoApprove::Mediate, &mut connection).await.is_err() { break; }
                            }
                        }
                    }
                    _ => break,
                }
                continue;
            }
        };
        let GrokTurn {
            context,
            prompt,
            sink,
            mut cancel,
            launch_permit,
            respond_to,
        } = request;
        if process.is_none() {
            match JsonLineProcess::spawn_trusted(&spec, launch_permit).await {
                Ok(spawned) => process = Some(spawned),
                Err(error) => {
                    let _ = respond_to.send(Err(error));
                    break;
                }
            }
        } else {
            drop(launch_permit);
        }
        let mut options = runtime.clone();
        // A reused process cannot apply a new CLI fallback; require a protocol ACK.
        if last_sink.is_some() {
            options.allow_cli_config_fallback = false;
        }
        let result = tokio::select! {
            result = run_acp_turn_controlled(process.as_mut().unwrap(), context, prompt, &sink, &mut cancel, options, &mut connection, Some(&mut controls)) => result,
            _ = shutdown.changed() => { let _ = respond_to.send(Err(AppError::TurnCancelled)); break; }
        };
        let reusable = result.as_ref().is_ok_and(|result| !result.cancelled);
        last_sink = Some(sink);
        while let Ok(control) = controls.try_recv() {
            let _ = control.respond_to.send(Err(AppError::InvalidRequest(
                "Grok turn already ended".to_owned(),
            )));
        }
        if !reusable {
            turns.close();
            controls.close();
        }
        let _ = respond_to.send(result);
        if !reusable {
            break;
        }
        idle_deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    }
    turns.close();
    controls.close();
    if let Some(process) = process.as_mut() {
        process.terminate().await;
    }
    let _ = stopped.send(true);
}

async fn initialize_process(process: &mut JsonLineProcess) -> Result<Value, AppError> {
    let result = control_request(process, "initialize", "initialize", json!({
        "protocolVersion":1,
        "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"session":{"configOptions":{}}},
        "clientInfo":{"name":"todex-agentd","title":"TodeX 2.0","version":crate::version::APP_VERSION},
    })).await?;
    if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Err(AppError::Unsupported(
            "Grok negotiated an unsupported ACP protocol version".to_owned(),
        ));
    }
    Ok(result)
}

async fn control_request(
    process: &mut JsonLineProcess,
    id: &str,
    method: &str,
    params: Value,
) -> Result<Value, AppError> {
    process
        .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .await?;
    tokio::time::timeout(DIAGNOSTIC_TIMEOUT, async {
        loop {
            let Some(message) = process.read().await? else {
                return Err(AppError::ProviderUnavailable("Grok closed stdout during control request".to_owned()));
            };
            if message.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(AppError::ProviderUnavailable(format!("Grok {method} failed: {}", safe_message(error))));
                }
                return message.get("result").cloned().ok_or_else(|| AppError::InvalidRequest("Grok control response has no result".to_owned()));
            }
            if let Some(id) = message.get("id") {
                process.send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"client capability is not supported during control request"}})).await?;
            }
        }
    }).await.map_err(|_| AppError::ProviderUnavailable(format!("Grok {method} timed out")))?
}

pub(crate) async fn inspect_grok(
    config: &AgentConfig,
    workspace: &Path,
) -> Result<Value, AppError> {
    let mut spec = CommandSpec::new(&config.grok_bin, workspace);
    spec.args = vec![
        "--no-auto-update".to_owned(),
        "inspect".to_owned(),
        "--json".to_owned(),
    ];
    spec.env = grok_environment(&config.grok_env_allowlist);
    let output = run_bounded_command(&spec, INSPECT_MAX_BYTES, DIAGNOSTIC_TIMEOUT).await?;
    if !output.success {
        let stderr = redact_sensitive_text(&String::from_utf8_lossy(&output.stderr));
        return Err(AppError::ProviderUnavailable(if stderr.trim().is_empty() {
            "grok inspect failed".to_owned()
        } else {
            format!("grok inspect failed: {}", stderr.trim())
        }));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| AppError::InvalidRequest(format!("invalid grok inspect JSON: {error}")))
}

fn grok_command_spec(
    binary: &str,
    env_allowlist: &[String],
    workspace: &Path,
    prompt: Option<&DriverPrompt>,
) -> CommandSpec {
    let mut spec = CommandSpec::new(binary, workspace);
    spec.args = vec![
        "--no-auto-update".to_owned(),
        "agent".to_owned(),
        "--no-leader".to_owned(),
    ];
    if let Some(model) = prompt.and_then(|prompt| prompt.model.as_deref()) {
        spec.args.push("--model".to_owned());
        spec.args.push(model.to_owned());
    }
    if let Some(effort) = prompt.and_then(|prompt| prompt.reasoning_effort.as_deref()) {
        spec.args.push("--reasoning-effort".to_owned());
        spec.args.push(effort.to_owned());
    }
    spec.args.push("stdio".to_owned());
    spec.env = grok_environment(env_allowlist);
    spec
}

fn grok_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        ("GROK_DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()),
        ("NO_COLOR".to_owned(), "1".to_owned()),
    ]);
    for key in allowlist {
        if !valid_env_name(key) || key.starts_with("TODEX_AGENTD_") {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }
    env
}

fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn parse_models(initialize: &Value) -> Vec<ProviderModelDescriptor> {
    let current = initialize
        .pointer("/_meta/modelState/currentModelId")
        .and_then(Value::as_str);
    initialize
        .pointer("/_meta/modelState/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model
                .get("modelId")
                .or_else(|| model.get("id"))
                .and_then(Value::as_str)?
                .to_owned();
            let efforts = reasoning_efforts(model);
            Some(ProviderModelDescriptor {
                display_name: model
                    .get("name")
                    .or_else(|| model.get("displayName"))
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                description: model
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                is_default: current == Some(id.as_str()),
                default_reasoning_effort: model
                    .pointer("/_meta/defaultReasoningEffort")
                    .or_else(|| model.pointer("/_meta/reasoningEffort"))
                    .or_else(|| model.get("defaultReasoningEffort"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                context_window: model
                    .pointer("/_meta/totalContextTokens")
                    .or_else(|| model.get("contextWindow"))
                    .and_then(Value::as_u64),
                id,
                supported_reasoning_efforts: efforts,
                image_input: Some(true),
            })
        })
        .collect()
}

fn reasoning_efforts(model: &Value) -> Vec<String> {
    [
        model.get("supportedReasoningEfforts"),
        model.get("reasoningEfforts"),
        model.pointer("/_meta/supportedReasoningEfforts"),
        model.pointer("/_meta/reasoningEfforts"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|effort| {
        effort
            .as_str()
            .or_else(|| effort.get("value").and_then(Value::as_str))
            .or_else(|| effort.get("id").and_then(Value::as_str))
            .map(ToOwned::to_owned)
    })
    .collect()
}

pub(super) fn parse_commands(initialize: &Value) -> Vec<ProviderCommandDescriptor> {
    initialize
        .pointer("/_meta/availableCommands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let name = command.get("name").and_then(Value::as_str)?.to_owned();
            Some(ProviderCommandDescriptor {
                source: if name.contains(':') {
                    "skill-or-plugin".to_owned()
                } else {
                    "builtin".to_owned()
                },
                source_info: command.get("sourceInfo").cloned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                argument_hint: command
                    .pointer("/input/hint")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                invocation: "provider-prompt".to_owned(),
                package_name: None,
                package_version: None,
                name,
            })
        })
        .collect()
}

fn safe_message(value: &Value) -> String {
    redact_sensitive_text(
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider returned an error"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_models_and_commands_without_hard_coding_catalog_values() {
        let initialize = json!({
            "_meta": {
                "modelState": {
                    "currentModelId": "grok-current",
                    "availableModels": [{
                        "modelId": "grok-current",
                        "name": "Current Grok",
                        "description": "fixture",
                        "_meta": {
                            "totalContextTokens": 1000000,
                            "reasoningEfforts": ["low", "high"]
                        }
                    }]
                },
                "availableCommands": [{
                    "name": "repo:review",
                    "description": "Review changes",
                    "input": { "hint": "[path]" }
                }]
            }
        });
        let models = parse_models(&initialize);
        assert_eq!(models.len(), 1);
        assert!(models[0].is_default);
        assert_eq!(models[0].supported_reasoning_efforts, ["low", "high"]);
        assert_eq!(models[0].context_window, Some(1_000_000));
        let commands = parse_commands(&initialize);
        assert_eq!(commands[0].name, "repo:review");
        assert_eq!(commands[0].argument_hint.as_deref(), Some("[path]"));
    }

    #[test]
    fn environment_names_are_strict_and_todex_configuration_is_not_forwarded() {
        assert!(valid_env_name("XAI_API_KEY"));
        assert!(!valid_env_name("XAI-API-KEY"));
        assert!(!valid_env_name("1SECRET"));
    }

    #[test]
    fn command_spec_matches_supported_stdio_argv() {
        let prompt = DriverPrompt {
            turn_id: "turn-1".to_owned(),
            text: "hello".to_owned(),
            content: Vec::new(),
            skills: Vec::new(),
            model: Some("grok-4.5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        let spec = grok_command_spec("grok", &[], Path::new("/tmp"), Some(&prompt));
        assert_eq!(
            spec.args,
            [
                "--no-auto-update",
                "agent",
                "--no-leader",
                "--model",
                "grok-4.5",
                "--reasoning-effort",
                "high",
                "stdio",
            ]
        );
    }

    #[test]
    fn parses_legacy_reasoning_metadata() {
        let models = parse_models(&json!({
            "_meta": { "modelState": {
                "currentModelId": "grok-4.5",
                "availableModels": [{
                    "modelId": "grok-4.5",
                    "name": "Grok 4.5",
                    "_meta": {
                        "reasoningEffort": "high",
                        "reasoningEfforts": [
                            { "id": "high", "label": "High" },
                            { "id": "low", "label": "Low" }
                        ]
                    }
                }]
            }}
        }));
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(models[0].supported_reasoning_efforts, ["high", "low"]);
    }
    #[cfg(unix)]
    struct Fixture {
        root: PathBuf,
        driver: std::sync::Arc<GrokBuildDriver>,
        store: crate::conversation::ConversationStore,
        manifest: crate::conversation::ConversationManifest,
        trust: crate::workspace_trust::WorkspaceTrustStore,
    }

    #[cfg(unix)]
    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-grok-wire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let binary = root.join("grok-fixture");
            std::fs::write(
                &binary,
                include_str!("../../tests/fixtures/grok_acp_fixture.py"),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let driver = std::sync::Arc::new(GrokBuildDriver {
                binary: binary.to_string_lossy().to_string(),
                auth_method: None,
                env_allowlist: vec![],
                sessions: Mutex::new(HashMap::new()),
            });
            let store = crate::conversation::ConversationStore::new(root.join("data"))
                .await
                .unwrap();
            let manifest = store
                .create(crate::conversation::ConversationManifest::new(
                    ProviderKind::GrokBuild,
                    root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap();
            let trust =
                crate::workspace_trust::WorkspaceTrustStore::new(root.join("data"), root.clone())
                    .await
                    .unwrap();
            trust.set_owned("local", &root, true).await.unwrap();
            Self {
                root,
                driver,
                store,
                manifest,
                trust,
            }
        }

        async fn start(
            &self,
            turn_id: &str,
            text: &str,
        ) -> (
            watch::Sender<bool>,
            tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
        ) {
            self.start_with_model(turn_id, text, None).await
        }

        async fn start_with_model(
            &self,
            turn_id: &str,
            text: &str,
            model: Option<&str>,
        ) -> (
            watch::Sender<bool>,
            tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
        ) {
            let context = DriverContext {
                manifest: self.manifest.clone(),
                provider_state: self.store.provider_state(&self.manifest.id).await.unwrap(),
            };
            let prompt = DriverPrompt {
                turn_id: turn_id.to_owned(),
                text: text.to_owned(),
                content: vec![],
                skills: vec![],
                model: model.map(ToOwned::to_owned),
                reasoning_effort: None,
                permission_mode: None,
                work_mode: None,
                permission_profile: None,
                sandbox_mode: None,
                approval_policy: None,
            };
            let sink = DriverEventSink::new(
                self.store.clone(),
                crate::conversation::ConversationEventHub::default(),
                super::super::types::PermissionBroker::default(),
                &self.manifest.id,
            )
            .with_turn_id(turn_id);
            let permit = self.trust.acquire_owned("local", &self.root).await.unwrap();
            let driver = self.driver.clone();
            let (cancel, receiver) = watch::channel(false);
            let task = tokio::spawn(async move {
                driver
                    .run_turn(context, prompt, sink, receiver, permit)
                    .await
            });
            (cancel, task)
        }

        async fn wait_for_method(&self, method: &str) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if self
                        .requests()
                        .iter()
                        .any(|request| request["method"] == method)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture request deadline");
        }

        fn requests(&self) -> Vec<Value> {
            std::fs::read_to_string(self.root.join("grok-requests.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }

        async fn finish(self) {
            self.driver.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_routes_extensions_preserves_final_metadata_and_reuses_session() {
        let fixture = Fixture::new().await;
        for turn in ["turn-first", "turn-second"] {
            let (_cancel, task) = fixture.start(turn, "normal").await;
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(result.stop_reason, "end_turn");
        }
        let requests = fixture.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "initialize")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/new")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "session/load")
                .count(),
            0
        );
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        for kind in [
            "subagent.started",
            "subagent.completed",
            "compaction.started",
            "compaction.completed",
            "usage.updated",
        ] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == kind)
                    .count(),
                2,
                "{kind}"
            );
        }
        assert!(events.iter().any(
            |event| event.payload.pointer("/metadata/update/sessionUpdate")
                == Some(&json!("future_vendor_update"))
        ));
        assert!(events.iter().any(
            |event| event.payload.pointer("/metadata/structured_output/ok") == Some(&json!(true))
        ));
        assert!(events
            .iter()
            .filter(|event| event.event_type == "usage.updated")
            .all(|event| event.payload["usage"]["last"]["input"] == 100
                && event.payload["aggregation"] == "snapshot"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_live_controls_acknowledge_effective_config_even_after_prompt_terminal() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-live", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let stale = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "old-turn",
                "stale",
                ProviderControl::Steer {
                    text: "ignored".to_owned(),
                },
            )
            .await;
        assert!(stale.is_err());
        let steer = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "steer-1",
                ProviderControl::Steer {
                    text: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(steer["result"]["status"], "queued");
        let config = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "config-1",
                ProviderControl::Configure {
                    model: Some("grok-other".to_owned()),
                    reasoning_effort: Some("high".to_owned()),
                },
            )
            .await
            .unwrap();
        assert_eq!(config["source"], "provider-confirmed");
        assert_eq!(
            config["effectiveConfig"],
            json!({"model":"grok-other", "reasoningEffort":"high", "source":"provider-confirmed"})
        );
        assert!(!task.await.unwrap().unwrap().cancelled);
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "_x.ai/interject")
                .count(),
            1
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_drains_metadata_then_reload_keeps_ask_policy() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-cancel", "cancel").await;
        fixture.wait_for_method("session/prompt").await;
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        let (_cancel, task) = fixture.start("turn-resume", "normal").await;
        assert_eq!(task.await.unwrap().unwrap().stop_reason, "end_turn");
        let load = fixture
            .requests()
            .into_iter()
            .find(|request| request["method"] == "session/load")
            .unwrap();
        assert_eq!(
            load["params"]["_meta"],
            json!({"noReplay":true,"yoloMode":false,"autoMode":false})
        );
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events.iter().any(|event| event.payload["providerMethod"]
            == "session/cancel/result"
            && event.payload["metadata"]["graceful"] == true));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.updated")
                .count(),
            2
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_discovers_project_commands_and_forks_native_history() {
        let fixture = Fixture::new().await;
        let commands = fixture
            .driver
            .discover_commands(&fixture.root)
            .await
            .unwrap();
        assert_eq!(commands[0].name, "project:review");
        let mut state = ProviderState::new(ProviderKind::GrokBuild);
        state.native_session_id = Some("source-session".to_owned());
        let forked = fixture
            .driver
            .fork_session(
                DriverContext {
                    manifest: fixture.manifest.clone(),
                    provider_state: state,
                },
                fixture
                    .trust
                    .acquire_owned("local", &fixture.root)
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forked.native_session_id.as_deref(), Some("forked-session"));
        assert!(forked.recoverable);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_control_remains_responsive_during_permission_request() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-permission", "permission").await;
        fixture.wait_for_method("session/prompt").await;
        // The reverse permission request is pending while steering is acknowledged.
        let response = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-permission",
                "steer-permission",
                ProviderControl::Steer {
                    text: "continue".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["status"], "queued");
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_rejected_configuration_does_not_report_success_or_end_the_prompt() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-reject", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let rejected = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-reject",
                "reject-config",
                ProviderControl::Configure {
                    model: Some("reject".to_owned()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(matches!(rejected, Err(AppError::InvalidRequest(_))));
        assert!(!task.is_finished());
        fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-reject",
                "finish-reject",
                ProviderControl::Steer {
                    text: "finish".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap().stop_reason, "end_turn");
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_control_timeout_discards_the_resident_process_before_next_turn() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-timeout", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let rejected = tokio::time::timeout(
            Duration::from_secs(12),
            fixture.driver.control(
                &fixture.manifest.id,
                "turn-timeout",
                "hang-config",
                ProviderControl::Configure {
                    model: Some("hang".to_owned()),
                    reasoning_effort: None,
                },
            ),
        )
        .await
        .unwrap();
        assert!(rejected.is_err());
        assert!(task.await.unwrap().is_err());
        let (_cancel, next) = fixture.start("turn-after-timeout", "normal").await;
        assert_eq!(next.await.unwrap().unwrap().stop_reason, "end_turn");
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "initialize")
                .count(),
            2
        );
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_warm_session_reports_the_last_acknowledged_model_without_resending_it() {
        let fixture = Fixture::new().await;
        let (_cancel, first) = fixture
            .start_with_model("turn-configured", "normal", Some("grok-other"))
            .await;
        first.await.unwrap().unwrap();
        let (_cancel, second) = fixture.start("turn-warm", "normal").await;
        second.await.unwrap().unwrap();
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["turnId"] == "turn-warm"
                && event.payload["effectiveConfig"]["model"] == "grok-other"));
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "session/set_config_option")
                .count(),
            1
        );
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_after_prompt_terminal_keeps_metadata_without_waiting_for_it_again() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-cached-terminal", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let driver = fixture.driver.clone();
        let id = fixture.manifest.id.clone();
        let control = tokio::spawn(async move {
            driver
                .control(
                    &id,
                    "turn-cached-terminal",
                    "cached-config",
                    ProviderControl::Configure {
                        model: Some("hang-terminal".to_owned()),
                        reasoning_effort: None,
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fixture
                    .store
                    .complete_history(&fixture.manifest.id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|event| event.event_type == "usage.updated")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.send(true).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .cancelled
        );
        assert!(control.await.unwrap().is_err());
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.updated")
                .count(),
            1
        );
        assert!(events.iter().any(|event| event.payload["providerMethod"]
            == "session/cancel/result"
            && event.payload["metadata"]["graceful"] == true));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_drain_never_replies_to_a_late_control_response() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-late", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let driver = fixture.driver.clone();
        let id = fixture.manifest.id.clone();
        let control = tokio::spawn(async move {
            driver
                .control(
                    &id,
                    "turn-late",
                    "late-config",
                    ProviderControl::Configure {
                        model: Some("late-ack".to_owned()),
                        reasoning_effort: None,
                    },
                )
                .await
        });
        fixture.wait_for_method("session/set_config_option").await;
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        assert!(control.await.unwrap().is_err());
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events.iter().any(|event| event.payload["providerMethod"]
            == "session/cancel/result"
            && event.payload["metadata"]["graceful"] == true));
        fixture.finish().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn wire_malformed_configuration_ack_is_unknown_and_discards_the_process() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-malformed", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let outcome = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-malformed",
                "malformed-config",
                ProviderControl::Configure {
                    model: Some("malformed".to_owned()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(matches!(outcome, Err(AppError::ProviderUnavailable(_))));
        assert!(task.await.unwrap().is_err());
        fixture.finish().await;
    }
}
