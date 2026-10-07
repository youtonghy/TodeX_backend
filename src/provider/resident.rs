//! One resident ACP agent process per conversation, kept between turns
//! (Grok Build, Devin, OpenCode).
//!
//! Each conversation gets an actor owning the process: it runs turns one at
//! a time, forwards live controls into the running turn, keeps serving
//! notifications between turns and stops after the profile's idle timeout.
//! The provider supplies how to launch it; everything else is shared.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::acp::{run_acp_turn_controlled, AcpConnectionState, AcpRuntimeOptions};
use super::process::{CommandSpec, JsonLineProcess};
use super::profile::ProviderProfile;
use super::rpc::{RpcClient, RpcPeer};
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult, PendingProviderControl,
    ProviderControl,
};

/// How long a live control waits for the running turn to acknowledge it.
const CONTROL_ACK_TIMEOUT: Duration = Duration::from_secs(20);
/// How long stopping a session waits for its process to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(6);

/// How to start a conversation's process: command and ACP options.
pub(super) struct ResidentLaunch {
    pub spec: CommandSpec,
    pub runtime: AcpRuntimeOptions,
}

#[derive(Clone)]
struct SessionHandle {
    workspace: PathBuf,
    turns: mpsc::Sender<ResidentTurn>,
    controls: mpsc::Sender<PendingProviderControl>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}

struct ResidentTurn {
    context: DriverContext,
    prompt: DriverPrompt,
    sink: DriverEventSink,
    cancel: watch::Receiver<bool>,
    launch_permit: WorkspaceTrustPermit,
    respond_to: oneshot::Sender<Result<DriverTurnResult, AppError>>,
}

/// The resident sessions of one provider.
pub(super) struct ResidentSessions {
    provider: ProviderKind,
    /// The provider as named in errors ("Grok", "Devin", "OpenCode").
    label: &'static str,
    idle_timeout: Duration,
    max_sessions: usize,
    sessions: Mutex<HashMap<String, SessionHandle>>,
}

impl ResidentSessions {
    /// Sessions sized by `profile`'s resident process model.
    pub(super) fn new(profile: &ProviderProfile, label: &'static str) -> Self {
        Self {
            provider: profile.kind,
            label,
            idle_timeout: profile
                .process_model
                .idle_timeout()
                .expect("resident providers declare an idle timeout"),
            max_sessions: profile
                .process_model
                .max_sessions()
                .expect("resident providers declare a session limit"),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Runs a turn on the conversation's session, starting it with `launch`
    /// when none is running.
    pub(super) async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
        launch: impl FnOnce(&DriverContext, &DriverPrompt) -> Result<ResidentLaunch, AppError>,
    ) -> Result<DriverTurnResult, AppError> {
        let label = self.label;
        let conversation_id = context.manifest.id.clone();
        let handle = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, handle| !handle.turns.is_closed());
            if let Some(handle) = sessions.get(&conversation_id) {
                if handle.workspace != context.manifest.workspace {
                    return Err(AppError::InvalidRequest(format!(
                        "{label} resident session workspace changed"
                    )));
                }
                handle.clone()
            } else {
                if sessions.len() >= self.max_sessions {
                    return Err(AppError::ProviderUnavailable(format!(
                        "{label} resident session limit reached ({}); close an idle session",
                        self.max_sessions
                    )));
                }
                let ResidentLaunch { spec, runtime } = launch(&context, &prompt)?;
                let (turns, turn_rx) = mpsc::channel(1);
                let (controls, control_rx) = mpsc::channel(16);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let (stopped_tx, stopped) = watch::channel(false);
                let handle = SessionHandle {
                    workspace: context.manifest.workspace.clone(),
                    turns,
                    controls,
                    shutdown,
                    stopped,
                };
                tokio::spawn(
                    SessionActor {
                        provider: self.provider,
                        label,
                        idle_timeout: self.idle_timeout,
                        spec,
                        runtime,
                    }
                    .run(turn_rx, control_rx, shutdown_rx, stopped_tx),
                );
                sessions.insert(conversation_id, handle.clone());
                handle
            }
        };
        let (respond_to, response) = oneshot::channel();
        handle
            .turns
            .send(ResidentTurn {
                context,
                prompt,
                sink,
                cancel,
                launch_permit,
                respond_to,
            })
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable(format!("{label} session closed before turn started"))
            })?;
        response.await.map_err(|_| {
            AppError::ProviderUnavailable(format!("{label} session stopped before turn completed"))
        })?
    }

    /// Forwards a live control into the conversation's running turn.
    pub(super) async fn control(
        &self,
        conversation_id: &str,
        expected_turn_id: &str,
        request_id: &str,
        control: ProviderControl,
    ) -> Result<Value, AppError> {
        let label = self.label;
        let handle = self
            .sessions
            .lock()
            .await
            .get(conversation_id)
            .cloned()
            .ok_or_else(|| AppError::InvalidRequest(format!("{label} session is not active")))?;
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
                AppError::ProviderUnavailable(format!(
                    "{label} control channel is unavailable or full"
                ))
            })?;
        tokio::time::timeout(CONTROL_ACK_TIMEOUT, response)
            .await
            .map_err(|_| {
                AppError::ProviderUnavailable(format!("{label} control acknowledgement timed out"))
            })?
            .map_err(|_| {
                AppError::ProviderUnavailable(format!(
                    "{label} turn ended before control acknowledgement"
                ))
            })?
    }

    /// Stops the conversation's session, if any, and waits for it to exit.
    pub(super) async fn stop(&self, conversation_id: &str) {
        let handle = self.sessions.lock().await.remove(conversation_id);
        if let Some(handle) = handle {
            stop_session(handle).await;
        }
    }

    /// Stops every session: all are signalled before any is awaited.
    pub(super) async fn stop_all(&self) {
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
}

