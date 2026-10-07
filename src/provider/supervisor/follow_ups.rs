//! Backend follow-up queue: prompts submitted while a conversation is busy.
//!
//! The queue lives beside the conversation journal (`queue.json`) and holds
//! complete prompt requests, attachments and skills included, for every
//! provider. A turn that completes starts the head item; a turn that fails,
//! is cancelled, or is interrupted pauses the queue until a client resumes
//! it, and so does a daemon restart. A turn that failed on an exhausted plan
//! window instead gets a continuation prompt at the head and the queue
//! resumes on its own once the window resets, at least a minute later; a
//! continuation that keeps hitting the limit backs off and gives up after
//! [`MAX_RATE_LIMIT_CONTINUATIONS`] tries. Nothing starts once the daemon is
//! shutting down. Clients learn the queue through `followups.updated` events
//! and the list command; events never carry inline image data.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::{prepare_prompt_content, ConversationPrompt, ConversationSupervisor};
use crate::error::AppError;

/// Items one conversation may hold, matching the clients' former local cap.
pub(super) const MAX_FOLLOW_UP_ITEMS: usize = 32;
/// Serialized size cap for one conversation's queue (inline images count).
pub(super) const MAX_FOLLOW_UP_QUEUE_BYTES: usize = 32 * 1024 * 1024;
const FOLLOW_UP_QUEUE_SCHEMA_VERSION: u32 = 1;
pub(super) const FOLLOW_UP_QUEUE_EVENT: &str = "followups.updated";
/// Pause reason of a queue waiting for a provider plan window to reset.
const RATE_LIMITED: &str = "rate_limited";
/// Item id prefix of the prompt that continues a rate-limited turn.
const RATE_LIMIT_CONTINUE_PREFIX: &str = "rate-limit-continue-";
/// Model-facing, so it stays English whatever the client locale: the
/// interrupted request already sits in the provider transcript, so this
/// resumes it rather than repeating it.
const RATE_LIMIT_CONTINUE_TEXT: &str = "The previous request was interrupted by a provider usage limit before it could finish. Continue where it left off and complete the task.";
/// Longest single sleep while waiting for a reset. Monotonic timers stop
/// while the machine sleeps, so the wall clock is re-read at least this often.
const RATE_LIMIT_RECHECK: Duration = Duration::from_secs(30);
/// Shortest wait before a continuation runs, whatever reset the provider
/// reported: a reset already in the past (clock skew, a rounded wall-clock
/// time) must not start a turn that fails again right away. Each further
/// consecutive continuation that hits the limit doubles it.
pub(super) const RATE_LIMIT_RETRY_FLOOR: Duration = Duration::from_secs(60);
/// Consecutive continuations that may fail on the limit before the queue
/// stops continuing on its own.
const MAX_RATE_LIMIT_CONTINUATIONS: u32 = 3;
/// Pauses a rate-limit continuation must not override: they wait for the
/// user, so the continuation joins the queue without lifting them.
const USER_HELD_PAUSES: &[&str] = &["turn_cancelled", "start_failed", "daemon_restarted"];

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FollowUpQueue {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    items: Vec<FollowUpItem>,
    #[serde(default)]
    paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause_message: Option<String>,
    /// When a `rate_limited` pause lifts by itself. Also kept, for display
    /// only, when a continuation was queued under a pause the user holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_at: Option<DateTime<Utc>>,
    /// Consecutive continuation turns that failed on an exhausted plan
    /// window. Persisted so a restart does not restart the backoff; cleared
    /// when a turn completes.
    #[serde(default, skip_serializing_if = "is_zero")]
    rate_limit_failures: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowUpItem {
    id: String,
    queued_at: DateTime<Utc>,
    /// Carries `client_request_id == id`, so a start replayed after a crash
    /// is recognized by the prompt path instead of running twice.
    request: ConversationPrompt,
}

/// Outcome of adding to the queue: either it waits, or the conversation was
/// idle with nothing ahead of it and the prompt started right away.
#[derive(Debug, PartialEq, Eq)]
pub enum FollowUpAddOutcome {
    Queued,
    Started(String),
}

impl FollowUpQueue {
    /// Client view: no inline image bytes, only what a queue row shows.
    fn snapshot(&self) -> Value {
        let items = self
            .items
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "text": item.request.text,
                    "status": "queued",
                    "queuedAt": item.queued_at,
                    "contentCount": item.request.content.len(),
                    "skills": item.request.skills.iter()
                        .map(|skill| skill.name.clone().unwrap_or_else(|| skill.resource_id.clone()))
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "items": items,
            "paused": self.paused,
            "pauseReason": self.pause_reason,
            "pauseMessage": self.pause_message,
            "resumeAt": self.resume_at,
        })
    }

    fn pause(&mut self, reason: &str, message: Option<String>) {
        self.paused = true;
        self.pause_reason = Some(reason.to_owned());
        self.pause_message = message.map(|text| text.chars().take(1000).collect());
    }

    fn unpause(&mut self) {
        self.paused = false;
        self.pause_reason = None;
        self.pause_message = None;
        self.resume_at = None;
    }

    fn rate_limited(&self) -> bool {
        self.paused && self.pause_reason.as_deref() == Some(RATE_LIMITED)
    }

    fn held_by_user(&self) -> bool {
        self.paused
            && self
                .pause_reason
                .as_deref()
                .is_some_and(|reason| USER_HELD_PAUSES.contains(&reason))
    }

    /// The next item starts as soon as the conversation is idle.
    fn pending(&self) -> bool {
        !self.paused && !self.items.is_empty()
    }
}

