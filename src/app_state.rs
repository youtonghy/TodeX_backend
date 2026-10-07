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
    agent_desktop::AgentDesktop,
    agent_mcp::AgentMcp,
    agent_providers::AgentProviderService,
    catalog::CatalogService,
    codex_gateway::{CodexGatewayStore, CodexLocalAdapterSupervisor},
    config::Config,
    conversation::{migrate_legacy_codex_sessions, ConversationEventHub, ConversationStore},
    device_auth::DeviceAuthenticator,
    device_pairing::DevicePairingRegistry,
    devices::DeviceRegistry,
    error::Result,
    event::EventBus,
    history_keys::HistoryKeys,
    kanban_store::KanbanTaskStore,
    local_terminal::LocalTerminalManager,
    provider::{CliManager, ConversationSupervisor},
    quota_store::QuotaStore,
    remote_fs::RemoteSessions,
    ssh::SshService,
    transport_crypto::PairingKeyStore,
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
    /// The hub `conversations` publishes through; websocket subscriptions use
    /// it to reclaim a channel once its last receiver is gone.
    pub conversation_hub: ConversationEventHub,
    pub cli_manager: CliManager,
    pub(crate) cli_execution_gate: Arc<tokio::sync::RwLock<()>>,
    conversation_store: ConversationStore,
    pub local_terminals: LocalTerminalManager,
    pub pairing_keys: PairingKeyStore,
    pub(crate) device_auth: DeviceAuthenticator,
    pub(crate) device_pairing: DevicePairingRegistry,
    /// History encryption recipients, keyrings and DEKs; see
    /// [`crate::history_keys`].
    pub(crate) history_keys: HistoryKeys,
    pub workspaces: WorkspaceStore,
    pub kanban_tasks: KanbanTaskStore,
    pub agent_providers: AgentProviderService,
    pub workspace_trust: WorkspaceTrustStore,
    pub ssh: SshService,
    /// Newest plan-quota snapshot per provider (`quota.updated` events plus
    /// on-demand `/v2/providers/quota` refreshes).
    pub quota: QuotaStore,
    /// SSH tools for agents; see [`crate::agent_mcp`].
    pub agent_mcp: AgentMcp,
    /// Desktop executors for agent tools; see [`crate::agent_desktop`].
    pub agent_desktop: AgentDesktop,
    /// Open SFTP/FTP file sessions; in memory only.
    pub(crate) remote_files: RemoteSessions,
    pub(crate) audit_log: crate::event::AuditLog,
    /// Terminal audit (`audit-terminal.jsonl`), kept apart so per-message
    /// terminal records cannot rotate the main trail away.
    pub(crate) terminal_audit_log: crate::event::AuditLog,
    websocket_connections: Arc<AtomicUsize>,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        tokio::fs::create_dir_all(&config.data_dir).await?;
        tokio::fs::create_dir_all(config.data_dir.join("logs")).await?;
        tokio::fs::create_dir_all(config.data_dir.join("audit")).await?;
        for root in &config.workspace_roots {
            if let Err(error) = tokio::fs::create_dir_all(root).await {
                // An unavailable root (for example an unmounted volume) must not
                // abort startup; workspace validation already rejects paths
                // under missing roots.
                tracing::warn!(
                    root = %root.display(),
                    error = %error,
                    "workspace root is unavailable; continuing startup without it"
                );
            }
        }
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
            WorkspaceTrustStore::new(config.data_dir.clone(), config.workspace_roots.clone())
                .await?;
        let workspaces =
            WorkspaceStore::new(config.data_dir.clone(), config.workspace_roots.clone()).await?;
        let kanban_tasks = KanbanTaskStore::new(config.data_dir.clone()).await?;
        let agent_providers = AgentProviderService::new(config.data_dir.clone()).await?;
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
        let ssh = SshService::new(&config.data_dir, config.agent.ssh_bin.clone()).await?;
        let agent_desktop = AgentDesktop::load(&config.data_dir).await?;
        let agent_mcp = AgentMcp::new(&config.data_dir, ssh.clone(), agent_desktop.clone()).await?;
        let devices = DeviceRegistry::load(&config.data_dir)?;
        crate::config::warn_retired_history_encryption(&config.data_dir);
        let history_keys = HistoryKeys::load(
            &config.data_dir,
            config.security.enable_auth.then(|| devices.clone()),
        )?;
        let conversation_store =
            ConversationStore::open(config.data_dir.clone(), history_keys.clone()).await?;
        let conversation_hub = ConversationEventHub::default();
        let quota = QuotaStore::default();
        let conversations = ConversationSupervisor::new_with_execution_gate(
            config.clone(),
            conversation_store.clone(),
            conversation_hub.clone(),
            workspace_trust.clone(),
            cli_execution_gate.clone(),
        )
        .with_quota(quota.clone())
        .with_agent_mcp(agent_mcp.clone());
        conversations.recover_all().await?;
        let local_terminals = LocalTerminalManager::new(events.clone());
        let cli_manager = CliManager::default();
        let pairing_keys = PairingKeyStore::load(&config.data_dir).await?;
        let device_auth = DeviceAuthenticator::new(devices.clone());
        // Pairing reads the same key store and configured protocol as the
        // handshake, so the key it delivers is the one the transport uses.
        let device_pairing = DevicePairingRegistry::new(
            &config.data_dir,
            config.security.enable_auth,
            devices,
            pairing_keys.clone(),
            config.pairing_encryption,
        )?;
        let websocket_connections = Arc::new(AtomicUsize::new(0));
        let audit_log = crate::event::AuditLog::default();

        Ok(Self {
            config,
            catalog,
            events,
            codex_gateway,
            codex_local_adapters,
            conversations,
            conversation_hub,
            cli_manager,
            cli_execution_gate,
            conversation_store,
            local_terminals,
            pairing_keys,
            device_auth,
            device_pairing,
            history_keys,
            workspaces,
            kanban_tasks,
            agent_providers,
            workspace_trust,
            ssh,
            quota,
            agent_mcp,
            agent_desktop,
            remote_files: RemoteSessions::default(),
            audit_log,
            terminal_audit_log: crate::event::AuditLog::terminal(),
            websocket_connections,
        })
    }

    /// [`Self::new`] plus a history recovery recipient (seed
    /// [`TEST_HISTORY_RECIPIENT`]) so conversations can be written: history
    /// is always end-to-end encrypted and a store without a recipient
    /// refuses writes with `HISTORY_KEY_REQUIRED`. Tests read the content
    /// back with `conversation::e2e_support`.
    #[cfg(test)]
    pub(crate) async fn new_for_tests(config: Config) -> Result<Self> {
        let state = Self::new(config).await?;
        state.history_keys.recipients().set_recovery(
            &crate::history_keys::test_support::recipient(TEST_HISTORY_RECIPIENT),
        )?;
        Ok(state)
    }

    #[cfg(test)]
    pub(crate) fn conversation_store(&self) -> &ConversationStore {
        &self.conversation_store
    }

    pub(crate) fn spawn_legacy_conversation_migration(&self) -> JoinHandle<()> {
        let data_dir = self.config.data_dir.clone();
        let workspace_roots = self.config.workspace_roots.clone();
        let store = self.conversation_store.clone();
        tokio::spawn(async move {
            match migrate_legacy_codex_sessions(&data_dir, &workspace_roots, &store).await {
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

    /// Background conversion of sealed journal files and migration-backup
    /// cleanup.
    pub(crate) fn spawn_journal_maintenance(&self) -> JoinHandle<()> {
        self.conversation_store.spawn_maintenance()
    }

    /// The one-time scan marking legacy plaintext conversations read-only
    /// (docs/history-encryption.md §8); `None` once it has completed.
    pub(crate) fn spawn_legacy_history_scan(&self) -> Option<JoinHandle<()>> {
        self.conversation_store
            .spawn_legacy_scan(self.config.data_dir.clone())
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

/// Seed byte of the recipient [`AppState::new_for_tests`] registers
/// (`history_keys::test_support::recipient`).
#[cfg(test)]
pub(crate) const TEST_HISTORY_RECIPIENT: u8 = 0xE2;

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

    /// Opt-in: the legacy plaintext scan over a COPY of a real data
    /// directory (`TODEX_REAL_DATA`, e.g. an APFS clone of
    /// `~/.todex-agent/conversations` in a scratch directory). Prints the
    /// scan's duration and counts, then checks that a legacy conversation
    /// refuses a prompt with `HISTORY_READ_ONLY` and still reads.
    #[tokio::test]
    #[ignore = "opt-in: scans a COPY of a real data directory (TODEX_REAL_DATA)"]
    async fn measure_legacy_scan_on_a_real_data_copy() {
        let data_dir =
            PathBuf::from(std::env::var("TODEX_REAL_DATA").expect("set TODEX_REAL_DATA"));
        let workspace_root = data_dir.join("scan-workspaces");
        std::fs::create_dir_all(&workspace_root).unwrap();
        let state = AppState::new(Config {
            data_dir: data_dir.clone(),
            workspace_roots: vec![workspace_root],
            ..Config::default()
        })
        .await
        .unwrap();
        let store = state.conversation_store();
        let started = std::time::Instant::now();
        let scan = store.scan_legacy(&data_dir).await.unwrap();
        let first_ms = started.elapsed().as_millis();
        let manifests = store.list().await.unwrap();
        let legacy = manifests
            .iter()
            .filter(|manifest| manifest.legacy_plaintext)
            .collect::<Vec<_>>();
        let encrypted = manifests
            .iter()
            .filter(|manifest| manifest.history_encrypted_at.is_some())
            .count();
        let started = std::time::Instant::now();
        let again = store.scan_legacy(&data_dir).await.unwrap();
        let second_ms = started.elapsed().as_millis();
        eprintln!(
            "legacy scan: {scan:?} in {first_ms} ms; conversations={} legacy={} encrypted={} undecided={}; second pass {again:?} in {second_ms} ms",
            manifests.len(),
            legacy.len(),
            encrypted,
            manifests.len() - legacy.len() - encrypted,
        );
        assert!(again.skipped);
        let sample = legacy.first().expect("the copy holds legacy history");
        let refused = state
            .conversations
            .prompt_owned(
                &sample.owner_id,
                &sample.id,
                crate::provider::ConversationPrompt {
                    client_request_id: Some("legacy-check".to_owned()),
                    text: "hello".to_owned(),
                    model: None,
                    reasoning_effort: None,
                    skills: Vec::new(),
                    content: Vec::new(),
                    permission_mode: None,
                    work_mode: None,
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code(), "HISTORY_READ_ONLY");
        let page = store.replay(&sample.id, 0, 50).await.unwrap();
        eprintln!(
            "prompt on legacy {} -> {}; replay read {} events (last {})",
            sample.id,
            refused.code(),
            page.events.len(),
            sample.last_sequence
        );
        assert!(!page.events.is_empty());
    }

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

        let workspaces = WorkspaceStore::new(root.clone(), vec![workspace_root.clone()])
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
        WorkspaceTrustStore::new(root.clone(), vec![workspace_root.clone()])
            .await
            .unwrap()
            .set_owned("local", &revoked, false)
            .await
            .unwrap();

        let config = Config {
            data_dir: root.clone(),
            workspace_roots: vec![workspace_root],
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

    #[tokio::test]
    async fn startup_tolerates_unavailable_workspace_root() {
        let root = std::env::temp_dir().join(format!(
            "todex-app-state-missing-root-{}",
            Uuid::new_v4().simple()
        ));
        let workspace_root = root.join("workspaces");
        tokio::fs::create_dir_all(&workspace_root).await.unwrap();
        // A root nested under a regular file can never be created, the same way
        // a root on an unmounted volume cannot.
        let blocker = root.join("blocker");
        tokio::fs::write(&blocker, b"not a directory")
            .await
            .unwrap();
        let unavailable_root = blocker.join("nested");

        let config = Config {
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root, unavailable_root],
            ..Config::default()
        };
        AppState::new(config).await.unwrap();

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
            icon: None,
            icon_color: None,
            ring_style: None,
            created_at: 1,
            updated_at: 1,
            sort_order: None,
        }
    }
}
