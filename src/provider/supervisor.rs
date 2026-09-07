use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{watch, RwLock};
use tokio::time::{sleep, Duration, Instant};
use uuid::Uuid;

use crate::catalog::CatalogService;
use crate::config::Config;
use crate::conversation::{
    ConversationEventHub, ConversationManifest, ConversationReplay, ConversationStatus,
    ConversationStore, ProviderKind,
};
use crate::error::AppError;
use crate::mcp;
use crate::workspace_paths::validate_workspace_directory_text;
use crate::workspace_store::stable_workspace_id;
use crate::workspace_trust::WorkspaceTrustStore;

use super::acp::AcpDriver;
use super::claude::ClaudeDriver;
use super::codex::CodexDriver;
use super::grok::GrokBuildDriver;
use super::pi::PiDriver;
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverPromptContent, DriverSkill, ImageInputMode,
    PermissionBroker, PermissionDecision, PermissionOutcome, ProviderCommandDescriptor,
    ProviderControl, ProviderDescriptor, ProviderDriver, ProviderImageInputCapability,
    ProviderModelDescriptor,
};

fn prompt_fingerprint(prompt: &ConversationPrompt) -> Result<String, AppError> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(prompt)?)))
}

fn control_failure_event(error: &AppError) -> &'static str {
    match error {
        AppError::InvalidRequest(_) | AppError::Unsupported(_) => "control.rejected",
        AppError::Conflict(message)
            if ![
                "timeout",
                "timed out",
                "unknown",
                "not confirmed",
                "could not be confirmed",
            ]
            .iter()
            .any(|fragment| message.to_ascii_lowercase().contains(fragment)) =>
        {
            "control.rejected"
        }
        _ => "control.unknown",
    }
}

const MAX_PROMPT_BYTES: usize = 512 * 1024;
const MAX_PROMPT_CONTENT_ITEMS: usize = 16;
const MAX_PROMPT_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSkillRef {
    pub resource_id: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPrompt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_mode: Option<String>,
    #[serde(default)]
    pub client_request_id: Option<String>,
    pub text: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub skills: Vec<PromptSkillRef>,
    pub content: Vec<PromptContentRef>,
    pub permission_profile: Option<String>,
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum PromptContentRef {
    Text {
        text: String,
    },
    LocalImage {
        path: PathBuf,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: String,
    },
    File {
        path: PathBuf,
        #[serde(default)]
        name: Option<String>,
    },
}

#[derive(Clone)]
pub struct DriverRegistry {
    drivers: Arc<BTreeMap<ProviderKind, Arc<dyn ProviderDriver>>>,
}

impl DriverRegistry {
    pub fn new(config: &Config) -> Self {
        let drivers: BTreeMap<ProviderKind, Arc<dyn ProviderDriver>> = BTreeMap::from([
            (
                ProviderKind::Acp,
                Arc::new(AcpDriver::new(&config.agent)) as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::Codex,
                Arc::new(CodexDriver::new(&config.agent)) as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::Pi,
                Arc::new(PiDriver::new(&config.agent)) as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::ClaudeCode,
                Arc::new(ClaudeDriver::new(&config.agent)) as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::GrokBuild,
                Arc::new(GrokBuildDriver::new(&config.agent)) as Arc<dyn ProviderDriver>,
            ),
        ]);
        Self {
            drivers: Arc::new(drivers),
        }
    }

    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        ProviderKind::ALL
            .iter()
            .filter_map(|provider| self.drivers.get(provider))
            .map(|driver| driver.descriptor())
            .collect()
    }

    pub fn driver(&self, provider: ProviderKind) -> Result<Arc<dyn ProviderDriver>, AppError> {
        self.drivers
            .get(&provider)
            .cloned()
            .ok_or_else(|| AppError::Unsupported(format!("provider {}", provider.as_str())))
    }
}

#[derive(Clone)]
pub struct ConversationSupervisor {
    config: Arc<Config>,
    store: ConversationStore,
    hub: ConversationEventHub,
    catalog: CatalogService,
    registry: DriverRegistry,
    permissions: PermissionBroker,
    active: Arc<DashMap<String, ActiveTurn>>,
    request_gates: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    cli_execution_gate: Arc<RwLock<()>>,
    workspace_trust: WorkspaceTrustStore,
}

struct ActiveTurn {
    turn_id: String,
    cancel: watch::Sender<bool>,
}

struct ActiveTurnCleanup {
    active: Arc<DashMap<String, ActiveTurn>>,
    conversation_id: String,
}

impl Drop for ActiveTurnCleanup {
    fn drop(&mut self) {
        self.active.remove(&self.conversation_id);
    }
}

