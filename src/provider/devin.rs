use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::config::AgentConfig;
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::acp::{
    declares_session_fork, run_acp_turn_controlled, select_auth_method, AcpConnectionState,
    AcpRuntimeOptions, FORK_PROBE_TTL, INTERACTIVE_AUTH_TIMEOUT,
};
use super::process::{executable_available, redact_sensitive_text, CommandSpec, JsonLineProcess};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, ImageInputMode,
    PendingProviderControl, ProviderCapabilities, ProviderCommandDescriptor, ProviderControl,
    ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
};

const MAX_DEVIN_SESSIONS: usize = 32;
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_DRAIN: Duration = Duration::from_millis(1500);
/// Model/command discovery spawns an authenticated probe process; cache it so
/// routine client refreshes do not re-authenticate on every query.
const DISCOVERY_TTL: Duration = Duration::from_secs(300);
/// Concurrent probe sessions for the per-model `thought_level` sweep. `devin
/// acp` serves config requests on different sessions in parallel: a 95-model
/// catalog takes ~18s one model at a time and ~4s across eight sessions.
const THOUGHT_LEVEL_PROBE_LANES: usize = 8;
/// Keeps the sweep inside `discovery_timeout`; models it does not reach keep
/// an empty effort list instead of failing the whole catalog.
const THOUGHT_LEVEL_PROBE_BUDGET: Duration = Duration::from_secs(15);
/// Probe sessions are deleted best-effort right before the process exits.
const PROBE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct DiscoverySnapshot {
    fetched_at: Instant,
    models: Vec<ProviderModelDescriptor>,
    commands: Vec<ProviderCommandDescriptor>,
}

pub struct DevinDriver {
    binary: String,
    auth_method: Option<String>,
    api_key_env: Option<String>,
    cli_credentials: bool,
    env_allowlist: Vec<String>,
    sessions: Mutex<HashMap<String, DevinSessionHandle>>,
    discovery: Mutex<HashMap<PathBuf, DiscoverySnapshot>>,
    /// Cached `sessionCapabilities.fork` probe: Devin exposes session forking
    /// over ACP only once the installed CLI implements `session/fork`, so the
    /// advertised capability tracks the installed binary.
    fork_probe: std::sync::Mutex<Option<(bool, Instant)>>,
}

#[derive(Clone)]
struct DevinSessionHandle {
    workspace: PathBuf,
    turns: mpsc::Sender<DevinTurn>,
    controls: mpsc::Sender<PendingProviderControl>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}

struct DevinTurn {
    context: DriverContext,
    prompt: DriverPrompt,
    sink: DriverEventSink,
    cancel: watch::Receiver<bool>,
    launch_permit: WorkspaceTrustPermit,
    respond_to: oneshot::Sender<Result<DriverTurnResult, AppError>>,
}

