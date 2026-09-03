use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::sync::watch;
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
    DriverContext, DriverEventSink, DriverPrompt, DriverPromptContent, DriverSkill,
    PermissionBroker, PermissionDecision, PermissionOutcome, ProviderCommandDescriptor,
    ProviderDescriptor, ProviderDriver, ProviderModelDescriptor,
};

const MAX_PROMPT_BYTES: usize = 512 * 1024;
const MAX_PROMPT_CONTENT_ITEMS: usize = 16;
const MAX_PROMPT_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct PromptSkillRef {
    pub resource_id: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ConversationPrompt {
    pub text: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub skills: Vec<PromptSkillRef>,
    pub content: Vec<PromptContentRef>,
}

#[derive(Clone, Debug, serde::Deserialize)]
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
    pub fn new(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        workspace_trust: WorkspaceTrustStore,
    ) -> Self {
        let catalog = CatalogService::new(config.clone());
        Self::new_with_catalog(config, store, hub, catalog, workspace_trust)
    }

    pub fn new_with_catalog(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        catalog: CatalogService,
        workspace_trust: WorkspaceTrustStore,
    ) -> Self {
        Self {
            registry: DriverRegistry::new(&config),
            config,
            store,
            hub,
            catalog,
            permissions: PermissionBroker::default(),
            active: Arc::new(DashMap::new()),
            workspace_trust,
        }
    }

    pub async fn recover_all(&self) -> Result<(), AppError> {
        for manifest in self.store.list().await? {
            let was_active = matches!(
                manifest.status,
                ConversationStatus::Running | ConversationStatus::WaitingPermission
            );
            let recovered = self.store.recover(&manifest.id).await?;
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

    pub async fn models_live(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
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

    pub async fn commands_live(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
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
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        if matches!(
            manifest.status,
            ConversationStatus::Running | ConversationStatus::WaitingPermission
        ) {
            return Err(AppError::Conflict(
                "active conversation cannot be deleted".to_owned(),
            ));
        }
        self.store.delete(&manifest.id).await?;
        Ok(manifest)
    }

    pub async fn cleanup_expired(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<ConversationManifest>, AppError> {
        self.store.cleanup_before(cutoff).await
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
                text,
                model,
                reasoning_effort: None,
                skills: Vec::new(),
                content: Vec::new(),
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
        let ConversationPrompt {
            text,
            model,
            reasoning_effort,
            skills,
            content,
        } = prompt;
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

        if let Err(error) = self
            .emit(
                conversation_id,
                "message.created",
                json!({ "turnId": turn_id, "role": "user", "content": user_text }),
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
                json!({ "turnId": turn_id, "provider": manifest.provider }),
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
            );
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
                    let mut state = provider_state;
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
    }

    async fn emit(
        &self,
        conversation_id: &str,
        event_type: &str,
        payload: Value,
    ) -> Result<(), AppError> {
        let event = self
            .store
            .append(conversation_id, event_type, payload)
            .await?;
        self.hub.publish(event);
        Ok(())
    }
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
    if matches!(provider, ProviderKind::Codex | ProviderKind::Pi) {
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
                auto_trust_workspaces: false,
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
                auto_trust_workspaces: false,
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
                auto_trust_workspaces: false,
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
                auto_trust_workspaces: false,
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
        printf '{"id":"thread","result":{}}\n'
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
        printf '{"id":"state","type":"response","success":true}\n'
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
      printf '{{"id":"state","type":"response","success":true,"data":{{}}}}\n'
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
    async fn typed_images_are_rejected_for_unsupported_providers() {
        let root = temp_dir("todex-unsupported-image");
        fs::create_dir_all(&root).unwrap();
        let error = prepare_prompt_content(
            ProviderKind::ClaudeCode,
            &root,
            vec![PromptContentRef::Image {
                data: "cG5n".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
        )
        .await
        .expect_err("unsupported image input must fail explicitly");
        assert!(matches!(error, AppError::Unsupported(_)));
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
                auto_trust_workspaces: false,
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
                    text: "hello".to_owned(),
                    model: None,
                    reasoning_effort: None,
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
