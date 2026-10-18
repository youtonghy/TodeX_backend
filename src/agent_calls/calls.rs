//! Agent calls made through `todex_agents`: which child conversation each
//! parent started, the state of each child's running turn, and what the
//! parent's journal records about them (`agent.call.*`).
//!
//! A started turn is followed by a background monitor that reads the
//! child's [`LiveTap`] until the turn ends, whether or not the tool call
//! that started it is still waiting, so a later `agent_wait` can pick up
//! the result and the parent learns about the end. Results (the child's
//! assistant text) live in memory only: dropped once a tool returned them,
//! when the child's next turn starts, or [`RESULT_TTL`] after the turn
//! ended.
//!
//! Lifecycle: a parent turn that is cancelled or interrupted cancels its
//! running children; a deleted parent takes its children with it (running
//! ones are cancelled first); running calls are listed in
//! `$DATA_DIR/agent-calls.json`, and calls a daemon restart cut off are
//! reported to their parent as `agent.call.failed` with
//! `error.code = "DAEMON_RESTARTED"`.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;

use super::events::{assistant_text, ends_turn, TurnEvents};
use super::{StartedTurn, LIMITS};
use crate::conversation::{ConversationEvent, ConversationManifest, LiveTap, ProviderKind};
use crate::error::AppError;
use crate::provider::{ConversationHooks, ConversationSupervisor};
use crate::secure_fs;

const CALLS_FILE: &str = "agent-calls.json";
/// Assistant text kept per turn for the tool result; more is cut.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Output recorded in a parent's `agent.call.*` event.
const MAX_EVENT_OUTPUT_BYTES: usize = 64 * 1024;
/// Prompt text recorded in a parent's `agent.call.started` event.
const MAX_EVENT_TEXT_BYTES: usize = 16 * 1024;
/// How long an unread result is kept after its turn ended.
pub(crate) const RESULT_TTL: Duration = Duration::from_secs(30 * 60);
/// How long deleting a parent waits for a cancelled child turn to stop.
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const TRUNCATED_NOTE: &str = "\n[output truncated]";

/// The state of one agent-call turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CallStatus {
    Running,
    WaitingPermission,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl CallStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingPermission => "waiting_permission",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::WaitingPermission)
    }

    /// From a terminal event type (`turn.completed`, `conversation.failed`,
    /// ...).
    fn from_terminal(event_type: &str) -> Self {
        let status = event_type
            .strip_prefix("turn.")
            .or_else(|| event_type.strip_prefix("conversation."))
            .unwrap_or(event_type);
        match status {
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => Self::Failed,
        }
    }

    /// The status a tool result reports: waiting for a permission is still
    /// running.
    fn reported(self) -> &'static str {
        match self {
            Self::WaitingPermission => Self::Running.as_str(),
            other => other.as_str(),
        }
    }
}

/// Why a call failed: `{ code?, message }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CallError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

impl CallError {
    /// From the payload of a terminal event (`code`, and `message` or
    /// `error`, which may be a string or `{ code?, message }`).
    fn from_terminal(payload: &Value) -> Self {
        let error = payload.get("error");
        let code = payload
            .get("code")
            .or_else(|| error.and_then(|error| error.get("code")))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.and_then(Value::as_str))
            .or_else(|| {
                error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("the agent call did not complete")
            .to_owned();
        Self { code, message }
    }
}

/// What `agent_run`, `agent_turn` and `agent_wait` return.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CallResult {
    pub conversation_id: String,
    pub turn_id: String,
    pub status: CallStatus,
    pub output: String,
    pub error: Option<CallError>,
}

impl CallResult {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "conversationId": self.conversation_id,
            "turnId": self.turn_id,
            "status": self.status.reported(),
            "output": self.output,
            "error": self.error,
        })
    }
}

#[derive(Clone, Debug)]
struct TurnState {
    status: CallStatus,
    output: String,
    truncated: bool,
    error: Option<CallError>,
}

impl TurnState {
    fn push_output(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.output.len());
        if text.len() <= room {
            self.output.push_str(text);
        } else {
            self.output.push_str(prefix(text, room));
            self.output.push_str(TRUNCATED_NOTE);
            self.truncated = true;
        }
    }
}