async fn stop_session(mut handle: SessionHandle) {
    let _ = handle.shutdown.send(true);
    if !*handle.stopped.borrow() {
        let _ = tokio::time::timeout(STOP_TIMEOUT, handle.stopped.changed()).await;
    }
}

struct SessionActor {
    provider: ProviderKind,
    label: &'static str,
    idle_timeout: Duration,
    spec: CommandSpec,
    runtime: AcpRuntimeOptions,
}

impl SessionActor {
    async fn run(
        self,
        mut turns: mpsc::Receiver<ResidentTurn>,
        mut controls: mpsc::Receiver<PendingProviderControl>,
        mut shutdown: watch::Receiver<bool>,
        stopped: watch::Sender<bool>,
    ) {
        let label = self.label;
        let mut process: Option<JsonLineProcess> = None;
        let mut connection = AcpConnectionState::default();
        let mut last_sink: Option<DriverEventSink> = None;
        let mut idle_deadline = tokio::time::Instant::now() + self.idle_timeout;
        loop {
            let request = tokio::select! {
                request = turns.recv() => match request { Some(request) => request, None => break },
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep_until(idle_deadline) => break,
                control = controls.recv() => {
                    if let Some(control) = control {
                        let _ = control.respond_to.send(Err(AppError::InvalidRequest(format!("{label} has no active turn"))));
                    }
                    continue;
                }
                notification = async { match process.as_mut() { Some(process) => process.read().await, None => std::future::pending().await } } => {
                    let Ok(Some(message)) = notification else { break };
                    let Some(process) = process.as_mut() else { continue };
                    if let Some(id) = message.get("id").filter(|_| message.get("method").is_some()) {
                        if process.send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32800,"message":"no active turn"}})).await.is_err() { break; }
                    } else if let Some(sink) = &last_sink {
                        super::acp::observe_config_options(&mut connection, &message);
                        let (_tx, mut cancel) = watch::channel(false);
                        if super::acp::handle_acp_message(process, message, sink, &mut cancel, self.provider, true, super::acp::AutoApprove::Mediate, &mut connection).await.is_err() { break; }
                    }
                    continue;
                }
            };
            let ResidentTurn {
                context,
                prompt,
                sink,
                mut cancel,
                launch_permit,
                respond_to,
            } = request;
            if process.is_none() {
                match JsonLineProcess::spawn_trusted(&self.spec, launch_permit).await {
                    Ok(spawned) => process = Some(spawned),
                    Err(error) => {
                        let _ = respond_to.send(Err(error));
                        break;
                    }
                }
            } else {
                drop(launch_permit);
            }
            let mut options = self.runtime.clone();
            // A reused process cannot apply a new CLI fallback; require a
            // protocol ACK.
            if last_sink.is_some() {
                options.allow_cli_config_fallback = false;
            }
            let Some(running) = process.as_mut() else {
                break;
            };
            let result = tokio::select! {
                result = run_acp_turn_controlled(running, context, prompt, &sink, &mut cancel, options, &mut connection, Some(&mut controls)) => result,
                _ = shutdown.changed() => { let _ = respond_to.send(Err(AppError::TurnCancelled)); break; }
            };
            let reusable = result.as_ref().is_ok_and(|result| !result.cancelled);
            last_sink = Some(sink);
            while let Ok(control) = controls.try_recv() {
                let _ = control
                    .respond_to
                    .send(Err(AppError::InvalidRequest(format!(
                        "{label} turn already ended"
                    ))));
            }
            if !reusable {
                turns.close();
                controls.close();
            }
            let _ = respond_to.send(result);
            if !reusable {
                break;
            }
            idle_deadline = tokio::time::Instant::now() + self.idle_timeout;
        }
        turns.close();
        controls.close();
        if let Some(process) = process.as_mut() {
            process.terminate().await;
        }
        let _ = stopped.send(true);
    }
}

