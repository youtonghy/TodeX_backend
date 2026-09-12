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
use super::process::{executable_available, redact_sensitive_text, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PendingProviderControl, ProviderCapabilities, ProviderCommandDescriptor, ProviderControl,
    ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
};

const MAX_OPENCODE_SESSIONS: usize = 32;
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_DRAIN: Duration = Duration::from_millis(1500);

pub struct OpencodeDriver {
    binary: String,
    env_allowlist: Vec<String>,
    sessions: Mutex<HashMap<String, OpencodeSessionHandle>>,
}

#[derive(Clone)]
struct OpencodeSessionHandle {
    workspace: PathBuf,
    turns: mpsc::Sender<OpencodeTurn>,
    controls: mpsc::Sender<PendingProviderControl>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}

struct OpencodeTurn {
    context: DriverContext,
    prompt: DriverPrompt,
    sink: DriverEventSink,
    cancel: watch::Receiver<bool>,
    launch_permit: WorkspaceTrustPermit,
    respond_to: oneshot::Sender<Result<DriverTurnResult, AppError>>,
}

impl OpencodeDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.opencode_bin.clone(),
            env_allowlist: config.opencode_env_allowlist.clone(),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn command_spec(&self, workspace: &Path) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec!["acp".to_owned()];
        spec.env = opencode_environment(&self.env_allowlist);
        spec
    }

    /// OpenCode keeps credentials in its own store (`opencode auth login`); the
    /// advertised `opencode-login` method only describes that terminal flow, so
    /// the daemon never sends `authenticate`.
    fn runtime_options(&self) -> AcpRuntimeOptions {
        AcpRuntimeOptions {
            suppress_load_replay: true,
            ..Default::default()
        }
    }

    /// Models and slash commands live behind `session/new`. The session is left
    /// open so callers can issue follow-up probes (per-model effort options);
    /// callers must close it best-effort via `close_probe_session`.
    async fn session_probe(
        &self,
        workspace: &Path,
    ) -> Result<(JsonLineProcess, Value, Vec<Value>), AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace)).await?;
        let result = async {
            let _initialize = initialize_process(&mut process).await?;
            let mut updates = Vec::new();
            let response = control_request_updates(
                &mut process,
                "session",
                "session/new",
                json!({ "cwd": workspace, "mcpServers": [] }),
                &mut updates,
                DIAGNOSTIC_TIMEOUT,
            )
            .await?;
            response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    AppError::InvalidRequest("invalid OpenCode session/new response".to_owned())
                })?;
            // Command announcements trail the session/new response.
            let deadline = tokio::time::Instant::now() + COMMAND_DRAIN;
            loop {
                let read = tokio::time::timeout_at(deadline, process.read()).await;
                let Ok(Ok(Some(message))) = read else {
                    break;
                };
                if message.get("method").and_then(Value::as_str) == Some("session/update") {
                    let update = message.get("params").cloned().unwrap_or(Value::Null);
                    let is_commands = update
                        .pointer("/update/sessionUpdate")
                        .and_then(Value::as_str)
                        == Some("available_commands_update");
                    updates.push(update);
                    if is_commands {
                        break;
                    }
                } else if let Some(request_id) =
                    message.get("id").filter(|_| message.get("method").is_some())
                {
                    process
                        .send(&json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"client capability is not supported during probe"}}))
                        .await?;
                }
            }
            Ok((response, updates))
        }
        .await;
        match result {
            Ok((response, updates)) => Ok((process, response, updates)),
            Err(error) => {
                process.terminate().await;
                Err(error)
            }
        }
    }
}

/// Best-effort close for a probe session; failures are ignored because the
/// process is terminated immediately afterwards either way.
async fn close_probe_session(process: &mut JsonLineProcess, session: &Value) {
    let Some(session_id) = session.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let _ = control_request(
        process,
        "session:close",
        "session/close",
        json!({ "sessionId": session_id }),
        DIAGNOSTIC_TIMEOUT,
    )
    .await;
}

