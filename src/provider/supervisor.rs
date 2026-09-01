use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::{sleep, timeout, Duration, Instant};
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

use super::acp::AcpDriver;
use super::claude::ClaudeDriver;
use super::codex::CodexDriver;
use super::pi::PiDriver;
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, PermissionBroker, PermissionDecision,
    PermissionOutcome, ProviderCommandDescriptor, ProviderDescriptor, ProviderDriver,
};

const MAX_PROMPT_BYTES: usize = 512 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct PromptSkillRef {
    pub resource_id: String,
    pub name: Option<String>,
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
    pub fn new(config: Arc<Config>, store: ConversationStore, hub: ConversationEventHub) -> Self {
        let catalog = CatalogService::new(config.clone());
        Self::new_with_catalog(config, store, hub, catalog)
    }

    pub fn new_with_catalog(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        catalog: CatalogService,
    ) -> Self {
        Self {
            registry: DriverRegistry::new(&config),
            config,
            store,
            hub,
            catalog,
            permissions: PermissionBroker::default(),
            active: Arc::new(DashMap::new()),
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

    pub async fn providers_live(&self, workspace: &Path) -> Vec<ProviderDescriptor> {
        let mut descriptors = self.registry.descriptors();
        for descriptor in &mut descriptors {
            if let Ok(driver) = self.registry.driver(descriptor.id) {
                if let Ok(Ok(models)) = tokio::time::timeout(Duration::from_secs(8), driver.discover_models(workspace)).await {
                    descriptor.models = models;
                }
            }
        }
        descriptors
    }

    pub async fn commands_live(&self, provider: ProviderKind, workspace: &Path) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        self.registry.driver(provider)?.discover_commands(workspace).await
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
        self.prompt_owned("local", conversation_id, text, model, None, Vec::new())
            .await
    }

    pub async fn prompt_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        text: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        skills: Vec<PromptSkillRef>,
    ) -> Result<String, AppError> {
        let text = text.trim().to_owned();
        if text.is_empty() && skills.is_empty() {
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
        let injected = self.load_prompt_skills(&manifest, &skills).await?;
        let user_text = if text.is_empty() {
            "请使用已选择的 Skill。".to_owned()
        } else {
            text
        };
        let provider_text = compose_prompt_with_skills(&user_text, &injected);
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
            let result = timeout(
                Duration::from_secs(120),
                driver.run_turn(
                    DriverContext {
                        manifest,
                        provider_state: provider_state.clone(),
                    },
                    DriverPrompt {
                        turn_id: spawned_turn_id.clone(),
                        text: provider_text,
                        model,
                        reasoning_effort,
                    },
                    sink,
                    cancel_rx,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(AppError::ProviderUnavailable("provider turn timed out after 120 seconds".to_owned())));
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
        let active = self.active.get(conversation_id).ok_or_else(|| {
            AppError::Conflict(format!("conversation {conversation_id} has no active turn"))
        })?;
        active
            .cancel
            .send(true)
            .map_err(|_| AppError::Conflict("turn has already stopped".to_owned()))
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
                    Err(AppError::InvalidRequest("mcp tool returned an error".to_owned()))
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
    ) -> Result<Vec<(String, String)>, AppError> {
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
            injected.push((resource.descriptor.name, resource.content));
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

    use tokio::time::timeout;

    use super::*;
    use crate::config::{AcpProfileConfig, AgentConfig, PairingEncryption, SecurityConfig};
    use crate::conversation::{ConversationEventHub, ConversationStore};

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
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture_text.clone(),
                claude_bin: fixture_text.clone(),
                pi_bin: fixture_text,
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
        let supervisor =
            ConversationSupervisor::new(config, store.clone(), ConversationEventHub::default());
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
            let replay = timeout(Duration::from_secs(10), async {
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
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture.to_string_lossy().to_string(),
                claude_bin: fixture.to_string_lossy().to_string(),
                pi_bin: fixture.to_string_lossy().to_string(),
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
        let supervisor =
            ConversationSupervisor::new(config, store, ConversationEventHub::default());
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
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture.to_string_lossy().to_string(),
                claude_bin: fixture.to_string_lossy().to_string(),
                pi_bin: fixture.to_string_lossy().to_string(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        });
        let store = ConversationStore::new(data_dir.clone()).await.unwrap();
        let supervisor =
            ConversationSupervisor::new(config, store.clone(), ConversationEventHub::default());
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

if [ "$mode" = "acp" ]; then
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
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable,
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        });
        let store = ConversationStore::new(config.data_dir.clone()).await.unwrap();
        let supervisor =
            ConversationSupervisor::new(config, store, ConversationEventHub::default());
        let manifest = supervisor
            .create_owned("owner-a", ProviderKind::Codex, workspace, None, None)
            .await
            .unwrap();
        let error = supervisor
            .prompt_owned(
                "owner-a",
                &manifest.id,
                "hello".to_owned(),
                None,
                None,
                vec![PromptSkillRef {
                    resource_id: "res_missing".to_owned(),
                    name: Some("missing".to_owned()),
                }],
            )
            .await
            .expect_err("missing skill must be rejected");
        assert!(error.to_string().contains("skill resource"));
        let _ = fs::remove_dir_all(root);
    }
}