impl DevinDriver {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            binary: config.devin_bin.clone(),
            auth_method: config.devin_auth_method.clone(),
            api_key_env: config.devin_api_key_env.clone(),
            cli_credentials: cli_credentials_enabled(),
            env_allowlist: config.devin_env_allowlist.clone(),
            sessions: Mutex::new(HashMap::new()),
            discovery: Mutex::new(HashMap::new()),
            fork_probe: std::sync::Mutex::new(None),
        }
    }

    fn command_spec(&self, workspace: &Path) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.binary, workspace);
        spec.args = vec!["acp".to_owned()];
        spec.env = devin_environment(&self.env_allowlist);
        spec
    }

    /// `devin acp` intentionally ignores local CLI credentials; the host must
    /// authenticate every ACP process. An API key goes through `_meta.api_key`
    /// (headless); without one the advertised browser method runs a PKCE flow.
    /// The key resolves from the configured environment variable first, then
    /// from the `windsurf_api_key` written by `devin auth login` when
    /// `devin_cli_credentials` is enabled.
    fn api_key(&self) -> Result<Option<String>, AppError> {
        if let Some(name) = self
            .api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(Some)
                .ok_or_else(|| {
                    AppError::ProviderUnavailable(format!(
                        "Devin API key environment variable '{name}' is not set"
                    ))
                });
        }
        if self.cli_credentials {
            return Ok(cli_credentials_path().and_then(|path| cli_credentials_key(&path)));
        }
        Ok(None)
    }

    fn auth_params(&self, initialize: &Value) -> Result<Option<Value>, AppError> {
        let Some(method) =
            select_auth_method(initialize, self.auth_method.as_deref(), ProviderKind::Devin)?
        else {
            return Ok(None);
        };
        let mut params = json!({ "methodId": method });
        if let Some(key) = self.api_key()? {
            params["_meta"] = json!({ "api_key": key });
        }
        Ok(Some(params))
    }

    /// Without an API key the advertised `devin-browser` method opens a PKCE
    /// page the user must approve, which outlasts the diagnostic timeout.
    fn auth_timeout(&self) -> Result<Option<Duration>, AppError> {
        Ok((self.api_key()?.is_none()).then_some(INTERACTIVE_AUTH_TIMEOUT))
    }

    fn runtime_options(&self) -> Result<AcpRuntimeOptions, AppError> {
        Ok(AcpRuntimeOptions {
            authenticate: true,
            auth_method: self.auth_method.clone(),
            auth_meta: self.api_key()?.map(|key| json!({ "api_key": key })),
            auth_timeout: self.auth_timeout()?,
            suppress_load_replay: true,
            ..Default::default()
        })
    }

    async fn authenticate(
        &self,
        process: &mut JsonLineProcess,
        initialize: &Value,
    ) -> Result<(), AppError> {
        if let Some(params) = self.auth_params(initialize)? {
            control_request(
                process,
                "authenticate",
                "authenticate",
                params,
                self.auth_timeout()?.unwrap_or(DIAGNOSTIC_TIMEOUT),
            )
            .await?;
        }
        Ok(())
    }

    /// Whether the installed `devin acp` declares `sessionCapabilities.fork`.
    /// Initialize needs no authentication, so the probe stays headless.
    async fn probe_fork_capability(&self) -> bool {
        let run = async {
            let mut process =
                JsonLineProcess::spawn(&self.command_spec(&std::env::temp_dir())).await?;
            let result = async {
                let initialize = initialize_process(&mut process).await?;
                Ok::<_, AppError>(declares_session_fork(&initialize))
            }
            .await;
            process.terminate().await;
            result
        };
        match tokio::time::timeout(DIAGNOSTIC_TIMEOUT, run).await {
            Ok(Ok(capable)) => capable,
            Ok(Err(error)) => {
                tracing::debug!(error = %error, "Devin fork capability probe failed");
                false
            }
            Err(_) => {
                tracing::debug!("Devin fork capability probe timed out");
                false
            }
        }
    }

    /// Models and slash commands live behind `session/new`, which requires an
    /// authenticated session. The session is left open so callers can issue
    /// follow-up probes (per-model `thought_level` options); callers must
    /// delete it best-effort via `DevinProbe::delete_sessions`.
    async fn session_probe(
        &self,
        workspace: &Path,
    ) -> Result<(JsonLineProcess, Value, Vec<Value>), AppError> {
        let mut process = JsonLineProcess::spawn(&self.command_spec(workspace)).await?;
        let result = async {
            let initialize = initialize_process(&mut process).await?;
            self.authenticate(&mut process, &initialize).await?;
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
                    AppError::InvalidRequest("invalid Devin session/new response".to_owned())
                })?;
            // Command and config announcements trail the session/new response.
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

    /// One authenticated probe yields both the model catalog and the command
    /// catalog; reuse it briefly so independent client queries share a single
    /// spawn (and a single interactive authentication when no key is set).
    async fn discovery_snapshot(&self, workspace: &Path) -> Result<DiscoverySnapshot, AppError> {
        let mut cache = self.discovery.lock().await;
        if let Some(snapshot) = cache.get(workspace) {
            if snapshot.fetched_at.elapsed() < DISCOVERY_TTL {
                return Ok(snapshot.clone());
            }
        }
        let (mut process, session, updates) = self.session_probe(workspace).await?;
        let mut models = parse_devin_models(&session);
        let mut probe = DevinProbe::new(&mut process, &session);
        let complete = probe_thought_levels(
            &mut probe,
            workspace,
            &session,
            &mut models,
            tokio::time::Instant::now() + THOUGHT_LEVEL_PROBE_BUDGET,
        )
        .await;
        probe.delete_sessions().await;
        process.terminate().await;
        let snapshot = DiscoverySnapshot {
            fetched_at: Instant::now(),
            models,
            commands: parse_devin_commands(&updates),
        };
        // A sweep cut short by its budget is served but not cached, so the next
        // query fills in the missing thinking levels.
        if complete {
            cache.insert(workspace.to_path_buf(), snapshot.clone());
        }
        Ok(snapshot)
    }
}