/// When a continuation may run: the provider's reset, but no sooner than the
/// floor after `now`, doubled for every consecutive continuation beyond the
/// first that failed on the limit.
pub(super) fn rate_limit_resume_at(
    reset: DateTime<Utc>,
    now: DateTime<Utc>,
    failures: u32,
    floor: Duration,
) -> DateTime<Utc> {
    let factor = 1u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let wait =
        chrono::Duration::from_std(floor.saturating_mul(factor)).unwrap_or(chrono::Duration::MAX);
    reset.max(
        now.checked_add_signed(wait)
            .unwrap_or(DateTime::<Utc>::MAX_UTC),
    )
}

fn validate_item_id(item_id: &str) -> Result<(), AppError> {
    if item_id.is_empty() || item_id.len() > 200 {
        return Err(AppError::InvalidRequest(
            "itemId must contain 1 to 200 bytes".to_owned(),
        ));
    }
    Ok(())
}

impl ConversationSupervisor {
    async fn load_follow_ups(&self, conversation_id: &str) -> Result<FollowUpQueue, AppError> {
        match self.store.follow_up_queue(conversation_id).await? {
            Some(value) => Ok(serde_json::from_value(value)?),
            None => Ok(FollowUpQueue::default()),
        }
    }

    /// Persists, then publishes the client snapshot. Callers hold the
    /// conversation's request gate.
    async fn save_follow_ups(
        &self,
        conversation_id: &str,
        queue: &mut FollowUpQueue,
    ) -> Result<(), AppError> {
        queue.schema_version = FOLLOW_UP_QUEUE_SCHEMA_VERSION;
        let value = serde_json::to_value(&*queue)?;
        let bytes = serde_json::to_vec(&value)?.len();
        if bytes > MAX_FOLLOW_UP_QUEUE_BYTES {
            return Err(AppError::ResourceExhausted(format!(
                "the follow-up queue would hold {bytes} bytes, above the {MAX_FOLLOW_UP_QUEUE_BYTES} byte limit"
            )));
        }
        self.store
            .save_follow_up_queue(conversation_id, &value)
            .await?;
        if queue.pending() {
            self.pending_follow_ups.insert(conversation_id.to_owned());
        } else {
            self.pending_follow_ups.remove(conversation_id);
        }
        self.emit(conversation_id, FOLLOW_UP_QUEUE_EVENT, queue.snapshot())
            .await
    }

