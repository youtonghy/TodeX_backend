//! Backend follow-up queue: prompts submitted while a conversation is busy.
//!
//! The queue lives beside the conversation journal (`queue.json`) and holds
//! complete prompt requests, attachments and skills included, for every
//! provider. A turn that completes starts the head item; a turn that fails,
//! is cancelled, or is interrupted pauses the queue until a client resumes
//! it, and so does a daemon restart. Clients learn the queue through
//! `followups.updated` events and the list command; events never carry
//! inline image data.

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
    }
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
        self.emit(conversation_id, FOLLOW_UP_QUEUE_EVENT, queue.snapshot())
            .await
    }

    /// Whether the journal already recorded a user message for this request.
    async fn delivered_turn(
        &self,
        conversation_id: &str,
        request_id: &str,
    ) -> Result<Option<String>, AppError> {
        let history = self.store.complete_history(conversation_id).await?;
        Ok(history
            .iter()
            .rev()
            .find(|event| {
                event.event_type == "message.created"
                    && event.payload.get("clientRequestId").and_then(Value::as_str)
                        == Some(request_id)
            })
            .and_then(|event| event.payload.get("turnId").and_then(Value::as_str))
            .map(str::to_owned))
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
        let manifest = self.get_owned(owner_id, conversation_id).await?;
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
        self.get_owned(owner_id, conversation_id).await?;
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
    pub(super) fn schedule_after_turn(&self, conversation_id: String, terminal: &'static str) {
        let supervisor = self.clone();
        tokio::spawn(async move { supervisor.after_turn(&conversation_id, terminal).await });
    }

    /// Called once a turn or operation released the conversation. A
    /// completed turn or finished compaction advances the queue; other turn
    /// endings pause it.
    async fn after_turn(&self, conversation_id: &str, terminal: &str) {
        if matches!(terminal, "turn.completed" | "compaction.finished") {
            self.drain_follow_ups(conversation_id).await;
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
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        if self.active.contains_key(conversation_id) {
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
    /// an explicit resume instead of continuing on their own.
    pub(super) async fn pause_follow_ups_after_restart(&self, conversation_id: &str) {
        self.pause_follow_ups(conversation_id, "daemon_restarted", None)
            .await;
    }
}