#[async_trait]
impl ProviderDriver for DevinDriver {
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
            .ok_or_else(|| AppError::InvalidRequest("Devin session is not active".to_owned()))?;
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
                    "Devin control channel is unavailable or full".to_owned(),
                )
            })?;
        tokio::time::timeout(CONTROL_TIMEOUT, response)
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable("Devin control acknowledgement timed out".to_owned())
            })?
            .map_err(|_| {
                AppError::ProviderUnavailable(
                    "Devin turn ended before control acknowledgement".to_owned(),
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
            json!({"provider":"devin","conversationId":conversation_id,"status":"stopped","reason":"user_closed"}),
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

    async fn refresh_control_capabilities(&self) {
        let stale = self
            .fork_probe
            .lock()
            .map(|entry| entry.is_none_or(|(_, at)| at.elapsed() >= FORK_PROBE_TTL))
            .unwrap_or(true);
        if !stale || !executable_available(&self.binary) {
            return;
        }
        let capable = self.probe_fork_capability().await;
        if let Ok(mut entry) = self.fork_probe.lock() {
            *entry = Some((capable, Instant::now()));
        }
    }

    fn supports_native_fork(&self) -> bool {
        self.fork_probe
            .lock()
            .map(|entry| entry.is_some_and(|(capable, _)| capable))
            .unwrap_or(false)
    }

    async fn fork_session(
        &self,
        context: DriverContext,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<crate::conversation::ProviderState, AppError> {
        let source = context
            .provider_state
            .native_session_id
            .as_deref()
            .ok_or_else(|| {
                AppError::Unsupported("conversation has no native Devin session to fork".to_owned())
            })?
            .to_owned();
        let mut process = JsonLineProcess::spawn_trusted(
            &self.command_spec(&context.manifest.workspace),
            launch_permit,
        )
        .await?;
        let result = async {
            let initialize = initialize_process(&mut process).await?;
            let capable = declares_session_fork(&initialize);
            if let Ok(mut entry) = self.fork_probe.lock() {
                *entry = Some((capable, Instant::now()));
            }
            if !capable {
                return Err(AppError::Unsupported(
                    "Devin does not declare sessionCapabilities.fork".to_owned(),
                ));
            }
            // `session/fork` may require an authenticated agent; only the
            // headless api-key path runs here — the interactive browser flow
            // cannot prompt the user mid-fork.
            if self.api_key()?.is_some() {
                self.authenticate(&mut process, &initialize).await?;
            }
            let response = control_request(
                &mut process,
                "fork",
                "session/fork",
                json!({ "sessionId": source, "cwd": context.manifest.workspace }),
                DIAGNOSTIC_TIMEOUT,
            )
            .await?;
            let forked = response
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && *id != source)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "invalid Devin fork response: missing distinct sessionId".to_owned(),
                    )
                })?;
            let mut state = crate::conversation::ProviderState::new(ProviderKind::Devin);
            state.native_session_id = Some(forked.to_owned());
            state.recoverable = initialize
                .pointer("/agentCapabilities/loadSession")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(state)
        }
        .await;
        process.terminate().await;
        result
    }

    fn descriptor(&self) -> ProviderDescriptor {
        let available = executable_available(&self.binary);
        ProviderDescriptor {
            id: ProviderKind::Devin,
            display_name: "Devin",
            available,
            unavailable_reason: (!available).then(|| {
                format!(
                    "executable '{}' was not found; install Devin CLI from https://devin.ai",
                    self.binary
                )
            }),
            profiles: Vec::new(),
            capabilities: ProviderCapabilities {
                permission_config: super::types::permission_config_capabilities(
                    ProviderKind::Devin,
                ),
                native_fork: self.supports_native_fork(),
                native_compact: false,
                native_resume: true,
                cancel: true,
                permissions: true,
                tool_events: true,
                native_skills: true,
                native_mcp: true,
                managed_mcp: false,
                model_selection: true,
                image_input: ProviderKind::Devin.supports_image_input(),
                image_input_mode: ImageInputMode::Always,
            },
            models: Vec::new(),
        }
    }

    /// A cold probe authenticates a session, drains the command drain window,
    /// and sweeps every catalog model for its `thought_level` options within
    /// `THOUGHT_LEVEL_PROBE_BUDGET` (~4s for the 95-model catalog).
    fn discovery_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn discover_models(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        Ok(self.discovery_snapshot(workspace).await?.models)
    }

    async fn discover_commands(
        &self,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        Ok(self.discovery_snapshot(workspace).await?.commands)
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
                        "Devin resident session workspace changed".to_owned(),
                    ));
                }
                handle.clone()
            } else {
                if sessions.len() >= MAX_DEVIN_SESSIONS {
                    return Err(AppError::ProviderUnavailable(
                        "Devin resident session limit reached (32); close an idle session"
                            .to_owned(),
                    ));
                }
                let (turns, turn_rx) = mpsc::channel(1);
                let (controls, control_rx) = mpsc::channel(16);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let (stopped_tx, stopped) = watch::channel(false);
                let handle = DevinSessionHandle {
                    workspace: context.manifest.workspace.clone(),
                    turns,
                    controls,
                    shutdown,
                    stopped,
                };
                let spec = self.command_spec(&context.manifest.workspace);
                let runtime = self.runtime_options()?;
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
            .send(DevinTurn {
                context,
                prompt,
                sink,
                cancel,
                launch_permit,
                respond_to,
            })
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable("Devin session closed before turn started".to_owned())
            })?;
        response.await.map_err(|_| {
            AppError::ProviderUnavailable("Devin session stopped before turn completed".to_owned())
        })?
    }
}

