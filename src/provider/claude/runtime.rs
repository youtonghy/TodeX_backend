//! One resident Claude Code process per conversation.
//!
//! A worker task owns each conversation's `claude -p --input-format
//! stream-json` process: turns are written to the same stdin, so only the
//! first turn pays for process start-up and `initialize`. Between turns the
//! worker drains stdout and refuses control requests; output that is not
//! part of a turn retires the process instead of reaching the journal. A
//! turn whose launch settings ([`LaunchKey`]) differ from the running
//! process restarts it with `--resume`. When every slot runs a turn, a new
//! conversation falls back to a process for just that turn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::time::Instant;

use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

use super::super::process::JsonLineProcess;
use super::super::types::{DriverContext, DriverEventSink, DriverPrompt, DriverTurnResult};
use super::Catalogs;

/// How long a cancelled turn may take to settle after `interrupt` before
/// its process is stopped instead of reused.
const INTERRUPT_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long stopping a session waits for its worker to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(6);

/// Everything a turn needs to start Claude.
#[derive(Clone)]
pub(in crate::provider) struct Launcher {
    pub binary: String,
    pub catalogs: Catalogs,
}

pub(in crate::provider) struct Run {
    pub context: DriverContext,
    pub prompt: DriverPrompt,
    pub sink: DriverEventSink,
    pub cancel: watch::Receiver<bool>,
    pub permit: WorkspaceTrustPermit,
}

struct Turn {
    run: Run,
    reply: oneshot::Sender<Reply>,
}

enum Reply {
    Finished(Result<DriverTurnResult, AppError>),
    /// The worker stopped before starting this turn; `retry` is false when
    /// the stop was requested, so the turn must not reopen the session.
    NotStarted {
        run: Box<Run>,
        retry: bool,
    },
}

#[derive(Clone)]
struct Worker {
    turns: mpsc::Sender<Turn>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
    /// When the worker last finished a turn; `None` while one is running.
    idle_since: Arc<std::sync::Mutex<Option<Instant>>>,
}

impl Worker {
    fn set_busy(&self) {
        *self.idle_since.lock().expect("Claude worker activity lock") = None;
    }

    fn idle_since(&self) -> Option<Instant> {
        *self.idle_since.lock().expect("Claude worker activity lock")
    }
}

pub(in crate::provider) struct Sessions {
    workers: Mutex<HashMap<String, Worker>>,
    idle_timeout: Duration,
    max_workers: usize,
}

impl Sessions {
    pub fn new(idle_timeout: Duration, max_workers: usize) -> Self {
        Self {
            workers: Mutex::new(HashMap::new()),
            idle_timeout,
            max_workers,
        }
    }

    /// Runs a turn on the conversation's resident process. A turn that raced
    /// its worker's own exit (idle timeout, retired process) never reached
    /// Claude and is retried once on a new worker.
    pub async fn run(
        &self,
        launcher: Launcher,
        mut run: Run,
    ) -> Result<DriverTurnResult, AppError> {
        let id = run.context.manifest.id.clone();
        let mut retried = false;
        loop {
            let Some(worker) = self.acquire(&id, &launcher).await else {
                return run_once(&launcher, run).await;
            };
            let (reply, response) = oneshot::channel();
            let retry = match worker.turns.send(Turn { run, reply }).await {
                Ok(()) => match response.await {
                    Ok(Reply::Finished(result)) => return result,
                    Ok(Reply::NotStarted {
                        run: returned,
                        retry,
                    }) => {
                        run = *returned;
                        retry
                    }
                    Err(_) => return Err(unavailable()),
                },
                // Closed between lookup and send: the worker is stopping.
                Err(mpsc::error::SendError(turn)) => {
                    run = turn.run;
                    !*worker.shutdown.borrow()
                }
            };
            if !retry || retried {
                return Err(AppError::Conflict(
                    "Claude Code session closed before the turn started; retry".to_owned(),
                ));
            }
            retried = true;
            wait_stopped(worker).await;
        }
    }

