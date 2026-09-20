use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::broadcast;

use super::ConversationEvent;

const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone, Default)]
pub struct ConversationEventHub {
    channels: Arc<DashMap<String, broadcast::Sender<ConversationEvent>>>,
}

impl ConversationEventHub {
    pub fn subscribe(&self, conversation_id: &str) -> broadcast::Receiver<ConversationEvent> {
        self.channel(conversation_id).subscribe()
    }

    /// Drops the broadcast channel for a conversation. Existing receivers see
    /// `RecvError::Closed`, which lets websocket subscription tasks exit and
    /// release their per-connection slot. A concurrent publish/subscribe simply
    /// recreates an empty channel.
    pub fn remove(&self, conversation_id: &str) {
        self.channels.remove(conversation_id);
    }

    pub fn publish(&self, event: ConversationEvent) {
        let _ = self.channel(&event.conversation_id).send(event);
    }

    fn channel(&self, conversation_id: &str) -> broadcast::Sender<ConversationEvent> {
        self.channels
            .entry(conversation_id.to_owned())
            .or_insert_with(|| broadcast::channel(DEFAULT_CHANNEL_CAPACITY).0)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        hub.publish(ConversationEvent::new(
            "conversation-2",
            1,
            "fixture.event",
            serde_json::json!({}),
        ));
        assert!(other.recv().await.is_ok());
    }
}