impl ConversationSupervisor {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        workspace_trust: WorkspaceTrustStore,
    ) -> Self {
        Self::new_with_execution_gate(
            config,
            store,
            hub,
            workspace_trust,
            Arc::new(RwLock::new(())),
        )
    }

    pub fn new_with_execution_gate(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        workspace_trust: WorkspaceTrustStore,
        cli_execution_gate: Arc<RwLock<()>>,
    ) -> Self {
        let catalog = CatalogService::new(config.clone());
        Self::new_with_catalog_and_gate(
            config,
            store,
            hub,
            catalog,
            workspace_trust,
            cli_execution_gate,
        )
    }

    fn new_with_catalog_and_gate(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        catalog: CatalogService,
        workspace_trust: WorkspaceTrustStore,
        cli_execution_gate: Arc<RwLock<()>>,
    ) -> Self {
        Self {
            registry: DriverRegistry::new(&config),
            config,
            store,
            hub,
            catalog,
            permissions: PermissionBroker::default(),
            active: Arc::new(DashMap::new()),
            request_gates: Arc::new(DashMap::new()),
            workspace_trust,
            cli_execution_gate,
        }
    }

    pub async fn recover_all(&self) -> Result<(), AppError> {
        for manifest in self.store.list().await? {
            let was_active = matches!(
                manifest.status,
                ConversationStatus::Running | ConversationStatus::WaitingPermission
            );
            let recovered = self.store.recover(&manifest.id).await?;
            let mut expired = std::collections::BTreeSet::new();
            for event in self.store.complete_history(&manifest.id).await? {
                if let Some(id) = event.payload.get("permissionId").and_then(Value::as_str) {
                    match event.event_type.as_str() {
                        "permission.requested" | "tool.awaitingApproval" => {
                            expired.insert(id.to_owned());
                        }
                        "permission.resolved" => {
                            expired.remove(id);
                        }
                        _ => {}
                    }
                }
            }
            for permission_id in expired {
                self.emit(&manifest.id, "permission.resolved", json!({
                    "permissionId": permission_id, "outcome": "cancelled", "optionId": Value::Null,
                    "reason": "daemon_restarted",
                })).await?;
            }

            if was_active {
                self.emit(
                    &recovered.id,
                    "conversation.interrupted",
                    json!({
                        "reason": "daemon_restarted",
                        "message": "The previous in-progress turn was interrupted; it was not replayed.",
                    }),
                )
                .await?;
            }
        }
        self.permissions.expire_all();
        Ok(())
    }

    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        self.registry.descriptors()
    }

    pub fn has_active_turns(&self) -> bool {
        !self.active.is_empty()
    }

    pub async fn models_live(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        let _cli_permit = self.cli_execution_gate.try_read().map_err(|_| {
            AppError::Conflict("CLI discovery is unavailable during an upgrade".to_owned())
        })?;
        let _launch_permit = self
            .workspace_trust
            .acquire_owned(owner_id, workspace)
            .await?;
        tokio::time::timeout(
            Duration::from_secs(8),
            self.registry.driver(provider)?.discover_models(workspace),
        )
        .await
        .map_err(|_| {
            AppError::ProviderUnavailable(format!(
                "{} model discovery timed out",
                provider.as_str()
            ))
        })?
    }

    pub async fn image_input_live(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: &Path,
        profile: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderImageInputCapability, AppError> {
        let descriptor = self.registry.driver(provider)?.descriptor();
        let (image_input, source, reason) = match descriptor.capabilities.image_input_mode {
            ImageInputMode::Always => (true, "provider".to_owned(), None),
            ImageInputMode::None => (
                false,
                "provider".to_owned(),
                Some("this provider does not support image input".to_owned()),
            ),
            ImageInputMode::Model => {
                let models = self.models_live(owner_id, provider, workspace).await?;
                let selected = model
                    .and_then(|id| models.iter().find(|candidate| candidate.id == id))
                    .or_else(|| models.iter().find(|candidate| candidate.is_default));
                match selected {
                    Some(selected) => (
                        selected.image_input.unwrap_or(false),
                        "model".to_owned(),
                        (!selected.image_input.unwrap_or(false)).then(|| {
                            format!("model '{}' does not support image input", selected.id)
                        }),
                    ),
                    None => (
                        false,
                        "model".to_owned(),
                        Some("the selected model's image capability is unknown".to_owned()),
                    ),
                }
            }
            ImageInputMode::Profile => {
                let _cli_permit = self.cli_execution_gate.try_read().map_err(|_| {
                    AppError::Conflict("CLI discovery is unavailable during an upgrade".to_owned())
                })?;
                let _launch_permit = self
                    .workspace_trust
                    .acquire_owned(owner_id, workspace)
                    .await?;
                let image_input = tokio::time::timeout(
                    Duration::from_secs(8),
                    self.registry
                        .driver(provider)?
                        .discover_image_input(workspace, profile),
                )
                .await
                .map_err(|_| {
                    AppError::ProviderUnavailable(format!(
                        "{} image capability discovery timed out",
                        provider.as_str()
                    ))
                })??;
                (
                    image_input,
                    "profile".to_owned(),
                    (!image_input).then(|| {
                        "the selected ACP profile does not advertise image input".to_owned()
                    }),
                )
            }
        };
        Ok(ProviderImageInputCapability {
            provider,
            profile: profile.map(ToOwned::to_owned),
            model: model.map(ToOwned::to_owned),
            image_input,
            source,
            reason,
        })
    }

    pub async fn commands_live(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let _cli_permit = self.cli_execution_gate.try_read().map_err(|_| {
            AppError::Conflict("CLI discovery is unavailable during an upgrade".to_owned())
        })?;
        let _launch_permit = self
            .workspace_trust
            .acquire_owned(owner_id, workspace)
            .await?;
        tokio::time::timeout(
            Duration::from_secs(8),
            self.registry.driver(provider)?.discover_commands(workspace),
        )
        .await
        .map_err(|_| {
            AppError::ProviderUnavailable(format!(
                "{} command discovery timed out",
                provider.as_str()
            ))
        })?
    }

    #[allow(dead_code)]
    pub async fn create(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
        title: Option<String>,
        provider_profile: Option<String>,
    ) -> Result<ConversationManifest, AppError> {
        self.create_owned("local", provider, workspace, title, provider_profile)
            .await
    }

    pub async fn create_owned(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: PathBuf,
        title: Option<String>,
        provider_profile: Option<String>,
    ) -> Result<ConversationManifest, AppError> {
        validate_owner_id(owner_id)?;
        let workspace = validate_workspace_directory_text(
            &self.config.workspace_root,
            workspace.to_str().ok_or_else(|| {
                AppError::InvalidRequest("workspace path is not UTF-8".to_owned())
            })?,
        )?;
        let descriptor = self.registry.driver(provider)?.descriptor();
        if !descriptor.available {
            return Err(AppError::ProviderUnavailable(
                descriptor
                    .unavailable_reason
                    .unwrap_or_else(|| format!("provider {} is unavailable", provider.as_str())),
            ));
        }
        let provider_profile = normalize_profile(provider, provider_profile, &descriptor.profiles)?;
        let title = title
            .map(|title| title.trim().chars().take(200).collect::<String>())
            .filter(|title| !title.is_empty());
        let manifest = ConversationManifest::new(provider, workspace, title, provider_profile);
        let mut manifest = manifest;
        manifest.owner_id = owner_id.to_owned();
        manifest.workspace_id = Some(stable_workspace_id(&manifest.workspace));
        let manifest = self.store.create(manifest).await?;
        self.emit(
            &manifest.id,
            "conversation.created",
            json!({
                "provider": provider,
                "workspace": manifest.workspace,
                "providerProfile": manifest.provider_profile,
            }),
        )
        .await?;
        self.store.get(&manifest.id).await
    }

    #[allow(dead_code)]
    pub async fn list(&self) -> Result<Vec<ConversationManifest>, AppError> {
        self.store.list().await
    }

    pub async fn list_owned(&self, owner_id: &str) -> Result<Vec<ConversationManifest>, AppError> {
        validate_owner_id(owner_id)?;
        Ok(self
            .store
            .list()
            .await?
            .into_iter()
            .filter(|manifest| manifest.owner_id == owner_id)
            .collect())
    }

    pub async fn get(&self, conversation_id: &str) -> Result<ConversationManifest, AppError> {
        self.store.get(conversation_id).await
    }

    pub async fn get_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationManifest, AppError> {
        let manifest = self.get(conversation_id).await?;
        ensure_owner(&manifest, owner_id)?;
        Ok(manifest)
    }

    pub async fn update_metadata_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        title: Option<Option<String>>,
        archived: Option<bool>,
    ) -> Result<ConversationManifest, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        let updated = self
            .store
            .update_metadata(&manifest.id, title, archived)
            .await?;
        Ok(updated)
    }

    pub async fn delete_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationManifest, AppError> {
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        if self.active.contains_key(conversation_id)
            || matches!(
                manifest.status,
                ConversationStatus::Running | ConversationStatus::WaitingPermission
            )
        {
            return Err(AppError::Conflict(
                "active conversation cannot be deleted".to_owned(),
            ));
        }
        self.registry
            .driver(manifest.provider)?
            .shutdown_session(&manifest.id)
            .await;
        self.store.delete(&manifest.id).await?;
        Ok(manifest)
    }

    pub async fn cleanup_expired(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<ConversationManifest>, AppError> {
        // Prevent a new turn/fork reservation between capturing active IDs and cleanup.
        let _cli_gate = self.cli_execution_gate.write().await;
        let protected = self
            .active
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let removed = self.store.cleanup_before(cutoff, &protected).await?;
        for manifest in &removed {
            self.registry
                .driver(manifest.provider)?
                .shutdown_session(&manifest.id)
                .await;
        }
        Ok(removed)
    }

    pub async fn replay(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        self.store
            .replay(conversation_id, after_sequence, limit)
            .await
    }

    pub async fn replay_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<ConversationReplay, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        self.replay(conversation_id, after_sequence, limit).await
    }

    pub async fn retry_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        client_request_id: Option<String>,
    ) -> Result<String, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let latest = self
            .store
            .last_user_message(conversation_id)
            .await?
            .ok_or_else(|| {
                AppError::Conflict("conversation has no user message to retry".to_owned())
            })?;
        let snapshot = self.store.last_request(conversation_id).await?
            .ok_or_else(|| AppError::Unsupported("This older request has no complete retry snapshot; submit it explicitly with its attachments and settings.".to_owned()))?;
        if snapshot.get("turnId") != latest.payload.get("turnId") {
            return Err(AppError::Conflict(
                "The latest request snapshot is incomplete; retry was not submitted.".to_owned(),
            ));
        }
        let files: Vec<RequestFileFingerprint> = serde_json::from_value(snapshot["files"].clone())?;
        for file in files {
            if fingerprint_file(&file.path).await? != file.sha256 {
                return Err(AppError::Conflict("An attached file or skill changed since this request. Submit a new request to use its current contents.".to_owned()));
            }
        }
        let mut prompt: ConversationPrompt = serde_json::from_value(snapshot["request"].clone())?;
        prompt.client_request_id =
            client_request_id.or_else(|| Some(format!("retry_{}", Uuid::new_v4().simple())));
        self.prompt_inner(owner_id, conversation_id, prompt).await
    }

    pub async fn fork_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        title: Option<String>,
    ) -> Result<ConversationManifest, AppError> {
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let source = self.get_owned(owner_id, conversation_id).await?;
        let driver = self.registry.driver(source.provider)?;
        if !driver.supports_native_fork() {
            return Err(AppError::Unsupported(
                "Native conversation fork is not supported by this provider.".to_owned(),
            ));
        }
        let _cli_permit = self.cli_execution_gate.read().await;
        let (cancel, _) = watch::channel(false);
        match self.active.entry(conversation_id.to_owned()) {
            Entry::Occupied(_) => {
                return Err(AppError::Conflict(
                    "Wait for the active turn to finish before forking.".to_owned(),
                ))
            }
            Entry::Vacant(entry) => {
                entry.insert(ActiveTurn {
                    turn_id: "fork".to_owned(),
                    cancel,
                });
            }
        }
        let _cleanup = ActiveTurnCleanup {
            active: self.active.clone(),
            conversation_id: conversation_id.to_owned(),
        };
        let launch_permit = self
            .workspace_trust
            .acquire_owned(owner_id, &source.workspace)
            .await?;
        let provider_state = self.store.provider_state(conversation_id).await?;
        let history = self.store.complete_history(conversation_id).await?;
        let request = self.store.last_request(conversation_id).await?;
        let native_fork = driver
            .fork_session(
                DriverContext {
                    manifest: source.clone(),
                    provider_state,
                },
                launch_permit,
            )
            .await?;
        let mut fork = ConversationManifest::new(
            source.provider,
            source.workspace.clone(),
            title.or_else(|| source.title.as_ref().map(|value| format!("{value} (fork)"))),
            source.provider_profile.clone(),
        );
        fork.owner_id = owner_id.to_owned();
        fork.workspace_id = source.workspace_id.clone();
        let mut copied = Vec::with_capacity(history.len() + 1);
        for event in history {
            let mut next = crate::conversation::ConversationEvent::new(
                &fork.id,
                copied.len() as u64 + 1,
                event.event_type,
                event.payload,
            );
            next.provider = Some(source.provider);
            next.time = event.time;
            copied.push(next);
        }
        let mut completed = crate::conversation::ConversationEvent::new(
            &fork.id,
            copied.len() as u64 + 1,
            "conversation.forked",
            json!({ "sourceConversationId": source.id, "sourceSequence": copied.len() }),
        );
        completed.provider = Some(source.provider);
        copied.push(completed);
        self.store
            .create_with_history(fork, copied, Some(native_fork), request)
            .await
    }

    pub async fn compact_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        client_request_id: &str,
    ) -> Result<String, AppError> {
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        let driver = self.registry.driver(manifest.provider)?;
        if !driver.supports_native_compact() {
            return Err(AppError::Unsupported(
                "Native compaction is not supported by this provider.".to_owned(),
            ));
        }
        let _cli_permit = self.cli_execution_gate.read().await;
        let (cancel, cancel_rx) = watch::channel(false);
        let operation_id = format!("compact_{}", Uuid::new_v4().simple());
        match self.active.entry(conversation_id.to_owned()) {
            Entry::Occupied(_) => {
                return Err(AppError::Conflict(
                    "Wait for the active turn to finish before compacting.".to_owned(),
                ))
            }
            Entry::Vacant(entry) => {
                entry.insert(ActiveTurn {
                    turn_id: operation_id.clone(),
                    cancel,
                });
            }
        }
        let cleanup = ActiveTurnCleanup {
            active: self.active.clone(),
            conversation_id: conversation_id.to_owned(),
        };
        let launch_permit = self
            .workspace_trust
            .acquire_owned(owner_id, &manifest.workspace)
            .await?;
        let provider_state = self.store.provider_state(conversation_id).await?;
        if provider_state.native_session_id.is_none() {
            return Err(AppError::Unsupported(
                "Conversation has no native session to compact.".to_owned(),
            ));
        }
        self.emit(
            conversation_id,
            "compaction.started",
            json!({ "operationId": operation_id, "clientRequestId": client_request_id }),
        )
        .await?;
        let supervisor = self.clone();
        let conversation_id = conversation_id.to_owned();
        let request_id = client_request_id.to_owned();
        let spawned_id = operation_id.clone();
        tokio::spawn(async move {
            let _cleanup = cleanup;
            let result = driver
                .compact_session(
                    DriverContext {
                        manifest,
                        provider_state,
                    },
                    cancel_rx,
                    launch_permit,
                )
                .await;
            let (event_type, payload) = match result {
                Ok(()) => (
                    "compaction.completed",
                    json!({ "operationId": spawned_id, "clientRequestId": request_id }),
                ),
                Err(AppError::TurnCancelled) => (
                    "compaction.cancelled",
                    json!({ "operationId": spawned_id, "clientRequestId": request_id }),
                ),
                Err(error) => (
                    "compaction.failed",
                    json!({ "operationId": spawned_id, "clientRequestId": request_id, "code": error.code(), "message": error.to_string() }),
                ),
            };
            if let Err(error) = supervisor.emit(&conversation_id, event_type, payload).await {
                tracing::error!(conversation_id, error = %error, "failed to persist native compaction outcome");
            }
        });
        Ok(operation_id)
    }

    pub fn supports_native_compact(&self, provider: &str) -> bool {
        provider
            .parse::<ProviderKind>()
            .ok()
            .and_then(|kind| self.registry.driver(kind).ok())
            .is_some_and(|driver| driver.supports_native_compact())
    }

    pub fn supports_native_fork(&self, provider: &str) -> bool {
        provider
            .parse::<ProviderKind>()
            .ok()
            .and_then(|kind| self.registry.driver(kind).ok())
            .is_some_and(|driver| driver.supports_native_fork())
    }

    pub async fn refresh_control_capabilities(&self, provider: &str) {
        if let Some(driver) = provider
            .parse::<ProviderKind>()
            .ok()
            .and_then(|kind| self.registry.driver(kind).ok())
        {
            driver.refresh_control_capabilities().await;
        }
    }

    pub fn control_probe(&self, provider: &str) -> Option<Value> {
        provider
            .parse::<ProviderKind>()
            .ok()
            .and_then(|kind| self.registry.driver(kind).ok())
            .and_then(|driver| driver.control_probe())
    }

    pub fn supports_live_controls(&self, provider: &str) -> bool {
        provider
            .parse::<ProviderKind>()
            .ok()
            .and_then(|kind| self.registry.driver(kind).ok())
            .is_some_and(|driver| driver.supports_live_controls())
    }

    pub fn supports_native_queue(&self, provider: &str) -> bool {
        provider
            .parse::<ProviderKind>()
            .ok()
            .and_then(|kind| self.registry.driver(kind).ok())
            .is_some_and(|driver| driver.supports_native_queue())
    }

    /// Persist an intent before sending any non-idempotent control. A lost
    /// acknowledgement is never grounds for submitting an interjection twice.
    pub async fn control_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        expected_turn_id: &str,
        request_id: &str,
        control: ProviderControl,
    ) -> Result<Value, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        let guard = self.request_gate(conversation_id).lock_owned().await;
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        self.workspace_trust
            .ensure_trusted(owner_id, &manifest.workspace)
            .await?;
        validate_provider_control(request_id, expected_turn_id, &control)?;
        let serialized = serde_json::to_value(&control)?;
        let history = self.store.complete_history(conversation_id).await?;
        let prior = history.iter().find(|event| {
            event.event_type == "control.requested"
                && event.payload.get("requestId").and_then(Value::as_str) == Some(request_id)
        });
        if let Some(prior) = prior {
            if prior.payload.get("control") != Some(&serialized)
                || prior.payload.get("turnId").and_then(Value::as_str) != Some(expected_turn_id)
            {
                return Err(AppError::Conflict(
                    "Control request ID was already used with different input.".to_owned(),
                ));
            }
            if let Some(done) = history.iter().rev().find(|event| {
                matches!(
                    event.event_type.as_str(),
                    "control.completed" | "control.rejected" | "control.unknown"
                ) && event.payload.get("requestId").and_then(Value::as_str) == Some(request_id)
            }) {
                if done.event_type == "control.completed" {
                    return Ok(done.payload.get("result").cloned().unwrap_or(Value::Null));
                }
                if done.event_type == "control.unknown" {
                    return Err(AppError::ProviderUnavailable("Control delivery outcome is unknown; it was not sent again. Inspect the effective state before issuing a new request.".to_owned()));
                }
                return Err(AppError::Conflict(
                    done.payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Control was rejected.")
                        .to_owned(),
                ));
            }
            return Err(AppError::ProviderUnavailable("Control was already submitted; its outcome is unknown. Inspect the conversation record before trying a new request.".to_owned()));
        }
        let driver = self.registry.driver(manifest.provider)?;
        if !driver.supports_live_controls() {
            return Err(AppError::Unsupported(
                "This provider does not support live controls.".to_owned(),
            ));
        }
        if matches!(
            control,
            ProviderControl::QueueAdd { .. }
                | ProviderControl::QueueRemove { .. }
                | ProviderControl::QueueList
                | ProviderControl::QueueClear
        ) && !driver.supports_native_queue()
        {
            return Err(AppError::Unsupported(
                "This provider does not expose a native follow-up queue.".to_owned(),
            ));
        }
        let matches_active = self
            .active
            .get(conversation_id)
            .is_some_and(|active| active.turn_id == expected_turn_id && !*active.cancel.borrow());
        if !matches_active {
            return Err(AppError::Conflict(
                "The target turn is no longer running; settings were not changed.".to_owned(),
            ));
        }
        self.emit(
            conversation_id,
            "control.requested",
            json!({
                "turnId": expected_turn_id, "requestId": request_id, "control": serialized,
                "status": "pending",
            }),
        )
        .await?;
        let supervisor = self.clone();
        let conversation_id = conversation_id.to_owned();
        let turn_id = expected_turn_id.to_owned();
        let request_id = request_id.to_owned();
        // Continue to record the outcome even if the socket waiting for the ACK
        // disconnects. The detached task never retries the native command.
        tokio::spawn(async move {
            let _guard = guard;
            let result = driver
                .control(&conversation_id, &turn_id, &request_id, control)
                .await;
            let (event_type, payload) = match &result {
                Ok(value) => (
                    "control.completed",
                    json!({ "turnId": turn_id, "requestId": request_id, "result": value }),
                ),
                Err(error) => (
                    control_failure_event(error),
                    json!({ "turnId": turn_id, "requestId": request_id,
                    "message": error.to_string(), "code": error.code() }),
                ),
            };
            supervisor
                .emit(&conversation_id, event_type, payload)
                .await?;
            result
        })
        .await
        .map_err(|error| {
            AppError::Conflict(format!("Control outcome could not be confirmed: {error}"))
        })?
    }

    #[allow(dead_code)]
    pub async fn prompt(
        &self,
        conversation_id: &str,
        text: String,
        model: Option<String>,
    ) -> Result<String, AppError> {
        self.prompt_owned(
            "local",
            conversation_id,
            ConversationPrompt {
                client_request_id: None,
                text,
                model,
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
    }

    pub async fn prompt_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        prompt: ConversationPrompt,
    ) -> Result<String, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        self.prompt_inner(owner_id, conversation_id, prompt).await
    }

    fn request_gate(&self, conversation_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.request_gates
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn prompt_inner(
        &self,
        owner_id: &str,
        conversation_id: &str,
        prompt: ConversationPrompt,
    ) -> Result<String, AppError> {
        let request_fingerprint = prompt_fingerprint(&prompt)?;
        let request_snapshot = prompt.clone();
        let ConversationPrompt {
            client_request_id,
            text,
            model,
            reasoning_effort,
            skills,
            content,
            permission_mode,
            work_mode,
            permission_profile,
            sandbox_mode,
            approval_policy,
        } = prompt;
        if client_request_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 200)
        {
            return Err(AppError::InvalidRequest(
                "clientRequestId must contain 1 to 200 bytes".to_owned(),
            ));
        }
        let text = text.trim().to_owned();
        if text.is_empty() && skills.is_empty() && content.is_empty() {
            return Err(AppError::InvalidRequest(
                "prompt cannot be empty".to_owned(),
            ));
        }
        if text.len() > MAX_PROMPT_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "prompt exceeds {MAX_PROMPT_BYTES} bytes"
            )));
        }
        let manifest = self.store.get(conversation_id).await?;
        ensure_owner(&manifest, owner_id)?;
        self.workspace_trust
            .ensure_trusted(owner_id, &manifest.workspace)
            .await?;
        if let Some(request_id) = client_request_id.as_deref() {
            let history = self.store.complete_history(conversation_id).await?;
            if let Some(previous) = history.iter().find(|event| {
                event.event_type == "message.created"
                    && event.payload.get("clientRequestId").and_then(Value::as_str)
                        == Some(request_id)
            }) {
                let matches = if let Some(recorded) = previous
                    .payload
                    .get("requestFingerprint")
                    .and_then(Value::as_str)
                {
                    recorded == request_fingerprint
                } else {
                    // Native queue/steering deliveries have an original durable control,
                    // rather than a prompt snapshot. Recognize exactly that text-only
                    // submission so reconnect cannot execute its tools a second time.
                    let control_text = history.iter().find_map(|event| {
                        if event.event_type != "control.requested" {
                            return None;
                        }
                        let control = event.payload.get("control")?;
                        let matches_id = match control.get("action").and_then(Value::as_str) {
                            Some("queueAdd") => {
                                control.get("itemId").and_then(Value::as_str) == Some(request_id)
                            }
                            Some("steer") => {
                                event.payload.get("requestId").and_then(Value::as_str)
                                    == Some(request_id)
                            }
                            _ => false,
                        };
                        matches_id
                            .then(|| control.get("text").and_then(Value::as_str))
                            .flatten()
                    });
                    if let Some(control_text) = control_text {
                        request_snapshot.text == control_text
                            && request_snapshot.content.is_empty()
                            && request_snapshot.skills.is_empty()
                            && request_snapshot.model.is_none()
                            && request_snapshot.reasoning_effort.is_none()
                            && request_snapshot.permission_mode.is_none()
                            && request_snapshot.work_mode.is_none()
                            && request_snapshot.permission_profile.is_none()
                            && request_snapshot.sandbox_mode.is_none()
                            && request_snapshot.approval_policy.is_none()
                    } else if let Some(saved) = self.store.last_request(conversation_id).await? {
                        saved.get("turnId") == previous.payload.get("turnId")
                            && saved.get("request").is_some_and(|request| {
                                request
                                    == &serde_json::to_value(&request_snapshot)
                                        .unwrap_or(Value::Null)
                            })
                    } else {
                        false
                    }
                };
                if !matches {
                    return Err(AppError::Conflict(
                        "clientRequestId was already used with different or unverifiable input."
                            .to_owned(),
                    ));
                }
                if let Some(turn_id) = previous.payload.get("turnId").and_then(Value::as_str) {
                    return Ok(turn_id.to_owned());
                }
                return Err(AppError::Conflict(
                    "Request was recorded but its execution state is unknown.".to_owned(),
                ));
            }
        }
        let (content_text, driver_content) =
            prepare_prompt_content(manifest.provider, &manifest.workspace, content).await?;
        let loaded_skills = self.load_prompt_skills(&manifest, &skills).await?;
        let mut user_text = if text.is_empty() && !skills.is_empty() {
            "请使用已选择的 Skill。".to_owned()
        } else {
            text
        };
        if !content_text.is_empty() {
            if !user_text.is_empty() {
                user_text.push_str("\n\n");
            }
            user_text.push_str(&content_text);
        }
        let injected = loaded_skills
            .iter()
            .map(|skill| (skill.name.clone(), skill.content.clone()))
            .collect::<Vec<_>>();
        let provider_text = if manifest.provider == ProviderKind::Codex {
            user_text.clone()
        } else {
            compose_prompt_with_skills(&user_text, &injected)
        };
        if provider_text.len() > MAX_PROMPT_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "prompt exceeds {MAX_PROMPT_BYTES} bytes after skill injection"
            )));
        }
        let effective_permissions = super::types::resolve_execution_config(
            manifest.provider,
            permission_mode.as_deref(),
            work_mode.as_deref(),
            permission_profile.as_deref(),
            sandbox_mode.as_deref(),
            approval_policy.as_deref(),
        )?;
        let driver = self.registry.driver(manifest.provider)?;
        let descriptor = driver.descriptor();
        if !descriptor.available {
            return Err(AppError::ProviderUnavailable(
                descriptor
                    .unavailable_reason
                    .unwrap_or_else(|| "provider is unavailable".to_owned()),
            ));
        }
        let provider_state = self.store.provider_state(conversation_id).await?;

        let mut snapshot_files = Vec::new();
        for item in &driver_content {
            let path = match item {
                DriverPromptContent::File { path, .. } => Some(path),
                DriverPromptContent::Image { path, .. } => path.as_ref(),
            };
            if let Some(path) = path {
                snapshot_files.push(RequestFileFingerprint {
                    path: path.clone(),
                    sha256: fingerprint_file(path).await?,
                });
            }
        }
        for skill in &loaded_skills {
            snapshot_files.push(RequestFileFingerprint {
                path: skill.path.clone(),
                sha256: fingerprint_file(&skill.path).await?,
            });
        }
        let cli_start_permit = self.cli_execution_gate.read().await;
        let turn_id = format!("turn_{}", Uuid::new_v4().simple());
        let (cancel, cancel_rx) = watch::channel(false);
        match self.active.entry(conversation_id.to_owned()) {
            Entry::Occupied(entry) => {
                return Err(AppError::Conflict(format!(
                    "conversation {conversation_id} is already running turn {}",
                    entry.get().turn_id
                )));
            }
            Entry::Vacant(entry) => {
                entry.insert(ActiveTurn {
                    turn_id: turn_id.clone(),
                    cancel,
                });
            }
        }
        drop(cli_start_permit);

        let launch_permit = match self
            .workspace_trust
            .acquire_owned(owner_id, &manifest.workspace)
            .await
        {
            Ok(permit) => permit,
            Err(error) => {
                self.active.remove(conversation_id);
                return Err(error);
            }
        };

        if let Err(error) = self.store.save_request(conversation_id, &json!({
            "schemaVersion": 1, "turnId": turn_id, "request": request_snapshot, "files": snapshot_files,
        })).await {
            self.active.remove(conversation_id);
            return Err(error);
        }
        if let Err(error) = self
            .emit(
                conversation_id,
                "message.created",
                json!({ "turnId": turn_id, "clientRequestId": client_request_id, "requestFingerprint": request_fingerprint, "role": "user", "content": user_text }),
            )
            .await
        {
            self.active.remove(conversation_id);
            return Err(error);
        }
        if !injected.is_empty() {
            if let Err(error) = self
                .emit(
                    conversation_id,
                    "skill.injected",
                    json!({
                        "turnId": turn_id,
                        "skills": injected.iter().map(|(name, content)| json!({
                            "name": name,
                            "bytes": content.len(),
                        })).collect::<Vec<_>>(),
                    }),
                )
                .await
            {
                self.active.remove(conversation_id);
                return Err(error);
            }
        }
        if let Err(error) = self
            .emit(
                conversation_id,
                "turn.started",
                json!({ "turnId": turn_id, "clientRequestId": client_request_id, "provider": manifest.provider,
                    "requestedPermissions": { "permissionMode": permission_mode, "workMode": work_mode, "profile": permission_profile, "sandboxMode": sandbox_mode, "approvalPolicy": approval_policy },
                    "effectivePermissions": effective_permissions, "configurationStatus": "validated" }),
            )
            .await
        {
            self.active.remove(conversation_id);
            return Err(error);
        }

        tracing::info!(
            conversation_id,
            skill_count = injected.len(),
            prompt_bytes = provider_text.len(),
            "provider prompt includes injected skill context"
        );

        let supervisor = self.clone();
        let conversation_id = conversation_id.to_owned();
        let spawned_turn_id = turn_id.clone();
        let cleanup = ActiveTurnCleanup {
            active: self.active.clone(),
            conversation_id: conversation_id.clone(),
        };
        tokio::spawn(async move {
            let _cleanup = cleanup;
            let sink = DriverEventSink::new(
                supervisor.store.clone(),
                supervisor.hub.clone(),
                supervisor.permissions.clone(),
                conversation_id.clone(),
            )
            .with_turn_id(spawned_turn_id.clone());
            let result = driver
                .run_turn(
                    DriverContext {
                        manifest,
                        provider_state: provider_state.clone(),
                    },
                    DriverPrompt {
                        turn_id: spawned_turn_id.clone(),
                        text: provider_text,
                        content: driver_content,
                        skills: loaded_skills,
                        model,
                        reasoning_effort,
                        permission_mode,
                        work_mode,
                        permission_profile,
                        sandbox_mode,
                        approval_policy,
                    },
                    sink,
                    cancel_rx,
                    launch_permit,
                )
                .await;
            match result {
                Ok(result) if result.cancelled => {
                    if let Err(error) = supervisor
                        .emit(
                            &conversation_id,
                            "turn.cancelled",
                            json!({
                                "turnId": spawned_turn_id,
                                "clientRequestId": client_request_id,
                                "stopReason": result.stop_reason,
                                "nativeSessionId": result.native_session_id,
                            }),
                        )
                        .await
                    {
                        tracing::error!(conversation_id, error = %error, "failed to persist cancelled turn");
                    }
                }
                Ok(result) => {
                    if let Err(error) = supervisor
                        .emit(
                            &conversation_id,
                            "turn.completed",
                            json!({
                                "turnId": spawned_turn_id,
                                "clientRequestId": client_request_id,
                                "stopReason": result.stop_reason,
                                "nativeSessionId": result.native_session_id,
                            }),
                        )
                        .await
                    {
                        tracing::error!(conversation_id, error = %error, "failed to persist completed turn");
                    }
                }
                Err(AppError::TurnCancelled) => {
                    if let Err(error) = supervisor
                        .emit(
                            &conversation_id,
                            "turn.cancelled",
                            json!({
                                "turnId": spawned_turn_id,
                                "clientRequestId": client_request_id,
                                "stopReason": "cancelled",
                                "nativeSessionId": Value::Null,
                            }),
                        )
                        .await
                    {
                        tracing::error!(conversation_id, error = %error, "failed to persist cancelled turn");
                    }
                }
                Err(error) => {
                    // A driver may have persisted a new native session or an
                    // extension switch during this turn. Do not roll it back.
                    let mut state = supervisor
                        .store
                        .provider_state(&conversation_id)
                        .await
                        .unwrap_or(provider_state);
                    state.last_error = Some(error.to_string().chars().take(1000).collect());
                    if let Err(save_error) = supervisor
                        .store
                        .save_provider_state(&conversation_id, state)
                        .await
                    {
                        tracing::error!(conversation_id, error = %save_error, "failed to persist provider error state");
                    }
                    if let Err(save_error) = supervisor
                        .emit(
                            &conversation_id,
                            "turn.failed",
                            json!({
                                "turnId": spawned_turn_id,
                                "clientRequestId": client_request_id,
                                "code": error.code(),
                                "message": error.to_string(),
                            }),
                        )
                        .await
                    {
                        tracing::error!(conversation_id, error = %save_error, "failed to persist failed turn");
                    }
                }
            }
        });
        Ok(turn_id)
    }

    #[allow(dead_code)]
    pub async fn cancel(&self, conversation_id: &str) -> Result<(), AppError> {
        self.cancel_owned("local", conversation_id).await
    }

    pub async fn cancel_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        ensure_owner(&self.store.get(conversation_id).await?, owner_id)?;
        let Some(active) = self.active.get(conversation_id) else {
            return Ok(());
        };
        active
            .cancel
            .send(true)
            .map_err(|_| AppError::Conflict("turn has already stopped".to_owned()))
    }

    pub async fn cancel_workspace_owned(
        &self,
        owner_id: &str,
        workspace: &Path,
    ) -> Result<usize, AppError> {
        let mut cancelled = 0;
        for manifest in self.list_owned(owner_id).await? {
            if manifest.workspace != workspace {
                continue;
            }
            let Some(active) = self.active.get(&manifest.id) else {
                continue;
            };
            if active.cancel.send(true).is_ok() {
                cancelled += 1;
            }
        }
        Ok(cancelled)
    }

    #[allow(dead_code)]
    pub async fn resolve_permission(
        &self,
        conversation_id: &str,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AppError> {
        self.resolve_permission_owned("local", conversation_id, permission_id, decision)
            .await
    }

    pub async fn resolve_permission_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AppError> {
        ensure_owner(&self.store.get(conversation_id).await?, owner_id)?;
        self.permissions
            .resolve(conversation_id, permission_id, decision)
            .await
    }

    pub fn subscribe(
        &self,
        conversation_id: &str,
    ) -> tokio::sync::broadcast::Receiver<crate::conversation::ConversationEvent> {
        self.hub.subscribe(conversation_id)
    }

    pub async fn list_mcp_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<crate::catalog::McpCatalog, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        let mut catalog = self
            .catalog
            .mcp(manifest.provider, manifest.workspace.clone())
            .await?;
        catalog.servers.retain(|server| server.enabled);
        Ok(catalog)
    }

    pub async fn refresh_mcp_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        resource_id: &str,
    ) -> Result<crate::catalog::McpServerDescriptor, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        self.workspace_trust
            .ensure_trusted(owner_id, &manifest.workspace)
            .await?;
        let mut target = self
            .catalog
            .mcp_target(manifest.provider, manifest.workspace.clone(), resource_id)
            .await?;
        match mcp::list_tools(&target).await {
            Ok(tools) => {
                target.descriptor.tools = tools;
                target.descriptor.error = None;
                target.descriptor.auth_status = Some("ready".to_owned());
            }
            Err(error) => {
                target.descriptor.error = Some(error.to_string());
                target.descriptor.auth_status = Some("error".to_owned());
            }
        }
        Ok(target.descriptor)
    }

    pub async fn call_mcp_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        resource_id: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        self.workspace_trust
            .ensure_trusted(owner_id, &manifest.workspace)
            .await?;
        let target = self
            .catalog
            .mcp_target(manifest.provider, manifest.workspace.clone(), resource_id)
            .await?;
        let request_id = format!("mcp_{}", Uuid::new_v4().simple());
        self.emit(
            conversation_id,
            "mcp.requested",
            json!({
                "requestId": request_id,
                "resourceId": resource_id,
                "server": target.descriptor.name,
                "tool": tool_name,
            }),
        )
        .await?;
        let sink = DriverEventSink::new(
            self.store.clone(),
            self.hub.clone(),
            self.permissions.clone(),
            conversation_id,
        );
        let (_cancel_tx, mut cancel_rx) = watch::channel(false);
        let decision = sink
            .request_permission(
                request_id.clone(),
                "mcp_tool",
                format!("Allow MCP tool {}", tool_name),
                json!({
                    "server": target.descriptor.name,
                    "tool": tool_name,
                    "resourceId": resource_id,
                }),
                json!([
                    { "id": "allow_once", "label": "Allow once" },
                    { "id": "reject_once", "label": "Reject" }
                ]),
                &mut cancel_rx,
            )
            .await?;
        if !matches!(
            decision.outcome,
            PermissionOutcome::AllowOnce | PermissionOutcome::AllowAlways
        ) {
            self.emit(
                conversation_id,
                "mcp.failed",
                json!({
                    "requestId": request_id,
                    "code": "PERMISSION_DENIED",
                    "message": "mcp tool call was rejected",
                }),
            )
            .await?;
            return Err(AppError::Unauthorized(
                "mcp tool call was rejected".to_owned(),
            ));
        }
        self.emit(
            conversation_id,
            "mcp.started",
            json!({
                "requestId": request_id,
                "server": target.descriptor.name,
                "tool": tool_name,
            }),
        )
        .await?;
        match mcp::call_tool(&target, tool_name, arguments).await {
            Ok(result) => {
                let event_type = if result.is_error {
                    "mcp.failed"
                } else {
                    "mcp.completed"
                };
                self.emit(
                    conversation_id,
                    event_type,
                    json!({
                        "requestId": request_id,
                        "server": target.descriptor.name,
                        "tool": tool_name,
                        "result": result.content,
                    }),
                )
                .await?;
                if result.is_error {
                    Err(AppError::InvalidRequest(
                        "mcp tool returned an error".to_owned(),
                    ))
                } else {
                    Ok(result.content)
                }
            }
            Err(error) => {
                self.emit(
                    conversation_id,
                    "mcp.failed",
                    json!({
                        "requestId": request_id,
                        "code": error.code(),
                        "message": error.to_string(),
                    }),
                )
                .await?;
                Err(error)
            }
        }
    }

    async fn load_prompt_skills(
        &self,
        manifest: &ConversationManifest,
        skills: &[PromptSkillRef],
    ) -> Result<Vec<DriverSkill>, AppError> {
        let mut injected = Vec::new();
        for skill in skills {
            let resource = self
                .catalog
                .skill_resource(
                    manifest.provider,
                    manifest.workspace.clone(),
                    &skill.resource_id,
                )
                .await?;
            if !resource.descriptor.valid || !resource.descriptor.active {
                return Err(AppError::InvalidRequest(format!(
                    "skill {} is not active",
                    resource.descriptor.name
                )));
            }
            if let Some(name) = skill.name.as_deref() {
                if !name.trim().is_empty() && name.trim() != resource.descriptor.name {
                    return Err(AppError::InvalidRequest(format!(
                        "skill name '{name}' does not match resource {}",
                        resource.descriptor.name
                    )));
                }
            }
            injected.push(DriverSkill {
                name: resource.descriptor.name,
                path: resource.descriptor.path,
                content: resource.content,
            });
        }
        Ok(injected)
    }

    pub async fn shutdown_all(&self) {
        for entry in self.active.iter() {
            let _ = entry.cancel.send(true);
        }
        self.permissions.expire_all();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while !self.active.is_empty() && Instant::now() < deadline {
            sleep(Duration::from_millis(25)).await;
        }
        if !self.active.is_empty() {
            tracing::warn!(
                active_turns = self.active.len(),
                "provider turns did not stop before shutdown deadline"
            );
        }
        for driver in self.registry.drivers.values() {
            driver.shutdown().await;
        }
    }

    async fn emit(
        &self,
        conversation_id: &str,
        event_type: &str,
        payload: Value,
    ) -> Result<(), AppError> {
        self.store
            .append_and_publish(conversation_id, event_type, payload, &self.hub)
            .await?;
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RequestFileFingerprint {
    path: PathBuf,
    sha256: String,
}

async fn fingerprint_file(path: &Path) -> Result<String, AppError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer).await?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn prepare_prompt_content(
    provider: ProviderKind,
    workspace: &Path,
    content: Vec<PromptContentRef>,
) -> Result<(String, Vec<DriverPromptContent>), AppError> {
    if content.len() > MAX_PROMPT_CONTENT_ITEMS {
        return Err(AppError::InvalidRequest(format!(
            "prompt content allows at most {MAX_PROMPT_CONTENT_ITEMS} items"
        )));
    }
    let workspace = tokio::fs::canonicalize(workspace).await?;
    let mut text = Vec::new();
    let mut driver_content = Vec::new();
    let mut image_bytes = 0usize;
    for item in content {
        match item {
            PromptContentRef::Text { text: value } => {
                if !value.trim().is_empty() {
                    text.push(value);
                }
            }
            PromptContentRef::LocalImage { path } => {
                ensure_image_provider(provider)?;
                let path = canonical_workspace_file(&workspace, path).await?;
                let file_bytes =
                    usize::try_from(tokio::fs::metadata(&path).await?.len()).unwrap_or(usize::MAX);
                ensure_image_budget(image_bytes.saturating_add(file_bytes))?;
                let bytes = tokio::fs::read(&path).await?;
                image_bytes = image_bytes.saturating_add(bytes.len());
                driver_content.push(DriverPromptContent::Image {
                    mime_type: image_mime_type(&path)?.to_owned(),
                    data: BASE64_STANDARD.encode(bytes),
                    path: Some(path),
                });
            }
            PromptContentRef::Image { data, mime_type } => {
                ensure_image_provider(provider)?;
                let mime_type = mime_type.trim().to_ascii_lowercase();
                if !matches!(
                    mime_type.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    return Err(AppError::InvalidRequest(format!(
                        "unsupported prompt image MIME type {mime_type:?}"
                    )));
                }
                let data = data.trim();
                let remaining = MAX_PROMPT_IMAGE_BYTES.saturating_sub(image_bytes);
                let max_encoded_len = remaining.saturating_add(2) / 3 * 4 + 4;
                if data.len() > max_encoded_len {
                    return Err(AppError::InvalidRequest(format!(
                        "prompt images exceed {MAX_PROMPT_IMAGE_BYTES} decoded bytes"
                    )));
                }
                let bytes = BASE64_STANDARD.decode(data).map_err(|error| {
                    AppError::InvalidRequest(format!("invalid prompt image base64: {error}"))
                })?;
                image_bytes = image_bytes.saturating_add(bytes.len());
                ensure_image_budget(image_bytes)?;
                driver_content.push(DriverPromptContent::Image {
                    path: None,
                    data: BASE64_STANDARD.encode(bytes),
                    mime_type,
                });
            }
            PromptContentRef::File { path, name } => {
                let path = canonical_workspace_file(&workspace, path).await?;
                let fallback = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("file");
                let name = name.as_deref().unwrap_or(fallback).trim().to_owned();
                if name.is_empty() || name.len() > 255 || name.contains(['\r', '\n']) {
                    return Err(AppError::InvalidRequest(
                        "prompt file name is invalid".to_owned(),
                    ));
                }
                let relative = path.strip_prefix(&workspace).unwrap_or(&path);
                if provider != ProviderKind::Codex {
                    text.push(format!("Attached file: @{}", relative.display()));
                }
                driver_content.push(DriverPromptContent::File { path, name });
            }
        }
    }
    Ok((text.join("\n\n"), driver_content))
}