async fn stop_session(mut handle: DevinSessionHandle) {
    let _ = handle.shutdown.send(true);
    if !*handle.stopped.borrow() {
        let _ = tokio::time::timeout(Duration::from_secs(6), handle.stopped.changed()).await;
    }
}

async fn run_session_actor(
    spec: CommandSpec,
    runtime: AcpRuntimeOptions,
    mut turns: mpsc::Receiver<DevinTurn>,
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
                if let Some(control) = control { let _ = control.respond_to.send(Err(AppError::InvalidRequest("Devin has no active turn".to_owned()))); }
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
                                if super::acp::handle_acp_message(process, message, sink, &mut cancel, ProviderKind::Devin, true, super::acp::AutoApprove::Mediate, &mut connection).await.is_err() { break; }
                            }
                        }
                    }
                    _ => break,
                }
                continue;
            }
        };
        let DevinTurn {
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
                "Devin turn already ended".to_owned(),
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
            "Devin negotiated an unsupported ACP protocol version".to_owned(),
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
                return Err(AppError::ProviderUnavailable("Devin closed stdout during control request".to_owned()));
            };
            if message.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(AppError::ProviderUnavailable(format!("Devin {method} failed: {}", safe_message(error))));
                }
                return message.get("result").cloned().ok_or_else(|| AppError::InvalidRequest("Devin control response has no result".to_owned()));
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                updates.push(message.get("params").cloned().unwrap_or(Value::Null));
            }
            if let Some(request_id) = message.get("id").filter(|_| message.get("method").is_some()) {
                process.send(&json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"client capability is not supported during control request"}})).await?;
            }
        }
    }).await.map_err(|_| AppError::ProviderUnavailable(format!("Devin {method} timed out")))?
}

fn devin_environment(allowlist: &[String]) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([("NO_COLOR".to_owned(), "1".to_owned())]);
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

/// Bookkeeping for one discovery `devin acp` process. Every `session/new`
/// answer is recorded whenever it arrives, even after the batch that sent it
/// gave up, so each session Devin creates is deleted before the process exits.
struct DevinProbe<'a> {
    process: &'a mut JsonLineProcess,
    sessions: Vec<String>,
    /// Request ids of `session/new` calls still awaiting an answer.
    opening: HashSet<String>,
}

impl<'a> DevinProbe<'a> {
    fn new(process: &'a mut JsonLineProcess, session: &Value) -> Self {
        Self {
            process,
            sessions: session
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .into_iter()
                .collect(),
            opening: HashSet::new(),
        }
    }

    async fn send(&mut self, id: String, method: &str, params: Value) -> Result<(), AppError> {
        self.process
            .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        if method == "session/new" {
            self.opening.insert(id);
        }
        Ok(())
    }

