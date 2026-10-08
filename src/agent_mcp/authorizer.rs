//! Every question TodeX's own MCP tools ask a person: first-use grants,
//! per-app approvals, sensitive-action confirmations, and approvals the
//! conversation's permission mode requires.
//!
//! A prompt names who may answer ([`Answerer`]): any paired device (a
//! `permission.requested` like a provider's tool approval), or only the
//! person at the daemon's host (a native dialog there). Concurrent asks for
//! the same thing in one conversation ask once, except one-action
//! confirmations ([`Prompt::once`]), which each get their own answer; a
//! declined or unanswered
//! prompt is not shown again for [`DECLINE_BACKOFF`], so a looping agent
//! cannot flood anyone with dialogs. Cancelling the tool call ends the wait.
//!
//! None of this is a security boundary against software running as the
//! same OS user (which can read the provider's environment and reuse its
//! token); it keeps agents from acting without the user's say.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rmcp::{service::RequestContext, RoleServer};
use serde_json::{json, Value};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    agent_desktop::{AgentDesktop, KeyedLocks, HOST_DEVICE_ID},
    provider::{ConversationSupervisor, PermissionOutcome},
};

/// How long a prompt waits for its answer.
pub(super) const CONFIRM_TIMEOUT: Duration = Duration::from_secs(300);
/// After a decline (or no answer) the same prompt fails fast this long.
pub(super) const DECLINE_BACKOFF: Duration = Duration::from_secs(30);
/// A host dialog answered after every caller gave up still counts for the
/// next identical prompt within this long.
const LATE_ANSWER_TTL: Duration = Duration::from_secs(60);

/// Who may answer a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answerer {
    /// The person at the daemon's host, in a native dialog there.
    Host,
    /// Any paired device of the owner (`permission.requested`).
    AnyDevice,
}

/// How a conversation's permission mode treats tools with side effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolMode {
    /// Plan work: refused.
    Plan,
    /// Manual approval: each call is approved like a provider tool.
    Ask,
    /// `auto` / `full-access`: no extra approval.
    Unrestricted,
}

impl ToolMode {
    /// From a turn's effective (product-level) permission config.
    pub(crate) fn from_turn(permission_mode: &str, work_mode: &str) -> Self {
        if work_mode == "plan" {
            return Self::Plan;
        }
        match permission_mode {
            "auto" | "full-access" => Self::Unrestricted,
            // `ask`, and anything unknown, asks.
            _ => Self::Ask,
        }
    }
}

/// A refused or unanswered prompt, reported to the agent as `CODE: message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Denied {
    pub code: &'static str,
    pub message: String,
}

impl Denied {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Replaces the message of a plain decline (not backoff, timeout, ...).
    pub(super) fn or_declined(mut self, message: impl Into<String>) -> Self {
        if self.code == "DECLINED" && !self.message.contains("not ask again") {
            self.message = message.into();
        }
        self
    }
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A yes from someone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Approval {
    /// The answering device ([`HOST_DEVICE_ID`] for the host).
    pub device_id: String,
    /// "Allow for this conversation" rather than once.
    pub always: bool,
}

/// One question.
pub(super) struct Prompt {
    /// Identifies "the same question" within a conversation, for
    /// deduplication and backoff, e.g. `browser-grant` or `app:<id>`.
    pub key: String,
    pub answerer: Answerer,
    /// `permission.requested` kind ([`Answerer::AnyDevice`]).
    pub kind: &'static str,
    pub title: String,
    /// The host dialog's text ([`Answerer::Host`]).
    pub message: String,
    /// `permission.requested` details ([`Answerer::AnyDevice`]).
    pub details: Value,
    /// `permission.requested` options ([`Answerer::AnyDevice`]).
    pub options: Value,
    /// Approves one action only (see [`action_key`]): its host dialog is
    /// never shared with another caller and an answer nobody waited for
    /// is dropped instead of kept for the next ask.
    pub once: bool,
}

/// The key of a one-action prompt: `base` plus a digest of what exactly
/// would run, so declines back off per action and no other action can be
/// mistaken for this one.
pub(super) fn action_key(base: &str, action: &Value) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(action.to_string().as_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{base}:{hex}")
}

/// The tool call was abandoned (the client cancelled the request).
#[derive(Clone)]
pub(super) struct CancelSignal(watch::Receiver<bool>);

impl CancelSignal {
    /// Follows the request's cancellation token until the signal is dropped.
    pub(super) fn from_context(context: &RequestContext<RoleServer>) -> Self {
        let (sender, receiver) = watch::channel(false);
        let token = context.ct.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = token.cancelled() => { let _ = sender.send(true); }
                () = sender.closed() => {}
            }
        });
        Self(receiver)
    }

    #[cfg(test)]
    pub(super) fn manual() -> (watch::Sender<bool>, Self) {
        let (sender, receiver) = watch::channel(false);
        (sender, Self(receiver))
    }

    pub(super) fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub(super) async fn cancelled(&self) {
        let mut receiver = self.0.clone();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                // Nobody can cancel any more.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// What a host dialog came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostAnswer {
    Allowed,
    Declined,
    /// Nobody can be asked on this computer.
    Nobody,
    /// Asking failed.
    Failed,
}

