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