    /// Returns the next response before `deadline`, declining agent requests
    /// and recording opened sessions on the way. `None` means the deadline
    /// passed or the process stopped responding.
    async fn next_response(&mut self, deadline: tokio::time::Instant) -> Option<Value> {
        loop {
            let Ok(Ok(Some(message))) =
                tokio::time::timeout_at(deadline, self.process.read()).await
            else {
                return None;
            };
            if message.get("method").is_some() {
                if let Some(request_id) = message.get("id") {
                    self.process
                        .send(&json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"client capability is not supported during probe"}}))
                        .await
                        .ok()?;
                }
                continue;
            }
            if let Some(id) = message.get("id").and_then(Value::as_str) {
                if self.opening.remove(id) {
                    if let Some(session) = message
                        .pointer("/result/sessionId")
                        .and_then(Value::as_str)
                        .filter(|session| !session.is_empty())
                    {
                        self.sessions.push(session.to_owned());
                    }
                }
            }
            return Some(message);
        }
    }

    /// Issues `count` `method` requests over `lanes` concurrent slots, keeping
    /// at most one request in flight per lane so per-session state (the
    /// selected model) is never raced. Returns each response message by
    /// request index; `None` marks a request unanswered before `deadline`.
    async fn pipelined(
        &mut self,
        method: &str,
        lanes: usize,
        count: usize,
        deadline: tokio::time::Instant,
        mut params: impl FnMut(usize, usize) -> Value,
    ) -> Vec<Option<Value>> {
        let mut responses = vec![None; count];
        let mut in_flight = HashMap::new();
        let mut next = 0;
        while next < lanes.min(count) {
            let id = format!("probe:{method}:{next}");
            if self
                .send(id.clone(), method, params(next, next))
                .await
                .is_err()
            {
                return responses;
            }
            in_flight.insert(id, (next, next));
            next += 1;
        }
        while !in_flight.is_empty() {
            let Some(message) = self.next_response(deadline).await else {
                break;
            };
            let Some((lane, index)) = message
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| in_flight.remove(id))
            else {
                continue;
            };
            responses[index] = Some(message);
            if next < count {
                let id = format!("probe:{method}:{next}");
                if self
                    .send(id.clone(), method, params(lane, next))
                    .await
                    .is_err()
                {
                    break;
                }
                in_flight.insert(id, (lane, next));
                next += 1;
            }
        }
        responses
    }

    /// Best-effort cleanup: waits for outstanding `session/new` answers, then
    /// deletes every probe session. Failures are ignored because the process
    /// is terminated immediately afterwards either way.
    async fn delete_sessions(&mut self) {
        let deadline = tokio::time::Instant::now() + PROBE_CLEANUP_TIMEOUT;
        while !self.opening.is_empty() && self.next_response(deadline).await.is_some() {}
        let sessions = std::mem::take(&mut self.sessions);
        self.pipelined(
            "session/delete",
            sessions.len(),
            sessions.len(),
            deadline,
            |_, index| json!({ "sessionId": sessions[index] }),
        )
        .await;
    }
}

/// Devin exposes `thought_level` only for the currently selected model, and
/// each model advertises a different level set, so every catalog model is
/// selected once to learn its thinking levels. The sweep fans out over extra
/// probe sessions and stops at `deadline`. Returns whether every model
/// answered; older `devin acp` builds expose no such option at all, so the
/// sweep is skipped then.
async fn probe_thought_levels(
    probe: &mut DevinProbe<'_>,
    workspace: &Path,
    session: &Value,
    models: &mut [ProviderModelDescriptor],
    deadline: tokio::time::Instant,
) -> bool {
    let exposes_thought_level = session
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|option| option.get("id").and_then(Value::as_str) == Some("thought_level"));
    if !exposes_thought_level {
        return true;
    }
    let extra = THOUGHT_LEVEL_PROBE_LANES
        .min(models.len())
        .saturating_sub(probe.sessions.len());
    probe
        .pipelined(
            "session/new",
            extra,
            extra,
            deadline,
            |_, _| json!({ "cwd": workspace, "mcpServers": [] }),
        )
        .await;
    let lanes = probe.sessions.clone();
    if lanes.is_empty() {
        return false;
    }
    let responses = probe
        .pipelined(
            "session/set_config_option",
            lanes.len(),
            models.len(),
            deadline,
            |lane, index| {
                json!({"sessionId": lanes[lane], "configId": "model", "value": models[index].id})
            },
        )
        .await;
    let mut complete = true;
    for (model, response) in models.iter_mut().zip(responses) {
        let Some(response) = response else {
            complete = false;
            continue;
        };
        let Some((efforts, default)) = response.get("result").and_then(thought_level_option) else {
            continue;
        };
        model.supported_reasoning_efforts = efforts;
        model.default_reasoning_effort = default;
    }
    complete
}

fn thought_level_option(response: &Value) -> Option<(Vec<String>, Option<String>)> {
    let option = response
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("id").and_then(Value::as_str) == Some("thought_level"))?;
    let efforts = option
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
    let default = option
        .get("currentValue")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((efforts, default))
}

pub(super) fn parse_devin_models(session: &Value) -> Vec<ProviderModelDescriptor> {
    let option = session
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("id").and_then(Value::as_str) == Some("model"));
    let Some(option) = option else {
        return Vec::new();
    };
    let current = option.get("currentValue").and_then(Value::as_str);
    option
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model.get("value").and_then(Value::as_str)?.to_owned();
            Some(ProviderModelDescriptor {
                display_name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_owned(),
                description: model
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                is_default: current == Some(id.as_str()),
                default_reasoning_effort: None,
                context_window: None,
                supported_reasoning_efforts: Vec::new(),
                image_input: model
                    .get("_meta")
                    .and_then(|meta| meta.get("cognition.ai/supportsImages"))
                    .and_then(Value::as_bool),
                id,
            })
        })
        .collect()
}