    /// Whether the journal already recorded a user message for this request.
    async fn delivered_turn(
        &self,
        conversation_id: &str,
        request_id: &str,
    ) -> Result<Option<String>, AppError> {
        // The newest such message decides, as a resubmission records anew.
        self.store
            .digest(conversation_id, |digest| {
                digest
                    .client_request(request_id)
                    .and_then(|facts| facts.last_turn_id.clone())
            })
            .await
    }

    /// Queues `prompt` behind the running turn, or starts it when the
    /// conversation is idle and nothing waits ahead of it. Re-adding an id
    /// that is queued or already delivered is a no-op reporting its state.
    pub async fn queue_add_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        item_id: &str,
        mut prompt: ConversationPrompt,
        front: bool,
    ) -> Result<FollowUpAddOutcome, AppError> {
        validate_item_id(item_id)?;
        // Legacy plaintext history is read-only: refused before queue.json
        // is touched.
        let manifest = self.writable_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        prompt.client_request_id = Some(item_id.to_owned());
        let mut queue = self.load_follow_ups(conversation_id).await?;
        if queue.items.iter().any(|item| item.id == item_id) {
            return Ok(FollowUpAddOutcome::Queued);
        }
        if let Some(turn_id) = self.delivered_turn(conversation_id, item_id).await? {
            return Ok(FollowUpAddOutcome::Started(turn_id));
        }
        let busy = self.active.contains_key(conversation_id);
        if !busy && queue.items.is_empty() {
            return self
                .prompt_inner(owner_id, conversation_id, prompt)
                .await
                .map(FollowUpAddOutcome::Started);
        }
        if queue.items.len() >= MAX_FOLLOW_UP_ITEMS {
            return Err(AppError::ResourceExhausted(format!(
                "the follow-up queue holds at most {MAX_FOLLOW_UP_ITEMS} items"
            )));
        }
        // Reject what could never start now, rather than when it is reached.
        let text = prompt.text.trim();
        if text.is_empty() && prompt.skills.is_empty() && prompt.content.is_empty() {
            return Err(AppError::InvalidRequest(
                "prompt cannot be empty".to_owned(),
            ));
        }
        if text.len() > super::MAX_PROMPT_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "prompt exceeds {} bytes",
                super::MAX_PROMPT_BYTES
            )));
        }
        prepare_prompt_content(
            manifest.provider,
            &manifest.workspace,
            prompt.content.clone(),
        )
        .await?;
        self.load_prompt_skills(&manifest, &prompt.skills).await?;
        let item = FollowUpItem {
            id: item_id.to_owned(),
            queued_at: Utc::now(),
            request: prompt,
        };
        if front {
            queue.items.insert(0, item);
        } else {
            queue.items.push(item);
        }
        self.save_follow_ups(conversation_id, &mut queue).await?;
        // An idle conversation with a paused queue keeps waiting for resume;
        // an idle one that is not paused (a turn ended while this request
        // waited for the gate) starts the head now.
        if !busy && !queue.paused {
            drop(_request_guard);
            self.drain_follow_ups(conversation_id).await;
        }
        Ok(FollowUpAddOutcome::Queued)
    }

    pub async fn queue_remove_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        item_id: &str,
    ) -> Result<Value, AppError> {
        validate_item_id(item_id)?;
        self.get_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let mut queue = self.load_follow_ups(conversation_id).await?;
        let before = queue.items.len();
        queue.items.retain(|item| item.id != item_id);
        if queue.items.len() == before {
            return Err(AppError::NotFound(format!("follow-up {item_id}")));
        }
        if queue.items.is_empty() {
            queue.unpause();
        }
        self.save_follow_ups(conversation_id, &mut queue).await?;
        Ok(queue.snapshot())
    }

    pub async fn queue_clear_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Value, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let mut queue = self.load_follow_ups(conversation_id).await?;
        queue.items.clear();
        queue.unpause();
        self.save_follow_ups(conversation_id, &mut queue).await?;
        Ok(queue.snapshot())
    }

    /// Lifts a pause; an idle conversation starts its head item right away.
    pub async fn queue_resume_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Value, AppError> {
        self.writable_owned(owner_id, conversation_id).await?;
        let snapshot = {
            let _request_guard = self.request_gate(conversation_id).lock_owned().await;
            let mut queue = self.load_follow_ups(conversation_id).await?;
            if queue.paused {
                queue.unpause();
                self.save_follow_ups(conversation_id, &mut queue).await?;
            }
            queue.snapshot()
        };
        self.drain_follow_ups(conversation_id).await;
        Ok(snapshot)
    }

    pub async fn queue_list_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Value, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        Ok(self.load_follow_ups(conversation_id).await?.snapshot())
    }

    /// Runs [`Self::after_turn`] on its own task. Turn tasks call this
    /// rather than awaiting it: the next prompt spawns another turn task, and
    /// awaiting it inline would make the turn future's type recursive.
    pub(super) fn schedule_after_turn(
        &self,
        conversation_id: String,
        turn_id: Option<String>,
        terminal: &'static str,
    ) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            supervisor
                .after_turn(&conversation_id, turn_id.as_deref(), terminal)
                .await
        });
    }

    /// Called once a turn or operation released the conversation. A
    /// completed turn or finished compaction advances the queue; a turn that
    /// failed on an exhausted plan window waits for its reset; other turn
    /// endings pause it.
    async fn after_turn(&self, conversation_id: &str, turn_id: Option<&str>, terminal: &str) {
        let exhausted_until = turn_id.and_then(|turn_id| self.quota.take_turn_exhaustion(turn_id));
        if matches!(terminal, "turn.completed" | "compaction.finished") {
            if terminal == "turn.completed" {
                self.settle_rate_limit_after_completion(conversation_id)
                    .await;
            }
            self.drain_follow_ups(conversation_id).await;
            return;
        }
        if let (Some(turn_id), Some(until), "turn.failed") = (turn_id, exhausted_until, terminal) {
            self.wait_for_rate_limit_reset(conversation_id, turn_id, until)
                .await;
            return;
        }
        let reason = match terminal {
            "turn.cancelled" => "turn_cancelled",
            "turn.interrupted" => "turn_interrupted",
            _ => "turn_failed",
        };
        self.pause_follow_ups(conversation_id, reason, None).await;
    }

    async fn pause_follow_ups(&self, conversation_id: &str, reason: &str, message: Option<String>) {
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let result = async {
            let mut queue = self.load_follow_ups(conversation_id).await?;
            if queue.items.is_empty() || queue.paused {
                return Ok(());
            }
            queue.pause(reason, message);
            self.save_follow_ups(conversation_id, &mut queue).await
        }
        .await;
        if let Err(error) = result {
            tracing::error!(conversation_id, reason, error = %error, "failed to pause follow-up queue");
        }
    }

    /// Starts the head item when the conversation is idle and the queue is
    /// not paused. A start that fails keeps the item at the head and pauses
    /// the queue with the reason, so nothing is dropped silently.
    pub(super) async fn drain_follow_ups(&self, conversation_id: &str) {
        if self.is_shutting_down() {
            return;
        }
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        // Checked again under the gate: shutdown may have begun while this
        // waited for it.
        if self.is_shutting_down() || self.active.contains_key(conversation_id) {
            return;
        }
        let result = async {
            let mut queue = self.load_follow_ups(conversation_id).await?;
            let Some(head) = queue.items.first().cloned() else {
                return Ok(());
            };
            if queue.paused {
                return Ok(());
            }
            let manifest = self.store.get(conversation_id).await?;
            match self
                .prompt_inner(&manifest.owner_id, conversation_id, head.request)
                .await
            {
                Ok(_) => {
                    queue.items.retain(|item| item.id != head.id);
                    self.save_follow_ups(conversation_id, &mut queue).await
                }
                Err(error) => {
                    tracing::warn!(conversation_id, item_id = %head.id, error = %error, "queued follow-up could not start");
                    queue.pause("start_failed", Some(error.to_string()));
                    self.save_follow_ups(conversation_id, &mut queue).await
                }
            }
        }
        .await;
        if let Err(error) = result {
            tracing::error!(conversation_id, error = %error, "failed to advance follow-up queue");
        }
    }

    /// A restart interrupted whatever ran before it; queued items wait for
    /// an explicit resume instead of continuing on their own. A queue already
    /// waiting for a plan window keeps that wait and its timer is re-armed.
    pub(super) async fn restore_follow_ups_after_restart(&self, conversation_id: &str) {
        let resume_at = match self.load_follow_ups(conversation_id).await {
            Ok(queue) if queue.rate_limited() => queue.resume_at,
            Ok(_) => None,
            Err(error) => {
                tracing::error!(conversation_id, error = %error, "failed to read follow-up queue after restart");
                None
            }
        };
        match resume_at {
            Some(until) => self.spawn_rate_limit_timer(conversation_id, until),
            None => {
                self.pause_follow_ups(conversation_id, "daemon_restarted", None)
                    .await
            }
        }
    }

    /// `turn_id` failed because its provider plan window is exhausted until
    /// `until`. A continuation prompt joins the head of the queue (once,
    /// however often the limit is hit before the reset) and the queue pauses
    /// until it may run, when it resumes by itself. A pause the user holds
    /// (cancel, failed start, restart) stays: the continuation waits behind
    /// it with `resumeAt` shown but no timer. After
    /// [`MAX_RATE_LIMIT_CONTINUATIONS`] consecutive continuations hit the
    /// limit, no further one is queued and the queue pauses as failed.
    async fn wait_for_rate_limit_reset(
        &self,
        conversation_id: &str,
        turn_id: &str,
        until: DateTime<Utc>,
    ) {
        let result = async {
            let _request_guard = self.request_gate(conversation_id).lock_owned().await;
            let mut queue = self.load_follow_ups(conversation_id).await?;
            let failed_request = self.turn_request(conversation_id, turn_id).await?;
            let was_continuation = failed_request
                .as_ref()
                .and_then(|request| request.client_request_id.as_deref())
                .is_some_and(|id| id.starts_with(RATE_LIMIT_CONTINUE_PREFIX));
            queue.rate_limit_failures = if was_continuation {
                queue.rate_limit_failures.saturating_add(1)
            } else {
                0
            };
            let held = queue.held_by_user();
            if queue.rate_limit_failures >= MAX_RATE_LIMIT_CONTINUATIONS {
                if !held {
                    queue.pause(
                        "turn_failed",
                        Some(format!(
                            "The provider usage limit was still reached after {MAX_RATE_LIMIT_CONTINUATIONS} automatic continuations, so TodeX stopped continuing on its own. Resume the queue or send a message to try again."
                        )),
                    );
                    queue.resume_at = None;
                }
                self.save_follow_ups(conversation_id, &mut queue).await?;
                return Ok(None);
            }
            let resume_at = rate_limit_resume_at(
                until,
                Utc::now(),
                queue.rate_limit_failures,
                self.rate_limit_retry_floor,
            );
            if !queue
                .items
                .iter()
                .any(|item| item.id.starts_with(RATE_LIMIT_CONTINUE_PREFIX))
            {
                let item_id = format!("{RATE_LIMIT_CONTINUE_PREFIX}{turn_id}");
                let request = continuation_prompt(failed_request, &item_id);
                // The cap bounds client additions; this item replaces the
                // failed turn rather than adding work, so it may exceed it.
                queue.items.insert(
                    0,
                    FollowUpItem {
                        id: item_id,
                        queued_at: Utc::now(),
                        request,
                    },
                );
            }
            if !held {
                queue.pause(RATE_LIMITED, None);
            }
            queue.resume_at = Some(resume_at);
            self.save_follow_ups(conversation_id, &mut queue).await?;
            Ok::<_, AppError>((!held).then_some(resume_at))
        }
        .await;
        match result {
            Ok(Some(resume_at)) => self.spawn_rate_limit_timer(conversation_id, resume_at),
            Ok(None) => {}
            Err(error) => {
                tracing::error!(conversation_id, turn_id, error = %error, "failed to queue the rate-limit continuation");
                self.pause_follow_ups(conversation_id, "turn_failed", None)
                    .await;
            }
        }
    }

    /// The request that started `turn_id`, when it is the conversation's
    /// latest one.
    async fn turn_request(
        &self,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<Option<ConversationPrompt>, AppError> {
        let snapshot = self
            .store
            .last_request(conversation_id)
            .await?
            .filter(|snapshot| snapshot.get("turnId").and_then(Value::as_str) == Some(turn_id));
        match snapshot {
            Some(mut snapshot) => Ok(Some(serde_json::from_value(snapshot["request"].take())?)),
            None => Ok(None),
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn spawn_rate_limit_timer(&self, conversation_id: &str, until: DateTime<Utc>) {
        let supervisor = self.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            // `to_std` fails once the reset is in the past.
            while let Ok(remaining) = (until - Utc::now()).to_std() {
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(remaining.min(RATE_LIMIT_RECHECK)).await;
            }
            supervisor
                .resume_after_rate_limit(&conversation_id, until)
                .await;
        });
    }

    /// Lifts the pause armed for `until`. A queue that was resumed, cleared or
    /// re-armed for a later reset in the meantime is left alone.
    async fn resume_after_rate_limit(&self, conversation_id: &str, until: DateTime<Utc>) {
        // A shutdown leaves the wait persisted; recovery re-arms it.
        if self.is_shutting_down() {
            return;
        }
        let result = async {
            let _request_guard = self.request_gate(conversation_id).lock_owned().await;
            let mut queue = self.load_follow_ups(conversation_id).await?;
            if !queue.rate_limited() || queue.resume_at != Some(until) {
                return Ok(false);
            }
            queue.unpause();
            self.save_follow_ups(conversation_id, &mut queue).await?;
            Ok::<_, AppError>(true)
        }
        .await;
        match result {
            Ok(true) => self.drain_follow_ups(conversation_id).await,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(conversation_id, error = %error, "failed to resume the follow-up queue after a rate limit");
            }
        }
    }

    /// A turn completed, so the plan window is open again. A pending
    /// continuation would repeat work the user already went on with by hand:
    /// drop it and lift the wait. The pause left when continuations were
    /// given up is lifted too, and the continuation count starts over.
    async fn settle_rate_limit_after_completion(&self, conversation_id: &str) {
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let result = async {
            let mut queue = self.load_follow_ups(conversation_id).await?;
            let mut changed = false;
            let gave_up = queue.rate_limit_failures >= MAX_RATE_LIMIT_CONTINUATIONS
                && queue.paused
                && queue.pause_reason.as_deref() == Some("turn_failed");
            if queue.rate_limited() || gave_up {
                queue
                    .items
                    .retain(|item| !item.id.starts_with(RATE_LIMIT_CONTINUE_PREFIX));
                queue.unpause();
                changed = true;
            }
            if queue.rate_limit_failures != 0 {
                queue.rate_limit_failures = 0;
                changed = true;
            }
            if !changed {
                return Ok(());
            }
            self.save_follow_ups(conversation_id, &mut queue).await
        }
        .await;
        if let Err(error) = result {
            tracing::error!(conversation_id, error = %error, "failed to settle the rate-limit wait");
        }
    }
}

/// The failed turn's settings (model, effort, permissions) with its text
/// replaced by the continuation instruction. Attachments and skills are
/// already part of the provider transcript, so they are not sent again.
fn continuation_prompt(failed: Option<ConversationPrompt>, item_id: &str) -> ConversationPrompt {
    let mut prompt = failed.unwrap_or_else(|| ConversationPrompt {
        permission_mode: None,
        work_mode: None,
        client_request_id: None,
        text: String::new(),
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        content: Vec::new(),
        permission_profile: None,
        sandbox_mode: None,
        approval_policy: None,
    });
    prompt.client_request_id = Some(item_id.to_owned());
    RATE_LIMIT_CONTINUE_TEXT.clone_into(&mut prompt.text);
    prompt.skills.clear();
    prompt.content.clear();
    prompt
}