/// The child environment of a resident ACP CLI: `defaults`, plus every
/// allowlisted variable set in the daemon's environment. Invalid names and
/// the daemon's own `TODEX_AGENTD_*` settings are never forwarded.
pub(super) fn allowlisted_environment(
    defaults: &[(&str, &str)],
    allowlist: &[String],
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = defaults
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
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

pub(super) fn valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// The ACP `initialize` handshake of a resident CLI, requiring protocol
/// version 1. `client_meta` becomes `clientCapabilities._meta`.
pub(super) async fn initialize_acp(
    process: &mut JsonLineProcess,
    peer: RpcPeer,
    client_meta: Option<Value>,
    timeout: Option<Duration>,
) -> Result<Value, AppError> {
    let mut capabilities = json!({
        "fs": {"readTextFile": false, "writeTextFile": false},
        "terminal": false,
        "session": {"configOptions": {}},
    });
    if let Some(meta) = client_meta {
        capabilities["_meta"] = meta;
    }
    let result = RpcClient::new(process, peer)
        .request(
            "initialize",
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": capabilities,
                "clientInfo": {"name": "todex-agentd", "title": "TodeX 2.0", "version": crate::version::APP_VERSION},
            }),
            timeout,
        )
        .await?;
    if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Err(AppError::Unsupported(format!(
            "{} negotiated an unsupported ACP protocol version",
            peer.name
        )));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_forwards_only_valid_allowlisted_names() {
        assert!(valid_env_name("XAI_API_KEY"));
        assert!(!valid_env_name("XAI-API-KEY"));
        assert!(!valid_env_name("1SECRET"));
        let env = allowlisted_environment(
            &[("NO_COLOR", "1")],
            &[
                "PATH".to_owned(),
                "TODEX_AGENTD_PORT".to_owned(),
                "BAD-NAME".to_owned(),
            ],
        );
        assert_eq!(env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert!(env.contains_key("PATH"));
        assert!(!env
            .keys()
            .any(|key| key.starts_with("TODEX_AGENTD_") || key == "BAD-NAME"));
    }
}