/// `devin acp` ignores `devin auth login` credentials by design; TodeX opts
/// back in so an existing CLI login authenticates ACP sessions headlessly.
/// Set `TODEX_AGENTD_DEVIN_CLI_CREDENTIALS=false` to require explicit key or
/// browser authentication instead.
fn cli_credentials_enabled() -> bool {
    match std::env::var("TODEX_AGENTD_DEVIN_CLI_CREDENTIALS") {
        Ok(value) => !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// The fixed credentials store written by `devin auth login`
/// (`~/.local/share/devin/credentials.toml`).
fn cli_credentials_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/devin/credentials.toml"))
}

fn cli_credentials_key(path: &Path) -> Option<String> {
    let document = std::fs::read_to_string(path)
        .ok()?
        .parse::<toml::Value>()
        .ok()?;
    document
        .get("windsurf_api_key")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_devin_commands(updates: &[Value]) -> Vec<ProviderCommandDescriptor> {
    updates
        .iter()
        .rev()
        .find_map(|update| {
            update
                .pointer("/update/availableCommands")
                .filter(|commands| commands.is_array())
                .cloned()
        })
        .map(|commands| {
            super::grok::parse_commands(&json!({ "_meta": { "availableCommands": commands } }))
        })
        .unwrap_or_default()
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

    #[tokio::test]
    #[ignore = "requires the real devin CLI and credentials; run manually"]
    async fn real_devin_discovery_lists_models() {
        let config = crate::config::Config::default();
        let driver = DevinDriver::new(&config.agent);
        let workspace = PathBuf::from(
            std::env::var("TODEX_DEVIN_TEST_WORKSPACE")
                .unwrap_or_else(|_| "/Users/youtonghy/github/Project".to_owned()),
        );
        let models = driver.discover_models(&workspace).await.unwrap();
        assert!(!models.is_empty());
        let thinking = models
            .iter()
            .filter(|model| !model.supported_reasoning_efforts.is_empty())
            .count();
        assert!(thinking > 0);
        eprintln!(
            "discovered {} models ({thinking} with thinking levels)",
            models.len()
        );
    }

    #[test]
    fn parses_model_config_option_into_descriptors() {
        let session = json!({
            "sessionId": "devin-native",
            "configOptions": [
                {
                    "id": "mode", "category": "mode", "name": "Mode", "type": "select",
                    "currentValue": "accept-edits",
                    "options": [
                        {"value": "accept-edits", "name": "Accept Edits"},
                        {"value": "bypass", "name": "Bypass Permissions"}
                    ]
                },
                {
                    "id": "model", "category": "model", "name": "Model", "type": "select",
                    "currentValue": "swe-2-high",
                    "options": [
                        {"value": "swe-2-high", "name": "SWE 2 High", "description": "Default"},
                        {"value": "claude-opus-5-low", "name": "Opus 5 Low", "_meta": {"cognition.ai/supportsImages": true}},
                        {"value": "claude-opus-5-fast", "name": "Opus 5 Fast", "_meta": {"cognition.ai/supportsImages": false}}
                    ]
                }
            ]
        });
        let models = parse_devin_models(&session);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "swe-2-high");
        assert!(models[0].is_default);
        assert_eq!(models[1].image_input, Some(true));
        assert_eq!(models[2].image_input, Some(false));
        assert!(!models.iter().any(|model| model.id == "accept-edits"));
    }

    #[test]
    fn thought_level_option_reads_levels_and_default() {
        let response = json!({
            "configOptions": [
                {"id": "model", "currentValue": "swe-2-high"},
                {
                    "id": "thought_level",
                    "currentValue": "high",
                    "options": [
                        {"value": "medium"},
                        {"value": "high"},
                        {"value": "max"}
                    ]
                }
            ]
        });
        let (efforts, default) = thought_level_option(&response).unwrap();
        assert_eq!(efforts, vec!["medium", "high", "max"]);
        assert_eq!(default.as_deref(), Some("high"));

        assert!(thought_level_option(&json!({"configOptions": []})).is_none());
        assert!(thought_level_option(&json!({})).is_none());
    }

    #[test]
    fn missing_model_option_yields_empty_catalog() {
        assert!(parse_devin_models(&json!({"sessionId":"s"})).is_empty());
        assert!(parse_devin_models(&json!({"configOptions":[]})).is_empty());
    }

    #[test]
    fn devin_environment_filters_names_and_daemon_env() {
        unsafe {
            std::env::set_var("DEVIN_TEST_ALLOWED_KEY", "present");
            std::env::set_var("TODEX_AGENTD_SECRET_TEST", "daemon-only");
        }
        let env = devin_environment(&[
            "DEVIN_TEST_ALLOWED_KEY".to_owned(),
            "TODEX_AGENTD_SECRET_TEST".to_owned(),
            "NOT_A_VALID NAME".to_owned(),
            "DEVIN_TEST_MISSING_KEY".to_owned(),
        ]);
        assert_eq!(
            env.get("DEVIN_TEST_ALLOWED_KEY"),
            Some(&"present".to_owned())
        );
        assert!(!env.contains_key("TODEX_AGENTD_SECRET_TEST"));
        assert!(!env.contains_key("NOT_A_VALID NAME"));
        assert!(!env.contains_key("DEVIN_TEST_MISSING_KEY"));
        assert_eq!(env.get("NO_COLOR"), Some(&"1".to_owned()));
    }

    #[test]
    fn api_key_requires_configured_env_name() {
        let driver = DevinDriver {
            binary: "devin".to_owned(),
            auth_method: None,
            api_key_env: None,
            cli_credentials: false,
            env_allowlist: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
            discovery: Mutex::new(HashMap::new()),
            fork_probe: std::sync::Mutex::new(None),
        };
        assert_eq!(driver.api_key().unwrap(), None);

        let driver = DevinDriver {
            api_key_env: Some("DEVIN_TEST_CONFIGURED_KEY".to_owned()),
            ..DevinDriver {
                binary: "devin".to_owned(),
                auth_method: None,
                api_key_env: None,
                cli_credentials: false,
                env_allowlist: Vec::new(),
                sessions: Mutex::new(HashMap::new()),
                discovery: Mutex::new(HashMap::new()),
                fork_probe: std::sync::Mutex::new(None),
            }
        };
        assert!(driver.api_key().is_err());
        unsafe {
            std::env::set_var("DEVIN_TEST_CONFIGURED_KEY", "key-value");
        }
        assert_eq!(driver.api_key().unwrap().as_deref(), Some("key-value"));
        unsafe {
            std::env::remove_var("DEVIN_TEST_CONFIGURED_KEY");
        }
    }

    #[test]
    fn interactive_auth_gets_extended_timeout() {
        let driver = DevinDriver {
            binary: "devin".to_owned(),
            auth_method: None,
            api_key_env: None,
            cli_credentials: false,
            env_allowlist: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
            discovery: Mutex::new(HashMap::new()),
            fork_probe: std::sync::Mutex::new(None),
        };
        assert_eq!(
            driver.auth_timeout().unwrap(),
            Some(INTERACTIVE_AUTH_TIMEOUT)
        );
        assert_eq!(
            driver.runtime_options().unwrap().auth_timeout,
            Some(INTERACTIVE_AUTH_TIMEOUT)
        );

        unsafe {
            std::env::set_var("DEVIN_TEST_TIMEOUT_KEY", "key-value");
        }
        let keyed = DevinDriver {
            api_key_env: Some("DEVIN_TEST_TIMEOUT_KEY".to_owned()),
            ..driver
        };
        assert_eq!(keyed.auth_timeout().unwrap(), None);
        unsafe {
            std::env::remove_var("DEVIN_TEST_TIMEOUT_KEY");
        }
    }

    #[tokio::test]
    async fn discovery_cache_serves_snapshot_without_respawning() {
        let driver = DevinDriver {
            binary: "definitely-missing-devin-binary".to_owned(),
            auth_method: None,
            api_key_env: None,
            cli_credentials: false,
            env_allowlist: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
            discovery: Mutex::new(HashMap::new()),
            fork_probe: std::sync::Mutex::new(None),
        };
        let workspace = PathBuf::from("/tmp/todex-devin-discovery-test");
        let snapshot = DiscoverySnapshot {
            fetched_at: Instant::now(),
            models: vec![ProviderModelDescriptor {
                id: "swe-2-high".to_owned(),
                display_name: "SWE 2 High".to_owned(),
                description: String::new(),
                is_default: true,
                supported_reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                context_window: None,
                image_input: None,
            }],
            commands: Vec::new(),
        };
        driver
            .discovery
            .lock()
            .await
            .insert(workspace.clone(), snapshot);
        let models = driver.discover_models(&workspace).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "swe-2-high");
    }

    #[test]
    fn cli_credentials_key_reads_windsurf_api_key() {
        let path =
            std::env::temp_dir().join(format!("todex-devin-creds-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "windsurf_api_key = \"test-key-123\"\napi_server_url = \"https://example\"\n",
        )
        .unwrap();
        assert_eq!(cli_credentials_key(&path).as_deref(), Some("test-key-123"));
        std::fs::write(&path, "windsurf_api_key = \"\"\n").unwrap();
        assert_eq!(cli_credentials_key(&path), None);
        std::fs::write(&path, "not = [valid").unwrap();
        assert_eq!(cli_credentials_key(&path), None);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            cli_credentials_key(Path::new("/missing/credentials.toml")),
            None
        );
    }

    /// The fixture is a Python script named `acp` in the workspace, and the
    /// driver's binary is the interpreter: the driver's `<binary> acp` spawn
    /// then runs it on every platform without a shebang or wrapper.
    fn fixture_driver(markers: &[&str]) -> (DevinDriver, PathBuf) {
        let python = ["python3", "python"]
            .into_iter()
            .find(|name| {
                std::process::Command::new(name)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
            })
            .expect("python3 or python is required for the Devin ACP fixture");
        let root = std::env::temp_dir().join(format!("todex-devin-wire-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(
            root.join("acp"),
            include_str!("../../tests/fixtures/devin_acp_fixture.py"),
        )
        .unwrap();
        for marker in markers {
            std::fs::write(root.join(marker), "").unwrap();
        }
        let driver = DevinDriver {
            binary: python.to_owned(),
            auth_method: None,
            api_key_env: None,
            cli_credentials: false,
            env_allowlist: Vec::new(),
            sessions: Mutex::new(HashMap::new()),
            discovery: Mutex::new(HashMap::new()),
            fork_probe: std::sync::Mutex::new(None),
        };
        (driver, root)
    }

    fn fixture_calls(root: &Path, method: &str) -> Vec<Value> {
        std::fs::read_to_string(root.join("journal.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry| entry["method"] == method)
            .map(|entry| entry["params"].clone())
            .collect()
    }

    fn session_ids(calls: &[Value]) -> std::collections::BTreeSet<String> {
        calls
            .iter()
            .map(|params| params["sessionId"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn discovery_sweeps_thought_levels_across_parallel_sessions() {
        let (driver, root) = fixture_driver(&[]);
        let models = driver.discover_models(&root).await.unwrap();
        assert_eq!(models.len(), 12);
        for (index, model) in models.iter().enumerate() {
            if index % 2 == 0 {
                assert_eq!(model.supported_reasoning_efforts, ["medium", "high", "max"]);
                assert_eq!(model.default_reasoning_effort.as_deref(), Some("high"));
            } else {
                assert!(model.supported_reasoning_efforts.is_empty(), "{}", model.id);
            }
        }
        assert!(driver.discovery.lock().await.contains_key(&root));

        assert_eq!(
            fixture_calls(&root, "session/new").len(),
            THOUGHT_LEVEL_PROBE_LANES
        );
        let sweep = fixture_calls(&root, "session/set_config_option");
        assert_eq!(sweep.len(), 12);
        assert_eq!(session_ids(&sweep).len(), THOUGHT_LEVEL_PROBE_LANES);
        let deleted = fixture_calls(&root, "session/delete");
        assert_eq!(session_ids(&deleted).len(), THOUGHT_LEVEL_PROBE_LANES);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn thought_level_sweep_returns_partial_levels_at_deadline() {
        let (driver, root) = fixture_driver(&["stall"]);
        let (mut process, session, _) = driver.session_probe(&root).await.unwrap();
        let mut models = parse_devin_models(&session);
        let mut probe = DevinProbe::new(&mut process, &session);
        let complete = probe_thought_levels(
            &mut probe,
            &root,
            &session,
            &mut models,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await;
        probe.delete_sessions().await;
        process.terminate().await;

        assert!(!complete);
        assert_eq!(models.len(), 13);
        let stalled = models
            .iter()
            .find(|model| model.id == "stall-model")
            .unwrap();
        assert!(stalled.supported_reasoning_efforts.is_empty());
        assert_eq!(
            models[0].supported_reasoning_efforts,
            ["medium", "high", "max"]
        );
        assert_eq!(
            session_ids(&fixture_calls(&root, "session/delete")).len(),
            THOUGHT_LEVEL_PROBE_LANES
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn sessions_opened_after_the_sweep_deadline_are_still_deleted() {
        let (driver, root) = fixture_driver(&["slow-open"]);
        let (mut process, session, _) = driver.session_probe(&root).await.unwrap();
        let mut models = parse_devin_models(&session);
        let mut probe = DevinProbe::new(&mut process, &session);
        let complete = probe_thought_levels(
            &mut probe,
            &root,
            &session,
            &mut models,
            tokio::time::Instant::now() + Duration::from_millis(300),
        )
        .await;
        probe.delete_sessions().await;
        process.terminate().await;

        assert!(!complete);
        assert_eq!(
            fixture_calls(&root, "session/new").len(),
            THOUGHT_LEVEL_PROBE_LANES
        );
        let deleted = session_ids(&fixture_calls(&root, "session/delete"));
        let expected = (0..THOUGHT_LEVEL_PROBE_LANES)
            .map(|index| format!("session-{index}"))
            .collect();
        assert_eq!(deleted, expected);
        let _ = std::fs::remove_dir_all(&root);
    }
}
