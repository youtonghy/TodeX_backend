//! Decrypted event delivery for the API: replay from a cursor, then follow
//! the live hub, as JSON pages or as Server-Sent Events.

use std::{convert::Infallible, time::Duration};

use axum::response::sse::{Event, KeepAlive, Sse};
use serde_json::Value;
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use super::ApiKeyContext;
use crate::app_state::AppState;
use crate::conversation::server_decrypt::HistoryReader;
use crate::conversation::{
    ConversationEvent, ConversationSubscription, ReplayDetail, CONVERSATION_TERMINAL_EVENTS,
    TURN_TERMINAL_EVENTS,
};
use crate::error::AppError;

const REPLAY_PAGE: usize = 200;
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// How often an open stream rechecks that its key is still active.
const KEY_RECHECK: Duration = Duration::from_secs(5);
const STREAM_BUFFER: usize = 64;

/// One conversation's events after `cursor`, decrypted for `key`: the
/// journal first, then the hub. Subscribing before the first replay page
/// and skipping what the cursor already passed means nothing is lost or
/// repeated; a gap or a lagged receiver falls back to the journal.
pub(super) struct EventPump {
    state: AppState,
    key: ApiKeyContext,
    conversation_id: String,
    reader: HistoryReader,
    cursor: u64,
    catching_up: bool,
    /// A live event received but not yet presented; see [`Self::next_batch`].
    pending: Option<ConversationEvent>,
    subscription: ConversationSubscription,
}

impl EventPump {
    pub(super) async fn new(
        state: &AppState,
        key: &ApiKeyContext,
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
            reader: HistoryReader::new(
                state.history_keys.clone(),
                conversation_id,
                key.seed.clone(),
            ),
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
    pub(super) async fn next_batch(&mut self) -> Result<Vec<ConversationEvent>, AppError> {
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
    fn key_active(&self) -> bool {
        self.state
            .api_keys
            .get(&self.key.record.id)
            .ok()
            .flatten()
            .is_some_and(|record| record.is_active(crate::api_keys::unix_ms()))
    }
}

/// Whether `event` ends `turn_id` (or, without one, any turn).
pub(super) fn ends_turn(event: &ConversationEvent, turn_id: Option<&str>) -> bool {
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
pub(super) fn assistant_text(event: &ConversationEvent) -> Option<&str> {
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

fn sse_event(event: &ConversationEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event.event_type.clone())
        .json_data(event)
        .unwrap_or_else(|_| Event::default().comment("unserializable event"))
}

fn sse_error(error: &AppError) -> Event {
    Event::default()
        .event("error")
        .json_data(serde_json::json!({ "code": error.code(), "message": error.to_string() }))
        .unwrap_or_else(|_| Event::default().comment("error"))
}

/// Streams `pump` as SSE until the client leaves, the key stops being
/// active, or — with `until_turn` — that turn ends.
pub(super) fn sse(
    mut pump: EventPump,
    until_turn: Option<Option<String>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
    tokio::spawn(async move {
        let mut recheck = tokio::time::interval(KEY_RECHECK);
        recheck.tick().await;
        loop {
            tokio::select! {
                batch = pump.next_batch() => {
                    let events = match batch {
                        Ok(events) => events,
                        Err(error) => {
                            let _ = sender.send(Ok(sse_error(&error))).await;
                            return;
                        }
                    };
                    for event in &events {
                        if sender.send(Ok(sse_event(event))).await.is_err() {
                            return;
                        }
                        if let Some(turn_id) = &until_turn {
                            if ends_turn(event, turn_id.as_deref()) {
                                return;
                            }
                        }
                    }
                }
                _ = recheck.tick() => {
                    if !pump.key_active() {
                        let _ = sender.send(Ok(sse_error(&AppError::Unauthenticated))).await;
                        return;
                    }
                }
                _ = sender.closed() => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(receiver)).keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
}