/// One child turn an agent call started.
struct CallTurn {
    turn_id: String,
    state: watch::Sender<TurnState>,
    started: Instant,
}

impl CallTurn {
    fn status(&self) -> CallStatus {
        self.state.borrow().status
    }
}

struct ChildCall {
    parent_id: String,
    owner_id: String,
    /// The latest turn an agent call started.
    turn: Option<Arc<CallTurn>>,
}

#[derive(Default)]
struct State {
    /// child conversation id → its call.
    children: HashMap<String, ChildCall>,
    /// parent conversation id → calls being started.
    reservations: HashMap<String, usize>,
}

impl State {
    fn running(&self, parent_id: &str) -> usize {
        self.children
            .values()
            .filter(|child| {
                child.parent_id == parent_id
                    && child
                        .turn
                        .as_ref()
                        .is_some_and(|turn| turn.status().is_running())
            })
            .count()
    }
}

/// A running call as `agent-calls.json` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunningCall {
    parent_conversation_id: String,
    child_conversation_id: String,
    child_turn_id: String,
    agent: ProviderKind,
    started_at: DateTime<Utc>,
}

#[derive(Default, Serialize, Deserialize)]
struct CallsFile {
    #[serde(default)]
    calls: Vec<RunningCall>,
}

/// The agent calls of every conversation.
#[derive(Clone)]
pub(crate) struct AgentCalls {
    inner: Arc<Inner>,
}

struct Inner {
    supervisor: ConversationSupervisor,
    path: PathBuf,
    state: Mutex<State>,
    /// Running calls as persisted, by child turn id.
    persisted: Mutex<HashMap<String, RunningCall>>,
    /// Conversations that have (or may have) children, so removing any
    /// other conversation does not scan for children.
    parents: Mutex<HashSet<String>>,
    /// Serializes writes of `agent-calls.json`.
    write: tokio::sync::Mutex<()>,
}

/// A slot for a call being started, counted against the parent's limit
/// until [`AgentCalls::track`] takes it or it is dropped.
pub(crate) struct Reservation {
    inner: Arc<Inner>,
    parent_id: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if let Some(count) = state.reservations.get_mut(&self.parent_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.reservations.remove(&self.parent_id);
            }
        }
    }
}

impl AgentCalls {
    /// The calls of a daemon whose conversations run on `supervisor`, and
    /// the supervisor hooks that apply their lifecycle.
    pub(crate) fn new(data_dir: &Path, supervisor: ConversationSupervisor) -> Self {
        let calls = Self {
            inner: Arc::new(Inner {
                supervisor: supervisor.clone(),
                path: data_dir.join(CALLS_FILE),
                state: Mutex::default(),
                persisted: Mutex::default(),
                parents: Mutex::default(),
                write: tokio::sync::Mutex::new(()),
            }),
        };
        supervisor.set_hooks(Arc::new(Hooks(Arc::downgrade(&calls.inner))));
        calls
    }

    /// Startup, after conversation recovery: learns which conversations
    /// have children and reports the calls the previous daemon left running
    /// to their parents (`agent.call.failed`, `DAEMON_RESTARTED`), then
    /// clears the list.
    pub(crate) async fn start(&self) {
        match self.inner.supervisor.list().await {
            Ok(manifests) => {
                let mut parents = self.inner.parents();
                parents.extend(
                    manifests
                        .into_iter()
                        .filter_map(|manifest| manifest.parent_conversation_id),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "cannot list conversations to find agent call children")
            }
        }
        self.recover_leftovers().await;
    }

    /// `parent_id` is about to get a child: deleting it deletes them.
    pub(crate) fn note_parent(&self, parent_id: &str) {
        self.inner.parents().insert(parent_id.to_owned());
    }

