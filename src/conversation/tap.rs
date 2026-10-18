//! In-memory plaintext taps on a conversation's journal.
//!
//! History is sealed before it reaches the journal or the event hub, so a
//! daemon-side reader that needs a conversation's content (an agent waiting
//! for the agent it called, see `crate::agent_calls`) cannot decrypt it.
//! A tap gets each committed event's plaintext instead, right before it is
//! sealed, reduced by the tap's own projection to the little it needs.
//! Projected events only live in the tap's channel: they are never written,
//! published or logged, and unregistering (dropping the [`LiveTap`], or the
//! conversation being removed) discards whatever was not read.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use dashmap::{mapref::entry::Entry, DashMap};
use tokio::sync::mpsc;

use super::ConversationEvent;
use crate::error::AppError;

/// What a tap keeps of an event: `None` skips it.
pub type TapProjection = fn(&ConversationEvent) -> Option<ConversationEvent>;

struct Slot {
    id: u64,
    project: TapProjection,
    sender: mpsc::UnboundedSender<ConversationEvent>,
}

/// The taps of every conversation, at most one per conversation.
#[derive(Clone, Default)]
pub struct LiveTaps {
    slots: Arc<DashMap<String, Slot>>,
    next_id: Arc<AtomicU64>,
    /// Test hook: conversations whose deliveries are lost.
    #[cfg(test)]
    muted: Arc<DashMap<String, ()>>,
}

impl LiveTaps {
    /// Taps `conversation_id` from its next committed event on. A
    /// conversation has one reader at a time: while another tap of it is
    /// live this is refused (`CONFLICT`) rather than cutting that reader
    /// off; a tap whose reader is gone is replaced.
    pub fn register(
        &self,
        conversation_id: &str,
        project: TapProjection,
    ) -> Result<LiveTap, AppError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        let slot = Slot {
            id,
            project,
            sender,
        };
        match self.slots.entry(conversation_id.to_owned()) {
            Entry::Occupied(mut entry) if entry.get().sender.is_closed() => {
                entry.insert(slot);
            }
            Entry::Occupied(_) => {
                return Err(AppError::Conflict(format!(
                    "conversation {conversation_id} is already being followed"
                )));
            }
            Entry::Vacant(entry) => {
                entry.insert(slot);
            }
        }
        Ok(LiveTap {
            taps: self.clone(),
            conversation_id: conversation_id.to_owned(),
            id,
            receiver,
        })
    }

    /// The projection of `events` for the conversation's tap, or `None`
    /// when it has none. Called with the plaintext events just before they
    /// are sealed.
    pub(super) fn project(
        &self,
        conversation_id: &str,
        events: &[ConversationEvent],
    ) -> Option<Vec<ConversationEvent>> {
        let slot = self.slots.get(conversation_id)?;
        Some(events.iter().filter_map(slot.project).collect())
    }

    /// Hands committed, projected events to the conversation's tap.
    pub(super) fn deliver(&self, conversation_id: &str, events: Vec<ConversationEvent>) {
        if events.is_empty() {
            return;
        }
        #[cfg(test)]
        if self.muted.contains_key(conversation_id) {
            return;
        }
        let closed = match self.slots.get(conversation_id) {
            Some(slot) => events
                .into_iter()
                .any(|event| slot.sender.send(event).is_err()),
            None => false,
        };
        if closed {
            self.slots
                .remove_if(conversation_id, |_, slot| slot.sender.is_closed());
        }
    }

    /// Ends the conversation's tap (the conversation was removed).
    pub(super) fn remove(&self, conversation_id: &str) {
        self.slots.remove(conversation_id);
    }

    /// Loses every later delivery for `conversation_id`.
    #[cfg(all(test, unix))]
    pub(crate) fn mute(&self, conversation_id: &str) {
        self.muted.insert(conversation_id.to_owned(), ());
    }

    #[cfg(test)]
    pub(crate) fn is_tapped(&self, conversation_id: &str) -> bool {
        self.slots.contains_key(conversation_id)
    }
}

/// One conversation's tapped events, in journal order. Dropping it
/// unregisters the tap.
pub struct LiveTap {
    taps: LiveTaps,
    conversation_id: String,
    id: u64,
    receiver: mpsc::UnboundedReceiver<ConversationEvent>,
}

impl LiveTap {
    /// The events delivered since the last call, waiting for one when there
    /// are none; `None` once the tap was removed. Cancel-safe.
    pub async fn recv_batch(&mut self) -> Option<Vec<ConversationEvent>> {
        let first = self.receiver.recv().await?;
        let mut batch = vec![first];
        while let Ok(event) = self.receiver.try_recv() {
            batch.push(event);
        }
        Some(batch)
    }
}

impl Drop for LiveTap {
    fn drop(&mut self) {
        let id = self.id;
        self.taps
            .slots
            .remove_if(&self.conversation_id, |_, slot| slot.id == id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keep_deltas(event: &ConversationEvent) -> Option<ConversationEvent> {
        (event.event_type == "message.delta").then(|| event.clone())
    }

    #[tokio::test]
    async fn taps_project_deliver_and_unregister() {
        let taps = LiveTaps::default();
        let events = vec![
            ConversationEvent::new("c1", 1, "message.delta", json!({ "delta": "a" })),
            ConversationEvent::new("c1", 2, "tool.started", json!({})),
        ];
        assert!(taps.project("c1", &events).is_none());
        let mut tap = taps.register("c1", keep_deltas).unwrap();
        let projected = taps.project("c1", &events).unwrap();
        assert_eq!(projected.len(), 1);
        taps.deliver("c1", projected);
        let batch = tap.recv_batch().await.unwrap();
        assert_eq!(batch[0].payload["delta"], "a");

        // A live tap is never cut off by another reader.
        assert!(matches!(
            taps.register("c1", keep_deltas),
            Err(AppError::Conflict(_))
        ));
        taps.deliver("c1", taps.project("c1", &events).unwrap());
        assert_eq!(tap.recv_batch().await.unwrap().len(), 1);
        // Once it is dropped the next reader gets the conversation.
        drop(tap);
        assert!(!taps.is_tapped("c1"));
        let replacement = taps.register("c1", keep_deltas).unwrap();
        assert!(taps.is_tapped("c1"));
        drop(replacement);
        assert!(!taps.is_tapped("c1"));
    }
}