impl HostAnswer {
    /// Whether the person at the host decided: only such an answer is kept
    /// for the next ask when nobody was waiting.
    fn is_decision(self) -> bool {
        matches!(self, Self::Allowed | Self::Declined)
    }
}

/// Daemon-wide prompt state, shared by every TodeX MCP server.
#[derive(Default)]
pub(crate) struct AuthorizerState {
    /// Conversation → the permission mode of its running turn.
    modes: Mutex<HashMap<String, ToolMode>>,
    /// Conversation → `server.tool` allowed for the rest of it (ask mode).
    always: Mutex<HashMap<String, HashSet<String>>>,
    /// (conversation, prompt key) → when it was last declined.
    declined: Mutex<HashMap<(String, String), Instant>>,
    /// (conversation, prompt key) → lock, so a question is asked once.
    asking: KeyedLocks,
    /// (conversation, prompt key) → the host dialog on screen now. Native
    /// dialogs cannot be withdrawn; a caller that gives up leaves it here
    /// and the next caller waits on it instead of stacking another.
    host_dialogs: Mutex<HashMap<String, watch::Receiver<Option<HostAnswer>>>>,
    /// (conversation, prompt key) → a host answer nobody waited for.
    late_answers: Mutex<HashMap<String, (Instant, HostAnswer)>>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn prompt_key(conversation_id: &str, key: &str) -> String {
    format!("{conversation_id}\u{0}{key}")
}

impl AuthorizerState {
    pub(crate) fn set_mode(&self, conversation_id: &str, mode: ToolMode) {
        locked(&self.modes).insert(conversation_id.to_owned(), mode);
    }

    pub(crate) fn clear_mode(&self, conversation_id: &str) {
        locked(&self.modes).remove(conversation_id);
    }

    /// `None` before the conversation's first turn in this daemon (only
    /// tests call tools then: providers are started by turns).
    pub(crate) fn mode(&self, conversation_id: &str) -> Option<ToolMode> {
        locked(&self.modes).get(conversation_id).copied()
    }

    /// The mode side-effect tools follow. No running turn asks, like an
    /// unknown mode: unknown state never skips approval.
    pub(crate) fn tool_mode(&self, conversation_id: &str) -> ToolMode {
        self.mode(conversation_id).unwrap_or(ToolMode::Ask)
    }

    /// Conversation deleted, revoked or expired.
    pub(crate) fn forget(&self, conversation_id: &str) {
        locked(&self.modes).remove(conversation_id);
        locked(&self.always).remove(conversation_id);
        locked(&self.declined).retain(|(conversation, _), _| conversation != conversation_id);
        self.forget_host_answers(conversation_id);
    }

    /// Voids the host answers given to nobody in particular (kept for the
    /// next identical ask): a "yes" given before a revoke must not
    /// authorize anything after it. Declines keep backing off, and an
    /// answer given after this call stays valid.
    pub(crate) fn forget_host_answers(&self, conversation_id: &str) {
        let prefix = prompt_key(conversation_id, "");
        locked(&self.late_answers).retain(|key, _| !key.starts_with(&prefix));
    }

