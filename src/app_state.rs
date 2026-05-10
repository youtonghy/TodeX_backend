use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::{
    codex_gateway::{CodexGatewayStore, CodexLocalAdapterSupervisor},
    config::Config,
    error::Result,
    event::EventBus,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub events: EventBus,
    pub codex_gateway: CodexGatewayStore,
    pub codex_local_adapters: CodexLocalAdapterSupervisor,
    websocket_connections: Arc<AtomicUsize>,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        tokio::fs::create_dir_all(config.data_dir.join("logs")).await?;
        tokio::fs::create_dir_all(config.data_dir.join("audit")).await?;

        let config = Arc::new(config);
        let events = EventBus::new(4096);
        let codex_gateway = CodexGatewayStore::new(config.data_dir.clone());
        let codex_local_adapters =
            CodexLocalAdapterSupervisor::new(codex_gateway.clone(), events.clone());
        let websocket_connections = Arc::new(AtomicUsize::new(0));

        Ok(Self {
            config,
            events,
            codex_gateway,
            codex_local_adapters,
            websocket_connections,
        })
    }

    pub fn websocket_connection_count(&self) -> usize {
        self.websocket_connections.load(Ordering::Relaxed)
    }

    pub fn increment_websocket_connections(&self) -> usize {
        self.websocket_connections.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn decrement_websocket_connections(&self) -> usize {
        self.websocket_connections
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1)
    }
}
