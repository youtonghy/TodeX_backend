use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use crate::{
    catalog::CatalogService,
    codex_gateway::{CodexGatewayStore, CodexLocalAdapterSupervisor},
    config::Config,
    conversation::{migrate_legacy_codex_sessions, ConversationEventHub, ConversationStore},
    error::Result,
    event::EventBus,
    local_terminal::LocalTerminalManager,
    provider::ConversationSupervisor,
    transport_crypto::PairingKeys,
    workspace_store::WorkspaceStore,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog: CatalogService,
    pub events: EventBus,
    pub codex_gateway: CodexGatewayStore,
    pub codex_local_adapters: CodexLocalAdapterSupervisor,
    pub conversations: ConversationSupervisor,
    pub local_terminals: LocalTerminalManager,
    pub pairing_keys: PairingKeys,
    pub workspaces: WorkspaceStore,
    pub(crate) audit_write_lock: Arc<tokio::sync::Mutex<()>>,
    websocket_connections: Arc<AtomicUsize>,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        tokio::fs::create_dir_all(&config.data_dir).await?;
        tokio::fs::create_dir_all(config.data_dir.join("logs")).await?;
        tokio::fs::create_dir_all(config.data_dir.join("audit")).await?;
        tokio::fs::create_dir_all(&config.workspace_root).await?;
        set_owner_only_directory(&config.data_dir).await?;
        set_owner_only_directory(&config.data_dir.join("logs")).await?;
        set_owner_only_directory(&config.data_dir.join("audit")).await?;

        let config = Arc::new(config);
        let catalog = CatalogService::new(config.clone());
        let events = EventBus::new(4096);
        let codex_gateway = CodexGatewayStore::new(config.data_dir.clone());
        let codex_local_adapters =
            CodexLocalAdapterSupervisor::new(codex_gateway.clone(), events.clone());
        let conversation_store = ConversationStore::new(config.data_dir.clone()).await?;
        let migration = migrate_legacy_codex_sessions(
            &config.data_dir,
            &config.workspace_root,
            &conversation_store,
        )
        .await?;
        if migration.imported > 0 || migration.skipped > 0 {
            tracing::info!(
                imported = migration.imported,
                already_imported = migration.already_imported,
                skipped = migration.skipped,
                "legacy Codex conversation migration finished"
            );
        }
        let conversation_hub = ConversationEventHub::default();
        let conversations =
            ConversationSupervisor::new(config.clone(), conversation_store, conversation_hub);
        conversations.recover_all().await?;
        let local_terminals = LocalTerminalManager::new(events.clone());
        let pairing_keys = PairingKeys::load_or_generate(&config.data_dir).await?;
        let workspaces =
            WorkspaceStore::new(config.data_dir.clone(), config.workspace_root.clone()).await?;
        let websocket_connections = Arc::new(AtomicUsize::new(0));
        let audit_write_lock = Arc::new(tokio::sync::Mutex::new(()));

        Ok(Self {
            config,
            catalog,
            events,
            codex_gateway,
            codex_local_adapters,
            conversations,
            local_terminals,
            pairing_keys,
            workspaces,
            audit_write_lock,
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

async fn set_owner_only_directory(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