    #[cfg(test)]
    pub(crate) fn has_late_answer(&self, conversation_id: &str, key: &str) -> bool {
        locked(&self.late_answers).contains_key(&prompt_key(conversation_id, key))
    }

    #[cfg(test)]
    pub(crate) fn clear_declines(&self, conversation_id: &str) {
        locked(&self.declined).retain(|(conversation, _), _| conversation != conversation_id);
    }

    /// Recently declined: the remaining backoff.
    fn backoff(&self, conversation_id: &str, key: &str) -> Option<Duration> {
        let mut declined = locked(&self.declined);
        declined.retain(|_, at| at.elapsed() < DECLINE_BACKOFF);
        declined
            .get(&(conversation_id.to_owned(), key.to_owned()))
            .map(|at| DECLINE_BACKOFF.saturating_sub(at.elapsed()))
    }

    fn record_decline(&self, conversation_id: &str, key: &str) {
        locked(&self.declined).insert((conversation_id.to_owned(), key.to_owned()), Instant::now());
    }
}

fn dialog_stays() -> Denied {
    Denied::new(
        "CANCELLED",
        "the call was cancelled; the dialog on the host stays until answered",
    )
}

/// The prompts of one MCP call.
pub(super) struct Authorizer<'a> {
    pub state: &'a Arc<AuthorizerState>,
    pub conversations: &'a ConversationSupervisor,
    pub desktop: &'a AgentDesktop,
}