    /// The conversation's worker, marked busy, or `None` when every slot is
    /// running a turn. At the limit the longest-idle worker is stopped to
    /// make room.
    async fn acquire(&self, id: &str, launcher: &Launcher) -> Option<Worker> {
        loop {
            let stopping = {
                let mut workers = self.workers.lock().await;
                workers.retain(|_, worker| !*worker.stopped.borrow());
                match workers.get(id) {
                    // Its process is still exiting; never run two on one session.
                    Some(worker) if worker.turns.is_closed() => worker.clone(),
                    Some(worker) => {
                        worker.set_busy();
                        return Some(worker.clone());
                    }
                    None => {
                        if workers.len() >= self.max_workers {
                            let evict = workers
                                .iter()
                                .filter(|(_, worker)| !worker.turns.is_closed())
                                .filter_map(|(id, worker)| Some((worker.idle_since()?, id)))
                                .min()
                                .map(|(_, id)| id.clone());
                            let worker = workers.remove(&evict?)?;
                            tracing::debug!(
                                "Claude resident session limit reached; stopping the longest-idle session"
                            );
                            tokio::spawn(stop_worker(worker));
                        }
                        let worker = spawn_worker(launcher.clone(), self.idle_timeout);
                        worker.set_busy();
                        workers.insert(id.to_owned(), worker.clone());
                        return Some(worker);
                    }
                }
            };
            wait_stopped(stopping.clone()).await;
            let mut workers = self.workers.lock().await;
            if workers
                .get(id)
                .is_some_and(|current| current.stopped.same_channel(&stopping.stopped))
            {
                workers.remove(id);
            }
        }
    }

    /// Stops the conversation's process, if any, and waits for it to exit.
    pub async fn stop(&self, id: &str) {
        // Its own statement: a guard taken in an `if let` scrutinee would
        // hold every other session behind this shutdown.
        let worker = self.workers.lock().await.remove(id);
        if let Some(worker) = worker {
            stop_worker(worker).await;
        }
    }

    /// Stops every process: all are signalled before any is awaited.
    pub async fn stop_all(&self) {
        let workers: Vec<_> = self
            .workers
            .lock()
            .await
            .drain()
            .map(|(_, worker)| worker)
            .collect();
        for worker in &workers {
            let _ = worker.shutdown.send(true);
        }
        futures_util::future::join_all(workers.into_iter().map(stop_worker)).await;
    }

    #[cfg(test)]
    async fn worker_count(&self) -> usize {
        let mut workers = self.workers.lock().await;
        workers.retain(|_, worker| !*worker.stopped.borrow());
        workers.len()
    }
}

async fn stop_worker(worker: Worker) {
    let _ = worker.shutdown.send(true);
    wait_stopped(worker).await;
}

/// Waits, bounded by [`STOP_TIMEOUT`], for the worker to finish.
async fn wait_stopped(mut worker: Worker) {
    let _ = tokio::time::timeout(STOP_TIMEOUT, worker.stopped.wait_for(|stopped| *stopped)).await;
}

fn unavailable() -> AppError {
    AppError::ProviderUnavailable(
        "Claude Code session stopped before the turn completed".to_owned(),
    )
}

/// The launch settings of a Claude process. Every one is fixed by the
/// command line or read at start-up, so a turn whose key differs from the
/// running process's needs a new process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::provider) struct LaunchKey {
    binary: String,
    workspace: PathBuf,
    session_id: String,
    permission_mode: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    /// Digest of the injected agent MCP servers (command, env, token).
    mcp: Option<[u8; 32]>,
    config_dir: PathBuf,
    /// Digest of the user `settings.json`, which carries the active provider
    /// profile's environment (API key, base URL) and is read at start-up.
    settings: Option<[u8; 32]>,
}

