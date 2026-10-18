//! Where a caller reads a conversation's events from, and how a turn's
//! end and assistant text are recognized in them.
//!
//! [`EventPump`] decrypts the journal and the live hub for an API key. A
//! caller that cannot decrypt (an agent calling another agent) reads a
//! [`LiveTap`] instead: the plaintext of the events it needs, taken in
//! memory right before they are sealed (see [`tap_projection`]).

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use super::ApiKeyCaller;
use crate::app_state::AppState;
use crate::conversation::server_decrypt::HistoryReader;
use crate::conversation::{
    ConversationEvent, ConversationSubscription, LiveTap, ReplayDetail,
    CONVERSATION_TERMINAL_EVENTS, TURN_TERMINAL_EVENTS,
};
use crate::error::AppError;

const REPLAY_PAGE: usize = 200;

/// A conversation's events in sequence order, presented in plaintext.
#[async_trait]
pub(crate) trait TurnEvents: Send {
    /// The next events, waiting for live ones once caught up. Must be
    /// cancel-safe: a dropped call loses nothing.
    async fn next_batch(&mut self) -> Result<Vec<ConversationEvent>, AppError>;
}

/// One conversation's events after `cursor`, decrypted for an API key:
/// the journal first, then the hub. Subscribing before the first replay
/// page and skipping what the cursor already passed means nothing is lost
/// or repeated; a gap or a lagged receiver falls back to the journal.
pub(crate) struct EventPump {
    state: AppState,
    key: ApiKeyCaller,
    conversation_id: String,
    reader: HistoryReader,
    cursor: u64,
    catching_up: bool,
    /// A live event received but not yet presented; see [`Self::next_batch`].
    pending: Option<ConversationEvent>,
    subscription: ConversationSubscription,
}

impl EventPump {
    pub(crate) async fn new(
        state: &AppState,
        key: &ApiKeyCaller,
        conversation_id: &str,
        after: u64,
    ) -> Result<Self, AppError> {
        state
            .conversations
            .get_owned(&key.owner_id(), conversation_id)
            .await?;
        let receiver = state.conversations.subscribe(conversation_id);
        let subscription = state.conversation_hub.track(conversation_id, receiver);
        Ok(Self {
            reader: key.reader(state, conversation_id),
            state: state.clone(),
            key: key.clone(),
            conversation_id: conversation_id.to_owned(),
            cursor: after,
            catching_up: true,
            pending: None,
            subscription,
        })
    }

    /// The next decrypted events, waiting for live ones once caught up.
    /// Cancel-safe: state only advances after the awaits that produce a
    /// batch, so a dropped call loses nothing.
    pub(crate) async fn next_batch(&mut self) -> Result<Vec<ConversationEvent>, AppError> {
        loop {
            if let Some(event) = self.pending.clone() {
                let sequence = event.sequence;
                let event = self.reader.present(event, &serde_json::Map::new()).await;
                self.pending = None;
                self.cursor = sequence;
                return Ok(vec![event]);
            }
            if self.catching_up {
                let replay = self
                    .state
                    .conversations
                    .replay_owned(
                        &self.key.owner_id(),
                        &self.conversation_id,
                        self.cursor,
                        REPLAY_PAGE,
                        ReplayDetail::Full,
                    )
                    .await?;
                let caught_up = !replay.has_more || replay.events.is_empty();
                let mut cursor = self.cursor;
                let mut events = Vec::with_capacity(replay.events.len());
                for event in replay.events {
                    cursor = cursor.max(event.sequence);
                    events.push(self.reader.present(event, &replay.frames).await);
                }
                self.cursor = cursor;
                if caught_up {
                    self.catching_up = false;
                }
                if !events.is_empty() {
                    return Ok(events);
                }
                continue;
            }
            match self.subscription.recv().await {
                Ok(event) => {
                    if event.sequence <= self.cursor {
                        continue;
                    }
                    if event.sequence > self.cursor + 1 {
                        self.catching_up = true;
                        continue;
                    }
                    self.pending = Some(ConversationEvent::clone(&event));
                }
                Err(RecvError::Lagged(_)) => self.catching_up = true,
                Err(RecvError::Closed) => return Err(AppError::StreamClosed),
            }
        }
    }

    /// Whether the key behind this stream may still read it.
    pub(crate) fn key_active(&self) -> bool {
        self.state
            .api_keys
            .get(&self.key.record.id)
            .ok()
            .flatten()
            .is_some_and(|record| record.is_active(crate::api_keys::unix_ms()))
    }
}

#[async_trait]
impl TurnEvents for EventPump {
    async fn next_batch(&mut self) -> Result<Vec<ConversationEvent>, AppError> {
        EventPump::next_batch(self).await
    }
}

