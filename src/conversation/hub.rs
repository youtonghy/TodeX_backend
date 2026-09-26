use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::broadcast;

use super::ConversationEvent;

/// Every event is journaled before it is published, so a lagging receiver
/// recovers from the journal; a small ring keeps the per-channel preallocation
/// cheap.
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Live fan-out of conversation events. A channel exists only while someone
/// listens: `subscribe` creates it, and it is reclaimed once its last receiver
/// is gone (see [`ConversationSubscription`] and `publish`).
#[derive(Clone, Default)]
pub struct ConversationEventHub {
    channels: Arc<DashMap<String, broadcast::Sender<ConversationEvent>>>,
}

impl ConversationEventHub {
    pub fn subscribe(&self, conversation_id: &str) -> broadcast::Receiver<ConversationEvent> {
        // Subscribe while holding the entry lock so `release` cannot remove
        // the channel between its creation and the receiver being counted.
        self.channels
            .entry(conversation_id.to_owned())
            .or_insert_with(|| broadcast::channel(DEFAULT_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Ties a receiver obtained from `subscribe` to its channel: dropping the
    /// returned subscription drops the receiver and reclaims the channel if
    /// no other receiver is left.
    pub fn track(
        &self,
        conversation_id: &str,
        receiver: broadcast::Receiver<ConversationEvent>,
    ) -> ConversationSubscription {
        ConversationSubscription {
            hub: self.clone(),
            conversation_id: conversation_id.to_owned(),
            receiver: Some(receiver),
        }
    }

    /// Drops the broadcast channel for a conversation. Existing receivers see
    /// `RecvError::Closed`, which lets websocket subscription tasks exit and
    /// release their per-connection slot. A concurrent subscribe simply
    /// recreates an empty channel.
    pub fn remove(&self, conversation_id: &str) {
        self.channels.remove(conversation_id);
    }

    /// Delivers to current receivers only; without any the event is dropped
    /// (it is already journaled) and no channel is created.
    pub fn publish(&self, event: ConversationEvent) {
        let Some(sender) = self
            .channels
            .get(&event.conversation_id)
            .map(|sender| sender.clone())
        else {
            return;
        };
        // Every receiver is gone (e.g. dropped without `track`): reclaim.
        if let Err(broadcast::error::SendError(event)) = sender.send(event) {
            self.release(&event.conversation_id);
        }
    }

    /// Removes the channel if it has no receivers. The count is checked under
    /// the shard lock that `subscribe` also takes, so a receiver created
    /// concurrently either keeps the channel alive or gets a fresh one.
    fn release(&self, conversation_id: &str) {
        self.channels
            .remove_if(conversation_id, |_, sender| sender.receiver_count() == 0);
    }

    #[cfg(test)]
    pub(crate) fn has_channel(&self, conversation_id: &str) -> bool {
        self.channels.contains_key(conversation_id)
    }
}

/// A receiver that reclaims its hub channel when dropped, whether its owner
/// finished, was aborted, or its connection closed.
pub struct ConversationSubscription {
    hub: ConversationEventHub,
    conversation_id: String,
    /// Always `Some` until `drop`, which must drop the receiver before
    /// checking the channel's receiver count.
    receiver: Option<broadcast::Receiver<ConversationEvent>>,
}

impl ConversationSubscription {
    pub async fn recv(&mut self) -> Result<ConversationEvent, broadcast::error::RecvError> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.recv().await,
            None => Err(broadcast::error::RecvError::Closed),
        }
    }
}

impl Drop for ConversationSubscription {
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.hub.release(&self.conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(conversation_id: &str, sequence: u64) -> ConversationEvent {
        ConversationEvent::new(
            conversation_id,
            sequence,
            "fixture.event",
            serde_json::json!({}),
        )
    }

    #[tokio::test]
    async fn remove_closes_existing_receivers() {
        let hub = ConversationEventHub::default();
        let mut receiver = hub.subscribe("conversation-1");
        hub.remove("conversation-1");
        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn remove_does_not_affect_other_channels() {
        let hub = ConversationEventHub::default();
        let mut receiver = hub.subscribe("conversation-1");
        let mut other = hub.subscribe("conversation-2");
        hub.remove("conversation-1");
        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
        hub.publish(event("conversation-2", 1));
        assert!(other.recv().await.is_ok());
    }

    #[test]
    fn publish_without_subscribers_creates_no_channel() {
        let hub = ConversationEventHub::default();
        hub.publish(event("conversation-1", 1));
        assert!(!hub.has_channel("conversation-1"));
    }

    #[tokio::test]
    async fn channel_is_reclaimed_after_the_last_subscriber_leaves() {
        let hub = ConversationEventHub::default();
        let first = hub.track("conversation-1", hub.subscribe("conversation-1"));
        let mut second = hub.track("conversation-1", hub.subscribe("conversation-1"));
        drop(first);
        assert!(hub.has_channel("conversation-1"));
        hub.publish(event("conversation-1", 1));
        assert_eq!(second.recv().await.unwrap().sequence, 1);
        drop(second);
        assert!(!hub.has_channel("conversation-1"));
    }

    #[tokio::test]
    async fn publish_reclaims_a_channel_whose_receivers_were_dropped() {
        let hub = ConversationEventHub::default();
        drop(hub.subscribe("conversation-1"));
        assert!(hub.has_channel("conversation-1"));
        hub.publish(event("conversation-1", 1));
        assert!(!hub.has_channel("conversation-1"));
    }

    #[test]
    fn subscriber_racing_a_release_keeps_receiving() {
        let hub = ConversationEventHub::default();
        for round in 0..2_000u64 {
            let leaving = hub.track("conversation-1", hub.subscribe("conversation-1"));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let release = {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    drop(leaving);
                })
            };
            let arriving = {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    hub.track("conversation-1", hub.subscribe("conversation-1"))
                })
            };
            release.join().unwrap();
            let mut arriving = arriving.join().unwrap();
            hub.publish(event("conversation-1", round));
            let received = arriving
                .receiver
                .as_mut()
                .unwrap()
                .try_recv()
                .expect("the concurrent subscriber must keep its channel");
            assert_eq!(received.sequence, round);
            drop(arriving);
            assert!(!hub.has_channel("conversation-1"));
        }
    }
}
