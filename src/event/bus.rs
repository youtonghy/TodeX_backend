use std::sync::Arc;

use tokio::sync::broadcast;

use super::{EventRecord, PublishedEvent};

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Arc<PublishedEvent>>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<PublishedEvent>> {
        self.tx.subscribe()
    }

    /// Shares one copy of `event` with every subscriber; without any it is
    /// dropped before any routing work is done.
    pub async fn publish(&self, event: EventRecord) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let _ = self.tx.send(Arc::new(PublishedEvent::new(event)));
    }
}