#[async_trait]
impl ProviderDriver for OpencodeDriver {
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
            .ok_or_else(|| AppError::InvalidRequest("OpenCode session is not active".to_owned()))?;
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
                    "OpenCode control channel is unavailable or full".to_owned(),
                )
            })?;
        tokio::time::timeout(CONTROL_TIMEOUT, response)
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "OpenCode control acknowledgement timed out".to_owned(),
                )
            })?
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "OpenCode turn ended before control acknowledgement".to_owned(),
                )
            })?
    }

    async fn shutdown_session(&self, conversation_id: &str) {
        let handle = self.sessions.lock().await.remove(conversation_id);
        if let Some(handle) = handle {
            stop_session(handle).await;
        }
    }

    async fn shutdown_session_with_reason(&self, conversation_id: &str, _reason: &str) {
        self.shutdown_session(conversation_id).await;
    }

    fn supports_runtime_stop(&self) -> bool {
        true
    }

    async fn stop_runtime(&self, conversation_id: &str) -> Result<Value, AppError> {
        self.shutdown_session(conversation_id).await;
        Ok(
            json!({"provider":"opencode","conversationId":conversation_id,"status":"stopped","reason":"user_closed"}),
        )
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

    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::Opencode,
            display_name: "OpenCode",
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install OpenCode from https://opencode.ai",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                permission_config: super::types::permission_config_capabilities(
                    ProviderKind::Opencode,
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
                image_input: ProviderKind::Opencode.supports_image_input(),
                image_input_mode: ImageInputMode::Always,
            },
            models: Vec::new(),
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
            .ok_or_else(|| {
                AppError::InvalidRequest("OpenCode session has not started".to_owned())
            })?;
        let mut process = JsonLineProcess::spawn_trusted(
            &self.command_spec(&context.manifest.workspace),
            launch_permit,
        )
        .await?;
        let result = async {
            let _initialize = initialize_process(&mut process).await?;
            let response = control_request(
                &mut process,
                "fork",
                "session/fork",
                json!({"sessionId": source, "cwd": context.manifest.workspace}),
                DIAGNOSTIC_TIMEOUT,
            )
            .await?;
            let id = response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != source)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "invalid OpenCode fork response: missing distinct sessionId".to_owned(),
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

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        let (mut process, session, _updates) = self.session_probe(workspace).await?;
        let result = async {
            let mut models = super::devin::parse_devin_models(&session);
            let session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // OpenCode only exposes an `effort` config option for the currently
            // selected model, so probe each model to learn its effort levels.
            for model in &mut models {
                let response = control_request(
                    &mut process,
                    "probe:model",
                    "session/set_config_option",
                    json!({"sessionId": session_id, "configId": "model", "value": model.id}),
                    CONTROL_TIMEOUT,
                )
                .await;
                let Ok(response) = response else {
                    continue;
                };
                let Some(effort) = response
                    .get("configOptions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|option| option.get("id").and_then(Value::as_str) == Some("effort"))
                else {
                    continue;
                };
                model.supported_reasoning_efforts = effort
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|option| {
                                option
                                    .get("value")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                model.default_reasoning_effort = effort
                    .get("currentValue")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Ok(models)
        }
        .await;
        close_probe_session(&mut process, &session).await;
        process.terminate().await;
        result
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let (mut process, session, updates) = self.session_probe(workspace).await?;
        close_probe_session(&mut process, &session).await;
        process.terminate().await;
        let commands = updates.iter().rev().find_map(|update| {
            update
                .pointer("/update/availableCommands")
                .filter(|commands| commands.is_array())
                .cloned()
        });
        Ok(commands
            .map(|commands| {
                super::grok::parse_commands(&json!({ "_meta": { "availableCommands": commands } }))
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
        let conversation_id = context.manifest.id.clone();
        let handle = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, handle| !handle.turns.is_closed());
            if let Some(handle) = sessions.get(&conversation_id) {
                if handle.workspace != context.manifest.workspace {
                    return Err(AppError::InvalidRequest(
                        "OpenCode resident session workspace changed".to_owned(),
                    ));
                }
                handle.clone()
            } else {
                if sessions.len() >= MAX_OPENCODE_SESSIONS {
                    return Err(AppError::ProviderUnavailable(
                        "OpenCode resident session limit reached (32); close an idle session"
                            .to_owned(),
                    ));
                }
                let (turns, turn_rx) = mpsc::channel(1);
                let (controls, control_rx) = mpsc::channel(16);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let (stopped_tx, stopped) = watch::channel(false);
                let handle = OpencodeSessionHandle {
                    workspace: context.manifest.workspace.clone(),
                    turns,
                    controls,
                    shutdown,
                    stopped,
                };
                let spec = self.command_spec(&context.manifest.workspace);
                let runtime = self.runtime_options();
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
            .send(OpencodeTurn {
                context,
                prompt,
                sink,
                cancel,
                launch_permit,
                respond_to,
            })
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "OpenCode session closed before turn started".to_owned(),
                )
            })?;
        response.await.map_err(|_| {
            AppError::ProviderUnavailable(
                "OpenCode session stopped before turn completed".to_owned(),
            )
        })?
    }
}

async fn stop_session(mut handle: OpencodeSessionHandle) {
    let _ = handle.shutdown.send(true);
    if !*handle.stopped.borrow() {
        let _ = tokio::time::timeout(Duration::from_secs(6), handle.stopped.changed()).await;
    }
}

async fn run_session_actor(
    spec: CommandSpec,
    runtime: AcpRuntimeOptions,
    mut turns: mpsc::Receiver<OpencodeTurn>,
    mut controls: mpsc::Receiver<PendingProviderControl>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
) {
    let mut process: Option<JsonLineProcess> = None;
    let mut connection = AcpConnectionState::default();
    let mut last_sink: Option<DriverEventSink> = None;
    let mut idle_deadline = tokio::time::Instant::now() + SESSION_IDLE_TIMEOUT;
    loop {
        let request = tokio::select! {
            request = turns.recv() => match request { Some(request) => request, None => break },
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep_until(idle_deadline) => break,
            control = controls.recv() => {
                if let Some(control) = control { let _ = control.respond_to.send(Err(AppError::InvalidRequest("OpenCode has no active turn".to_owned()))); }
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
                                if super::acp::handle_acp_message(process, message, sink, &mut cancel, ProviderKind::Opencode, true, super::acp::AutoApprove::Mediate).await.is_err() { break; }
                            }
                        }
                    }
                    _ => break,
                }
                continue;
            }
        };
        let OpencodeTurn {
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
        let result = tokio::select! {
            result = run_acp_turn_controlled(process.as_mut().unwrap(), context, prompt, &sink, &mut cancel, runtime.clone(), &mut connection, Some(&mut controls)) => result,
            _ = shutdown.changed() => { let _ = respond_to.send(Err(AppError::TurnCancelled)); break; }
        };
        let reusable = result.as_ref().is_ok_and(|result| !result.cancelled);
        last_sink = Some(sink);
        while let Ok(control) = controls.try_recv() {
            let _ = control.respond_to.send(Err(AppError::InvalidRequest(
                "OpenCode turn already ended".to_owned(),
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
        idle_deadline = tokio::time::Instant::now() + SESSION_IDLE_TIMEOUT;
    }
    turns.close();
    controls.close();
    if let Some(process) = process.as_mut() {
        process.terminate().await;
    }
    let _ = stopped.send(true);
}

async fn initialize_process(process: &mut JsonLineProcess) -> Result<Value, AppError> {
    let result = control_request(
        process,
        "initialize",
        "initialize",
        json!({
        "protocolVersion":1,
        "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"session":{"configOptions":{}}},
        "clientInfo":{"name":"todex-agentd","title":"TodeX 2.0","version":crate::version::APP_VERSION},
        }),
        DIAGNOSTIC_TIMEOUT,
    ).await?;
    if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Err(AppError::Unsupported(
            "OpenCode negotiated an unsupported ACP protocol version".to_owned(),
        ));
    }
    Ok(result)
}

async fn control_request(
    process: &mut JsonLineProcess,
    id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, AppError> {
    let mut updates = Vec::new();
    control_request_updates(process, id, method, params, &mut updates, timeout).await
}

async fn control_request_updates(
    process: &mut JsonLineProcess,
    id: &str,
    method: &str,
    params: Value,
    updates: &mut Vec<Value>,
    timeout: Duration,
) -> Result<Value, AppError> {
    process
        .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .await?;
    tokio::time::timeout(timeout, async {
        loop {
            let Some(message) = process.read().await? else {
                return Err(AppError::ProviderUnavailable("OpenCode closed stdout during control request".to_owned()));
            };
            if message.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(AppError::ProviderUnavailable(format!("OpenCode {method} failed: {}", safe_message(error))));
                }
                return message.get("result").cloned().ok_or_else(|| AppError::InvalidRequest("OpenCode control response has no result".to_owned()));
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(message.get("params").cloned().unwrap_or(Value::Null));
            }
            if let Some(request_id) = message.get("id").filter(|_| message.get("method").is_some()) {
                process.send(&json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"client capability is not supported during control request"}})).await?;
            }
        }
    }).await.map_err(|_| AppError::ProviderUnavailable(format!("OpenCode {method} timed out")))?
}