async fn canonical_workspace_file(workspace: &Path, path: PathBuf) -> Result<PathBuf, AppError> {
    let candidate = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    let canonical = tokio::fs::canonicalize(&candidate).await.map_err(|error| {
        AppError::InvalidRequest(format!(
            "prompt file {} is not readable: {error}",
            candidate.display()
        ))
    })?;
    if !canonical.starts_with(workspace) {
        return Err(AppError::InvalidRequest(
            "prompt files must stay inside the trusted workspace".to_owned(),
        ));
    }
    if !tokio::fs::metadata(&canonical).await?.is_file() {
        return Err(AppError::InvalidRequest(format!(
            "prompt path {} is not a regular file",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn ensure_image_provider(provider: ProviderKind) -> Result<(), AppError> {
    if provider == ProviderKind::Acp || provider.supports_image_input() {
        Ok(())
    } else {
        Err(AppError::Unsupported(format!(
            "provider {provider:?} does not support typed prompt images"
        )))
    }
}

fn ensure_image_budget(bytes: usize) -> Result<(), AppError> {
    if bytes <= MAX_PROMPT_IMAGE_BYTES {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "prompt images exceed {MAX_PROMPT_IMAGE_BYTES} decoded bytes"
        )))
    }
}

fn image_mime_type(path: &Path) -> Result<&'static str, AppError> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("gif") => Ok("image/gif"),
        Some("webp") => Ok("image/webp"),
        _ => Err(AppError::InvalidRequest(format!(
            "unsupported prompt image file {}",
            path.display()
        ))),
    }
}