impl Authorizer<'_> {
    /// Serializes prompts with the same key in a conversation; the caller
    /// re-checks whatever the prompt would establish once it holds this.
    pub(super) async fn serialize(
        &self,
        conversation_id: &str,
        key: &str,
        cancel: &CancelSignal,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, Denied> {
        let lock = self.state.asking.get(&prompt_key(conversation_id, key));
        tokio::select! {
            guard = lock.lock_owned() => Ok(guard),
            () = cancel.cancelled() => Err(Denied::new("CANCELLED", "the call was cancelled")),
        }
    }

    /// Asks `prompt`; declines and unanswered prompts start the backoff.
    pub(super) async fn ask(
        &self,
        conversation_id: &str,
        prompt: Prompt,
        cancel: &CancelSignal,
    ) -> Result<Approval, Denied> {
        if let Some(left) = self.state.backoff(conversation_id, &prompt.key) {
            return Err(Denied::new(
                "DECLINED",
                format!(
                    "the user just declined this; TodeX will not ask again for {}s. \
                     Do not retry it now; continue without it or ask the user in the chat.",
                    left.as_secs().max(1)
                ),
            ));
        }
        let key = prompt.key.clone();
        let outcome = match prompt.answerer {
            Answerer::Host => self.ask_host(conversation_id, prompt, cancel).await,
            Answerer::AnyDevice => self.ask_devices(conversation_id, prompt, cancel).await,
        };
        if let Err(denied) = &outcome {
            if matches!(denied.code, "DECLINED" | "TIMEOUT") {
                self.state.record_decline(conversation_id, &key);
            }
        }
        outcome
    }

    async fn ask_devices(
        &self,
        conversation_id: &str,
        prompt: Prompt,
        cancel: &CancelSignal,
    ) -> Result<Approval, Denied> {
        // Ends the request on the call's cancellation or the timeout.
        let (stop_tx, stop_rx) = watch::channel(false);
        let timer = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(CONFIRM_TIMEOUT) => {}
                    () = cancel.cancelled() => {}
                }
                let _ = stop_tx.send(true);
            })
        };
        let result = self
            .conversations
            .request_agent_permission(
                conversation_id,
                format!("agent_mcp_{}", Uuid::new_v4().simple()),
                prompt.kind,
                prompt.title,
                prompt.details,
                prompt.options,
                None,
                stop_rx,
            )
            .await;
        timer.abort();
        match result {
            Ok((decision, device_id)) => match decision.outcome {
                PermissionOutcome::AllowOnce => Ok(Approval {
                    device_id,
                    always: false,
                }),
                PermissionOutcome::AllowAlways => Ok(Approval {
                    device_id,
                    always: true,
                }),
                _ => Err(Denied::new("DECLINED", "the user declined")),
            },
            Err(_) if cancel.is_cancelled() => {
                Err(Denied::new("CANCELLED", "the call was cancelled"))
            }
            Err(crate::error::AppError::TurnCancelled) => Err(Denied::new(
                "TIMEOUT",
                format!(
                    "nobody answered within {} minutes, or the turn was cancelled",
                    CONFIRM_TIMEOUT.as_secs() / 60
                ),
            )),
            Err(error) => Err(Denied::new(
                "UNAVAILABLE",
                format!("could not ask the user: {error}"),
            )),
        }
    }

    async fn ask_host(
        &self,
        conversation_id: &str,
        prompt: Prompt,
        cancel: &CancelSignal,
    ) -> Result<Approval, Denied> {
        let key = prompt_key(conversation_id, &prompt.key);
        let late = if prompt.once {
            None
        } else {
            locked(&self.state.late_answers)
                .remove(&key)
                .filter(|(at, _)| at.elapsed() < LATE_ANSWER_TTL)
                .map(|(_, answer)| answer)
        };
        let answer = match late {
            Some(answer) => answer,
            None => {
                let mut answer = self.host_dialog(key, prompt, cancel).await?;
                let waited = tokio::select! {
                    result = answer.wait_for(Option::is_some) => result.map(|answer| *answer).ok().flatten(),
                    () = cancel.cancelled() => return Err(dialog_stays()),
                };
                // The dialog task always sends before it ends.
                waited.unwrap_or(HostAnswer::Failed)
            }
        };
        match answer {
            HostAnswer::Allowed => Ok(Approval {
                device_id: HOST_DEVICE_ID.to_owned(),
                always: false,
            }),
            HostAnswer::Declined => Err(Denied::new(
                "DECLINED",
                "the person at this computer declined",
            )),
            HostAnswer::Nobody => Err(Denied::new(
                "UNAVAILABLE",
                "nobody can confirm on this computer; the TodeX backend must run in its desktop session.",
            )),
            HostAnswer::Failed => Err(Denied::new(
                "FAILED",
                "the confirmation dialog on this computer failed; try again",
            )),
        }
    }

    /// The host dialog for `key`: the one already on screen, or a new one.
    /// A one-action prompt never takes over another caller's dialog: it
    /// waits until that one is answered (so dialogs do not stack), then
    /// shows its own.
    async fn host_dialog(
        &self,
        key: String,
        prompt: Prompt,
        cancel: &CancelSignal,
    ) -> Result<watch::Receiver<Option<HostAnswer>>, Denied> {
        loop {
            let mut on_screen = {
                let mut dialogs = locked(&self.state.host_dialogs);
                match dialogs.get(&key) {
                    Some(answer) if !prompt.once => return Ok(answer.clone()),
                    Some(answer) => answer.clone(),
                    None => {
                        let (sender, receiver) = watch::channel(None);
                        dialogs.insert(key.clone(), receiver.clone());
                        self.show_host_dialog(key, prompt, sender);
                        return Ok(receiver);
                    }
                }
            };
            tokio::select! {
                // Answered (or its task ended): look again.
                _ = on_screen.wait_for(Option::is_some) => {}
                () = cancel.cancelled() => return Err(dialog_stays()),
            }
        }
    }

    fn show_host_dialog(
        &self,
        key: String,
        prompt: Prompt,
        sender: watch::Sender<Option<HostAnswer>>,
    ) {
        let state = self.state.clone();
        let computer = self.desktop.computer();
        tokio::spawn(async move {
            let answer = match computer
                .host()
                .confirm(prompt.title, prompt.message, CONFIRM_TIMEOUT)
                .await
            {
                Ok(Some(true)) => HostAnswer::Allowed,
                Ok(Some(false)) => HostAnswer::Declined,
                Ok(None) => HostAnswer::Nobody,
                Err(error) => {
                    tracing::warn!(%error, "the host confirmation failed");
                    HostAnswer::Failed
                }
            };
            locked(&state.host_dialogs).remove(&key);
            if sender.send(Some(answer)).is_err() && !prompt.once && answer.is_decision() {
                // Everyone gave up waiting: keep it for the next ask.
                locked(&state.late_answers).insert(key, (Instant::now(), answer));
            }
        });
    }

    /// The gate for a tool with side effects, by the conversation's
    /// permission mode: refused in Plan, approved like a provider tool in
    /// ask mode, free otherwise. Returns the approving device, if one did.
    pub(super) async fn allow_side_effect(
        &self,
        conversation_id: &str,
        server: &str,
        tool: &str,
        summary: &str,
        input: Value,
        cancel: &CancelSignal,
    ) -> Result<Option<String>, Denied> {
        match self.state.tool_mode(conversation_id) {
            ToolMode::Unrestricted => Ok(None),
            ToolMode::Plan => Err(Denied::new(
                "PLAN_MODE",
                format!(
                    "{tool} changes things and is not available while the conversation is in Plan \
                     mode. Read-only tools still work; describe the step in your plan instead."
                ),
            )),
            ToolMode::Ask => {
                let qualified = format!("{server}.{tool}");
                let always = |state: &AuthorizerState| {
                    locked(&state.always)
                        .get(conversation_id)
                        .is_some_and(|tools| tools.contains(&qualified))
                };
                if always(self.state) {
                    return Ok(None);
                }
                let key = format!("tool:{qualified}");
                let _serial = self.serialize(conversation_id, &key, cancel).await?;
                if always(self.state) {
                    return Ok(None);
                }
                let approval = self
                    .ask(
                        conversation_id,
                        Prompt {
                            key,
                            answerer: Answerer::AnyDevice,
                            // The kind provider tool approvals use.
                            kind: "tool",
                            title: format!("Allow {tool}: {summary}?"),
                            message: String::new(),
                            details: json!({
                                "tool_name": format!("mcp__{server}__{tool}"),
                                "server": server,
                                "tool": tool,
                                "summary": summary,
                                "input": input,
                            }),
                            options: json!([
                                { "id": "allow_once", "kind": "allow_once", "name": "Allow once" },
                                { "id": "allow_always", "kind": "allow_always", "name": "Always allow in this conversation" },
                                { "id": "reject_once", "kind": "reject_once", "name": "Reject" }
                            ]),
                            once: false,
                        },
                        cancel,
                    )
                    .await
                    .map_err(|denied| denied.or_declined(format!("the user rejected {tool}")))?;
                if approval.always {
                    locked(&self.state.always)
                        .entry(conversation_id.to_owned())
                        .or_default()
                        .insert(qualified);
                }
                Ok(Some(approval.device_id))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_modes_map_to_tool_modes() {
        assert_eq!(ToolMode::from_turn("ask", "plan"), ToolMode::Plan);
        assert_eq!(ToolMode::from_turn("full-access", "plan"), ToolMode::Plan);
        assert_eq!(ToolMode::from_turn("ask", "implement"), ToolMode::Ask);
        assert_eq!(ToolMode::from_turn("bogus", "implement"), ToolMode::Ask);
        assert_eq!(
            ToolMode::from_turn("auto", "implement"),
            ToolMode::Unrestricted
        );
        assert_eq!(
            ToolMode::from_turn("full-access", "implement"),
            ToolMode::Unrestricted
        );
    }

    #[test]
    fn declines_back_off_per_conversation_and_key_and_are_forgotten() {
        let state = AuthorizerState::default();
        state.record_decline("c1", "app:x");
        assert!(state.backoff("c1", "app:x").is_some());
        assert!(state.backoff("c1", "app:y").is_none());
        assert!(state.backoff("c2", "app:x").is_none());
        state.set_mode("c1", ToolMode::Plan);
        state.forget("c1");
        assert!(state.backoff("c1", "app:x").is_none());
        assert_eq!(state.mode("c1"), None);
        assert_eq!(state.tool_mode("c1"), ToolMode::Ask);
    }

    #[test]
    fn a_plain_decline_message_can_be_replaced_but_not_a_backoff() {
        let plain = Denied::new("DECLINED", "the user declined").or_declined("no browser");
        assert_eq!(plain.to_string(), "DECLINED: no browser");
        let backoff = Denied::new("DECLINED", "TodeX will not ask again for 3s").or_declined("x");
        assert!(backoff.message.contains("not ask again"));
        let cancelled = Denied::new("CANCELLED", "c").or_declined("x");
        assert_eq!(cancelled.message, "c");
    }
}