fn opencode_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("OPENCODE_DISABLE_AUTOUPDATE".to_owned(), "1".to_owned()),
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
    fn opencode_environment_filters_names_and_daemon_env() {
        unsafe {
            std::env::set_var("OPENCODE_TEST_ALLOWED_KEY", "present");
            std::env::set_var("TODEX_AGENTD_SECRET_TEST", "daemon-only");
        }
        let env = opencode_environment(&[
            "OPENCODE_TEST_ALLOWED_KEY".to_owned(),
            "TODEX_AGENTD_SECRET_TEST".to_owned(),
            "NOT_A_VALID NAME".to_owned(),
            "OPENCODE_TEST_MISSING_KEY".to_owned(),
        ]);
        assert_eq!(
            env.get("OPENCODE_TEST_ALLOWED_KEY"),
            Some(&"present".to_owned())
        );
        assert!(!env.contains_key("TODEX_AGENTD_SECRET_TEST"));
        assert!(!env.contains_key("NOT_A_VALID NAME"));
        assert!(!env.contains_key("OPENCODE_TEST_MISSING_KEY"));
        assert_eq!(env.get("NO_COLOR"), Some(&"1".to_owned()));
        assert_eq!(
            env.get("OPENCODE_DISABLE_AUTOUPDATE"),
            Some(&"1".to_owned())
        );
    }

    #[cfg(unix)]
    struct Fixture {
        root: PathBuf,
        driver: std::sync::Arc<OpencodeDriver>,
        store: crate::conversation::ConversationStore,
        manifest: crate::conversation::ConversationManifest,
        trust: crate::workspace_trust::WorkspaceTrustStore,
    }

    #[cfg(unix)]
    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-opencode-wire-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            let binary = root.join("opencode-fixture");
            std::fs::write(
                &binary,
                include_str!("../../tests/fixtures/opencode_acp_fixture.py"),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
            let driver = std::sync::Arc::new(OpencodeDriver {
                binary: binary.to_string_lossy().to_string(),
                env_allowlist: vec![],
                sessions: Mutex::new(HashMap::new()),
            });
            let store = crate::conversation::ConversationStore::new(root.join("data"))
                .await
                .unwrap();
            let manifest = store
                .create(crate::conversation::ConversationManifest::new(
                    ProviderKind::Opencode,
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
            self.start_with(turn_id, text, None, None, None, None).await
        }

        async fn start_with(
            &self,
            turn_id: &str,
            text: &str,
            model: Option<&str>,
            effort: Option<&str>,
            permission_mode: Option<&str>,
            work_mode: Option<&str>,
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
                reasoning_effort: effort.map(ToOwned::to_owned),
                permission_mode: permission_mode.map(ToOwned::to_owned),
                work_mode: work_mode.map(ToOwned::to_owned),
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
            std::fs::read_to_string(self.root.join("opencode-requests.jsonl"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }

        fn config_writes(&self) -> Vec<Value> {
            self.requests()
                .into_iter()
                .filter(|request| request["method"] == "session/set_config_option")
                .collect()
        }

        async fn finish(self) {
            self.driver.shutdown().await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_turns_reuse_resident_session_without_authenticate() {
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
        // OpenCode credentials live in its own store; never send authenticate.
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "authenticate")
                .count(),
            0
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
        // Every turn restates build mode so a previous plan turn cannot stick.
        assert_eq!(fixture.config_writes().len(), 2);
        assert!(fixture
            .config_writes()
            .iter()
            .all(|request| request["params"]["configId"] == "mode"
                && request["params"]["value"] == "build"));
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "message.delta")
                .count(),
            2
        );
        // usage_update notifications and final result usage both normalize.
        assert!(
            events
                .iter()
                .filter(|event| event.event_type == "usage.updated")
                .count()
                >= 2
        );
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["effectiveConfig"]["mode"] == "build"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_applies_model_and_plan_mode_then_resets_to_build() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start_with(
                "turn-plan",
                "normal",
                Some("opencode/other-model"),
                None,
                None,
                Some("plan"),
            )
            .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        let writes = fixture.config_writes();
        assert!(writes
            .iter()
            .any(|request| request["params"]["configId"] == "model"
                && request["params"]["value"] == "opencode/other-model"));
        assert!(writes
            .iter()
            .any(|request| request["params"]["configId"] == "mode"
                && request["params"]["value"] == "plan"));

        let (_cancel, task) = fixture.start("turn-implement", "normal").await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        let writes = fixture.config_writes();
        assert_eq!(
            writes
                .iter()
                .filter(|request| request["params"]["configId"] == "mode"
                    && request["params"]["value"] == "build")
                .count(),
            1
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_live_configure_model_acknowledges_effective_config() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-live", "hold").await;
        fixture.wait_for_method("session/prompt").await;
        let stale = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "old-turn",
                "stale",
                ProviderControl::Configure {
                    model: Some("opencode/other-model".to_owned()),
                    reasoning_effort: None,
                },
            )
            .await;
        assert!(stale.is_err());
        let config = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "config-1",
                ProviderControl::Configure {
                    model: Some("opencode/other-model".to_owned()),
                    reasoning_effort: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(config["source"], "provider-confirmed");
        assert_eq!(
            config["effectiveConfig"]["model"],
            json!("opencode/other-model")
        );
        let effort = fixture
            .driver
            .control(
                &fixture.manifest.id,
                "turn-live",
                "config-2",
                ProviderControl::Configure {
                    model: None,
                    reasoning_effort: Some("high".to_owned()),
                },
            )
            .await;
        assert!(effort.is_err());
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_cancel_then_resume_loads_native_session() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture.start("turn-cancel", "cancel").await;
        fixture.wait_for_method("session/prompt").await;
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        // A cancelled process is not reusable; the next turn loads the native session.
        let (_cancel, task) = fixture.start("turn-resume", "normal").await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        // OpenCode advertises sessionCapabilities.resume; resuming attaches
        // without replaying history instead of session/load.
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "session/resume")
                .count(),
            1
        );
        assert_eq!(
            fixture
                .requests()
                .iter()
                .filter(|request| request["method"] == "session/load")
                .count(),
            0
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_probe_discovers_models_and_commands_then_closes() {
        let fixture = Fixture::new().await;
        let models = fixture.driver.discover_models(&fixture.root).await.unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "opencode/fixture-model");
        assert!(models[0].is_default);
        assert!(models[0].supported_reasoning_efforts.is_empty());
        // `effort` is per-model in OpenCode; the probe sets each model to read it.
        let effort_model = models
            .iter()
            .find(|model| model.id == "opencode/effort-model")
            .unwrap();
        assert_eq!(
            effort_model.supported_reasoning_efforts,
            vec!["low".to_owned(), "high".to_owned()]
        );
        assert_eq!(
            effort_model.default_reasoning_effort.as_deref(),
            Some("low")
        );
        let commands = fixture
            .driver
            .discover_commands(&fixture.root)
            .await
            .unwrap();
        assert_eq!(commands[0].name, "project:review");
        assert_eq!(commands[0].description, "Project plugin");
        assert!(fixture
            .requests()
            .iter()
            .any(|request| request["method"] == "session/close"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_applies_effort_through_opencode_effort_option() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start_with(
                "turn-effort",
                "normal",
                Some("opencode/effort-model"),
                Some("high"),
                None,
                None,
            )
            .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        let writes = fixture.config_writes();
        assert!(writes
            .iter()
            .any(|request| request["params"]["configId"] == "effort"
                && request["params"]["value"] == "high"));
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "turn.configuration"
                && event.payload["effectiveConfig"]["reasoningEffort"] == "high"));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_auto_mode_answers_permission_requests_client_side() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start_with("turn-auto", "permission", None, None, Some("auto"), None)
            .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        // The fixture finishes the turn only after the client answered request
        // 17; reaching end_turn proves no broker wait occurred.
        let response = fixture
            .requests()
            .into_iter()
            .find(|request| request["id"] == 17)
            .expect("permission response recorded");
        assert_eq!(
            response["result"]["outcome"]["optionId"].as_str(),
            Some("once")
        );
        let events = fixture
            .store
            .complete_history(&fixture.manifest.id)
            .await
            .unwrap();
        assert!(events
            .iter()
            .any(|event| event.event_type == "permission.resolved"
                && event.payload["outcome"] == "allow_once"
                && event.payload["autoApproved"] == true));
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_full_access_prefers_allow_always_option() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture
            .start_with(
                "turn-full",
                "permission",
                None,
                None,
                Some("full-access"),
                None,
            )
            .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        let response = fixture
            .requests()
            .into_iter()
            .find(|request| request["id"] == 17)
            .expect("permission response recorded");
        assert_eq!(
            response["result"]["outcome"]["optionId"].as_str(),
            Some("always")
        );
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_ask_mode_brokers_permission_requests() {
        let fixture = Fixture::new().await;
        let (cancel, task) = fixture
            .start_with("turn-ask", "permission", None, None, Some("ask"), None)
            .await;
        // The broker waits for a user decision; no answer is written for request
        // 17 until cancellation ends the turn.
        fixture.wait_for_method("session/prompt").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(fixture
            .requests()
            .iter()
            .all(|request| request["id"] != 17 || request["method"].is_string()));
        cancel.send(true).unwrap();
        assert!(task.await.unwrap().unwrap().cancelled);
        fixture.finish().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wire_fork_session_creates_distinct_native_session() {
        let fixture = Fixture::new().await;
        let (_cancel, task) = fixture.start("turn-source", "normal").await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .stop_reason,
            "end_turn"
        );
        let context = DriverContext {
            manifest: fixture.manifest.clone(),
            provider_state: fixture
                .store
                .provider_state(&fixture.manifest.id)
                .await
                .unwrap(),
        };
        let permit = fixture
            .trust
            .acquire_owned("local", &fixture.root)
            .await
            .unwrap();
        let state = fixture.driver.fork_session(context, permit).await.unwrap();
        assert_eq!(state.native_session_id.as_deref(), Some("ses_forked"));
        assert!(fixture
            .requests()
            .iter()
            .any(|request| request["method"] == "session/fork"
                && request["params"]["sessionId"] == "ses_fixture"));
        fixture.finish().await;
    }
}
