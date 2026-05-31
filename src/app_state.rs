use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::{
    codex_gateway::{CodexGatewayStore, CodexLocalAdapterSupervisor},
    config::Config,
    error::Result,
    event::EventBus,
    local_terminal::LocalTerminalManager,
    transport::TransportAckStore,
    transport_crypto::PairingKeys,
    workspace_store::WorkspaceStore,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub events: EventBus,
    pub codex_gateway: CodexGatewayStore,
    pub codex_local_adapters: CodexLocalAdapterSupervisor,
    pub local_terminals: LocalTerminalManager,
    pub transport_acks: TransportAckStore,
    pub pairing_keys: PairingKeys,
    pub workspaces: WorkspaceStore,
    websocket_connections: Arc<AtomicUsize>,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        tokio::fs::create_dir_all(config.data_dir.join("logs")).await?;
        tokio::fs::create_dir_all(config.data_dir.join("audit")).await?;
        tokio::fs::create_dir_all(&config.workspace_root).await?;

        let config = Arc::new(config);
        let events = EventBus::new(4096);
        let codex_gateway = CodexGatewayStore::new(config.data_dir.clone());
        let codex_local_adapters =
            CodexLocalAdapterSupervisor::new(codex_gateway.clone(), events.clone());
        let local_terminals = LocalTerminalManager::new(events.clone());
        let transport_acks = TransportAckStore::new();
        let pairing_keys = PairingKeys::load_or_generate(&config.data_dir).await?;
        let workspaces =
            WorkspaceStore::new(config.data_dir.clone(), config.workspace_root.clone()).await?;
        let websocket_connections = Arc::new(AtomicUsize::new(0));

        Ok(Self {
            config,
            events,
            codex_gateway,
            codex_local_adapters,
            local_terminals,
            transport_acks,
            pairing_keys,
            workspaces,
            websocket_connections,
        })
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