impl LaunchKey {
    pub(in crate::provider) async fn new(
        binary: &str,
        context: &DriverContext,
        prompt: &DriverPrompt,
    ) -> Result<Self, AppError> {
        let controls = super::super::types::resolve_execution_config(
            context.manifest.provider,
            prompt.permission_mode.as_deref(),
            prompt.work_mode.as_deref(),
            prompt.permission_profile.as_deref(),
            prompt.sandbox_mode.as_deref(),
            prompt.approval_policy.as_deref(),
        )?;
        let config_dir = crate::agent_providers::claude_config_dir();
        let settings = tokio::fs::read(config_dir.join("settings.json"))
            .await
            .ok()
            .map(|bytes| Sha256::digest(bytes).into());
        Ok(Self {
            binary: binary.to_owned(),
            workspace: context.manifest.workspace.clone(),
            session_id: super::claude_session_id(context),
            permission_mode: controls.provider_mode,
            model: prompt.model.clone(),
            effort: prompt.reasoning_effort.clone(),
            mcp: context
                .agent_mcp
                .as_ref()
                .map(|launch| Sha256::digest(format!("{launch:?}")).into()),
            config_dir,
            settings,
        })
    }
}

struct Live {
    process: JsonLineProcess,
    key: LaunchKey,
}

fn spawn_worker(launcher: Launcher, idle_timeout: Duration) -> Worker {
    let (turns, turn_rx) = mpsc::channel(1);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (stopped_tx, stopped) = watch::channel(false);
    let idle_since = Arc::new(std::sync::Mutex::new(Some(Instant::now())));
    tokio::spawn(worker(
        launcher,
        idle_timeout,
        turn_rx,
        shutdown_rx,
        stopped_tx,
        idle_since.clone(),
    ));
    Worker {
        turns,
        shutdown,
        stopped,
        idle_since,
    }
}

/// What a turn on the worker's process left behind.
enum Ended {
    /// The process is between turns and can take the next one.
    Reusable,
    /// The process must be stopped.
    Retire,
}

async fn worker(
    launcher: Launcher,
    idle_timeout: Duration,
    mut turns: mpsc::Receiver<Turn>,
    mut shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
    idle_since: Arc<std::sync::Mutex<Option<Instant>>>,
) {
    let mut live: Option<Live> = None;
    let mut idle_deadline = Instant::now() + idle_timeout;
    loop {
        // Biased: a requested stop wins, then a waiting turn; the idle
        // deadline only fires when nothing else is ready.
        let turn = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            turn = turns.recv() => match turn { Some(turn) => turn, None => break },
            between = async {
                match live.as_mut() {
                    Some(live) => between_turns(&mut live.process).await,
                    None => std::future::pending().await,
                }
            } => match between {
                Ended::Reusable => continue,
                Ended::Retire => break,
            },
            _ = tokio::time::sleep_until(idle_deadline) => break,
        };
        let Turn { run, mut reply } = turn;
        let Run {
            context,
            prompt,
            sink,
            mut cancel,
            permit,
        } = run;
        let key = match LaunchKey::new(&launcher.binary, &context, &prompt).await {
            Ok(key) => key,
            Err(error) => {
                // Rejected before reaching Claude; the process is untouched.
                let _ = reply.send(Reply::Finished(Err(error)));
                *idle_since.lock().expect("Claude worker activity lock") = Some(Instant::now());
                continue;
            }
        };
        if let Some(mut stale) = live.take_if(|live| live.key != key) {
            // Never two processes on one transcript: the old one is gone
            // before the new one resumes it.
            stale.process.terminate().await;
        }
        let session_id = key.session_id.clone();
        let outcome = tokio::select! {
            outcome = async {
                if live.is_none() {
                    let process = super::start_claude(
                        &launcher.binary,
                        &launcher.catalogs,
                        &context,
                        &prompt,
                        permit,
                        &mut cancel,
                    )
                    .await?;
                    live = Some(Live { process, key });
                } else {
                    drop(permit);
                }
                let process = &mut live.as_mut().expect("started process").process;
                super::run_claude_turn(
                    process,
                    context,
                    prompt,
                    session_id,
                    &sink,
                    &launcher.catalogs,
                    &mut cancel,
                )
                .await
            } => Some(outcome),
            // The caller gave up on the turn (its task was aborted): nothing
            // will read the result, so stop the process mid-turn.
            _ = reply.closed() => None,
            _ = shutdown.changed() => {
                let _ = reply.send(Reply::Finished(Err(AppError::TurnCancelled)));
                break;
            }
        };
        let Some(outcome) = outcome else { break };
        let ended = match &outcome {
            Ok(result) if !result.cancelled => Ended::Reusable,
            Ok(_) => Ended::Retire,
            Err(_) => Ended::Retire,
        };
        let cancelled = outcome.as_ref().is_ok_and(|result| result.cancelled);
        // The caller has its result before any interrupt or teardown.
        let _ = reply.send(Reply::Finished(outcome));
        let ended = match (ended, live.as_mut()) {
            (Ended::Retire, Some(live)) if cancelled => {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    ended = interrupt(&mut live.process) => ended,
                }
            }
            (ended, _) => ended,
        };
        if matches!(ended, Ended::Retire) {
            break;
        }
        *idle_since.lock().expect("Claude worker activity lock") = Some(Instant::now());
        idle_deadline = Instant::now() + idle_timeout;
    }
    turns.close();
    if let Some(mut live) = live {
        live.process.terminate().await;
    }
    // A turn queued before the close never reached Claude: hand it back,
    // after the process is gone, so the caller can reopen the session.
    let retry = !*shutdown.borrow();
    while let Ok(Turn { run, reply }) = turns.try_recv() {
        let _ = reply.send(Reply::NotStarted {
            run: Box::new(run),
            retry,
        });
    }
    let _ = stopped.send(true);
}