pub(crate) fn compose_prompt_with_skills(user_text: &str, skills: &[(String, String)]) -> String {
    if skills.is_empty() {
        return user_text.to_owned();
    }
    let mut composed = String::from(
        "The following skills are attached to this request. Follow their instructions.\n",
    );
    for (name, content) in skills {
        composed.push_str("\n<skill name=\"");
        composed.push_str(name);
        composed.push_str("\">\n");
        composed.push_str(content);
        composed.push_str("\n</skill>\n");
    }
    composed.push('\n');
    composed.push_str(user_text);
    composed
}

fn validate_owner_id(owner_id: &str) -> Result<(), AppError> {
    if owner_id.trim().is_empty() || owner_id.len() > 256 {
        Err(AppError::InvalidRequest("owner id is invalid".to_owned()))
    } else {
        Ok(())
    }
}

fn validate_provider_control(
    request_id: &str,
    turn_id: &str,
    control: &ProviderControl,
) -> Result<(), AppError> {
    let valid_id = |value: &str| !value.trim().is_empty() && value.len() <= 200;
    if !valid_id(request_id) || !valid_id(turn_id) {
        return Err(AppError::InvalidRequest(
            "Control requires a request ID and expectedTurnId (1–200 bytes).".to_owned(),
        ));
    }
    match control {
        ProviderControl::Steer { text } | ProviderControl::QueueAdd { text, .. }
            if text.trim().is_empty() || text.len() > MAX_PROMPT_BYTES =>
        {
            return Err(AppError::InvalidRequest(
                "Control text must be nonempty and at most 512 KiB.".to_owned(),
            ));
        }
        ProviderControl::Configure {
            model,
            reasoning_effort,
        } => {
            if model.is_none() && reasoning_effort.is_none() {
                return Err(AppError::InvalidRequest(
                    "Configure requires model or reasoningEffort.".to_owned(),
                ));
            }
            if model
                .iter()
                .chain(reasoning_effort.iter())
                .any(|value| value.trim().is_empty() || value.len() > 200)
            {
                return Err(AppError::InvalidRequest(
                    "Configuration values must contain 1–200 bytes.".to_owned(),
                ));
            }
        }
        _ => {}
    }
    if let ProviderControl::QueueAdd { item_id, .. } | ProviderControl::QueueRemove { item_id } =
        control
    {
        if !valid_id(item_id) {
            return Err(AppError::InvalidRequest(
                "Queue itemId must contain 1–200 bytes.".to_owned(),
            ));
        }
    }
    Ok(())
}