    async fn recover_leftovers(&self) {
        let path = self.inner.path.clone();
        let leftovers = match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<CallsFile>(&bytes) {
                Ok(file) => file.calls,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "ignoring an unreadable agent call list");
                    Vec::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot read the agent call list");
                return;
            }
        };
        for call in leftovers {
            let payload = json!({
                "childConversationId": call.child_conversation_id,
                "childTurnId": call.child_turn_id,
                "status": CallStatus::Failed.as_str(),
                "error": CallError {
                    code: Some("DAEMON_RESTARTED".to_owned()),
                    message: "TodeX restarted while this agent call was running".to_owned(),
                },
            });
            if let Err(error) = self
                .inner
                .supervisor
                .append_agent_event(&call.parent_conversation_id, "agent.call.failed", payload)
                .await
            {
                if !matches!(error, AppError::NotFound(_)) {
                    tracing::warn!(
                        parent = %call.parent_conversation_id,
                        child = %call.child_conversation_id,
                        %error,
                        "cannot report an agent call cut off by a restart"
                    );
                }
            }
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot clear the agent call list")
            }
        }
    }

    /// A slot for one more running call of `parent_id`, or `None` at the
    /// limit ([`LIMITS`]).
    pub(crate) fn reserve(&self, parent_id: &str) -> Option<Reservation> {
        let mut state = self.inner.lock();
        let reserved = state.reservations.get(parent_id).copied().unwrap_or(0);
        if state.running(parent_id) + reserved >= LIMITS.max_concurrent_calls {
            return None;
        }
        *state.reservations.entry(parent_id.to_owned()).or_default() += 1;
        Some(Reservation {
            inner: self.inner.clone(),
            parent_id: parent_id.to_owned(),
        })
    }

    /// The turn an agent call is running in `child_id`, if any.
    pub(crate) fn running_turn(&self, child_id: &str) -> Option<String> {
        let state = self.inner.lock();
        let turn = state.children.get(child_id)?.turn.as_ref()?;
        turn.status().is_running().then(|| turn.turn_id.clone())
    }

    /// Records a started child turn, tells the parent
    /// (`agent.call.started`) and follows the turn in the background.
    pub(crate) async fn track(
        &self,
        reservation: Reservation,
        parent: &ConversationManifest,
        started: StartedTurn<LiveTap>,
        text: &str,
        title: Option<&str>,
    ) {
        let StartedTurn {
            manifest: child,
            turn_id,
            events,
            model,
            reasoning_effort,
        } = started;
        let (state, _) = watch::channel(TurnState {
            status: CallStatus::Running,
            output: String::new(),
            truncated: false,
            error: None,
        });
        let turn = Arc::new(CallTurn {
            turn_id: turn_id.clone(),
            state,
            started: Instant::now(),
        });
        let started_at = Utc::now();
        {
            let mut state = self.inner.lock();
            state.children.insert(
                child.id.clone(),
                ChildCall {
                    parent_id: parent.id.clone(),
                    owner_id: child.owner_id.clone(),
                    turn: Some(turn.clone()),
                },
            );
        }
        drop(reservation);
        self.inner
            .persist_running(RunningCall {
                parent_conversation_id: parent.id.clone(),
                child_conversation_id: child.id.clone(),
                child_turn_id: turn_id.clone(),
                agent: child.provider,
                started_at,
            })
            .await;
        let mut payload = json!({
            "childConversationId": child.id,
            "childTurnId": turn_id,
            "agent": child.provider,
            "text": bounded(text, MAX_EVENT_TEXT_BYTES),
            "status": CallStatus::Running.as_str(),
            "startedAt": started_at,
        });
        for (field, value) in [
            ("model", model.as_deref()),
            ("reasoningEffort", reasoning_effort.as_deref()),
            ("title", title),
        ] {
            if let Some(value) = value {
                payload[field] = json!(value);
            }
        }
        self.inner
            .record(&parent.id, "agent.call.started", payload)
            .await;
        let Some(events) = events else {
            return;
        };
        let inner = self.inner.clone();
        let parent_id = parent.id.clone();
        tokio::spawn(async move { inner.monitor(parent_id, child.id, turn, events).await });
    }

    /// Waits up to `wait` for the child's latest agent-call turn (only
    /// `turn_id`, if given) to end. A finished result's output is handed
    /// out once.
    pub(crate) async fn wait(
        &self,
        child_id: &str,
        turn_id: Option<&str>,
        wait: Duration,
    ) -> Result<CallResult, AppError> {
        let turn = {
            let state = self.inner.lock();
            state
                .children
                .get(child_id)
                .and_then(|child| child.turn.clone())
        };
        let Some(turn) = turn.filter(|turn| turn_id.is_none_or(|id| id == turn.turn_id)) else {
            return Err(AppError::NotFound(match turn_id {
                Some(turn_id) => format!("turn {turn_id} is not a known agent call turn"),
                None => "this conversation has no agent call turn to wait for".to_owned(),
            }));
        };
        let mut receiver = turn.state.subscribe();
        let _ =
            tokio::time::timeout(wait, receiver.wait_for(|state| !state.status.is_running())).await;
        let snapshot = turn.state.borrow().clone();
        if !snapshot.status.is_running() {
            turn.state.send_modify(|state| state.output = String::new());
        }
        Ok(CallResult {
            conversation_id: child_id.to_owned(),
            turn_id: turn.turn_id.clone(),
            status: snapshot.status,
            output: snapshot.output,
            error: snapshot.error,
        })
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn parents(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.parents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Journals `event_type` into the parent. A parent that is gone (or
    /// cannot be written) only costs the record.
    async fn record(&self, parent_id: &str, event_type: &str, payload: Value) {
        if let Err(error) = self
            .supervisor
            .append_agent_event(parent_id, event_type, payload)
            .await
        {
            if !matches!(error, AppError::NotFound(_)) {
                tracing::warn!(parent = %parent_id, event_type, %error, "cannot record an agent call event");
            }
        }
    }

    /// Follows a child turn until it ends, then records the end.
    async fn monitor(
        self: Arc<Self>,
        parent_id: String,
        child_id: String,
        turn: Arc<CallTurn>,
        mut events: LiveTap,
    ) {
        let mut pending_permissions = HashSet::new();
        let ended = 'follow: loop {
            let batch = match events.next_batch().await {
                Ok(batch) => batch,
                Err(_) => break 'follow None,
            };
            for event in batch {
                if self
                    .observe(
                        &parent_id,
                        &child_id,
                        &turn,
                        &event,
                        &mut pending_permissions,
                    )
                    .await
                {
                    break 'follow Some(event);
                }
            }
        };
        // Nothing more is read for this turn: stop tapping.
        drop(events);
        let (status, error) = match &ended {
            Some(event) => {
                let status = CallStatus::from_terminal(&event.event_type);
                let error = (status != CallStatus::Completed)
                    .then(|| CallError::from_terminal(&event.payload));
                (status, error)
            }
            None => (
                CallStatus::Failed,
                Some(CallError {
                    code: Some(AppError::StreamClosed.code().to_owned()),
                    message: "the called agent's conversation went away".to_owned(),
                }),
            ),
        };
        // Taken with the status change: a waiter may hand the output out
        // (and drop it) as soon as it sees the turn ended.
        let mut output = String::new();
        turn.state.send_modify(|state| {
            state.status = status;
            state.error = error.clone();
            output = bounded(&state.output, MAX_EVENT_OUTPUT_BYTES);
        });
        // Stopped by a daemon shutdown: left listed, so the next start
        // reports it to the parent as cut off by the restart.
        if self.supervisor.is_shutting_down() {
            return;
        }
        self.forget_running(&turn.turn_id).await;
        let duration_ms = u64::try_from(turn.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut payload = json!({
            "childConversationId": child_id,
            "childTurnId": turn.turn_id,
            "status": status.as_str(),
            "durationMs": duration_ms,
        });
        let event_type = match status {
            CallStatus::Completed => "agent.call.completed",
            CallStatus::Cancelled => "agent.call.cancelled",
            _ => "agent.call.failed",
        };
        if status == CallStatus::Completed || !output.is_empty() {
            payload["output"] = json!(output);
        }
        if let (Some(error), CallStatus::Failed | CallStatus::Interrupted) = (&error, status) {
            payload["error"] = json!(error);
        }
        self.record(&parent_id, event_type, payload).await;
        // An unread result does not stay in memory for good.
        let expiring = Arc::downgrade(&turn);
        tokio::spawn(async move {
            tokio::time::sleep(RESULT_TTL).await;
            if let Some(turn) = expiring.upgrade() {
                turn.state.send_modify(|state| state.output = String::new());
            }
        });
    }

    /// Applies one tapped event of the child to the turn; `true` once it
    /// ended the turn.
    async fn observe(
        &self,
        parent_id: &str,
        child_id: &str,
        turn: &CallTurn,
        event: &ConversationEvent,
        pending_permissions: &mut HashSet<String>,
    ) -> bool {
        let ours = event.payload.get("turnId").and_then(Value::as_str) == Some(&turn.turn_id);
        if ours {
            if let Some(text) = assistant_text(event) {
                turn.state.send_modify(|state| state.push_output(text));
            }
            let permission = event
                .payload
                .get("permissionId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let changed = match (event.event_type.as_str(), permission) {
                ("permission.requested", Some(id)) => {
                    pending_permissions.insert(id);
                    self.set_running_status(turn, CallStatus::WaitingPermission)
                }
                ("permission.resolved", Some(id)) => {
                    pending_permissions.remove(&id);
                    pending_permissions.is_empty()
                        && self.set_running_status(turn, CallStatus::Running)
                }
                _ => false,
            };
            if changed {
                let status = turn.status();
                self.record(
                    parent_id,
                    "agent.call.updated",
                    json!({
                        "childConversationId": child_id,
                        "childTurnId": turn.turn_id,
                        "status": status.as_str(),
                    }),
                )
                .await;
            }
        }
        ends_turn(event, Some(&turn.turn_id))
    }

    /// Moves a running turn to `status`; whether it changed.
    fn set_running_status(&self, turn: &CallTurn, status: CallStatus) -> bool {
        turn.state.send_if_modified(|state| {
            if state.status.is_running() && state.status != status {
                state.status = status;
                true
            } else {
                false
            }
        })
    }

    async fn persist_running(&self, call: RunningCall) {
        self.persisted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call.child_turn_id.clone(), call);
        self.write_persisted().await;
    }

    async fn forget_running(&self, turn_id: &str) {
        let removed = self
            .persisted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(turn_id)
            .is_some();
        if removed {
            self.write_persisted().await;
        }
    }

    /// Writes the running calls (owner-only, atomic); an empty list removes
    /// the file.
    async fn write_persisted(&self) {
        let _write = self.write.lock().await;
        // Read under the write lock, so the last writer writes the latest.
        let mut calls = self
            .persisted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        calls.sort_by_key(|call| call.started_at);
        let path = self.path.clone();
        let result = if calls.is_empty() {
            match tokio::fs::remove_file(&path).await {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
                _ => Ok(()),
            }
        } else {
            match serde_json::to_vec_pretty(&CallsFile { calls }) {
                Ok(bytes) => tokio::task::spawn_blocking(move || {
                    secure_fs::write_owner_only_atomic(&path, &bytes)
                })
                .await
                .map_err(|error| AppError::Anyhow(error.into()))
                .and_then(|written| written.map_err(AppError::from)),
                Err(error) => Err(error.into()),
            }
        };
        if let Err(error) = result {
            tracing::warn!(path = %self.path.display(), %error, "cannot save the agent call list");
        }
    }

    /// Cancels the running agent-call turns of `parent_id`'s children.
    fn cancel_children(&self, parent_id: &str) {
        let running = {
            let state = self.lock();
            state
                .children
                .iter()
                .filter(|(_, child)| child.parent_id == parent_id)
                .filter_map(|(id, child)| {
                    let turn = child.turn.as_ref()?;
                    turn.status()
                        .is_running()
                        .then(|| (id.clone(), child.owner_id.clone(), turn.turn_id.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (child_id, owner_id, turn_id) in running {
            let supervisor = self.supervisor.clone();
            tokio::spawn(async move {
                if let Err(error) = supervisor
                    .cancel_owned(&owner_id, &child_id, Some(&turn_id))
                    .await
                {
                    tracing::warn!(child = %child_id, %error, "cannot cancel an agent call");
                }
            });
        }
    }

    /// Deletes every child of a removed parent, cancelling running turns
    /// first.
    async fn delete_children(self: Arc<Self>, parent_id: String) {
        let children = match self.supervisor.list().await {
            Ok(manifests) => manifests
                .into_iter()
                .filter(|manifest| manifest.parent_conversation_id.as_deref() == Some(&parent_id))
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!(parent = %parent_id, %error, "cannot list the children of a deleted conversation");
                return;
            }
        };
        for child in children {
            if self.supervisor.active_turn_id(&child.id).is_some() {
                let _ = self
                    .supervisor
                    .cancel_owned(&child.owner_id, &child.id, None)
                    .await;
                let deadline = Instant::now() + CHILD_STOP_TIMEOUT;
                while self.supervisor.active_turn_id(&child.id).is_some()
                    && Instant::now() < deadline
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            // The turn's end may still be settling the status.
            let mut attempts = 0;
            loop {
                match self
                    .supervisor
                    .delete_owned(&child.owner_id, &child.id)
                    .await
                {
                    Ok(_) | Err(AppError::NotFound(_)) => break,
                    Err(AppError::Conflict(_)) if attempts < 40 => {
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(error) => {
                        tracing::warn!(child = %child.id, %error, "cannot delete an agent call of a deleted conversation");
                        break;
                    }
                }
            }
        }
    }
}

/// Applies the agent call lifecycle to supervisor events. Weak, so the
/// supervisor does not keep the calls alive.
struct Hooks(Weak<Inner>);

impl ConversationHooks for Hooks {
    fn turn_ended(&self, conversation_id: &str, terminal: &str) {
        if !matches!(
            terminal,
            "turn.cancelled" | "turn.interrupted" | "conversation.interrupted"
        ) {
            return;
        }
        // A shutdown stops every turn itself.
        if let Some(inner) = self
            .0
            .upgrade()
            .filter(|inner| !inner.supervisor.is_shutting_down())
        {
            inner.cancel_children(conversation_id);
        }
    }

    fn conversation_removed(&self, manifest: &ConversationManifest) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        if manifest.parent_conversation_id.is_some() {
            inner.lock().children.remove(&manifest.id);
            return;
        }
        if !inner.parents().remove(&manifest.id) {
            return;
        }
        let parent_id = manifest.id.clone();
        tokio::spawn(inner.delete_children(parent_id));
    }
}

/// The longest prefix of `text` within `bytes`, on a char boundary.
fn prefix(text: &str, bytes: usize) -> &str {
    if text.len() <= bytes {
        return text;
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` cut to `bytes` with a note when it was longer.
fn bounded(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        text.to_owned()
    } else {
        format!("{}{TRUNCATED_NOTE}", prefix(text, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_capped_on_a_char_boundary() {
        let mut state = TurnState {
            status: CallStatus::Running,
            output: String::new(),
            truncated: false,
            error: None,
        };
        state.push_output(&"é".repeat(MAX_OUTPUT_BYTES));
        assert!(state.truncated);
        assert!(state.output.ends_with(TRUNCATED_NOTE));
        assert!(state.output.len() <= MAX_OUTPUT_BYTES + TRUNCATED_NOTE.len());
        state.push_output("more");
        assert!(!state.output.ends_with("more"));
        assert_eq!(bounded("abc", 2), format!("ab{TRUNCATED_NOTE}"));
    }

    #[test]
    fn terminal_events_map_to_call_states() {
        assert_eq!(
            CallStatus::from_terminal("turn.completed"),
            CallStatus::Completed
        );
        assert_eq!(
            CallStatus::from_terminal("conversation.interrupted"),
            CallStatus::Interrupted
        );
        assert_eq!(
            CallStatus::from_terminal("conversation.failed"),
            CallStatus::Failed
        );
        assert_eq!(CallStatus::WaitingPermission.reported(), "running");
        let error = CallError::from_terminal(&json!({ "code": "X", "message": "boom" }));
        assert_eq!(error.code.as_deref(), Some("X"));
        assert_eq!(error.message, "boom");
        let nested = CallError::from_terminal(&json!({ "error": { "message": "m" } }));
        assert_eq!(nested.message, "m");
    }
}