/// Reads one frame while no turn runs. Nothing here reaches the journal.
async fn between_turns(process: &mut JsonLineProcess) -> Ended {
    let message = match process.read().await {
        Ok(Some(message)) => message,
        Ok(None) | Err(_) => return Ended::Retire,
    };
    match message.get("type").and_then(Value::as_str) {
        Some("control_request") => {
            // No turn owns a permission dialog now.
            let Some(request_id) = message.get("request_id").and_then(Value::as_str) else {
                return Ended::Reusable;
            };
            let response = idle_control_response(request_id);
            if process.send(&response).await.is_err() {
                return Ended::Retire;
            }
            Ended::Reusable
        }
        Some("assistant" | "stream_event" | "result") => {
            tracing::warn!("Claude Code produced output between turns; restarting its process");
            Ended::Retire
        }
        Some("system")
            if message.get("subtype").and_then(Value::as_str) == Some("session_state_changed")
                && message.get("state").and_then(Value::as_str) == Some("running") =>
        {
            tracing::warn!("Claude Code started work between turns; restarting its process");
            Ended::Retire
        }
        _ => Ended::Reusable,
    }
}

fn idle_control_response(request_id: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": "TodeX session is idle",
        }
    })
}

/// Interrupts a cancelled turn; the process is reusable when Claude settles
/// back to `idle` within [`INTERRUPT_SETTLE_TIMEOUT`].
async fn interrupt(process: &mut JsonLineProcess) -> Ended {
    let request_id = format!("todex-interrupt-{}", uuid::Uuid::new_v4());
    let request = json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    });
    if process.send(&request).await.is_err() {
        return Ended::Retire;
    }
    let settle = async {
        loop {
            let message = match process.read().await {
                Ok(Some(message)) => message,
                Ok(None) | Err(_) => return Ended::Retire,
            };
            match message.get("type").and_then(Value::as_str) {
                Some("control_request") => {
                    let Some(id) = message.get("request_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if process.send(&idle_control_response(id)).await.is_err() {
                        return Ended::Retire;
                    }
                }
                Some("system")
                    if message.get("subtype").and_then(Value::as_str)
                        == Some("session_state_changed")
                        && message.get("state").and_then(Value::as_str) == Some("idle") =>
                {
                    return Ended::Reusable;
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(INTERRUPT_SETTLE_TIMEOUT, settle)
        .await
        .unwrap_or(Ended::Retire)
}

/// A turn on a process of its own, for when every resident slot is busy.
/// The process is stopped after the turn, off the caller's path.
async fn run_once(launcher: &Launcher, run: Run) -> Result<DriverTurnResult, AppError> {
    let Run {
        context,
        prompt,
        sink,
        mut cancel,
        permit,
    } = run;
    let session_id = super::claude_session_id(&context);
    let mut process = super::start_claude(
        &launcher.binary,
        &launcher.catalogs,
        &context,
        &prompt,
        permit,
        &mut cancel,
    )
    .await?;
    let result = super::run_claude_turn(
        &mut process,
        context,
        prompt,
        session_id,
        &sink,
        &launcher.catalogs,
        &mut cancel,
    )
    .await;
    tokio::spawn(async move { process.terminate().await });
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::conversation::{
        ConversationEventHub, ConversationManifest, ConversationStore, ProviderKind, ProviderState,
    };
    use crate::provider::types::PermissionBroker;
    use crate::workspace_trust::WorkspaceTrustStore;
    use std::path::Path;

    /// A stream-json Claude stand-in. Each prompt's text picks a behavior:
    /// `block` waits for a `release` file, `silent` never answers nor
    /// honors `interrupt`, `interruptible` streams until interrupted,
    /// `late-permission` / `late-output` follow the turn with a frame while
    /// no turn runs. Every launch's argv and every stdin line are logged.
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/argv.log"
mode=
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/wire.jsonl"
  case "$line" in
    *'"subtype":"initialize"'*)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{"commands":[{"name":"review","description":"Review"}]}}}\n'
      continue ;;
    *'"subtype":"interrupt"'*)
      if [ "$mode" = interruptible ]; then
        rid=$(printf '%s' "$line" | /usr/bin/sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$rid"
        printf '{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}\n'
        printf '{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1}\n'
        printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
      fi
      continue ;;
    *'"type":"control_response"'*) continue ;;
  esac
  printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
  case "$line" in
    *block*) while [ ! -f "$dir/release" ]; do sleep 0.02; done ;;
  esac
  case "$line" in
    *silent*|*interruptible*)
      case "$line" in *silent*) mode=silent ;; *) mode=interruptible ;; esac
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"working"}}}\n'
      continue ;;
  esac
  mode=
  printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"ok"}}}\n'
  printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"ok"}\n'
  printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
  case "$line" in
    *late-permission*)
      sleep 0.1
      printf '{"type":"control_request","request_id":"late-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}\n' ;;
    *late-output*)
      sleep 0.1
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"unprompted"}]}}\n' ;;
  esac