fn ensure_owner(manifest: &ConversationManifest, owner_id: &str) -> Result<(), AppError> {
    validate_owner_id(owner_id)?;
    if manifest.owner_id != owner_id {
        return Err(AppError::NotFound("conversation".to_owned()));
    }
    Ok(())
}

fn normalize_profile(
    provider: ProviderKind,
    requested: Option<String>,
    profiles: &[String],
) -> Result<Option<String>, AppError> {
    if provider != ProviderKind::Acp {
        if requested.is_some() {
            return Err(AppError::InvalidRequest(
                "providerProfile is only valid for ACP conversations".to_owned(),
            ));
        }
        return Ok(None);
    }

    let requested = requested
        .map(|profile| profile.trim().to_owned())
        .filter(|profile| !profile.is_empty())
        .or_else(|| (profiles.len() == 1).then(|| profiles[0].clone()))
        .ok_or_else(|| AppError::InvalidRequest("ACP providerProfile is required".to_owned()))?;
    if !profiles.contains(&requested) {
        return Err(AppError::InvalidRequest(format!(
            "ACP profile '{requested}' is not configured"
        )));
    }
    Ok(Some(requested))
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;

    use super::*;
    use crate::config::{AcpProfileConfig, AgentConfig, PairingEncryption, SecurityConfig};
    use crate::conversation::{ConversationEventHub, ConversationStore};

    async fn trust_store(
        config: &Config,
        owner_id: &str,
        workspace: Option<&Path>,
    ) -> WorkspaceTrustStore {
        let trust =
            WorkspaceTrustStore::new(config.data_dir.clone(), config.workspace_root.clone())
                .await
                .unwrap();
        if let Some(workspace) = workspace {
            trust.set_owned(owner_id, workspace, true).await.unwrap();
        }
        trust
    }

    async fn control_fixture(
        label: &str,
    ) -> (PathBuf, ConversationStore, ConversationSupervisor, PathBuf) {
        let root = temp_dir(label);
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let executable = write_provider_fixture(&root).to_string_lossy().to_string();
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("test-token".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let trust = trust_store(&config, "local", Some(&workspace)).await;
        let supervisor = ConversationSupervisor::new(
            config,
            store.clone(),
            ConversationEventHub::default(),
            trust,
        );
        (root, store, supervisor, workspace)
    }

    async fn wait_until_idle(supervisor: &ConversationSupervisor) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while supervisor.has_active_turns() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture operation should complete");
    }

    #[tokio::test]
    async fn retry_preserves_complete_request_after_first_page_and_rejects_changed_files() {
        let (root, store, supervisor, workspace) = control_fixture("todex-retry-snapshot").await;
        let manifest =
            ConversationManifest::new(ProviderKind::Codex, workspace.clone(), None, None);
        let history = (1..=1201)
            .map(|seq| {
                crate::conversation::ConversationEvent::new(
                    &manifest.id,
                    seq,
                    "message.created",
                    json!({ "role": "user", "content": "old", "turnId": format!("old-{seq}") }),
                )
            })
            .collect();
        store
            .create_with_history(manifest.clone(), history, None, None)
            .await
            .unwrap();
        assert!(matches!(
            supervisor.retry_owned("local", &manifest.id, None).await,
            Err(AppError::Unsupported(_))
        ));
        let file = workspace.join("attachment.txt");
        fs::write(&file, "original attachment").unwrap();
        let request = ConversationPrompt {
            client_request_id: Some("submit-1".to_owned()),
            text: "latest question".to_owned(),
            model: Some("fixture-model".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            skills: Vec::new(),
            content: vec![
                PromptContentRef::File {
                    path: file.clone(),
                    name: Some("attachment.txt".to_owned()),
                },
                PromptContentRef::Image {
                    data: "eA==".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ],
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: Some("read-only".to_owned()),
            approval_policy: Some("on-request".to_owned()),
        };
        supervisor
            .prompt_owned("local", &manifest.id, request.clone())
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let first_snapshot = store.last_request(&manifest.id).await.unwrap().unwrap();
        assert_eq!(
            first_snapshot["request"],
            serde_json::to_value(&request).unwrap()
        );
        supervisor
            .retry_owned("local", &manifest.id, Some("retry-2".to_owned()))
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let mut retried =
            store.last_request(&manifest.id).await.unwrap().unwrap()["request"].clone();
        assert_eq!(retried["clientRequestId"], "retry-2");
        retried["clientRequestId"] = json!("submit-1");
        assert_eq!(retried, first_snapshot["request"]);
        let last = store
            .last_user_message(&manifest.id)
            .await
            .unwrap()
            .unwrap();
        assert!(last.sequence > 1201);
        assert_eq!(last.payload["content"], "latest question");
        let events = store.complete_history(&manifest.id).await.unwrap();
        assert!(events.iter().any(|event| event.event_type == "turn.started"
            && event.payload["clientRequestId"] == "retry-2"));
        let before = store.get(&manifest.id).await.unwrap().last_sequence;
        fs::write(file, "changed attachment").unwrap();
        assert!(matches!(
            supervisor.retry_owned("local", &manifest.id, None).await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(store.get(&manifest.id).await.unwrap().last_sequence, before);
        assert!(!supervisor.has_active_turns());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn native_fork_keeps_full_history_and_uses_distinct_provider_session() {
        let (root, store, supervisor, workspace) = control_fixture("todex-native-fork").await;
        let manifest = ConversationManifest::new(
            ProviderKind::Codex,
            workspace,
            Some("source".to_owned()),
            None,
        );
        let history = (1..=1201)
            .map(|seq| {
                crate::conversation::ConversationEvent::new(
                    &manifest.id,
                    seq,
                    "message.created",
                    json!({ "role": "user", "content": format!("message-{seq}") }),
                )
            })
            .collect();
        let mut native = crate::conversation::ProviderState::new(ProviderKind::Codex);
        native.native_session_id = Some("codex-native".to_owned());
        store
            .create_with_history(manifest.clone(), history, Some(native), None)
            .await
            .unwrap();
        let fork = supervisor
            .fork_owned("local", &manifest.id, None)
            .await
            .unwrap();
        assert_eq!(fork.last_sequence, 1202);
        assert_eq!(fork.status, ConversationStatus::Idle);
        let forked = store.complete_history(&fork.id).await.unwrap();
        assert_eq!(forked[1200].payload["content"], "message-1201");
        assert_eq!(forked[1201].event_type, "conversation.forked");
        assert!(forked.iter().all(|event| event.conversation_id == fork.id));
        assert_eq!(
            store
                .provider_state(&fork.id)
                .await
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("codex-fork-native")
        );
        assert_eq!(
            store
                .provider_state(&manifest.id)
                .await
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("codex-native")
        );
        supervisor
            .compact_owned("local", &manifest.id, "compact-1")
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(history.last().unwrap().event_type, "compaction.completed");
        assert_eq!(
            history.last().unwrap().payload["clientRequestId"],
            "compact-1"
        );
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Idle
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reserved_operations_are_protected_from_delete_and_retention() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-retention-reservation").await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        let (cancel, _) = watch::channel(false);
        supervisor.active.insert(
            manifest.id.clone(),
            ActiveTurn {
                turn_id: "preparing".to_owned(),
                cancel,
            },
        );
        assert!(matches!(
            supervisor.delete_owned("local", &manifest.id).await,
            Err(AppError::Conflict(_))
        ));
        let cutoff = Utc::now() + chrono::Duration::hours(1);
        assert!(supervisor.cleanup_expired(cutoff).await.unwrap().is_empty());
        assert!(store.get(&manifest.id).await.is_ok());
        supervisor.active.remove(&manifest.id);
        assert_eq!(supervisor.cleanup_expired(cutoff).await.unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn restart_resolves_stale_permissions_and_keeps_interrupted_status() {
        let (root, store, supervisor, workspace) = control_fixture("todex-recover-approval").await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&manifest.id, "turn.started", json!({ "turnId": "t" }))
            .await
            .unwrap();
        store
            .append(
                &manifest.id,
                "permission.requested",
                json!({ "permissionId": "p", "turnId": "t" }),
            )
            .await
            .unwrap();
        supervisor.recover_all().await.unwrap();
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        let history = store.complete_history(&manifest.id).await.unwrap();
        let terminal = history
            .iter()
            .find(|event| event.event_type == "permission.resolved")
            .unwrap();
        assert_eq!(terminal.payload["outcome"], "cancelled");
        assert_eq!(terminal.payload["reason"], "daemon_restarted");
        let count = history.len();
        supervisor.recover_all().await.unwrap();
        assert_eq!(
            store.complete_history(&manifest.id).await.unwrap().len(),
            count
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_first_release_drivers_complete_native_fixture_turns() {
        let root = temp_dir("todex-provider-contract");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let fixture = write_provider_fixture(&root);
        let fixture_text = fixture.to_string_lossy().to_string();
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "fixture".to_owned(),
            AcpProfileConfig {
                command: fixture_text.clone(),
                args: vec!["acp".to_owned()],
                env: BTreeMap::new(),
            },
        );
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture_text.clone(),
                claude_bin: fixture_text.clone(),
                pi_bin: fixture_text.clone(),
                grok_bin: fixture_text,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: profiles,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("test-token".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let trust = trust_store(&config, "local", Some(&workspace)).await;
        let supervisor = ConversationSupervisor::new(
            config,
            store.clone(),
            ConversationEventHub::default(),
            trust,
        );
        assert!(supervisor
            .providers()
            .iter()
            .all(|descriptor| descriptor.available));

        for provider in ProviderKind::ALL {
            let profile = (provider == ProviderKind::Acp).then(|| "fixture".to_owned());
            let manifest = supervisor
                .create(
                    provider,
                    workspace.clone(),
                    Some(format!("{} fixture", provider.as_str())),
                    profile,
                )
                .await
                .unwrap();
            supervisor
                .prompt(
                    &manifest.id,
                    format!("hello from {}", provider.as_str()),
                    None,
                )
                .await
                .unwrap();
            let replay = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let replay = supervisor.replay(&manifest.id, 0, 100).await.unwrap();
                    if replay.events.iter().any(|event| {
                        matches!(event.event_type.as_str(), "turn.completed" | "turn.failed")
                    }) {
                        return replay;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture provider turn should finish");
            assert!(
                replay
                    .events
                    .iter()
                    .any(|event| event.event_type == "turn.completed"),
                "{} fixture failed: {:?}",
                provider.as_str(),
                replay
                    .events
                    .iter()
                    .filter(|event| event.event_type == "turn.failed")
                    .map(|event| &event.payload)
                    .collect::<Vec<_>>()
            );
            let state = store.provider_state(&manifest.id).await.unwrap();
            assert!(state.recoverable);
            assert!(state.native_session_id.is_some());
        }

        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pi_launches_only_after_workspace_trust_and_keeps_full_auto_argv() {
        let root = temp_dir("todex-pi-trust-contract");
        let data_dir = root.join("data");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let marker = root.join("pi-launches.log");
        let fixture = write_pi_launch_fixture(&root, &marker);
        let fixture_text = fixture.to_string_lossy().to_string();
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir,
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "pi".to_owned(),
                codex_bin: fixture_text.clone(),
                claude_bin: fixture_text.clone(),
                pi_bin: fixture_text.clone(),
                grok_bin: fixture_text,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let trust = trust_store(&config, "local", None).await;
        let supervisor = ConversationSupervisor::new(
            config,
            store.clone(),
            ConversationEventHub::default(),
            trust.clone(),
        );
        let manifest = supervisor
            .create(ProviderKind::Pi, workspace.clone(), None, None)
            .await
            .unwrap();

        assert!(matches!(
            supervisor
                .models_live("local", ProviderKind::Pi, &workspace)
                .await,
            Err(AppError::WorkspaceTrustRequired(_))
        ));
        assert!(matches!(
            supervisor
                .commands_live("local", ProviderKind::Pi, &workspace)
                .await,
            Err(AppError::WorkspaceTrustRequired(_))
        ));
        assert!(matches!(
            supervisor
                .prompt(&manifest.id, "before trust".to_owned(), None)
                .await,
            Err(AppError::WorkspaceTrustRequired(_))
        ));
        assert!(!marker.exists(), "untrusted Pi must not be spawned");

        trust.set_owned("local", &workspace, true).await.unwrap();
        supervisor
            .models_live("local", ProviderKind::Pi, &workspace)
            .await
            .unwrap();
        supervisor
            .commands_live("local", ProviderKind::Pi, &workspace)
            .await
            .unwrap();
        supervisor
            .prompt(&manifest.id, "after trust".to_owned(), None)
            .await
            .unwrap();
        let replay = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let replay = supervisor.replay(&manifest.id, 0, 100).await.unwrap();
                if replay
                    .events
                    .iter()
                    .any(|event| event.event_type == "turn.completed")
                {
                    break replay;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!replay
            .events
            .iter()
            .any(|event| event.event_type == "permission.requested"));
        assert!(
            !supervisor
                .providers()
                .into_iter()
                .find(|descriptor| descriptor.id == ProviderKind::Pi)
                .unwrap()
                .capabilities
                .permissions
        );

        let launches = fs::read_to_string(&marker).unwrap();
        let launches = launches.lines().collect::<Vec<_>>();
        assert_eq!(launches.len(), 3, "unexpected Pi launches: {launches:?}");
        assert!(launches.iter().all(|args| {
            args.split_whitespace()
                .filter(|arg| *arg == "--approve")
                .count()
                == 1
        }));
        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn conversation_owner_scope_prevents_cross_owner_access() {
        let root = temp_dir("todex-owner-scope");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let fixture = write_provider_fixture(&root);
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture.to_string_lossy().to_string(),
                claude_bin: fixture.to_string_lossy().to_string(),
                pi_bin: fixture.to_string_lossy().to_string(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let trust = trust_store(&config, "owner-a", None).await;
        let supervisor =
            ConversationSupervisor::new(config, store, ConversationEventHub::default(), trust);
        let manifest = supervisor
            .create_owned("owner-a", ProviderKind::Codex, workspace, None, None)
            .await
            .unwrap();
        assert!(supervisor.get_owned("owner-a", &manifest.id).await.is_ok());
        assert!(matches!(
            supervisor.get_owned("owner-b", &manifest.id).await,
            Err(AppError::NotFound(_))
        ));
        assert!(supervisor.list_owned("owner-b").await.unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_provider_state_does_not_reserve_a_turn_slot() {
        let root = temp_dir("todex-provider-state-failure");
        let data_dir = root.join("data");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let fixture = write_provider_fixture(&root);
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: data_dir.clone(),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture.to_string_lossy().to_string(),
                claude_bin: fixture.to_string_lossy().to_string(),
                pi_bin: fixture.to_string_lossy().to_string(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        });
        let store = ConversationStore::new(data_dir.clone()).await.unwrap();
        let trust = trust_store(&config, "local", Some(&workspace)).await;
        let supervisor = ConversationSupervisor::new(
            config,
            store.clone(),
            ConversationEventHub::default(),
            trust,
        );
        let manifest = supervisor
            .create(ProviderKind::Codex, workspace, None, None)
            .await
            .unwrap();
        fs::write(
            data_dir
                .join("conversations")
                .join(&manifest.id)
                .join("provider-state.json"),
            "{ malformed",
        )
        .unwrap();

        assert!(supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .is_err());
        assert!(supervisor.active.is_empty());
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Idle
        );
        assert_eq!(
            store
                .replay(&manifest.id, 0, 10)
                .await
                .unwrap()
                .events
                .len(),
            1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    fn write_provider_fixture(root: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("provider-fixture.sh");
        fs::create_dir_all(root).unwrap();
        fs::write(
            &path,
            r#"#!/bin/sh
mode="$1"
extract_id() {
  printf '%s' "$1" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'
}

if [ "$mode" = "--no-auto-update" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*)
        printf '{"jsonrpc":"2.0","id":"initialize","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"_meta":{"modelState":{"currentModelId":"grok-fixture","availableModels":[{"modelId":"grok-fixture","name":"Grok Fixture","_meta":{"supportsReasoningEffort":true}}]}}}}\n'
        ;;
      *'"method":"session/new"'*)
        printf '{"jsonrpc":"2.0","id":"session","result":{"sessionId":"grok-native","models":{"currentModelId":"grok-fixture","availableModels":[{"modelId":"grok-fixture","name":"Grok Fixture","_meta":{"supportsReasoningEffort":true}}]}}}\n'
        ;;
      *'"method":"session/load"'*)
        printf '{"jsonrpc":"2.0","id":"session","result":{"models":{"currentModelId":"grok-fixture","availableModels":[{"modelId":"grok-fixture","name":"Grok Fixture","_meta":{"supportsReasoningEffort":true}}]}}}\n'
        ;;
      *'"method":"session/prompt"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
        ;;
    esac
  done
elif [ "$mode" = "acp" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*)
        printf '{"jsonrpc":"2.0","id":"initialize","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n'
        ;;
      *'"method":"session/new"'*)
        printf '{"jsonrpc":"2.0","id":"session","result":{"sessionId":"acp-native"}}\n'
        ;;
      *'"method":"session/load"'*)
        printf '{"jsonrpc":"2.0","id":"session","result":{}}\n'
        ;;
      *'"method":"session/prompt"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
        ;;
    esac
  done
elif [ "$mode" = "app-server" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*)
        printf '{"id":"initialize","result":{}}\n'
        ;;
      *'"method":"initialized"'*)
        ;;
      *'"method":"thread/start"'*)
        printf '{"id":"thread","result":{"thread":{"id":"codex-native"}}}\n'
        ;;
      *'"method":"thread/resume"'*)
        id=$(extract_id "$line")
        printf '{"id":"%s","result":{}}\n' "$id"
        ;;
      *'"method":"thread/fork"'*)
        printf '{"id":"fork","result":{"thread":{"id":"codex-fork-native"}}}\n'
        ;;
      *'"method":"thread/compact/start"'*)
        printf '{"id":"compact","result":{}}\n'
        printf '{"method":"item/completed","params":{"threadId":"codex-native","item":{"type":"contextCompaction","id":"compact-item"}}}\n'
        ;;
      *'"method":"turn/start"'*)
        id=$(extract_id "$line")
        printf '{"id":"%s","result":{"turn":{"id":"codex-turn"}}}\n' "$id"
        printf '{"method":"item/agentMessage/delta","params":{"delta":"codex fixture"}}\n'
        printf '{"method":"turn/completed","params":{"turn":{"id":"codex-turn","status":"completed"}}}\n'
        ;;
    esac
  done
elif [ "$mode" = "--mode" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"type":"get_state"'*)
        id=$(extract_id "$line")
        printf '{"id":"%s","type":"response","success":true,"data":{"sessionId":"pi-native","isStreaming":false,"isCompacting":false,"pendingMessageCount":0}}\n' "$id"
        ;;
      *'"type":"prompt"'*)
        id=$(extract_id "$line")
        printf '{"id":"%s","type":"response","success":true}\n' "$id"
        printf '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"pi fixture"}}\n'
        printf '{"type":"agent_settled"}\n'
        ;;
    esac
  done
else
  while IFS= read -r line; do
    case "$line" in
      *'"subtype":"initialize"'*)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{}}}\n'
        continue
        ;;
    esac
    printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"claude fixture"}}}\n'
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"claude-native","result":"ok"}\n'
  done