#[async_trait]
impl TurnEvents for LiveTap {
    async fn next_batch(&mut self) -> Result<Vec<ConversationEvent>, AppError> {
        self.recv_batch().await.ok_or(AppError::StreamClosed)
    }
}

/// What an agent call keeps of a tapped child event: the main agent's text
/// (`message.delta` reduced to `{turnId, delta}`), the end of a turn (its
/// type with `turnId` and the error fields), and permission requests and
/// answers (`{turnId, permissionId}`), so the caller can follow the turn and
/// report its state. Everything else, tool input and output included, is
/// dropped before it leaves the store.
pub(crate) fn tap_projection(event: &ConversationEvent) -> Option<ConversationEvent> {
    const ERROR_FIELDS: [&str; 4] = ["turnId", "code", "message", "error"];
    const PERMISSION_FIELDS: [&str; 2] = ["turnId", "permissionId"];
    let keep = |fields: &[&str]| {
        let mut payload = serde_json::Map::new();
        for field in fields {
            if let Some(value) = event.payload.get(*field) {
                payload.insert((*field).to_owned(), value.clone());
            }
        }
        Value::Object(payload)
    };
    let payload = if let Some(text) = assistant_text(event) {
        let mut payload = keep(&["turnId"]);
        payload["delta"] = Value::String(text.to_owned());
        payload
    } else if ends_turn(event, None) {
        keep(&ERROR_FIELDS)
    } else if matches!(
        event.event_type.as_str(),
        "permission.requested" | "permission.resolved"
    ) {
        keep(&PERMISSION_FIELDS)
    } else {
        return None;
    };
    let mut projected = ConversationEvent::new(
        &event.conversation_id,
        event.sequence,
        event.event_type.clone(),
        payload,
    );
    projected.time = event.time;
    Some(projected)
}

/// Whether `event` ends `turn_id` (or, without one, any turn).
pub(crate) fn ends_turn(event: &ConversationEvent, turn_id: Option<&str>) -> bool {
    if CONVERSATION_TERMINAL_EVENTS.contains(&event.event_type.as_str()) {
        return true;
    }
    TURN_TERMINAL_EVENTS.contains(&event.event_type.as_str())
        && turn_id.is_none_or(|turn_id| {
            event.payload.get("turnId").and_then(Value::as_str) == Some(turn_id)
        })
}

/// Assistant text carried by a `message.delta` of the main agent, across
/// the providers' payload shapes (Codex `delta`; Claude, Antigravity and Pi
/// `delta.text`/`delta.delta`; ACP `content.text`).
pub(crate) fn assistant_text(event: &ConversationEvent) -> Option<&str> {
    if event.event_type != "message.delta" {
        return None;
    }
    let payload = &event.payload;
    if payload.get("subagentId").is_some_and(|id| !id.is_null()) {
        return None;
    }
    if let Some(text) = payload.get("delta").and_then(Value::as_str) {
        return Some(text);
    }
    if payload.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta") {
        return payload
            .pointer("/delta/text")
            .or_else(|| payload.pointer("/delta/delta"))
            .and_then(Value::as_str);
    }
    if payload.pointer("/content/type").and_then(Value::as_str) == Some("text") {
        return payload.pointer("/content/text").and_then(Value::as_str);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_tap_keeps_only_text_turn_ends_and_permission_ids() {
        let event = |kind: &str, payload: Value| ConversationEvent::new("c", 3, kind, payload);
        let text = tap_projection(&event(
            "message.delta",
            json!({ "turnId": "t", "delta": { "type": "text_delta", "text": "hi" }, "extra": 1 }),
        ))
        .unwrap();
        assert_eq!(text.payload, json!({ "turnId": "t", "delta": "hi" }));
        assert_eq!(text.sequence, 3);
        assert_eq!(assistant_text(&text), Some("hi"));
        assert!(tap_projection(&event(
            "message.delta",
            json!({ "turnId": "t", "delta": "sub", "subagentId": "s" })
        ))
        .is_none());
        assert!(tap_projection(&event("tool.output", json!({ "output": "secret" }))).is_none());
        let failed = tap_projection(&event(
            "turn.failed",
            json!({ "turnId": "t", "code": "X", "message": "boom", "stopReason": "s" }),
        ))
        .unwrap();
        assert_eq!(
            failed.payload,
            json!({ "turnId": "t", "code": "X", "message": "boom" })
        );
        let asked = tap_projection(&event(
            "permission.requested",
            json!({ "turnId": "t", "permissionId": "p", "details": { "command": "rm" } }),
        ))
        .unwrap();
        assert_eq!(asked.payload, json!({ "turnId": "t", "permissionId": "p" }));
    }
}