done
"#;

    struct Fixture {
        root: PathBuf,
        store: ConversationStore,
        trust: WorkspaceTrustStore,
        launcher: Launcher,
    }

    impl Fixture {
        async fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;
            let root =
                std::env::temp_dir().join(format!("todex-claude-runtime-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&root).await.unwrap();
            let root = tokio::fs::canonicalize(root).await.unwrap();
            let script = root.join("fake-claude");
            tokio::fs::write(&script, FAKE_CLAUDE).await.unwrap();
            tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .await
                .unwrap();
            let store = ConversationStore::new(root.join("data")).await.unwrap();
            let trust = WorkspaceTrustStore::new(root.join("trust"), vec![root.clone()])
                .await
                .unwrap();
            trust.set_owned("local", &root, true).await.unwrap();
            Self {
                launcher: Launcher {
                    binary: script.display().to_string(),
                    catalogs: Catalogs::default(),
                },
                root,
                store,
                trust,
            }
        }

        async fn conversation(&self) -> ConversationManifest {
            self.store
                .create(ConversationManifest::new(
                    ProviderKind::ClaudeCode,
                    self.root.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap()
        }

        async fn run(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
            text: &str,
            model: Option<&str>,
            cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
            let sink = DriverEventSink::new(
                self.store.clone(),
                ConversationEventHub::default(),
                PermissionBroker::default(),
                manifest.id.clone(),
            )
            .with_turn_id(turn_id.clone());
            let provider_state = self
                .store
                .provider_state(&manifest.id)
                .await
                .unwrap_or_else(|_| ProviderState::new(ProviderKind::ClaudeCode));
            let run = Run {
                context: DriverContext {
                    manifest: manifest.clone(),
                    provider_state,
                    agent_mcp: None,
                },
                prompt: DriverPrompt {
                    turn_id,
                    text: text.to_owned(),
                    content: vec![],
                    skills: vec![],
                    model: model.map(str::to_owned),
                    reasoning_effort: None,
                    permission_mode: None,
                    work_mode: None,
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                },
                sink,
                cancel,
                permit: self.trust.acquire_owned("local", &self.root).await.unwrap(),
            };
            tokio::time::timeout(
                Duration::from_secs(15),
                sessions.run(self.launcher.clone(), run),
            )
            .await
            .expect("turn finishes")
        }

        async fn turn(
            &self,
            sessions: &Sessions,
            manifest: &ConversationManifest,
            text: &str,
        ) -> DriverTurnResult {
            let (_cancel, cancel) = watch::channel(false);
            self.run(sessions, manifest, text, None, cancel)
                .await
                .unwrap()
        }

        fn launches(&self) -> Vec<String> {
            std::fs::read_to_string(self.root.join("argv.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn wire(&self) -> String {
            std::fs::read_to_string(self.root.join("wire.jsonl")).unwrap_or_default()
        }

        async fn wait_for_wire(&self, needle: &str) {
            wait_until(|| self.wire().contains(needle)).await;
        }
    }

    async fn wait_until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition reached in time");
    }

    fn sessions(max: usize) -> Sessions {
        Sessions::new(Duration::from_secs(300), max)
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn consecutive_turns_share_one_process_and_one_initialize() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        for text in ["first", "second"] {
            let result = fixture.turn(&sessions, &manifest, text).await;
            assert!(!result.cancelled);
            assert_eq!(
                result.native_session_id.as_deref(),
                Some(manifest.id.as_str())
            );
        }
        assert_eq!(fixture.launches().len(), 1);
        let wire = fixture.wire();
        assert_eq!(wire.matches(r#""subtype":"initialize""#).count(), 1);
        assert_eq!(wire.matches(r#""type":"user""#).count(), 2);
        // The catalog captured at spawn is still served between turns.
        let catalogs = fixture.launcher.catalogs.lock().await;
        assert_eq!(catalogs[&manifest.id].commands[0].name, "review");
        drop(catalogs);
        // The session id is persisted once, not after every turn.
        let state = fixture.store.provider_state(&manifest.id).await.unwrap();
        assert_eq!(
            state.native_session_id.as_deref(),
            Some(manifest.id.as_str())
        );
        sessions.stop_all().await;
        assert_eq!(sessions.worker_count().await, 0);
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_changed_launch_setting_restarts_the_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        fixture.turn(&sessions, &manifest, "first").await;
        let (_cancel, cancel) = watch::channel(false);
        fixture
            .run(&sessions, &manifest, "second", Some("haiku"), cancel)
            .await
            .unwrap();
        let (_cancel, cancel) = watch::channel(false);
        fixture
            .run(&sessions, &manifest, "third", Some("haiku"), cancel)
            .await
            .unwrap();
        let launches = fixture.launches();
        assert_eq!(launches.len(), 2, "{launches:?}");
        assert!(!launches[0].contains("--model"));
        assert!(launches[1].contains("--model haiku"));
        assert!(launches[1].contains(&manifest.id));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn the_limit_evicts_the_longest_idle_session_and_falls_back_when_all_are_busy() {
        let fixture = Fixture::new().await;
        let sessions = Arc::new(sessions(2));
        let first = fixture.conversation().await;
        let second = fixture.conversation().await;
        let third = fixture.conversation().await;
        fixture.turn(&sessions, &first, "one").await;
        fixture.turn(&sessions, &second, "two").await;
        // Both slots are taken; the third conversation stops the first,
        // which has been idle the longest.
        fixture.turn(&sessions, &third, "three").await;
        assert_eq!(sessions.worker_count().await, 2);
        assert!(!sessions.workers.lock().await.contains_key(&first.id));
        assert_eq!(fixture.launches().len(), 3);

        // With both slots running a turn, the first conversation still runs,
        // on a process of its own, instead of failing.
        let block = |manifest: ConversationManifest| {
            let sessions = sessions.clone();
            let fixture = &fixture;
            async move {
                let (_cancel, cancel) = watch::channel(false);
                fixture
                    .run(&sessions, &manifest, "block", None, cancel)
                    .await
            }
        };
        let blocked = futures_util::future::join(block(second.clone()), block(third.clone()));
        let fallback = async {
            fixture.wait_for_wire("block").await;
            wait_until(|| fixture.wire().matches("block").count() == 2).await;
            let result = fixture.turn(&sessions, &first, "fallback").await;
            assert_eq!(sessions.worker_count().await, 2);
            assert!(!sessions.workers.lock().await.contains_key(&first.id));
            std::fs::write(fixture.root.join("release"), "").unwrap();
            result
        };
        let ((second_result, third_result), fallback) =
            futures_util::future::join(blocked, fallback).await;
        assert!(!fallback.cancelled);
        second_result.unwrap();
        third_result.unwrap();
        assert_eq!(fixture.launches().len(), 4);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn permission_requests_between_turns_are_refused_and_output_retires_the_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        fixture.turn(&sessions, &manifest, "late-permission").await;
        fixture.wait_for_wire("TodeX session is idle").await;
        let refusal = fixture
            .wire()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|frame| frame["response"]["request_id"] == "late-1")
            .unwrap();
        assert_eq!(refusal["response"]["subtype"], "error");
        fixture.turn(&sessions, &manifest, "again").await;
        assert_eq!(fixture.launches().len(), 1);

        // Unprompted model output: the process is retired unseen and the
        // next turn resumes on a new one.
        fixture.turn(&sessions, &manifest, "late-output").await;
        wait_until(|| {
            sessions
                .workers
                .try_lock()
                .is_ok_and(|workers| workers.values().all(|worker| worker.turns.is_closed()))
        })
        .await;
        fixture.turn(&sessions, &manifest, "after").await;
        assert_eq!(fixture.launches().len(), 2);
        let history = fixture.store.complete_history(&manifest.id).await.unwrap();
        assert!(!history
            .iter()
            .any(|event| event.event_type == "permission.requested"));
        assert!(!history
            .iter()
            .any(|event| event.payload.to_string().contains("unprompted")));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_cancelled_turn_that_settles_keeps_its_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (cancel_tx, cancel) = watch::channel(false);
        let cancelling = async {
            fixture.wait_for_wire("interruptible").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_tx.send(true).unwrap();
        };
        let (result, ()) = futures_util::future::join(
            fixture.run(&sessions, &manifest, "interruptible", None, cancel),
            cancelling,
        )
        .await;
        let result = result.unwrap();
        assert!(result.cancelled);
        assert_eq!(result.stop_reason, "cancelled");
        fixture.turn(&sessions, &manifest, "next").await;
        assert_eq!(fixture.launches().len(), 1);
        assert!(fixture.wire().contains(r#""subtype":"interrupt""#));
        // Nothing the interrupted turn printed afterwards was journalled.
        let history = fixture.store.complete_history(&manifest.id).await.unwrap();
        assert!(!history
            .iter()
            .any(|event| event.payload.to_string().contains("partial")));
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn a_cancelled_turn_that_ignores_interrupt_is_stopped() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        let (cancel_tx, cancel) = watch::channel(false);
        let cancelling = async {
            fixture.wait_for_wire("silent").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_tx.send(true).unwrap();
        };
        let started = Instant::now();
        let (result, ()) = futures_util::future::join(
            fixture.run(&sessions, &manifest, "silent", None, cancel),
            cancelling,
        )
        .await;
        assert!(result.unwrap().cancelled);
        // The caller is not held for the interrupt window.
        assert!(started.elapsed() < INTERRUPT_SETTLE_TIMEOUT);
        // The next turn waits for the stuck process to be stopped, then
        // resumes on a new one.
        fixture.turn(&sessions, &manifest, "next").await;
        assert_eq!(fixture.launches().len(), 2);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }

    #[tokio::test]
    async fn stopping_a_session_ends_its_process() {
        let fixture = Fixture::new().await;
        let sessions = sessions(16);
        let manifest = fixture.conversation().await;
        fixture.turn(&sessions, &manifest, "first").await;
        assert_eq!(sessions.worker_count().await, 1);
        sessions.stop(&manifest.id).await;
        assert_eq!(sessions.worker_count().await, 0);
        fixture.turn(&sessions, &manifest, "second").await;
        assert_eq!(fixture.launches().len(), 2);
        sessions.stop_all().await;
        cleanup(&fixture.root);
    }
}