fi
"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn write_pi_launch_fixture(root: &Path, marker: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("pi-launch-fixture.sh");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
extract_id() {{
  printf '%s' "$1" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'
}}
while IFS= read -r line; do
  case "$line" in
    *'"type":"get_available_models"'*)
      printf '{{"id":"models","type":"response","success":true,"data":{{"models":[]}}}}\n'
      ;;
    *'"type":"get_commands"'*)
      printf '{{"id":"commands","type":"response","success":true,"data":{{"commands":[]}}}}\n'
      ;;
    *'"type":"get_state"'*)
      id=$(extract_id "$line")
      printf '{{"id":"%s","type":"response","success":true,"data":{{"sessionId":"pi-native","isStreaming":false,"isCompacting":false,"pendingMessageCount":0}}}}\n' "$id"
      ;;
    *'"type":"prompt"'*)
      id=$(extract_id "$line")
      printf '{{"id":"%s","type":"response","success":true}}\n' "$id"
      printf '{{"type":"message_update","assistantMessageEvent":{{"type":"text_delta","delta":"pi fixture"}}}}\n'
      printf '{{"type":"agent_settled"}}\n'
      ;;
  esac
done
"#,
            marker.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()))
    }

    #[test]
    fn skill_context_is_prefixed_to_provider_text() {
        let composed = super::compose_prompt_with_skills(
            "do the task",
            &[("build".to_owned(), "use pnpm install".to_owned())],
        );
        assert!(composed.contains("use pnpm install"));
        assert!(composed.contains("do the task"));
        assert!(composed.contains("<skill name=\"build\">"));
    }

    #[test]
    fn prompt_images_accept_camel_case_and_legacy_mime_fields() {
        for value in [
            serde_json::json!({ "type": "image", "data": "cG5n", "mimeType": "image/png" }),
            serde_json::json!({ "type": "image", "data": "cG5n", "mime_type": "image/png" }),
        ] {
            let content: PromptContentRef = serde_json::from_value(value).unwrap();
            assert!(matches!(
                content,
                PromptContentRef::Image { data, mime_type }
                    if data == "cG5n" && mime_type == "image/png"
            ));
        }

        let error = serde_json::from_value::<PromptContentRef>(serde_json::json!({
            "type": "image",
            "data": "cG5n",
            "mimeType": "image/png",
            "unexpected": true,
        }))
        .expect_err("unknown prompt image fields must remain rejected");
        assert!(error.to_string().contains("unknown field"));
    }

    #[tokio::test]
    async fn prompt_content_is_confined_to_workspace() {
        let root = temp_dir("todex-prompt-content");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let image = workspace.join("image.png");
        fs::write(&image, b"png fixture").unwrap();
        let outside = root.join("outside.txt");
        fs::write(&outside, b"outside").unwrap();

        let (text, content) = prepare_prompt_content(
            ProviderKind::Codex,
            &workspace,
            vec![
                PromptContentRef::Text {
                    text: "look here".to_owned(),
                },
                PromptContentRef::LocalImage { path: image },
            ],
        )
        .await
        .unwrap();
        assert_eq!(text, "look here");
        assert!(matches!(
            content.as_slice(),
            [DriverPromptContent::Image { path: Some(_), .. }]
        ));

        let error = prepare_prompt_content(
            ProviderKind::Codex,
            &workspace,
            vec![PromptContentRef::File {
                path: outside,
                name: None,
            }],
        )
        .await
        .expect_err("files outside the trusted workspace must be rejected");
        assert!(error.to_string().contains("trusted workspace"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn typed_image_support_matches_provider_capabilities() {
        let root = temp_dir("todex-provider-image-capabilities");
        fs::create_dir_all(&root).unwrap();
        for provider in [
            ProviderKind::Codex,
            ProviderKind::Pi,
            ProviderKind::ClaudeCode,
            ProviderKind::Acp,
            ProviderKind::GrokBuild,
        ] {
            let (_, content) = prepare_prompt_content(
                provider,
                &root,
                vec![PromptContentRef::Image {
                    data: "cG5n".to_owned(),
                    mime_type: "image/png".to_owned(),
                }],
            )
            .await
            .expect("declared image provider must accept typed image input");
            assert!(matches!(
                content.as_slice(),
                [DriverPromptContent::Image { path: None, .. }]
            ));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_skill_resource_is_rejected() {
        let root = temp_dir("todex-skill-reject");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let trust = trust_store(&config, "owner-a", Some(&workspace)).await;
        let supervisor =
            ConversationSupervisor::new(config, store, ConversationEventHub::default(), trust);
        let manifest = supervisor
            .create_owned("owner-a", ProviderKind::Codex, workspace, None, None)
            .await
            .unwrap();
        let error = supervisor
            .prompt_owned(
                "owner-a",
                &manifest.id,
                ConversationPrompt {
                    client_request_id: None,
                    text: "hello".to_owned(),
                    model: None,
                    reasoning_effort: None,
                    permission_mode: None,
                    work_mode: None,
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                    skills: vec![PromptSkillRef {
                        resource_id: "res_missing".to_owned(),
                        name: Some("missing".to_owned()),
                    }],
                    content: Vec::new(),
                },
            )
            .await
            .expect_err("missing skill must be rejected");
        assert!(error.to_string().contains("skill resource"));
        let _ = fs::remove_dir_all(root);
    }
}
