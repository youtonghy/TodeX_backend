use tokio::sync::broadcast;

use super::EventRecord;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EventRecord>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventRecord> {
        self.tx.subscribe()
    }

    pub async fn publish(&self, event: EventRecord) {
        let _ = self.tx.send(event);
    }
}
