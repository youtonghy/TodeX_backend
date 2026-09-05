use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::task::JoinHandle;

use crate::{
    catalog::CatalogService,
    codex_gateway::{CodexGatewayStore, CodexLocalAdapterSupervisor},
    config::Config,
    conversation::{migrate_legacy_codex_sessions, ConversationEventHub, ConversationStore},
    error::Result,
    event::EventBus,
    local_terminal::LocalTerminalManager,
    provider::{CliManager, ConversationSupervisor},
    transport_crypto::PairingKeys,
    workspace_store::WorkspaceStore,
    workspace_trust::WorkspaceTrustStore,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub catalog: CatalogService,
    pub events: EventBus,
    pub codex_gateway: CodexGatewayStore,
    pub codex_local_adapters: CodexLocalAdapterSupervisor,
    pub conversations: ConversationSupervisor,
    pub cli_manager: CliManager,
    pub(crate) cli_execution_gate: Arc<tokio::sync::RwLock<()>>,
    conversation_store: ConversationStore,
    pub local_terminals: LocalTerminalManager,
    pub pairing_keys: PairingKeys,
    pub workspaces: WorkspaceStore,
    pub workspace_trust: WorkspaceTrustStore,
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
        let cli_execution_gate = Arc::new(tokio::sync::RwLock::new(()));
        let codex_local_adapters = CodexLocalAdapterSupervisor::new_with_execution_gate(
            codex_gateway.clone(),
            events.clone(),
            cli_execution_gate.clone(),
        );
        let workspace_trust =
            WorkspaceTrustStore::new(config.data_dir.clone(), config.workspace_root.clone())
                .await?;
        let workspaces =
            WorkspaceStore::new(config.data_dir.clone(), config.workspace_root.clone()).await?;
        let mut workspace_paths_by_owner = HashMap::<String, Vec<PathBuf>>::new();
        for workspace in workspaces.snapshot().await.workspaces {
            workspace_paths_by_owner
                .entry(workspace.tenant_id)
                .or_default()
                .push(PathBuf::from(workspace.path));
        }
        for (owner_id, workspace_paths) in workspace_paths_by_owner {
            workspace_trust
                .auto_trust_undecided_owned(&owner_id, &workspace_paths)
                .await?;
        }
        let conversation_store = ConversationStore::new(config.data_dir.clone()).await?;
        let conversation_hub = ConversationEventHub::default();
        let conversations = ConversationSupervisor::new_with_execution_gate(
            config.clone(),
            conversation_store.clone(),
            conversation_hub,
            workspace_trust.clone(),
            cli_execution_gate.clone(),
        );
        conversations.recover_all().await?;
        let local_terminals = LocalTerminalManager::new(events.clone());
        let cli_manager = CliManager::default();
        let pairing_keys = PairingKeys::load_or_generate(&config.data_dir).await?;
        let websocket_connections = Arc::new(AtomicUsize::new(0));
        let audit_write_lock = Arc::new(tokio::sync::Mutex::new(()));

        Ok(Self {
            config,
            catalog,
            events,
            codex_gateway,
            codex_local_adapters,
            conversations,
            cli_manager,
            cli_execution_gate,
            conversation_store,
            local_terminals,
            pairing_keys,
            workspaces,
            workspace_trust,
            audit_write_lock,
            websocket_connections,
        })
    }

    pub(crate) fn spawn_legacy_conversation_migration(&self) -> JoinHandle<()> {
        let data_dir = self.config.data_dir.clone();
        let workspace_root = self.config.workspace_root.clone();
        let store = self.conversation_store.clone();
        tokio::spawn(async move {
            match migrate_legacy_codex_sessions(&data_dir, &workspace_root, &store).await {
                Ok(migration) if migration.imported > 0 || migration.skipped > 0 => {
                    tracing::info!(
                        imported = migration.imported,
                        already_imported = migration.already_imported,
                        skipped = migration.skipped,
                        "legacy Codex conversation migration finished"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "legacy Codex conversation migration failed");
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_store::WorkspaceRecord;
    use uuid::Uuid;

    #[tokio::test]
    async fn startup_trusts_registered_workspaces_without_overriding_revocation() {
        let root = std::env::temp_dir().join(format!(
            "todex-app-state-workspace-trust-{}",
            Uuid::new_v4().simple()
        ));
        let workspace_root = root.join("workspaces");
        let automatic = workspace_root.join("automatic");
        let revoked = workspace_root.join("revoked");
        tokio::fs::create_dir_all(&automatic).await.unwrap();
        tokio::fs::create_dir_all(&revoked).await.unwrap();

        let workspaces = WorkspaceStore::new(root.clone(), workspace_root.clone())
            .await
            .unwrap();
        workspaces
            .merge_owned(
                "local",
                vec![
                    workspace_record("automatic", &automatic),
                    workspace_record("revoked", &revoked),
                ],
            )
            .await
            .unwrap();
        WorkspaceTrustStore::new(root.clone(), workspace_root.clone())
            .await
            .unwrap()
            .set_owned("local", &revoked, false)
            .await
            .unwrap();

        let config = Config {
            data_dir: root.clone(),
            workspace_root,
            ..Config::default()
        };
        let state = AppState::new(config).await.unwrap();

        assert!(
            state
                .workspace_trust
                .status_owned("local", &automatic)
                .await
                .unwrap()
                .trusted
        );
        assert!(
            !state
                .workspace_trust
                .status_owned("local", &revoked)
                .await
                .unwrap()
                .trusted
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    fn workspace_record(name: &str, path: &std::path::Path) -> WorkspaceRecord {
        WorkspaceRecord {
            id: name.to_owned(),
            name: name.to_owned(),
            path: path.display().to_string(),
            session_id: String::new(),
            tenant_id: "local".to_owned(),
            thread_id: String::new(),
            model: "gpt-5.5".to_owned(),
            reasoning_effort: Some("medium".to_owned()),
            approval_policy: "on-request".to_owned(),
            sandbox_mode: "workspace-write".to_owned(),
            permission_profile: Some(":workspace".to_owned()),
            approvals_reviewer: Some("user".to_owned()),
            service_tier: None,
            local_adapter_state: None,
            created_at: 1,
            updated_at: 1,
        }
    }
}
