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

use crate::agent_mcp::{AgentMcp, AgentMcpLaunch};
use crate::catalog::CatalogService;
use crate::config::Config;
use crate::conversation::{
    control_mac, encrypted_content, seal_request_snapshot, ControlOutcome, ConversationEventHub,
    ConversationManifest, ConversationReplay, ConversationStatus, ConversationStore, ProviderKind,
    ProviderState, ReplayDetail, CONVERSATION_TERMINAL_EVENTS, TURN_TERMINAL_EVENTS,
};
use crate::error::AppError;
use crate::history_keys::FingerprintKey;
use crate::mcp;
use crate::workspace_paths::validate_workspace_directory_text;
use crate::workspace_store::stable_workspace_id;

use super::acp::AcpDriver;
use super::claude::ClaudeDriver;
use super::cli_manager::{CliLocks, CliPermits, ManagedCli};
use super::codex::CodexDriver;
use super::grok::GrokBuildDriver;
use super::pi::PiDriver;
use super::process::same_executable;
use super::profile::{profile, FileAttachmentStyle, McpInjection, SkillInjection};
#[cfg(all(test, unix))]
use super::types::ProviderDescriptor;
use super::types::{
    DriverContext, DriverEventSink, DriverPrompt, DriverPromptContent, DriverSkill,
    DriverTurnResult, ImageInputMode, PermissionBroker, PermissionDecision, PermissionOutcome,
    ProviderActivity, ProviderCommandDescriptor, ProviderControl, ProviderDriver,
    ProviderImageInputCapability, ProviderModelDescriptor, ProviderSnapshot, ProviderWork,
    ProviderWorkHandle, TurnHandles, TurnModeHandle,
};

mod follow_ups;
mod history_cleanup;
pub use follow_ups::FollowUpAddOutcome;
pub(crate) use history_cleanup::{
    plan_policy_deletions, CleanupSkip, NativeSessionRef, PolicyCandidate, PolicyRule,
};
pub use history_cleanup::{CleanupReport, CleanupThresholds};

fn prompt_fingerprint(prompt: &ConversationPrompt) -> Result<String, AppError> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(prompt)?)))
}

/// Whether a recorded `requestFingerprint` is `fingerprint`: the plain
/// SHA-256 written while history encryption was off, or its MAC written
/// while it was on (`docs/history-encryption.md` §5.2).
fn fingerprint_matches(recorded: &str, fingerprint: &str, key: Option<&FingerprintKey>) -> bool {
    recorded == fingerprint || key.is_some_and(|key| key.mac(fingerprint) == recorded)
}

/// The `request` of `last-request.json`: the prompt, sealed with
/// [`seal_request_snapshot`] while history encryption is on (`key`) so the
/// file holds no plaintext prompt (§7).
fn saved_request(
    prompt: &ConversationPrompt,
    key: Option<&FingerprintKey>,
) -> Result<Value, AppError> {
    let mut snapshot = serde_json::to_value(prompt)?;
    if let Some(key) = key {
        seal_request_snapshot(&mut snapshot, key);
    }
    Ok(snapshot)
}

/// The user `message.created` payload. While the request snapshot is sealed
/// the backend cannot read the request back, so the encrypted payload also
/// carries it as `retryRequest` — the trimmed `text` and the original
/// `content` items — for a client to decrypt and return with
/// `conversation.retry` (`docs/history-encryption.md` §7). Summaries drop it.
fn user_message_payload(
    turn_id: &str,
    client_request_id: Option<&str>,
    request_fingerprint: &str,
    user_text: &str,
    sealed_request: Option<&ConversationPrompt>,
) -> Value {
    let mut payload = json!({
        "turnId": turn_id,
        "clientRequestId": client_request_id,
        "requestFingerprint": request_fingerprint,
        "role": "user",
        "content": user_text,
    });
    if let Some(request) = sealed_request {
        payload[crate::conversation::RETRY_REQUEST_KEY] = json!({
            "text": request.text.trim(),
            "content": request.content,
        });
    }
    payload
}

/// The content a retry of a sealed request runs with. The snapshot keeps
/// only the file references plus a `contentMac` over the inline items
/// (`text` / `image`, see [`seal_request_snapshot`]), so the client returns
/// the original items from the decrypted `retryRequest`: their inline part
/// must match `contentMac` and their file part the kept references.
/// Without them only a request that had no inline items can be retried.
fn retry_content_checked(
    saved: &Value,
    supplied: Option<Vec<PromptContentRef>>,
    key: &FingerprintKey,
) -> Result<Vec<PromptContentRef>, AppError> {
    let kept = saved.get("content").cloned().unwrap_or_else(|| json!([]));
    let content_mac = saved.get("contentMac").and_then(Value::as_str);
    let Some(content) = supplied else {
        if content_mac.is_some() {
            return Err(AppError::Conflict(
                "This request had inline text or images; retry needs the original content items from the decrypted message."
                    .to_owned(),
            ));
        }
        return Ok(serde_json::from_value(kept)?);
    };
    let items = match serde_json::to_value(&content)? {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    let (inline, files): (Vec<Value>, Vec<Value>) = items.into_iter().partition(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("text" | "image")
        )
    });
    let inline_matches = match content_mac {
        Some(mac) => key.mac(&Value::Array(inline).to_string()) == mac,
        None => inline.is_empty(),
    };
    if !inline_matches || Value::Array(files) != kept {
        return Err(AppError::Conflict(
            "content is not the content of the request being retried".to_owned(),
        ));
    }
    Ok(content)
}

fn retry_prompt_required() -> AppError {
    AppError::InvalidRequest(
        "conversation.retry needs `prompt` (the decrypted text of the request) while history is end-to-end encrypted".to_owned(),
    )
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
/// How long after its last event a conversation may still hold background
/// work in a provider process: Claude Code keeps it up to 6 h past a turn.
const BACKGROUND_WORK_HORIZON_HOURS: i64 = 7;
/// How often an idle turn with a pending permission dialog is rechecked.
const IDLE_WATCHDOG_RECHECK: Duration = Duration::from_secs(5);
/// How long an idle-stopped driver may take to honor cancellation.
const IDLE_TURN_STOP_GRACE: Duration = Duration::from_secs(30);
/// Backoff between attempts to journal a turn's terminal event.
const TERMINAL_EVENT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

/// Runs `attempt(0)`, then one retry per delay while it fails, and returns
/// the last error once the delays are exhausted.
async fn retry_with_backoff<F, Fut>(delays: &[Duration], mut attempt: F) -> Result<(), AppError>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<(), AppError>>,
{
    let mut index = 0;
    loop {
        let error = match attempt(index).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Some(delay) = delays.get(index) else {
            return Err(error);
        };
        tracing::warn!(error = %error, attempt = index + 1, retry_in_ms = delay.as_millis() as u64, "retrying terminal turn event");
        sleep(*delay).await;
        index += 1;
    }
}

fn idle_timeout_message(limit: Duration) -> String {
    let seconds = limit.as_secs();
    let span = if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} minutes", seconds / 60)
    } else {
        format!("{:.1} seconds", limit.as_secs_f64())
    };
    format!("The provider produced no output for {span}; the turn was stopped.")
}

/// The failure code and message for a driver task that did not return.
fn driver_task_failure(error: tokio::task::JoinError) -> (&'static str, String) {
    if !error.is_panic() {
        return (
            "PROVIDER_TASK_CANCELLED",
            "The provider driver task was cancelled before it finished.".to_owned(),
        );
    }
    let panic = error.into_panic();
    let detail = panic
        .downcast_ref::<&str>()
        .map(|detail| (*detail).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no panic message".to_owned());
    (
        "PROVIDER_PANIC",
        format!("The provider driver crashed: {detail}")
            .chars()
            .take(1000)
            .collect(),
    )
}

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
            (
                ProviderKind::Devin,
                Arc::new(super::devin::DevinDriver::new(&config.agent)) as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::Opencode,
                Arc::new(super::opencode::OpencodeDriver::new(&config.agent))
                    as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::Antigravity,
                Arc::new(super::antigravity::AntigravityDriver::new(
                    &config.agent,
                    &config.data_dir,
                )) as Arc<dyn ProviderDriver>,
            ),
            (
                ProviderKind::DeepseekHarness,
                Arc::new(super::deepseek_harness::DeepseekHarnessDriver::new(
                    &config.agent,
                    &config.data_dir,
                )) as Arc<dyn ProviderDriver>,
            ),
        ]);
        Self {
            drivers: Arc::new(drivers),
        }
    }

    #[cfg(all(test, unix))]
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.drivers().map(|driver| driver.descriptor()).collect()
    }

    /// Registered drivers in `ProviderKind` order.
    fn drivers(&self) -> impl Iterator<Item = &Arc<dyn ProviderDriver>> {
        ProviderKind::ALL
            .iter()
            .filter_map(|provider| self.drivers.get(provider))
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
    /// Per-CLI locks an install or upgrade holds; starts of a CLI take its
    /// lock shared before `cli_execution_gate`, so a start waiting on an
    /// upgrade holds nothing every other Agent needs.
    cli_locks: CliLocks,
    /// `[agent].provider_idle_timeout_minutes`; `None` disables the watchdog.
    provider_idle_timeout: Option<Duration>,
    /// SSH tools injected into provider sessions; `None` in bare test setups.
    agent_mcp: Option<AgentMcp>,
    /// Account-level quota snapshots mirrored from `quota.updated` events.
    quota: crate::quota_store::QuotaStore,
    /// Results of recently completed controls, by `(conversation, request
    /// id)`, so a retried control gets the original result while history is
    /// encrypted and the journal only holds it as ciphertext. In memory
    /// only: control results may carry prompt text.
    control_results: Arc<std::sync::Mutex<lru::LruCache<(String, String), Value>>>,
    /// Set once [`Self::shutdown_all`] starts: the follow-up queue no longer
    /// starts items, so nothing new runs while turns are being stopped.
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    /// Backend follow-up queues by conversation. In memory only: queued
    /// prompts are never written to disk, so a restart loses them and the
    /// updater waits while any queue holds an item.
    follow_up_queues: Arc<dashmap::DashMap<String, follow_ups::FollowUpQueue>>,
    /// Shortest wait before a rate-limit continuation runs (doubled per
    /// consecutive failed continuation). Tests shorten it.
    rate_limit_retry_floor: Duration,
    /// Prewarms in flight across conversations; see [`MAX_PREWARMS`].
    prewarms: Arc<tokio::sync::Semaphore>,
    /// The muted sink of each conversation's latest prewarm, registered
    /// before the provider is awaited. Its next turn takes it over (see
    /// [`DriverEventSink::hand_over`]): the events the provider sent while no
    /// turn ran are journalled with that turn, and later ones go to it.
    prewarm_sinks: Arc<dashmap::DashMap<String, DriverEventSink>>,
    /// Observer of turn ends and conversation removals; see
    /// [`ConversationHooks`].
    hooks: Arc<std::sync::RwLock<Option<Arc<dyn ConversationHooks>>>>,
}

/// Told about conversation lifecycle events the supervisor itself does not
/// act on (agent calls cancel and delete a parent's children). Called
/// inline: implementations spawn anything that waits.
pub(crate) trait ConversationHooks: Send + Sync {
    /// A turn or operation of `conversation_id` ended with `terminal` (the
    /// terminal event type, e.g. `turn.cancelled`) and released it.
    fn turn_ended(&self, conversation_id: &str, terminal: &str);
    /// `manifest`'s conversation was removed (deleted, expired, cleaned up).
    fn conversation_removed(&self, manifest: &ConversationManifest);
}

/// Completed control results kept for retried requests.
const CONTROL_RESULT_CACHE: usize = 256;
/// Prewarms that may run at once; another is skipped as `busy`.
const MAX_PREWARMS: usize = 4;
/// How long a prewarm may take before its caller gets `timeout`; the
/// provider keeps preparing in the background.
const PREWARM_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of [`ConversationSupervisor::cancel_owned`].
#[derive(Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The active turn was told to stop, or nothing was running and the
    /// request named no turn.
    Signalled,
    /// The request named a turn that is not the active one; nothing was
    /// signalled. Carries the turn that is running, if any.
    NotActive { active_turn_id: Option<String> },
}

/// A turn that is registered and journalled, ready to run its driver.
struct SpawnTurn {
    owner_id: String,
    turn_id: String,
    client_request_id: Option<String>,
    manifest: ConversationManifest,
    provider_state: ProviderState,
    agent_mcp: Option<AgentMcpLaunch>,
    effective_permissions: super::types::EffectivePermissionConfig,
    driver: Arc<dyn ProviderDriver>,
    prompt: DriverPrompt,
    cancel: watch::Receiver<bool>,
    /// See [`TurnHandles::adopt`].
    adopt: Option<String>,
}

struct ActiveTurn {
    turn_id: String,
    cancel: watch::Sender<bool>,
    provider: ProviderKind,
    provider_profile: Option<String>,
}

struct ActiveTurnCleanup {
    active: Arc<DashMap<String, ActiveTurn>>,
    conversation_id: String,
    /// Set for prompt turns: their tool mode ends with them.
    agent_mcp: Option<AgentMcp>,
}

impl Drop for ActiveTurnCleanup {
    fn drop(&mut self) {
        // Before the turn stops being active, so the next turn's mode is
        // never cleared by this one.
        if let Some(agent_mcp) = &self.agent_mcp {
            agent_mcp.end_turn_mode(&self.conversation_id);
        }
        self.active.remove(&self.conversation_id);
    }
}

impl ConversationSupervisor {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(config: Arc<Config>, store: ConversationStore, hub: ConversationEventHub) -> Self {
        Self::new_with_execution_gate(config, store, hub, Arc::new(RwLock::new(())))
    }

    pub fn new_with_execution_gate(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        cli_execution_gate: Arc<RwLock<()>>,
    ) -> Self {
        let catalog = CatalogService::new(config.clone());
        Self::new_with_catalog_and_gate(config, store, hub, catalog, cli_execution_gate)
    }

    fn new_with_catalog_and_gate(
        config: Arc<Config>,
        store: ConversationStore,
        hub: ConversationEventHub,
        catalog: CatalogService,
        cli_execution_gate: Arc<RwLock<()>>,
    ) -> Self {
        let provider_idle_timeout = match config.agent.provider_idle_timeout_minutes {
            0 => None,
            minutes => Some(Duration::from_secs(minutes.saturating_mul(60))),
        };
        Self {
            registry: DriverRegistry::new(&config),
            provider_idle_timeout,
            config,
            store,
            hub,
            catalog,
            permissions: PermissionBroker::default(),
            active: Arc::new(DashMap::new()),
            request_gates: Arc::new(DashMap::new()),
            cli_execution_gate,
            cli_locks: CliLocks::default(),
            agent_mcp: None,
            quota: crate::quota_store::QuotaStore::default(),
            control_results: Arc::new(std::sync::Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(CONTROL_RESULT_CACHE)
                    .expect("the control result cache is not empty"),
            ))),
            shutting_down: Arc::default(),
            follow_up_queues: Arc::default(),
            rate_limit_retry_floor: follow_ups::RATE_LIMIT_RETRY_FLOOR,
            prewarms: Arc::new(tokio::sync::Semaphore::new(MAX_PREWARMS)),
            prewarm_sinks: Arc::default(),
            hooks: Arc::default(),
        }
    }

    fn control_results(&self) -> std::sync::MutexGuard<'_, lru::LruCache<(String, String), Value>> {
        self.control_results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn remember_control_result(&self, conversation_id: &str, request_id: &str, result: &Value) {
        self.control_results().put(
            (conversation_id.to_owned(), request_id.to_owned()),
            result.clone(),
        );
    }

    fn control_result(&self, conversation_id: &str, request_id: &str) -> Option<Value> {
        self.control_results()
            .get(&(conversation_id.to_owned(), request_id.to_owned()))
            .cloned()
    }

    pub fn with_agent_mcp(mut self, agent_mcp: AgentMcp) -> Self {
        self.agent_mcp = Some(agent_mcp);
        self
    }

    /// Standing answers for permission requests (API key approval policies).
    pub fn set_permission_policy(&self, policy: Arc<dyn super::types::PermissionPolicy>) {
        self.permissions.set_policy(policy);
    }

    pub fn with_cli_locks(mut self, cli_locks: CliLocks) -> Self {
        self.cli_locks = cli_locks;
        self
    }

    /// The managed CLIs a `provider` (with ACP `profile`) runs.
    fn clis_for(&self, provider: ProviderKind, profile: Option<&str>) -> Vec<ManagedCli> {
        ManagedCli::ALL
            .into_iter()
            .filter(|cli| {
                turn_runs_cli(
                    &self.config.agent,
                    provider,
                    profile,
                    cli.provider_kind(),
                    cli.binary(&self.config),
                )
            })
            .collect()
    }

    /// Discovery runs the CLI; it is refused, not queued, while an install
    /// or upgrade replaces it.
    fn try_discovery_permits(
        &self,
        provider: ProviderKind,
        profile: Option<&str>,
    ) -> Result<(CliPermits, tokio::sync::RwLockReadGuard<'_, ()>), AppError> {
        let busy =
            || AppError::Conflict("CLI discovery is unavailable during an upgrade".to_owned());
        let clis = self
            .cli_locks
            .try_read(&self.clis_for(provider, profile))
            .ok_or_else(busy)?;
        let gate = self.cli_execution_gate.try_read().map_err(|_| busy())?;
        Ok((clis, gate))
    }

    /// Waits while the CLI is installed or upgraded, then for the shared gate.
    async fn start_permits(
        &self,
        provider: ProviderKind,
        profile: Option<&str>,
    ) -> (CliPermits, tokio::sync::RwLockReadGuard<'_, ()>) {
        let clis = self.cli_locks.read(&self.clis_for(provider, profile)).await;
        (clis, self.cli_execution_gate.read().await)
    }

    pub fn with_quota(mut self, quota: crate::quota_store::QuotaStore) -> Self {
        self.quota = quota;
        self
    }

    /// TodeX's MCP servers for a turn, when any is enabled and the provider
    /// can load MCP servers TodeX supplies. `child`: the conversation is an
    /// agent-call child, which gets no `todex_agents` (agents call agents
    /// one level deep).
    async fn agent_mcp_for(
        &self,
        provider: ProviderKind,
        conversation_id: &str,
        child: bool,
    ) -> Option<AgentMcpLaunch> {
        let agent_mcp = self.agent_mcp.as_ref()?;
        match profile(provider).mcp_injection {
            McpInjection::None => None,
            McpInjection::GlobalConfigEnv => {
                agent_mcp.launch_global_for(conversation_id, child).await
            }
            _ => agent_mcp.launch_for(conversation_id, child).await,
        }
    }

    /// TodeX's MCP servers for a turn of `manifest`. API key conversations
    /// get the provider's own tools only: TodeX's SSH, desktop and browser
    /// tools reach past the key's scope. Agent-call children get every
    /// server but `todex_agents`.
    async fn turn_agent_mcp(&self, manifest: &ConversationManifest) -> Option<AgentMcpLaunch> {
        if manifest.owner_id.starts_with(crate::api_keys::OWNER_PREFIX) {
            None
        } else {
            self.agent_mcp_for(
                manifest.provider,
                &manifest.id,
                manifest.parent_conversation_id.is_some(),
            )
            .await
        }
    }

    /// Taps `conversation_id`'s committed events in plaintext, in memory
    /// only; see [`crate::conversation::LiveTap`]. `CONFLICT` while another
    /// tap of it is live.
    pub(crate) fn live_tap(
        &self,
        conversation_id: &str,
        project: crate::conversation::TapProjection,
    ) -> Result<crate::conversation::LiveTap, AppError> {
        self.store.live_tap(conversation_id, project)
    }

    /// The terminal event `turn_id` of `conversation_id` journalled
    /// (`turn.completed`, ...), if any, from the journal digest.
    pub(crate) async fn turn_terminal(
        &self,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<Option<&'static str>, AppError> {
        self.store
            .digest(conversation_id, |digest| {
                crate::conversation::TURN_TERMINAL_EVENTS
                    .iter()
                    .copied()
                    .find(|terminal| digest.has_turn_terminal(turn_id, terminal) == Some(true))
            })
            .await
    }

    /// The conversation's running turn, if any.
    pub(crate) fn active_turn_id(&self, conversation_id: &str) -> Option<String> {
        self.active
            .get(conversation_id)
            .map(|turn| turn.turn_id.clone())
    }

    /// Installs the observer of turn ends and removals (agent calls).
    pub(crate) fn set_hooks(&self, hooks: Arc<dyn ConversationHooks>) {
        *self
            .hooks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hooks);
    }

    fn hooks(&self) -> Option<Arc<dyn ConversationHooks>> {
        self.hooks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn revoke_agent_mcp(&self, conversation_id: &str) {
        if let Some(agent_mcp) = &self.agent_mcp {
            agent_mcp.revoke(conversation_id).await;
        }
    }

    /// Journals an event raised outside a provider stream (agent SSH tools),
    /// tagged with the running turn when there is one.
    pub(crate) async fn append_agent_event(
        &self,
        conversation_id: &str,
        event_type: &str,
        mut payload: Value,
    ) -> Result<(), AppError> {
        let turn_id = self
            .active
            .get(conversation_id)
            .map(|turn| turn.turn_id.clone());
        if let (Some(turn_id), Some(object)) = (turn_id, payload.as_object_mut()) {
            object.insert("turnId".to_owned(), Value::String(turn_id));
        }
        self.emit(conversation_id, event_type, payload).await
    }

    /// Journals that background work clients wait on (the tasks of a
    /// `background_tasks_pending` notice) is gone with its provider process.
    async fn journal_lost_work(
        &self,
        conversation_id: &str,
        provider: ProviderKind,
        task_ids: &[String],
        reason: &str,
    ) {
        let payload = json!({
            "provider": provider,
            "providerMethod": "background_tasks_ended",
            "metadata": { "taskIds": task_ids, "reason": reason },
        });
        if let Err(error) = self
            .append_agent_event(conversation_id, "provider.event", payload)
            .await
        {
            tracing::warn!(%conversation_id, %error, "could not journal lost background work");
        }
    }

    /// Asks the user from inside an agent tool call (outside any provider
    /// stream), tagged with the running turn. `allowed_devices` restricts who
    /// may answer. The turn's cancellation, or `cancel`, ends the request.
    /// Returns the decision and the device that answered.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request_agent_permission(
        &self,
        conversation_id: &str,
        request_id: String,
        kind: &str,
        title: String,
        details: Value,
        options: Value,
        allowed_devices: Option<Vec<String>>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(PermissionDecision, String), AppError> {
        let turn = self
            .active
            .get(conversation_id)
            .map(|turn| (turn.turn_id.clone(), turn.cancel.subscribe()));
        let mut sink = DriverEventSink::new(
            self.store.clone(),
            self.hub.clone(),
            self.permissions.clone(),
            conversation_id,
        );
        // Either the turn or the caller cancelling ends the request.
        let (merged_tx, mut merged_rx) = watch::channel(false);
        let forward = tokio::spawn({
            let turn_cancel = turn.as_ref().map(|(_, cancel)| cancel.clone());
            async move {
                let turn_cancelled = async {
                    match turn_cancel {
                        Some(mut cancel) => {
                            while !*cancel.borrow_and_update() {
                                if cancel.changed().await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            }
                        }
                        None => std::future::pending().await,
                    }
                };
                let caller_cancelled = async {
                    while !*cancel.borrow_and_update() {
                        if cancel.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                };
                tokio::select! {
                    _ = turn_cancelled => {}
                    _ = caller_cancelled => {}
                }
                let _ = merged_tx.send(true);
            }
        });
        if let Some((turn_id, _)) = turn {
            sink = sink.with_turn_id(turn_id);
        }
        let result = match allowed_devices {
            Some(devices) => {
                sink.request_permission_on_devices(
                    request_id,
                    kind,
                    title,
                    details,
                    options,
                    devices,
                    &mut merged_rx,
                )
                .await
            }
            None => {
                sink.request_permission_answered(
                    request_id,
                    kind,
                    title,
                    details,
                    options,
                    &mut merged_rx,
                )
                .await
            }
        };
        forward.abort();
        result
    }

    /// Creates a conversation without the provider availability check.
    #[cfg(test)]
    pub(crate) async fn create_for_tests(
        &self,
        provider: ProviderKind,
        workspace: PathBuf,
    ) -> Result<ConversationManifest, AppError> {
        self.store
            .create(ConversationManifest::new(provider, workspace, None, None))
            .await
    }

    #[cfg(test)]
    pub(crate) async fn history_for_tests(
        &self,
        conversation_id: &str,
    ) -> Vec<crate::conversation::ConversationEvent> {
        self.store.plain_history_for_tests(conversation_id).await
    }

    pub async fn recover_all(&self) -> Result<(), AppError> {
        let started = std::time::Instant::now();
        let manifests = self.store.list().await?;
        let total = manifests.len();
        tracing::info!(conversations = total, "recovering conversation journals");
        let mut failed = 0usize;
        let mut settled = 0usize;
        for (index, manifest) in manifests.into_iter().enumerate() {
            // `list` already cached the manifest; a settled journal needs no
            // bookkeeping and is validated (and salvaged) on its first read.
            // A single unreadable or exhausted journal must not keep the
            // daemon from starting; recover the rest and report the failures.
            if self.recovery_is_settled(&manifest).await {
                settled += 1;
                self.recover_lost_background_work(&manifest).await;
            } else if let Err(error) = self.recover_conversation(&manifest).await {
                failed += 1;
                tracing::error!(
                    conversation_id = %manifest.id,
                    error = %error,
                    "conversation recovery failed; continuing startup"
                );
            }
            self.import_legacy_follow_ups(&manifest.id).await;
            self.interrupt_schedule(&manifest.id).await;
            if (index + 1) % 25 == 0 || index + 1 == total {
                tracing::info!(
                    recovered = index + 1,
                    total,
                    settled,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "conversation recovery progress"
                );
            }
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                total,
                "conversation recovery finished with failures"
            );
        }
        self.permissions.expire_all();
        Ok(())
    }

    /// Whether startup can skip the full journal scan: the manifest claims no
    /// work in flight and was written after the journal's last event (equal
    /// `last_sequence`; status changes are persisted with the event that
    /// caused them), and either the manifest is `Idle` or the journal ends
    /// cleanly on a turn- or conversation-terminal event (or is empty).
    /// `Idle` alone is enough because `turn.started` persists `Running`
    /// immediately and only a finished turn or operation returns to `Idle`,
    /// so trailing informational events (imported Codex history, a provider's
    /// command catalog after a turn) no longer force a full scan. `Failed`
    /// and `Interrupted` still need the terminal tail: a paused workflow is
    /// `Interrupted` with its turn open. Providers whose profile asks for
    /// `recovery_full_scan` (Pi) are always scanned: their resident runtimes
    /// and session-scoped dialogs outlive turns, so only the whole journal
    /// shows whether one was left open. Any doubt, including an unreadable
    /// tail, falls back to the full path, which reports it.
    async fn recovery_is_settled(&self, manifest: &ConversationManifest) -> bool {
        if profile(manifest.provider).recovery_full_scan
            || matches!(
                manifest.status,
                ConversationStatus::Running | ConversationStatus::WaitingPermission
            )
        {
            return false;
        }
        let tail = match self.store.journal_tail(&manifest.id).await {
            Ok((tail, true)) => tail,
            Ok((_, false)) => return false,
            Err(error) => {
                tracing::debug!(conversation_id = %manifest.id, error = %error, "journal tail unreadable; running full recovery");
                return false;
            }
        };
        match tail {
            None => manifest.last_sequence == 0,
            Some(event) => {
                event.sequence == manifest.last_sequence
                    && (manifest.status == ConversationStatus::Idle
                        || TURN_TERMINAL_EVENTS.contains(&event.event_type.as_str())
                        || CONVERSATION_TERMINAL_EVENTS.contains(&event.event_type.as_str()))
            }
        }
    }

    /// A settled journal whose last turn left background tasks waiting:
    /// the provider process went down with the daemon, so they are lost.
    /// Only recent Claude Code and Pi journals can hold such work (their
    /// processes keep work at most 6 h past a turn), which
    /// keeps the digest reads off most conversations.
    async fn recover_lost_background_work(&self, manifest: &ConversationManifest) {
        if !matches!(
            manifest.provider,
            ProviderKind::ClaudeCode | ProviderKind::Pi
        ) || manifest.updated_at
            < Utc::now() - chrono::Duration::hours(BACKGROUND_WORK_HORIZON_HOURS)
        {
            return;
        }
        match self
            .store
            .digest(&manifest.id, |digest| digest.recovery().background_pending)
            .await
        {
            Ok(true) => {
                self.journal_lost_work(&manifest.id, manifest.provider, &[], "daemon_restarted")
                    .await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(conversation_id = %manifest.id, %error, "could not check for lost background work");
            }
        }
    }

    async fn recover_conversation(&self, manifest: &ConversationManifest) -> Result<(), AppError> {
        let recovered = self.store.recover(&manifest.id).await?;
        // Whether work was in flight comes from the journal, not the manifest:
        // the manifest is written after the journal line, so a crash between
        // the two leaves a stale status in either direction. The journal
        // digest carries the fold, so the history is never loaded whole.
        let facts = self
            .store
            .digest(&manifest.id, |digest| digest.recovery())
            .await?;
        let open_turn = facts.open_turn;
        let was_active = open_turn.is_some()
            || matches!(
                facts.status,
                ConversationStatus::Running | ConversationStatus::WaitingPermission
            );
        // Dialogs requested and never resolved, by permission id; resident
        // runtimes are those whose last record says `ready`, by runtime id.
        for (permission_id, context) in facts.pending_permissions {
            self.emit(&manifest.id, "permission.resolved", json!({
                "permissionId": permission_id, "outcome": "cancelled", "optionId": Value::Null,
                "reason": "daemon_restarted", "scope":context.scope,"runtimeId":context.runtime_id,
            })).await?;
        }
        for runtime_id in facts.resident_runtimes {
            self.emit(
                &manifest.id,
                "provider.runtime",
                json!({
                    "provider":manifest.provider,"runtimeId":runtime_id,"scope":"session",
                    "status":"stopped","reason":"daemon_restarted",
                }),
            )
            .await?;
        }

        if facts.background_pending && !was_active {
            self.journal_lost_work(&manifest.id, manifest.provider, &[], "daemon_restarted")
                .await;
        }
        if was_active {
            let mut payload = json!({
                "reason": "daemon_restarted",
                "message": "The previous in-progress turn was interrupted; it was not replayed.",
            });
            if let Some(turn_id) = open_turn {
                payload["turnId"] = json!(turn_id);
            }
            self.emit(&recovered.id, "conversation.interrupted", payload)
                .await?;
        }
        Ok(())
    }

    /// Descriptors without running capability probes.
    #[cfg(all(test, unix))]
    pub fn providers(&self) -> Vec<ProviderDescriptor> {
        self.registry.descriptors()
    }

    /// The `/v2/providers` list, in `ProviderKind` order. Each driver's
    /// capability probes (cached, single-flight) run first, all drivers in
    /// parallel, so each entry reflects them.
    pub async fn providers_snapshot(&self) -> Vec<ProviderSnapshot> {
        futures_util::future::join_all(self.registry.drivers().map(|driver| async move {
            driver.refresh_control_capabilities().await;
            ProviderSnapshot::of(driver.as_ref())
        }))
        .await
    }

    pub fn has_active_turns(&self) -> bool {
        !self.active.is_empty()
    }

    /// Whether some follow-up queue holds items, paused or not: they exist
    /// only in memory, so a restart would destroy them.
    pub fn has_pending_follow_ups(&self) -> bool {
        self.follow_up_queues
            .iter()
            .any(|queue| queue.holds_items())
    }

    pub fn has_active_turns_for_cli(&self, cli: ManagedCli) -> bool {
        let provider = cli.provider_kind();
        let binary = cli.binary(&self.config);
        self.active.iter().any(|entry| {
            let turn = entry.value();
            turn_runs_cli(
                &self.config.agent,
                turn.provider,
                turn.provider_profile.as_deref(),
                provider,
                binary,
            )
        })
    }

    pub async fn models_live(
        &self,
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        let _cli_permit = self.try_discovery_permits(provider, None)?;
        self.validate_workspace(workspace)?;
        let driver = self.registry.driver(provider)?;
        tokio::time::timeout(
            driver.discovery_timeout(),
            driver.discover_models(workspace),
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
        provider: ProviderKind,
        workspace: &Path,
        profile: Option<&str>,
        model: Option<&str>,
    ) -> Result<ProviderImageInputCapability, AppError> {
        let descriptor = self.registry.driver(provider)?.descriptor();
        let (image_input, source, reason) = match descriptor.capabilities.image_input_mode {
            ImageInputMode::Always => (true, "provider".to_owned(), None),
            ImageInputMode::Model => {
                let models = self.models_live(provider, workspace).await?;
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
                let _cli_permit = self.try_discovery_permits(provider, profile)?;
                self.validate_workspace(workspace)?;
                let driver = self.registry.driver(provider)?;
                let image_input = tokio::time::timeout(
                    driver.discovery_timeout(),
                    driver.discover_image_input(workspace, profile),
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
        provider: ProviderKind,
        workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        let _cli_permit = self.try_discovery_permits(provider, None)?;
        self.validate_workspace(workspace)?;
        let driver = self.registry.driver(provider)?;
        tokio::time::timeout(
            driver.discovery_timeout(),
            driver.discover_commands(workspace),
        )
        .await
        .map_err(|_| {
            AppError::ProviderUnavailable(format!(
                "{} command discovery timed out",
                provider.as_str()
            ))
        })?
    }

    /// A conversation query must use its authorized manifest, never a client's
    /// independently supplied provider or workspace. Only the owning worker may
    /// read the resident provider's stdout.
    pub async fn conversation_commands_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Value, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        let _cli_permit =
            self.try_discovery_permits(manifest.provider, manifest.provider_profile.as_deref())?;
        self.validate_workspace(&manifest.workspace)?;
        let driver = self.registry.driver(manifest.provider)?;
        let catalog = tokio::time::timeout(driver.discovery_timeout(), async {
            if let Some(catalog) = driver.session_commands(&manifest.id).await? {
                Ok(json!({
                    "provider": manifest.provider, "conversationId": manifest.id,
                    "runtimeId": catalog.runtime_id, "commands": catalog.commands,
                    "catalogSource": "session", "source": "provider-session",
                    "fetchedAt": chrono::Utc::now().to_rfc3339(),
                }))
            } else {
                let commands = driver.discover_commands(&manifest.workspace).await?;
                Ok(json!({
                    "provider": manifest.provider, "conversationId": manifest.id,
                    "commands": commands, "catalogSource": "discovery",
                    "source": "provider-discovery", "fetchedAt": chrono::Utc::now().to_rfc3339(),
                }))
            }
        })
        .await
        .map_err(|_| {
            AppError::ProviderUnavailable("Conversation command discovery timed out".to_owned())
        })?;
        catalog
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
        self.create_inner(
            owner_id,
            provider,
            Some(workspace),
            title,
            provider_profile,
            None,
        )
        .await
    }

    /// [`Self::create_owned`] for an agent-call child of `parent_id`
    /// (`manifest.parentConversationId`).
    pub(crate) async fn create_child_owned(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: PathBuf,
        title: Option<String>,
        provider_profile: Option<String>,
        parent_id: &str,
    ) -> Result<ConversationManifest, AppError> {
        self.create_inner(
            owner_id,
            provider,
            Some(workspace),
            title,
            provider_profile,
            Some(parent_id.to_owned()),
        )
        .await
    }

    /// Creates a conversation without a user workspace: the daemon makes
    /// `<data_dir>/workspace/<conversation id>` and uses it as the workspace.
    pub async fn create_scratch_owned(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        title: Option<String>,
        provider_profile: Option<String>,
    ) -> Result<ConversationManifest, AppError> {
        self.create_inner(owner_id, provider, None, title, provider_profile, None)
            .await
    }

    async fn create_inner(
        &self,
        owner_id: &str,
        provider: ProviderKind,
        workspace: Option<PathBuf>,
        title: Option<String>,
        provider_profile: Option<String>,
        parent_conversation_id: Option<String>,
    ) -> Result<ConversationManifest, AppError> {
        validate_owner_id(owner_id)?;
        let validate = |workspace: &Path| {
            validate_workspace_directory_text(
                &self.config.workspace_access_roots(),
                workspace.to_str().ok_or_else(|| {
                    AppError::InvalidRequest("workspace path is not UTF-8".to_owned())
                })?,
            )
        };
        let validated = workspace.as_deref().map(validate).transpose()?;
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
        let mut manifest = ConversationManifest::new(
            provider,
            validated.unwrap_or_default(),
            title,
            provider_profile,
        );
        if workspace.is_none() {
            let folder = self.config.scratch_workspace_root().join(&manifest.id);
            tokio::fs::create_dir_all(&folder).await?;
            manifest.workspace = validate(&folder)?;
            manifest.scratch_workspace = true;
        }
        manifest.owner_id = owner_id.to_owned();
        manifest.parent_conversation_id = parent_conversation_id;
        manifest.workspace_id = Some(stable_workspace_id(&manifest.workspace));
        let scratch_folder = manifest
            .scratch_workspace
            .then(|| manifest.workspace.clone());
        let manifest = match self.store.create(manifest).await {
            Ok(manifest) => manifest,
            Err(error) => {
                // The folder is still empty; do not leave it orphaned.
                if let Some(folder) = scratch_folder {
                    let _ = tokio::fs::remove_dir(folder).await;
                }
                return Err(error);
            }
        };
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

    /// [`Self::get_owned`] for an operation that writes history: legacy
    /// plaintext conversations are read-only (`HISTORY_READ_ONLY`,
    /// docs/history-encryption.md §8).
    pub async fn writable_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationManifest, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        self.store.ensure_not_legacy(conversation_id).await?;
        Ok(manifest)
    }

    /// Rejects a conversation workspace that is no longer an existing
    /// directory inside the configured (or scratch) workspace roots.
    fn validate_workspace(&self, workspace: &Path) -> Result<(), AppError> {
        crate::workspace_paths::validate_workspace_directory(
            &self.config.workspace_access_roots(),
            workspace,
        )
        .map(drop)
    }

    pub async fn update_metadata_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        title: Option<Option<String>>,
        archived: Option<bool>,
    ) -> Result<ConversationManifest, AppError> {
        // Archiving only touches the manifest; renaming writes a new title.
        let manifest = if title.is_some() {
            self.writable_owned(owner_id, conversation_id).await?
        } else {
            self.get_owned(owner_id, conversation_id).await?
        };
        let updated = self
            .store
            .update_metadata(&manifest.id, title, archived)
            .await?;
        // An archived conversation is not sent later; its held items stay
        // queued under a user pause.
        if archived == Some(true) && updated.schedule.is_some() {
            self.cancel_schedule(&manifest.id).await?;
            return self.store.get(&manifest.id).await;
        }
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
        let driver = self.registry.driver(manifest.provider)?;
        driver
            .shutdown_session_with_reason(&manifest.id, "conversation_deleted")
            .await;
        self.store.delete(&manifest.id).await?;
        self.release_removed(&manifest, None).await;
        Ok(manifest)
    }

    /// Releases what a removed conversation still holds outside its
    /// directory: the provider session (when `shutdown_reason` is set; a
    /// caller may have shut it down before removing), the follow-up queue,
    /// the agent MCP grant and the event hub channel. Shared by every
    /// deletion path so none of them leaks a resident process or a live
    /// subscription.
    pub(super) async fn release_removed(
        &self,
        manifest: &ConversationManifest,
        shutdown_reason: Option<&str>,
    ) {
        if let Some(reason) = shutdown_reason {
            match self.registry.driver(manifest.provider) {
                Ok(driver) => {
                    driver
                        .shutdown_session_with_reason(&manifest.id, reason)
                        .await
                }
                Err(error) => tracing::warn!(
                    conversation_id = %manifest.id,
                    error = %error,
                    "no driver to shut down the session of a removed conversation"
                ),
            }
        }
        self.follow_up_queues.remove(&manifest.id);
        self.prewarm_sinks.remove(&manifest.id);
        self.revoke_agent_mcp(&manifest.id).await;
        if let Some(hooks) = self.hooks() {
            hooks.conversation_removed(manifest);
        }
        // Close the broadcast channel so live websocket subscriptions exit
        // instead of holding their per-connection slot forever.
        self.hub.remove(&manifest.id);
    }

    pub async fn cleanup_expired(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<ConversationManifest>, AppError> {
        let expired = |manifest: &ConversationManifest| {
            manifest.updated_at < cutoff
                && !matches!(
                    manifest.status,
                    ConversationStatus::Running | ConversationStatus::WaitingPermission
                )
        };
        // Choose candidates before taking the gate: listing reads every
        // manifest, and the gate blocks turn starts meanwhile.
        let candidates = self
            .store
            .list()
            .await?
            .into_iter()
            .filter(|manifest| expired(manifest))
            .map(|manifest| manifest.id)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // No turn/fork reservation starts while candidates are checked
        // against the active set and removed; each is re-checked under its
        // conversation lock, since it may have been used since the listing.
        let _cli_gate = self.cli_execution_gate.write().await;
        let mut removed = Vec::new();
        for conversation_id in candidates {
            if self.active.contains_key(&conversation_id) {
                continue;
            }
            if let Some(manifest) = self.store.delete_if(&conversation_id, expired).await? {
                self.release_removed(&manifest, Some("conversation_expired"))
                    .await;
                removed.push(manifest);
            }
        }
        Ok(removed)
    }

    // Its tests are Unix-only.
    #[cfg(all(test, unix))]
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

    /// Forward replay page; encrypted records carry the ciphertext of
    /// `detail` (see [`ConversationStore::replay_detail`]).
    pub async fn replay_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
        detail: ReplayDetail,
    ) -> Result<ConversationReplay, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        self.store
            .replay_detail(conversation_id, after_sequence, limit, detail)
            .await
    }

    pub async fn replay_before_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        before_sequence: u64,
        limit: usize,
        detail: ReplayDetail,
    ) -> Result<ConversationReplay, AppError> {
        self.get_owned(owner_id, conversation_id).await?;
        self.store
            .replay_before_detail(conversation_id, before_sequence, limit, detail)
            .await
    }

    /// Re-submit the latest prompt with its saved settings. With history
    /// encryption on the saved request holds no prompt text, so the client
    /// sends the decrypted original as `prompt`; it must match the MAC the
    /// request recorded. While encryption is off and the saved request has
    /// its text, `prompt` is ignored.
    pub async fn retry_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        client_request_id: Option<String>,
        prompt_text: Option<String>,
        retry_content: Option<Vec<PromptContentRef>>,
    ) -> Result<String, AppError> {
        self.writable_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        self.ensure_not_scheduled(conversation_id).await?;
        let latest = match self
            .store
            .digest(conversation_id, |digest| digest.last_user_message())
            .await?
        {
            Some(sequence) => self.store.event_at(conversation_id, sequence).await?,
            None => None,
        }
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
        let saved = &snapshot["request"];
        let mut prompt: ConversationPrompt = serde_json::from_value(saved.clone())?;
        let encrypted = self.store.history_encrypted();
        if let Some(text_mac) = saved.get("textMac").and_then(Value::as_str) {
            let text = prompt_text.ok_or_else(retry_prompt_required)?;
            let key = self.store.fingerprint_key()?.ok_or_else(|| {
                AppError::Conflict(
                    "The saved request is encrypted but no history key is available.".to_owned(),
                )
            })?;
            // The MAC covers the trimmed text, which is what `message.created`
            // shows; snapshots sealed before that covered the raw text.
            if key.mac(text.trim()) != text_mac && key.mac(&text) != text_mac {
                return Err(AppError::Conflict(
                    "prompt is not the text of the request being retried".to_owned(),
                ));
            }
            prompt.text = text;
            prompt.content = retry_content_checked(saved, retry_content, &key)?;
        } else if encrypted {
            let text = prompt_text.ok_or_else(retry_prompt_required)?;
            if text.trim() != prompt.text.trim() {
                return Err(AppError::Conflict(
                    "prompt is not the text of the request being retried".to_owned(),
                ));
            }
        }
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
        // A fork of legacy plaintext history would copy plaintext into a
        // writable conversation.
        let source = self.writable_owned(owner_id, conversation_id).await?;
        let driver = self.registry.driver(source.provider)?;
        // ACP-family drivers learn fork support by probing the installed
        // agent; refresh so a direct conversation.fork does not fail on an
        // empty cache when /v2/providers was never fetched.
        driver.refresh_control_capabilities().await;
        if !driver.supports_native_fork() {
            return Err(AppError::Unsupported(
                "Native conversation fork is not supported by this provider.".to_owned(),
            ));
        }
        let _cli_permit = self
            .start_permits(source.provider, source.provider_profile.as_deref())
            .await;
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
                    provider: source.provider,
                    provider_profile: source.provider_profile.clone(),
                });
            }
        }
        let _cleanup = ActiveTurnCleanup {
            active: self.active.clone(),
            conversation_id: conversation_id.to_owned(),
            agent_mcp: None,
        };
        self.validate_workspace(&source.workspace)?;
        let provider_state = self.store.provider_state(conversation_id).await?;
        let request = self.store.last_request(conversation_id).await?;
        let native_fork = driver
            .fork_session(DriverContext {
                manifest: source.clone(),
                provider_state,
                agent_mcp: None,
                handles: Default::default(),
            })
            .await?;
        let mut fork = ConversationManifest::new(
            source.provider,
            source.workspace.clone(),
            title.or_else(|| source.title.as_ref().map(|value| format!("{value} (fork)"))),
            source.provider_profile.clone(),
        );
        fork.owner_id = owner_id.to_owned();
        fork.workspace_id = source.workspace_id.clone();
        // The history is streamed page by page into the new journal, so a
        // fork of any size copies in bounded memory.
        let fork_id = fork.id.clone();
        let provider = source.provider;
        let source_id = source.id.clone();
        self.store
            .create_from_journal(
                conversation_id,
                fork,
                Some(native_fork),
                request,
                |event, sequence| {
                    let mut next = crate::conversation::ConversationEvent::new(
                        &fork_id,
                        sequence,
                        event.event_type,
                        event.payload,
                    );
                    next.provider = Some(provider);
                    next.time = event.time;
                    next
                },
                |copied| {
                    let mut completed = crate::conversation::ConversationEvent::new(
                        &fork_id,
                        copied + 1,
                        "conversation.forked",
                        json!({ "sourceConversationId": source_id, "sourceSequence": copied }),
                    );
                    completed.provider = Some(provider);
                    vec![completed]
                },
            )
            .await
    }

    pub async fn compact_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        client_request_id: &str,
    ) -> Result<String, AppError> {
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        let manifest = self.writable_owned(owner_id, conversation_id).await?;
        let driver = self.registry.driver(manifest.provider)?;
        if !driver.supports_native_compact() {
            return Err(AppError::Unsupported(
                "Native compaction is not supported by this provider.".to_owned(),
            ));
        }
        let _cli_permit = self
            .start_permits(manifest.provider, manifest.provider_profile.as_deref())
            .await;
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
                    provider: manifest.provider,
                    provider_profile: manifest.provider_profile.clone(),
                });
            }
        }
        let cleanup = ActiveTurnCleanup {
            active: self.active.clone(),
            conversation_id: conversation_id.to_owned(),
            agent_mcp: None,
        };
        self.validate_workspace(&manifest.workspace)?;
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
            let cleanup = cleanup;
            let result = driver
                .compact_session(
                    DriverContext {
                        manifest,
                        provider_state,
                        agent_mcp: None,
                        handles: Default::default(),
                    },
                    cancel_rx,
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
            // Prompts queued during compaction continue after it.
            drop(cleanup);
            supervisor.schedule_after_turn(conversation_id, None, "compaction.finished");
        });
        Ok(operation_id)
    }

    pub async fn stop_runtime_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Value, AppError> {
        let _guard = self.request_gate(conversation_id).lock_owned().await;
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        // Closing a runtime is distinct from aborting its current model turn.
        // The worker closes pending dialogs and reports any interrupted turn.
        self.registry
            .driver(manifest.provider)?
            .stop_runtime(&manifest.id)
            .await
    }

    /// Starts the conversation's provider process ahead of its next prompt,
    /// prepared with the settings of its last request (`conversation.warm`).
    /// Best effort: `Ok(Some(reason))` when it was skipped, `Ok(None)` when
    /// it warmed. Only an unknown conversation or another owner's is an
    /// error. Nothing is journalled and the manifest is not touched.
    pub async fn warm_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
    ) -> Result<Option<&'static str>, AppError> {
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        if !self.config.agent.provider_prewarm {
            return Ok(Some("disabled"));
        }
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(Some("busy"));
        }
        if self.active.contains_key(conversation_id) {
            return Ok(Some("running"));
        }
        if manifest
            .schedule
            .as_ref()
            .is_some_and(crate::conversation::ConversationSchedule::is_active)
        {
            return Ok(Some("scheduled"));
        }
        match self.store.ensure_not_legacy(conversation_id).await {
            Ok(()) => {}
            Err(AppError::HistoryReadOnly) => return Ok(Some("read_only")),
            Err(error) => return Err(error),
        }
        let driver = self.registry.driver(manifest.provider)?;
        if !driver.descriptor().available {
            return Ok(Some("unavailable"));
        }
        let Ok(_slot) = self.prewarms.clone().try_acquire_owned() else {
            return Ok(Some("busy"));
        };
        // An install or upgrade of the CLI, or a cleanup, holds a lock
        // exclusively; never wait for it. An admission check only: the guard is dropped before the provider
        // is awaited. Tokio's RwLock is write-preferring, so a queued writer
        // (`cleanup_expired`, a CLI upgrade) would otherwise block every
        // prompt's read behind a slow warm for up to PREWARM_TIMEOUT. The
        // re-check after the warm covers a conversation removed meanwhile.
        if self
            .try_discovery_permits(manifest.provider, manifest.provider_profile.as_deref())
            .is_err()
        {
            return Ok(Some("busy"));
        }
        if let Err(error) = self.validate_workspace(&manifest.workspace) {
            tracing::debug!(conversation_id, error = %error, "prewarm skipped: workspace unavailable");
            return Ok(Some("workspace_unavailable"));
        }
        let provider_state = self.store.provider_state(conversation_id).await?;
        let settings = self.warm_settings(&manifest).await?;
        let agent_mcp = self.turn_agent_mcp(&manifest).await;
        let sink = DriverEventSink::new(
            self.store.clone(),
            self.hub.clone(),
            self.permissions.clone(),
            conversation_id,
        )
        .muted();
        let context = DriverContext {
            manifest,
            provider_state,
            agent_mcp,
            handles: Default::default(),
        };
        // Registered before the provider is awaited, so a turn submitted
        // during the warm (or after a timeout) still takes its events over.
        // An older prewarm's sink, which a resident process may still use,
        // is handed over to this one.
        if let Some(previous) = self
            .prewarm_sinks
            .insert(conversation_id.to_owned(), sink.clone())
        {
            previous.hand_over(&sink).await;
        }
        let warmed = tokio::time::timeout(
            PREWARM_TIMEOUT,
            driver.prewarm(context, settings, sink.clone()),
        )
        .await;
        // The conversation may have been deleted or expired, or the daemon
        // may have started shutting down, while the provider started: stop
        // what the warm revived instead of leaving it resident.
        let shutting_down = self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire);
        let deleted = matches!(
            self.store.get(conversation_id).await,
            Err(AppError::NotFound(_))
        );
        if shutting_down || deleted {
            driver
                .shutdown_session_with_reason(conversation_id, "conversation_deleted")
                .await;
            self.revoke_agent_mcp(conversation_id).await;
            self.prewarm_sinks.remove(conversation_id);
            return if deleted {
                Err(AppError::NotFound(format!(
                    "conversation {conversation_id}"
                )))
            } else {
                Ok(Some("busy"))
            };
        }
        Ok(match warmed {
            Ok(Ok(outcome)) => outcome.reason(),
            Ok(Err(error)) => {
                tracing::warn!(conversation_id, error = %error, "provider prewarm failed");
                Some("failed")
            }
            Err(_) => Some("timeout"),
        })
    }

    /// The settings of the conversation's last request (model, reasoning
    /// effort, permission and work mode), or the provider defaults when
    /// there is none or they no longer validate.
    async fn warm_settings(
        &self,
        manifest: &ConversationManifest,
    ) -> Result<DriverPrompt, AppError> {
        let request = self
            .store
            .last_request(&manifest.id)
            .await?
            .and_then(|snapshot| snapshot.get("request").cloned())
            .unwrap_or_default();
        let field = |name: &str| request.get(name).and_then(Value::as_str).map(str::to_owned);
        let mut settings = DriverPrompt {
            turn_id: format!("prewarm_{}", Uuid::new_v4().simple()),
            text: String::new(),
            content: Vec::new(),
            skills: Vec::new(),
            model: field("model"),
            reasoning_effort: field("reasoningEffort"),
            permission_mode: field("permissionMode"),
            work_mode: field("workMode"),
            permission_profile: field("permissionProfile"),
            sandbox_mode: field("sandboxMode"),
            approval_policy: field("approvalPolicy"),
        };
        if super::types::resolve_execution_config(
            manifest.provider,
            settings.permission_mode.as_deref(),
            settings.work_mode.as_deref(),
            settings.permission_profile.as_deref(),
            settings.sandbox_mode.as_deref(),
            settings.approval_policy.as_deref(),
        )
        .is_err()
        {
            settings.permission_mode = None;
            settings.work_mode = None;
            settings.permission_profile = None;
            settings.sandbox_mode = None;
            settings.approval_policy = None;
        }
        Ok(settings)
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
        self.writable_owned(owner_id, conversation_id).await?;
        let guard = self.request_gate(conversation_id).lock_owned().await;
        let manifest = self.get_owned(owner_id, conversation_id).await?;
        self.validate_workspace(&manifest.workspace)?;
        validate_provider_control(request_id, expected_turn_id, &control)?;
        let serialized = serde_json::to_value(&control)?;
        // The digest names the first request and newest outcome recorded
        // under this id; only those two events are read for their payloads.
        let recorded = self
            .store
            .digest(conversation_id, |digest| {
                digest.control(request_id).cloned()
            })
            .await?
            .unwrap_or_default();
        if let Some(prior) = recorded.requested {
            let prior = self.recorded_event(conversation_id, prior).await?;
            // An encrypted record keeps only the MAC of its control object.
            let same_input = if encrypted_content(&prior.payload).is_some() {
                let key = self.store.fingerprint_key()?;
                prior
                    .payload
                    .get("requestFingerprint")
                    .and_then(Value::as_str)
                    .is_some_and(|recorded| {
                        key.as_deref()
                            .is_some_and(|key| control_mac(key, &serialized) == recorded)
                    })
            } else {
                prior.payload.get("control") == Some(&serialized)
            };
            if !same_input
                || prior.payload.get("turnId").and_then(Value::as_str) != Some(expected_turn_id)
            {
                return Err(AppError::Conflict(
                    "Control request ID was already used with different input.".to_owned(),
                ));
            }
            if let Some((outcome, sequence)) = recorded.outcome {
                let done = self.recorded_event(conversation_id, sequence).await?;
                let encrypted = encrypted_content(&done.payload).is_some();
                if outcome == ControlOutcome::Completed {
                    if !encrypted {
                        return Ok(done.payload.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // Encrypted: the result is in the `control.completed`
                    // record only as ciphertext; this process may still
                    // remember it, else `null` (the client can read it).
                    return Ok(self
                        .control_result(conversation_id, request_id)
                        .unwrap_or(Value::Null));
                }
                if outcome == ControlOutcome::Unknown {
                    return Err(AppError::ProviderUnavailable("Control delivery outcome is unknown; it was not sent again. Inspect the effective state before issuing a new request.".to_owned()));
                }
                let message = match done.payload.get("message").and_then(Value::as_str) {
                    Some(message) => message.to_owned(),
                    None => match done.payload.get("code").and_then(Value::as_str) {
                        Some(code) => format!("Control was rejected ({code})."),
                        None => "Control was rejected.".to_owned(),
                    },
                };
                return Err(AppError::Conflict(message));
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
            if let Ok(value) = &result {
                supervisor.remember_control_result(&conversation_id, &request_id, value);
            }
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
        self.writable_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        // Queue delivery goes through `prompt_inner` and is not refused.
        self.ensure_not_scheduled(conversation_id).await?;
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
        // Every prompt path (direct, retry, queue delivery) passes here;
        // `save_request` checks again under the conversation lock.
        self.store.ensure_not_legacy(conversation_id).await?;
        let request_fingerprint = prompt_fingerprint(&prompt)?;
        let request_snapshot = prompt.clone();
        let fingerprint_key = self.store.fingerprint_key()?;
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
        self.validate_workspace(&manifest.workspace)?;
        if let Some(request_id) = client_request_id.as_deref() {
            // The digest names the first message recorded under this id and
            // any native queue/steer control that delivered its text; only
            // those events are read for their payloads.
            let (previous, control) = self
                .store
                .digest(conversation_id, |digest| {
                    (
                        digest.client_request(request_id).map(|facts| facts.first),
                        digest.text_control(request_id),
                    )
                })
                .await?;
            if let Some(previous) = previous {
                let previous = self.recorded_event(conversation_id, previous).await?;
                let matches = if let Some(recorded) = previous
                    .payload
                    .get("requestFingerprint")
                    .and_then(Value::as_str)
                {
                    fingerprint_matches(recorded, &request_fingerprint, fingerprint_key.as_deref())
                } else {
                    // Native queue/steering deliveries have an original durable control,
                    // rather than a prompt snapshot. Recognize exactly that text-only
                    // submission so reconnect cannot execute its tools a second time.
                    // An encrypted control keeps only the MAC of its text.
                    let control_text = match control {
                        Some(sequence) => {
                            let event = self.recorded_event(conversation_id, sequence).await?;
                            let control = &event.payload["control"];
                            match (control["text"].as_str(), control["textMac"].as_str()) {
                                (Some(text), _) => Some(request_snapshot.text == text),
                                (None, Some(mac)) => Some(
                                    fingerprint_key
                                        .as_deref()
                                        .is_some_and(|key| key.mac(&request_snapshot.text) == mac),
                                ),
                                (None, None) => None,
                            }
                        }
                        None => None,
                    };
                    if let Some(text_matches) = control_text {
                        text_matches
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
                        let plain = saved_request(&request_snapshot, None)?;
                        let sealed = saved_request(&request_snapshot, fingerprint_key.as_deref())?;
                        saved.get("turnId") == previous.payload.get("turnId")
                            && saved
                                .get("request")
                                .is_some_and(|request| *request == plain || *request == sealed)
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
        let (content_text, driver_content, content_hashes) =
            prepare_prompt_content_hashed(manifest.provider, &manifest.workspace, content).await?;
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
        let provider_text = provider_prompt_text(manifest.provider, &user_text, &injected);
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
        let agent_mcp = self.turn_agent_mcp(&manifest).await;

        let mut snapshot_files = Vec::new();
        for item in &driver_content {
            let path = match item {
                DriverPromptContent::File { path, .. } => Some(path),
                DriverPromptContent::Image { path, .. } => path.as_ref(),
            };
            if let Some(path) = path {
                // Images were hashed from the bytes sent to the provider;
                // only file attachments are read here.
                let sha256 = match content_hashes.get(path) {
                    Some(sha256) => sha256.clone(),
                    None => fingerprint_file(path).await?,
                };
                snapshot_files.push(RequestFileFingerprint {
                    path: path.clone(),
                    sha256,
                });
            }
        }
        for skill in &loaded_skills {
            // The skill's whole file, as read (UTF-8, unmodified) for injection.
            snapshot_files.push(RequestFileFingerprint {
                path: skill.path.clone(),
                sha256: sha256_hex(skill.content.as_bytes()),
            });
        }
        let (turn_id, cancel_rx) = self.begin_turn(conversation_id, &manifest).await?;

        let sealed_key = fingerprint_key
            .as_deref()
            .filter(|_| self.store.history_encrypted());
        let saved_request =
            saved_request(&request_snapshot, sealed_key).map(|saved| (saved, sealed_key.is_some()));
        let (saved_request, request_sealed) = match saved_request {
            Ok(saved) => saved,
            Err(error) => {
                self.active.remove(conversation_id);
                return Err(error);
            }
        };
        if let Err(error) = self.store.save_request(conversation_id, &json!({
            "schemaVersion": 1, "turnId": turn_id, "request": saved_request, "files": snapshot_files,
        })).await {
            self.active.remove(conversation_id);
            return Err(error);
        }
        // The user message, the injected skills and the turn start are
        // journalled with one write and one sync.
        let mut opening = vec![(
            "message.created".to_owned(),
            user_message_payload(
                &turn_id,
                client_request_id.as_deref(),
                &request_fingerprint,
                &user_text,
                request_sealed.then_some(&request_snapshot),
            ),
        )];
        if !injected.is_empty() {
            opening.push((
                "skill.injected".to_owned(),
                json!({
                    "turnId": turn_id,
                    "skills": injected.iter().map(|(name, content)| json!({
                        "name": name,
                        "bytes": content.len(),
                    })).collect::<Vec<_>>(),
                }),
            ));
        }
        opening.push((
            "turn.started".to_owned(),
            json!({ "turnId": turn_id, "clientRequestId": client_request_id, "provider": manifest.provider,
                "requestedPermissions": { "permissionMode": permission_mode, "workMode": work_mode, "profile": permission_profile, "sandboxMode": sandbox_mode, "approvalPolicy": approval_policy },
                "effectivePermissions": effective_permissions, "configurationStatus": "validated" }),
        ));
        if let Err(error) = self
            .store
            .append_many_and_publish(conversation_id, opening, &self.hub)
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

        self.spawn_turn(SpawnTurn {
            owner_id: owner_id.to_owned(),
            turn_id: turn_id.clone(),
            client_request_id,
            manifest,
            provider_state,
            agent_mcp,
            effective_permissions,
            driver,
            prompt: DriverPrompt {
                turn_id: turn_id.clone(),
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
            cancel: cancel_rx,
            adopt: None,
        });
        Ok(turn_id)
    }

    /// Registers `conversation_id`'s new turn as running once its workspace
    /// is still accessible. Fails when a turn already runs there.
    async fn begin_turn(
        &self,
        conversation_id: &str,
        manifest: &ConversationManifest,
    ) -> Result<(String, watch::Receiver<bool>), AppError> {
        let cli_start_permit = self
            .start_permits(manifest.provider, manifest.provider_profile.as_deref())
            .await;
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
                    provider: manifest.provider,
                    provider_profile: manifest.provider_profile.clone(),
                });
            }
        }
        drop(cli_start_permit);
        if let Err(error) = self.validate_workspace(&manifest.workspace) {
            self.active.remove(conversation_id);
            return Err(error);
        }
        Ok((turn_id, cancel_rx))
    }

    /// Runs a journalled, registered turn's driver in the background and
    /// journals its terminal event.
    fn spawn_turn(&self, turn: SpawnTurn) {
        let SpawnTurn {
            owner_id,
            turn_id,
            client_request_id,
            manifest,
            provider_state,
            agent_mcp,
            effective_permissions,
            driver,
            prompt: driver_prompt,
            cancel: cancel_rx,
            adopt,
        } = turn;
        let conversation_id = manifest.id.clone();
        // The turn has started: TodeX's own MCP tools follow its permission
        // mode until it ends (then they fall back to asking).
        if let Some(agent_mcp) = &self.agent_mcp {
            agent_mcp.record_turn_mode(
                &conversation_id,
                &effective_permissions.permission_mode,
                &effective_permissions.work_mode,
            );
        }
        let provider_work = {
            let supervisor = self.clone();
            let conversation_id = conversation_id.clone();
            let lost = (
                supervisor.clone(),
                conversation_id.clone(),
                manifest.provider,
            );
            ProviderWorkHandle::new(move |work| {
                let supervisor = supervisor.clone();
                let owner_id = owner_id.clone();
                let conversation_id = conversation_id.clone();
                Box::pin(async move {
                    supervisor
                        .start_provider_turn(&owner_id, &conversation_id, work)
                        .await
                })
            })
            .with_lost(move |work| {
                let (supervisor, conversation_id, provider) = lost.clone();
                Box::pin(async move {
                    supervisor
                        .journal_lost_work(&conversation_id, provider, &work.task_ids, work.reason)
                        .await;
                })
            })
        };
        let supervisor = self.clone();
        let spawned_turn_id = turn_id;
        let cleanup = ActiveTurnCleanup {
            active: self.active.clone(),
            conversation_id: conversation_id.clone(),
            agent_mcp: self.agent_mcp.clone(),
        };
        tokio::spawn(async move {
            let cleanup = cleanup;
            let activity = ProviderActivity::new();
            let sink = DriverEventSink::new(
                supervisor.store.clone(),
                supervisor.hub.clone(),
                supervisor.permissions.clone(),
                conversation_id.clone(),
            )
            .with_turn_id(spawned_turn_id.clone())
            .with_activity(activity.clone())
            .with_quota(supervisor.quota.clone());
            if let Some((_, warm)) = supervisor.prewarm_sinks.remove(&conversation_id) {
                warm.hand_over(&sink).await;
            }
            let driver_context = DriverContext {
                manifest,
                provider_state: provider_state.clone(),
                agent_mcp,
                handles: TurnHandles {
                    mode: supervisor
                        .agent_mcp
                        .clone()
                        .map(|agent_mcp| TurnModeHandle {
                            agent_mcp,
                            conversation_id: conversation_id.clone(),
                        }),
                    provider_work: Some(provider_work),
                    adopt,
                },
            };
            // The driver runs in its own task so a panic surfaces as a
            // JoinError here and still produces a terminal event.
            let mut driver_task = tokio::spawn(async move {
                driver
                    .run_turn(driver_context, driver_prompt, sink, cancel_rx)
                    .await
            });
            let mut idle_timeout = None;
            let outcome = match supervisor.provider_idle_timeout {
                None => (&mut driver_task).await,
                Some(limit) => tokio::select! {
                    outcome = &mut driver_task => outcome,
                    () = supervisor.wait_for_provider_idle(&conversation_id, &activity, limit) => {
                        idle_timeout = Some(limit);
                        supervisor
                            .stop_idle_turn(&conversation_id, &spawned_turn_id, limit, driver_task)
                            .await
                    }
                },
            };
            let terminal = supervisor
                .finish_turn(
                    &conversation_id,
                    &spawned_turn_id,
                    client_request_id,
                    provider_state,
                    outcome,
                    idle_timeout,
                )
                .await;
            // The queue's next prompt needs the conversation released first.
            drop(cleanup);
            supervisor.schedule_after_turn(conversation_id, Some(spawned_turn_id), terminal);
        });
    }

    /// Opens a turn for work the provider started on its own after the
    /// conversation's last turn ended: no user message, the last turn's
    /// launch settings, and `trigger: "provider"` on `turn.started`. Refused
    /// like a prompt when the conversation is busy, gone, read-only or its
    /// workspace is no longer accessible; the driver then stops the work.
    pub(crate) async fn start_provider_turn(
        &self,
        owner_id: &str,
        conversation_id: &str,
        work: ProviderWork,
    ) -> Result<(), AppError> {
        self.writable_owned(owner_id, conversation_id).await?;
        let _request_guard = self.request_gate(conversation_id).lock_owned().await;
        self.store.ensure_not_legacy(conversation_id).await?;
        let manifest = self.store.get(conversation_id).await?;
        ensure_owner(&manifest, owner_id)?;
        self.validate_workspace(&manifest.workspace)?;
        let ProviderWork {
            adoption,
            settings,
            trigger,
        } = work;
        let effective_permissions = super::types::resolve_execution_config(
            manifest.provider,
            settings.permission_mode.as_deref(),
            settings.work_mode.as_deref(),
            settings.permission_profile.as_deref(),
            settings.sandbox_mode.as_deref(),
            settings.approval_policy.as_deref(),
        )?;
        let driver = self.registry.driver(manifest.provider)?;
        let provider_state = self.store.provider_state(conversation_id).await?;
        let agent_mcp = self.turn_agent_mcp(&manifest).await;
        let (turn_id, cancel_rx) = self.begin_turn(conversation_id, &manifest).await?;
        let started = json!({
            "turnId": turn_id, "clientRequestId": null, "provider": manifest.provider,
            "requestedPermissions": { "permissionMode": settings.permission_mode, "workMode": settings.work_mode, "profile": settings.permission_profile, "sandboxMode": settings.sandbox_mode, "approvalPolicy": settings.approval_policy },
            "effectivePermissions": effective_permissions, "configurationStatus": "validated",
            "trigger": "provider", "triggerDetail": trigger,
        });
        if let Err(error) = self
            .store
            .append_many_and_publish(
                conversation_id,
                vec![("turn.started".to_owned(), started)],
                &self.hub,
            )
            .await
        {
            self.active.remove(conversation_id);
            return Err(error);
        }
        tracing::info!(
            conversation_id,
            "provider started work on its own; opened a turn for it"
        );
        self.spawn_turn(SpawnTurn {
            owner_id: owner_id.to_owned(),
            turn_id: turn_id.clone(),
            client_request_id: None,
            manifest,
            provider_state,
            agent_mcp,
            effective_permissions,
            driver,
            prompt: DriverPrompt {
                turn_id,
                text: String::new(),
                content: Vec::new(),
                skills: Vec::new(),
                ..settings
            },
            cancel: cancel_rx,
            adopt: Some(adoption),
        });
        Ok(())
    }

    /// Journals exactly one terminal event for a turn whose driver task ended,
    /// including a driver that panicked.
    async fn finish_turn(
        &self,
        conversation_id: &str,
        turn_id: &str,
        client_request_id: Option<String>,
        provider_state: ProviderState,
        outcome: Result<Result<DriverTurnResult, AppError>, tokio::task::JoinError>,
        idle_timeout: Option<Duration>,
    ) -> &'static str {
        let (event_type, payload) = match (outcome, idle_timeout) {
            // However the driver wound down, the watchdog stopped this turn.
            (outcome, Some(limit)) => {
                match outcome {
                    Ok(Ok(_)) | Ok(Err(AppError::TurnCancelled)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(conversation_id, turn_id, error = %error, "provider driver failed while stopping an idle turn")
                    }
                    Err(error) => {
                        tracing::warn!(conversation_id, turn_id, error = %error, "provider driver task ended abnormally while stopping an idle turn")
                    }
                }
                let message = idle_timeout_message(limit);
                self.record_provider_error(conversation_id, provider_state, &message)
                    .await;
                (
                    "turn.failed",
                    json!({
                        "turnId": turn_id,
                        "clientRequestId": client_request_id,
                        "code": "PROVIDER_IDLE_TIMEOUT",
                        "message": message,
                    }),
                )
            }
            (Ok(Ok(result)), None) => (
                if result.cancelled {
                    "turn.cancelled"
                } else {
                    "turn.completed"
                },
                json!({
                    "turnId": turn_id,
                    "clientRequestId": client_request_id,
                    "stopReason": result.stop_reason,
                    "nativeSessionId": result.native_session_id,
                }),
            ),
            (Ok(Err(AppError::TurnCancelled)), None) => (
                "turn.cancelled",
                json!({
                    "turnId": turn_id,
                    "clientRequestId": client_request_id,
                    "stopReason": "cancelled",
                    "nativeSessionId": Value::Null,
                }),
            ),
            (Ok(Err(error)), None) => {
                let message = error.to_string();
                self.record_provider_error(conversation_id, provider_state, &message)
                    .await;
                (
                    "turn.failed",
                    json!({
                        "turnId": turn_id,
                        "clientRequestId": client_request_id,
                        "code": error.code(),
                        "message": message,
                    }),
                )
            }
            (Err(error), None) => {
                let (code, message) = driver_task_failure(error);
                tracing::error!(
                    conversation_id,
                    turn_id,
                    code,
                    message,
                    "provider driver task failed"
                );
                self.record_provider_error(conversation_id, provider_state, &message)
                    .await;
                (
                    "turn.failed",
                    json!({
                        "turnId": turn_id,
                        "clientRequestId": client_request_id,
                        "code": code,
                        "message": message,
                    }),
                )
            }
        };
        self.emit_terminal(conversation_id, turn_id, event_type, payload)
            .await;
        // Seal a large active journal file at the turn boundary while its DEK
        // is in memory (docs/history-encryption.md §4.1); never on shutdown,
        // where the conversion could not finish anyway.
        if !self.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            self.store.seal_after_turn(conversation_id).await;
        }
        event_type
    }

    /// Resolves once the turn's provider has been silent for `limit` while no
    /// permission dialog of this conversation is waiting on the user.
    async fn wait_for_provider_idle(
        &self,
        conversation_id: &str,
        activity: &ProviderActivity,
        limit: Duration,
    ) {
        loop {
            let idle = activity.idle_for();
            if idle < limit {
                sleep(limit - idle).await;
                continue;
            }
            if !self.permissions.has_pending(conversation_id) {
                return;
            }
            // Resolving the dialog emits permission.resolved, which restarts
            // the idle clock; until then poll at a coarse interval.
            sleep(limit.min(IDLE_WATCHDOG_RECHECK)).await;
        }
    }

    /// Cancels an idle turn through its normal cancellation path and waits
    /// for the driver, aborting it (which kills its process) if it does not
    /// wind down in time.
    async fn stop_idle_turn(
        &self,
        conversation_id: &str,
        turn_id: &str,
        limit: Duration,
        mut driver_task: tokio::task::JoinHandle<Result<DriverTurnResult, AppError>>,
    ) -> Result<Result<DriverTurnResult, AppError>, tokio::task::JoinError> {
        tracing::warn!(
            conversation_id,
            turn_id,
            idle_secs = limit.as_secs(),
            "provider produced no output within the idle timeout; stopping the turn"
        );
        if let Some(active) = self
            .active
            .get(conversation_id)
            .filter(|active| active.turn_id == turn_id)
        {
            // The receiver lives in the driver task; a closed channel means it
            // is already finishing, which the join below observes.
            let _ = active.cancel.send(true);
        }
        match tokio::time::timeout(IDLE_TURN_STOP_GRACE, &mut driver_task).await {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::error!(
                    conversation_id,
                    turn_id,
                    "provider driver ignored cancellation after the idle timeout; aborting it"
                );
                driver_task.abort();
                driver_task.await
            }
        }
    }

    async fn record_provider_error(
        &self,
        conversation_id: &str,
        provider_state: ProviderState,
        message: &str,
    ) {
        // A driver may have persisted a new native session or an extension
        // switch during this turn. Do not roll it back.
        let mut state = match self.store.provider_state(conversation_id).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(conversation_id, error = %error, "failed to reload provider state; recording the error on the turn's snapshot");
                provider_state
            }
        };
        state.last_error = Some(message.chars().take(1000).collect());
        if let Err(error) = self.store.save_provider_state(conversation_id, state).await {
            tracing::error!(conversation_id, error = %error, "failed to persist provider error state");
        }
    }

    /// A terminal event is what returns a conversation to idle, so a transient
    /// store failure is retried. A retry first checks whether the previous
    /// attempt reached the journal before failing, so it never duplicates.
    async fn emit_terminal(
        &self,
        conversation_id: &str,
        turn_id: &str,
        event_type: &str,
        payload: Value,
    ) {
        let result = retry_with_backoff(&TERMINAL_EVENT_RETRY_DELAYS, |attempt| {
            let payload = payload.clone();
            async move {
                if attempt > 0
                    && self
                        .journal_has_turn_event(conversation_id, turn_id, event_type)
                        .await
                {
                    return Ok(());
                }
                self.emit(conversation_id, event_type, payload).await
            }
        })
        .await;
        if let Err(error) = result {
            tracing::error!(
                conversation_id,
                turn_id,
                event_type,
                error = %error,
                "failed to persist terminal turn event; restart recovery closes the turn in the journal"
            );
            if let Err(error) = self
                .store
                .set_status(conversation_id, ConversationStatus::Failed)
                .await
            {
                tracing::error!(
                    conversation_id,
                    error = %error,
                    "failed to mark the conversation failed after losing its terminal event"
                );
            }
        }
    }

    async fn journal_has_turn_event(
        &self,
        conversation_id: &str,
        turn_id: &str,
        event_type: &str,
    ) -> bool {
        let recorded = self
            .store
            .digest(conversation_id, |digest| {
                digest.has_turn_terminal(turn_id, event_type)
            })
            .await
            .and_then(|recorded| {
                recorded.ok_or_else(|| {
                    AppError::InvalidRequest(format!(
                        "{event_type} is not a turn terminal event the journal digest tracks"
                    ))
                })
            });
        match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                tracing::warn!(conversation_id, error = %error, "failed to read the journal before retrying a terminal event");
                false
            }
        }
    }

    #[allow(dead_code)]
    pub async fn cancel(&self, conversation_id: &str) -> Result<(), AppError> {
        self.cancel_owned("local", conversation_id, None)
            .await
            .map(drop)
    }

    /// Signals the conversation's active turn to stop. With
    /// `expected_turn_id`, only that turn is stopped: a request that names a
    /// turn which already ended (or another one) must not cancel whatever
    /// runs now, so it reports the active turn instead. Without it, whatever
    /// is active stops, and an idle conversation is a no-op.
    pub async fn cancel_owned(
        &self,
        owner_id: &str,
        conversation_id: &str,
        expected_turn_id: Option<&str>,
    ) -> Result<CancelOutcome, AppError> {
        ensure_owner(&self.store.get(conversation_id).await?, owner_id)?;
        // The map guard is held from the comparison through the send, so the
        // named turn cannot end and be replaced in between.
        let Some(active) = self.active.get(conversation_id) else {
            return Ok(match expected_turn_id {
                Some(_) => CancelOutcome::NotActive {
                    active_turn_id: None,
                },
                None => CancelOutcome::Signalled,
            });
        };
        if expected_turn_id.is_some_and(|expected| expected != active.turn_id) {
            return Ok(CancelOutcome::NotActive {
                active_turn_id: Some(active.turn_id.clone()),
            });
        }
        active
            .cancel
            .send(true)
            .map_err(|_| AppError::Conflict("turn has already stopped".to_owned()))?;
        Ok(CancelOutcome::Signalled)
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
            if let Some(active) = self.active.get(&manifest.id) {
                if active.cancel.send(true).is_ok() {
                    cancelled += 1;
                }
            }
            // Workspace deletion also stops resident extensions that are
            // running between model turns.
            self.registry
                .driver(manifest.provider)?
                .shutdown_session_with_reason(&manifest.id, "workspace_access_revoked")
                .await;
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
        self.resolve_permission_owned("local", "local", conversation_id, permission_id, decision)
            .await
    }

    /// `device_id` is the paired device answering, checked against requests
    /// that name the devices allowed to answer them.
    pub async fn resolve_permission_owned(
        &self,
        owner_id: &str,
        device_id: &str,
        conversation_id: &str,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AppError> {
        self.writable_owned(owner_id, conversation_id).await?;
        self.permissions
            .resolve(conversation_id, permission_id, device_id, decision)
            .await
    }

    pub fn subscribe(
        &self,
        conversation_id: &str,
    ) -> tokio::sync::broadcast::Receiver<crate::conversation::SharedConversationEvent> {
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
        self.validate_workspace(&manifest.workspace)?;
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
        self.validate_workspace(&manifest.workspace)?;
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
        let resource_ids = skills
            .iter()
            .map(|skill| skill.resource_id.as_str())
            .collect::<Vec<_>>();
        // One scan of the skill roots serves every selected skill.
        let resources = self
            .catalog
            .skill_resources(manifest.provider, manifest.workspace.clone(), &resource_ids)
            .await?;
        let mut injected = Vec::new();
        for (skill, resource) in skills.iter().zip(resources) {
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
        // Before any turn is cancelled: a turn that completes during shutdown
        // must not start the next queued item. Recovery pauses those queues
        // with `daemon_restarted` on the next start.
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        // Coalesced stream text waits on timers that die with the runtime.
        self.store.flush_pending_deltas().await;
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

    /// The journal event a digest lookup named. Journals keep every sequence
    /// (compaction and salvage only replace records in place), so a missing
    /// one means the journal was cut back underneath the lookup.
    async fn recorded_event(
        &self,
        conversation_id: &str,
        sequence: u64,
    ) -> Result<crate::conversation::ConversationEvent, AppError> {
        self.store
            .event_at(conversation_id, sequence)
            .await?
            .ok_or_else(|| {
                AppError::Conflict(format!(
                    "conversation {conversation_id} no longer holds journal event {sequence}"
                ))
            })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RequestFileFingerprint {
    path: PathBuf,
    sha256: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn fingerprint_file(path: &Path) -> Result<String, AppError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    // On the heap: a buffer held across `.await` is stored inline in this
    // future and from there in every caller's (`prompt_inner` and the
    // WebSocket, REST and follow-up queue paths above it). The inline 64 KiB
    // array made each of those futures 64 KiB larger, and unoptimized builds
    // keep several stack copies per frame, overflowing the 2 MiB worker stack.
    let mut buffer = vec![0u8; 64 * 1024];
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
    prepare_prompt_content_hashed(provider, workspace, content)
        .await
        .map(|(text, content, _)| (text, content))
}

/// [`prepare_prompt_content`], also returning the SHA-256 of each local file
/// it read, so the request snapshot need not read those files again.
async fn prepare_prompt_content_hashed(
    provider: ProviderKind,
    workspace: &Path,
    content: Vec<PromptContentRef>,
) -> Result<(String, Vec<DriverPromptContent>, BTreeMap<PathBuf, String>), AppError> {
    if content.len() > MAX_PROMPT_CONTENT_ITEMS {
        return Err(AppError::InvalidRequest(format!(
            "prompt content allows at most {MAX_PROMPT_CONTENT_ITEMS} items"
        )));
    }
    let workspace = tokio::fs::canonicalize(workspace).await?;
    let mut text = Vec::new();
    let mut driver_content = Vec::new();
    let mut image_bytes = 0usize;
    let mut hashes = BTreeMap::new();
    for item in content {
        match item {
            PromptContentRef::Text { text: value } => {
                if !value.trim().is_empty() {
                    text.push(value);
                }
            }
            PromptContentRef::LocalImage { path } => {
                let path = canonical_workspace_file(&workspace, path).await?;
                // An uploaded image is also named by its path, so the agent
                // can work with the file and not only look at it; agy names
                // every image itself.
                if provider != ProviderKind::Antigravity
                    && crate::prompt_uploads::contains(&path).await
                {
                    text.push(format!("Attached image: {}", path.display()));
                }
                let file_bytes =
                    usize::try_from(tokio::fs::metadata(&path).await?.len()).unwrap_or(usize::MAX);
                ensure_image_budget(image_bytes.saturating_add(file_bytes))?;
                let bytes = tokio::fs::read(&path).await?;
                image_bytes = image_bytes.saturating_add(bytes.len());
                hashes.insert(path.clone(), sha256_hex(&bytes));
                driver_content.push(DriverPromptContent::Image {
                    mime_type: image_mime_type(&path)?.to_owned(),
                    data: BASE64_STANDARD.encode(bytes),
                    path: Some(path),
                });
            }
            PromptContentRef::Image { data, mime_type } => {
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
                if profile(provider).file_attachments == FileAttachmentStyle::AtMention {
                    text.push(format!("Attached file: @{}", relative.display()));
                }
                driver_content.push(DriverPromptContent::File { path, name });
            }
        }
    }
    Ok((text.join("\n\n"), driver_content, hashes))
}

/// `path` canonicalized, when it is a regular file inside `workspace` or
/// the prompt upload directory.
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
    // Files uploaded with the prompt live outside the workspace, in the
    // upload directory the drivers let the agents read.
    if !canonical.starts_with(workspace) && !crate::prompt_uploads::contains(&canonical).await {
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

/// Whether a running turn executes `binary`, the CLI of `cli_provider`: its
/// own provider's CLI, or a configured profile launching the same executable.
fn turn_runs_cli(
    agent: &crate::config::AgentConfig,
    turn_provider: ProviderKind,
    turn_profile: Option<&str>,
    cli_provider: ProviderKind,
    binary: &str,
) -> bool {
    if turn_provider == cli_provider {
        return true;
    }
    if !profile(turn_provider).profile_required {
        return false;
    }
    turn_profile
        .and_then(|name| agent.acp_profiles.get(name))
        .is_some_and(|profile| same_executable(&profile.command, binary))
}

/// The prompt text a provider receives: providers that load selected skills
/// natively get the user text alone, the others the skills inlined.
fn provider_prompt_text(
    provider: ProviderKind,
    user_text: &str,
    injected: &[(String, String)],
) -> String {
    match profile(provider).skill_injection {
        SkillInjection::Native => user_text.to_owned(),
        SkillInjection::PromptText => compose_prompt_with_skills(user_text, injected),
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
    if !profile(provider).profile_required {
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
    mod follow_up_queue;
    mod history_cleanup;
    mod profile_golden;

    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;

    use super::*;
    use crate::config::{AcpProfileConfig, AgentConfig, PairingEncryption, SecurityConfig};
    use crate::conversation::{ConversationEvent, ConversationEventHub, ConversationStore};

    async fn control_fixture(
        label: &str,
    ) -> (PathBuf, ConversationStore, ConversationSupervisor, PathBuf) {
        control_fixture_with(label, false, false).await
    }

    /// `fixture_acp_profile` registers a `"fixture"` ACP profile that runs the
    /// shared provider script in `acp` mode; `fixture_devin` points the Devin
    /// CLI at the same script (`devin acp` also reaches the `acp` branch).
    async fn control_fixture_with(
        label: &str,
        fixture_acp_profile: bool,
        fixture_devin: bool,
    ) -> (PathBuf, ConversationStore, ConversationSupervisor, PathBuf) {
        control_fixture_keyed(label, fixture_acp_profile, fixture_devin, None).await
    }

    /// [`control_fixture`] with end-to-end encrypted history for one
    /// recipient (seed 1); also returns the history keys.
    async fn encrypted_control_fixture(
        label: &str,
    ) -> (
        PathBuf,
        ConversationStore,
        ConversationSupervisor,
        PathBuf,
        crate::history_keys::HistoryKeys,
    ) {
        let root = temp_dir(label);
        let data_dir = root.join("data");
        let keys = crate::history_keys::HistoryKeys::load(&data_dir, None).unwrap();
        keys.recipients()
            .register_device("dev_a", &crate::history_keys::test_support::recipient(1))
            .unwrap();
        let (root, store, supervisor, workspace) =
            control_fixture_keyed_at(root, false, false, Some(keys.clone())).await;
        (root, store, supervisor, workspace, keys)
    }

    async fn control_fixture_keyed(
        label: &str,
        fixture_acp_profile: bool,
        fixture_devin: bool,
        keys: Option<crate::history_keys::HistoryKeys>,
    ) -> (PathBuf, ConversationStore, ConversationSupervisor, PathBuf) {
        control_fixture_keyed_at(temp_dir(label), fixture_acp_profile, fixture_devin, keys).await
    }

    async fn control_fixture_keyed_at(
        root: PathBuf,
        fixture_acp_profile: bool,
        fixture_devin: bool,
        keys: Option<crate::history_keys::HistoryKeys>,
    ) -> (PathBuf, ConversationStore, ConversationSupervisor, PathBuf) {
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let workspace = fs::canonicalize(workspace).unwrap();
        let executable = write_provider_fixture(&root).to_string_lossy().to_string();
        let mut acp_profiles = BTreeMap::new();
        if fixture_acp_profile {
            acp_profiles.insert(
                "fixture".to_owned(),
                AcpProfileConfig {
                    command: executable.clone(),
                    args: vec!["acp".to_owned()],
                    env: BTreeMap::new(),
                    auth_method: None,
                    api_key_env: None,
                    resident: true,
                },
            );
        }
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable.clone(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: if fixture_devin {
                    executable.clone()
                } else {
                    "devin".to_owned()
                },
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles,
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
        });
        let mut store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        if let Some(keys) = keys {
            store = store.with_history_keys(keys);
        }
        let supervisor =
            ConversationSupervisor::new(config, store.clone(), ConversationEventHub::default());
        (root, store, supervisor, workspace)
    }

    #[tokio::test]
    async fn legacy_conversations_refuse_every_write_entry_point() {
        let (root, store, supervisor, workspace, _keys) =
            encrypted_control_fixture("todex-legacy-read-only").await;
        // Written in plaintext, as versions before mandatory encryption did.
        let legacy_writer = ConversationStore::new(root.join("data")).await.unwrap();
        let legacy = legacy_writer
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace.clone(),
                Some("old title".to_owned()),
                None,
            ))
            .await
            .unwrap();
        legacy_writer
            .append(
                &legacy.id,
                "message.created",
                json!({"role": "user", "content": "old prompt", "turnId": "old"}),
            )
            .await
            .unwrap();
        drop(legacy_writer);
        let id = legacy.id.clone();
        let prompt = || ConversationPrompt {
            client_request_id: Some("new".to_owned()),
            text: "new prompt".to_owned(),
            model: None,
            reasoning_effort: None,
            skills: Vec::new(),
            content: Vec::new(),
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        let read_only = |label: &str, error: AppError| {
            assert_eq!(error.code(), "HISTORY_READ_ONLY", "{label}: {error}");
        };
        read_only(
            "prompt",
            supervisor
                .prompt_owned("local", &id, prompt())
                .await
                .unwrap_err(),
        );
        read_only(
            "retry",
            supervisor
                .retry_owned("local", &id, None, Some("old prompt".to_owned()), None)
                .await
                .unwrap_err(),
        );
        read_only(
            "queue.add",
            supervisor
                .queue_add_owned("local", &id, "queued-1", prompt(), false, false)
                .await
                .err()
                .unwrap(),
        );
        assert!(!supervisor.has_pending_follow_ups());
        // A prewarm is skipped, not refused.
        assert_eq!(
            supervisor.warm_owned("local", &id).await.unwrap(),
            Some("read_only")
        );
        assert!(!root
            .join("data")
            .join("conversations")
            .join(&id)
            .join("queue.json")
            .exists());
        read_only(
            "queue.resume",
            supervisor
                .queue_resume_owned("local", &id)
                .await
                .unwrap_err(),
        );
        read_only(
            "compact",
            supervisor
                .compact_owned("local", &id, "compact-1")
                .await
                .unwrap_err(),
        );
        read_only(
            "fork",
            supervisor.fork_owned("local", &id, None).await.unwrap_err(),
        );
        read_only(
            "rename",
            supervisor
                .update_metadata_owned("local", &id, Some(Some("new".to_owned())), None)
                .await
                .unwrap_err(),
        );
        read_only(
            "permission",
            supervisor
                .resolve_permission_owned(
                    "local",
                    "local",
                    &id,
                    "perm_1",
                    PermissionDecision {
                        outcome: PermissionOutcome::AllowOnce,
                        option_id: Some("allow_once".to_owned()),
                        data: None,
                    },
                )
                .await
                .unwrap_err(),
        );
        read_only(
            "control",
            supervisor
                .control_owned(
                    "local",
                    &id,
                    "old",
                    "control-1",
                    ProviderControl::Steer {
                        text: "steer".to_owned(),
                    },
                )
                .await
                .unwrap_err(),
        );
        let manifest = store.get(&id).await.unwrap();
        assert!(manifest.legacy_plaintext);
        assert_eq!(manifest.title.as_deref(), Some("old title"));
        // Nothing was written; reading, archiving and deleting stay allowed.
        let history = supervisor.history_for_tests(&id).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].payload["content"], "old prompt");
        let archived = supervisor
            .update_metadata_owned("local", &id, None, Some(true))
            .await
            .unwrap();
        assert!(archived.archived_at.is_some());
        supervisor
            .update_metadata_owned("local", &id, None, Some(false))
            .await
            .unwrap();
        supervisor.delete_owned("local", &id).await.unwrap();
        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_finished_turn_seals_its_journal_once_it_reaches_the_seal_threshold() {
        let (root, store, supervisor, workspace, _keys) =
            encrypted_control_fixture("todex-turn-end-seal").await;
        let manifest = supervisor
            .create_owned("local", ProviderKind::Codex, workspace, None, None)
            .await
            .unwrap();
        let directory = root.join("data/conversations").join(&manifest.id);
        let prompt = |text: &str| ConversationPrompt {
            client_request_id: Some(text.to_owned()),
            text: text.to_owned(),
            model: Some("fixture-model".to_owned()),
            reasoning_effort: None,
            skills: Vec::new(),
            content: Vec::new(),
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        // Below the default threshold a turn leaves the active file alone.
        supervisor
            .prompt_owned("local", &manifest.id, prompt("first"))
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        assert!(!directory.join("events.000001.jsonl").exists());
        let before = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(before.last().unwrap().event_type, "turn.completed");

        // At the threshold the turn's end seals it for conversion.
        let store = store.with_seal_min_bytes(1);
        supervisor
            .prompt_owned("local", &manifest.id, prompt("second"))
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        assert!(directory.join("events.000001.jsonl").exists());
        assert_eq!(
            fs::metadata(directory.join("events.jsonl")).unwrap().len(),
            0
        );
        assert_eq!(store.seal_all_for_tests(&manifest.id).await.unwrap(), 1);
        assert!(directory.join("events.000001.seg").exists());
        let after = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(after.last().unwrap().event_type, "turn.completed");
        assert_eq!(
            after.last().unwrap().sequence,
            store.get(&manifest.id).await.unwrap().last_sequence
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn e2e_requests_titles_retries_and_forks_keep_prompts_encrypted() {
        let (root, store, supervisor, workspace, keys) =
            encrypted_control_fixture("todex-e2e-supervisor").await;
        let manifest = supervisor
            .create_owned(
                "local",
                ProviderKind::Codex,
                workspace.clone(),
                Some("TITLE-SECRET".to_owned()),
                None,
            )
            .await
            .unwrap();
        assert!(manifest.title.is_none() && manifest.title_enc.is_some());
        let renamed = supervisor
            .update_metadata_owned(
                "local",
                &manifest.id,
                Some(Some("RENAMED".to_owned())),
                None,
            )
            .await
            .unwrap();
        assert!(renamed.title.is_none());
        assert_ne!(renamed.title_enc, manifest.title_enc);
        let request = ConversationPrompt {
            client_request_id: Some("submit-1".to_owned()),
            // Prompts run trimmed and `message.created` shows them so: that
            // trimmed text is what a client can send back with a retry.
            text: "  PROMPT-SECRET\n".to_owned(),
            model: Some("fixture-model".to_owned()),
            reasoning_effort: None,
            skills: Vec::new(),
            content: vec![PromptContentRef::Text {
                text: "INLINE-SECRET".to_owned(),
            }],
            permission_mode: None,
            work_mode: None,
            permission_profile: None,
            sandbox_mode: None,
            approval_policy: None,
        };
        let turn = supervisor
            .prompt_owned("local", &manifest.id, request.clone())
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        // The same request again resolves to its turn (by MAC).
        assert_eq!(
            supervisor
                .prompt_owned("local", &manifest.id, request.clone())
                .await
                .unwrap(),
            turn
        );
        let saved = store.last_request(&manifest.id).await.unwrap().unwrap();
        assert_eq!(saved["request"]["text"], "");
        assert_eq!(saved["request"]["content"], json!([]));
        assert!(saved["request"]["textMac"].is_string());
        assert!(saved["request"]["contentMac"].is_string());
        let directory = root.join("data/conversations").join(&manifest.id);
        fn bytes_under(directory: &Path) -> Vec<u8> {
            let mut bytes = Vec::new();
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    bytes.extend(bytes_under(&path));
                } else {
                    bytes.extend(fs::read(&path).unwrap());
                }
            }
            bytes
        }
        let disk = bytes_under(&directory);
        for secret in ["TITLE-SECRET", "RENAMED", "PROMPT-SECRET", "INLINE-SECRET"] {
            assert!(
                !disk
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "{secret} is on disk"
            );
        }
        // Retry needs the decrypted prompt, and only the original one.
        assert!(matches!(
            supervisor
                .retry_owned("local", &manifest.id, None, None, None)
                .await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            supervisor
                .retry_owned("local", &manifest.id, None, Some("other".to_owned()), None)
                .await,
            Err(AppError::Conflict(_))
        ));
        // The request had inline content: the text alone no longer passes,
        // the client returns the `retryRequest` it decrypts from the
        // user message (the summary leaves it out).
        assert!(matches!(
            supervisor
                .retry_owned(
                    "local",
                    &manifest.id,
                    None,
                    Some("PROMPT-SECRET".to_owned()),
                    None
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        let device_keys =
            crate::conversation::e2e_support::client_keys(&keys, &manifest.id, 1).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        let user_message = history
            .iter()
            .find(|event| event.event_type == "message.created")
            .unwrap();
        let view = |summary| {
            crate::conversation::e2e_support::client_view(
                &device_keys,
                std::slice::from_ref(user_message),
                &serde_json::Map::new(),
                summary,
            )
            .remove(0)
            .payload
        };
        assert!(view(true).get("retryRequest").is_none());
        let retry_request = view(false)["retryRequest"].clone();
        assert_eq!(retry_request["text"], "PROMPT-SECRET");
        let original: Vec<PromptContentRef> =
            serde_json::from_value(retry_request["content"].clone()).unwrap();
        assert!(matches!(
            supervisor
                .retry_owned(
                    "local",
                    &manifest.id,
                    None,
                    Some("PROMPT-SECRET".to_owned()),
                    Some(vec![PromptContentRef::Text {
                        text: "OTHER".to_owned()
                    }])
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        let retried = supervisor
            .retry_owned(
                "local",
                &manifest.id,
                Some("retry-2".to_owned()),
                retry_request["text"].as_str().map(str::to_owned),
                Some(original),
            )
            .await
            .unwrap();
        assert_ne!(retried, turn);
        wait_until_idle(&supervisor).await;
        // A fork copies the ciphertext and the keys that read it.
        let fork = supervisor
            .fork_owned("local", &manifest.id, None)
            .await
            .unwrap();
        assert!(fork.title.is_none());
        let source = store.complete_history(&manifest.id).await.unwrap();
        let forked = store.complete_history(&fork.id).await.unwrap();
        assert_eq!(forked.len(), source.len() + 1);
        for (copy, original) in forked.iter().zip(&source) {
            assert_eq!(copy.payload["$enc"], original.payload["$enc"]);
        }
        // Plus the fork's own key, which encrypted its `conversation.forked`.
        assert_eq!(
            keys.keyrings().keys(&fork.id).await.unwrap().len(),
            keys.keyrings().keys(&manifest.id).await.unwrap().len() + 1
        );
        assert!(encrypted_content(&forked.last().unwrap().payload).is_some());
        supervisor.delete_owned("local", &fork.id).await.unwrap();
        fs::remove_dir_all(root).unwrap();
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

    /// Polls the journal until an event matches `predicate`; unlike
    /// `wait_until_idle` this observes progress while a turn is still open.
    async fn wait_for_journal_event(
        store: &ConversationStore,
        conversation_id: &str,
        predicate: impl Fn(&ConversationEvent) -> bool,
    ) -> ConversationEvent {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(event) = store
                    .complete_history(conversation_id)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|event| predicate(event))
                {
                    break event;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected journal event within the timeout")
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
            supervisor
                .retry_owned("local", &manifest.id, None, None, None)
                .await,
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
            .retry_owned(
                "local",
                &manifest.id,
                Some("retry-2".to_owned()),
                None,
                None,
            )
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
            supervisor
                .retry_owned("local", &manifest.id, None, None, None)
                .await,
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
    async fn native_fork_acp_profile_uses_declared_session_capability() {
        let (root, store, supervisor, workspace) =
            control_fixture_with("todex-native-fork-acp", true, false).await;
        let manifest = supervisor
            .create(
                ProviderKind::Acp,
                workspace,
                None,
                Some("fixture".to_owned()),
            )
            .await
            .unwrap();
        let mut native = crate::conversation::ProviderState::new(ProviderKind::Acp);
        native.native_session_id = Some("acp-native".to_owned());
        store
            .save_provider_state(&manifest.id, native)
            .await
            .unwrap();

        let fork = supervisor
            .fork_owned("local", &manifest.id, None)
            .await
            .unwrap();
        assert_eq!(fork.provider_profile.as_deref(), Some("fixture"));
        assert_eq!(
            store
                .provider_state(&fork.id)
                .await
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("acp-fork-native")
        );
        let history = store.complete_history(&fork.id).await.unwrap();
        assert_eq!(history.last().unwrap().event_type, "conversation.forked");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn native_fork_devin_follows_declared_acp_capability() {
        let (root, store, supervisor, workspace) =
            control_fixture_with("todex-native-fork-devin", false, true).await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Devin,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        let mut native = crate::conversation::ProviderState::new(ProviderKind::Devin);
        native.native_session_id = Some("devin-native".to_owned());
        store
            .save_provider_state(&manifest.id, native)
            .await
            .unwrap();

        let fork = supervisor
            .fork_owned("local", &manifest.id, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .provider_state(&fork.id)
                .await
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("acp-fork-native")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn native_fork_devin_stays_unsupported_without_declared_capability() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-native-fork-devin-off").await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Devin,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        assert!(matches!(
            supervisor.fork_owned("local", &manifest.id, None).await,
            Err(AppError::Unsupported(_))
        ));
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
                provider: ProviderKind::Codex,
                provider_profile: None,
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
    async fn retention_only_waits_for_starting_turns_when_it_removes_something() {
        let (root, store, supervisor, workspace) = control_fixture("todex-retention-gate").await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        // A prompt that is starting holds the gate for reading.
        let starting = supervisor.cli_execution_gate.clone().read_owned().await;
        let past = Utc::now() - chrono::Duration::hours(1);
        let nothing =
            tokio::time::timeout(Duration::from_secs(5), supervisor.cleanup_expired(past))
                .await
                .expect("a pass that removes nothing does not wait for the gate");
        assert!(nothing.unwrap().is_empty());

        let future = Utc::now() + chrono::Duration::hours(1);
        let cleanup = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.cleanup_expired(future).await }
        });
        sleep(Duration::from_millis(100)).await;
        assert!(
            !cleanup.is_finished(),
            "removal waits for the starting turn"
        );
        assert!(store.get(&manifest.id).await.is_ok());
        drop(starting);
        assert_eq!(cleanup.await.unwrap().unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancel_naming_a_turn_only_stops_that_turn() {
        let (root, store, supervisor, workspace) = control_fixture("todex-cancel-turn-id").await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        // Idle: a named turn is reported as not active, an unnamed cancel is
        // the old no-op.
        assert_eq!(
            supervisor
                .cancel_owned("local", &manifest.id, Some("turn_old"))
                .await
                .unwrap(),
            CancelOutcome::NotActive {
                active_turn_id: None
            }
        );
        assert_eq!(
            supervisor
                .cancel_owned("local", &manifest.id, None)
                .await
                .unwrap(),
            CancelOutcome::Signalled
        );

        let (cancel, cancelled) = watch::channel(false);
        supervisor.active.insert(
            manifest.id.clone(),
            ActiveTurn {
                turn_id: "turn_new".to_owned(),
                cancel,
                provider: ProviderKind::Codex,
                provider_profile: None,
            },
        );
        // A late cancel for the previous turn leaves the new one running.
        assert_eq!(
            supervisor
                .cancel_owned("local", &manifest.id, Some("turn_old"))
                .await
                .unwrap(),
            CancelOutcome::NotActive {
                active_turn_id: Some("turn_new".to_owned())
            }
        );
        assert!(!*cancelled.borrow());
        assert!(matches!(
            supervisor
                .cancel_owned("other", &manifest.id, Some("turn_new"))
                .await,
            Err(AppError::NotFound(_))
        ));
        assert!(!*cancelled.borrow());
        assert_eq!(
            supervisor
                .cancel_owned("local", &manifest.id, Some("turn_new"))
                .await
                .unwrap(),
            CancelOutcome::Signalled
        );
        assert!(*cancelled.borrow());
        supervisor.active.remove(&manifest.id);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cli_upgrade_blocking_scopes_to_the_running_agent() {
        let root = temp_dir("todex-cli-upgrade-scope");
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        fs::create_dir_all(&workspace).unwrap();
        let workspace_root = fs::canonicalize(workspace_root).unwrap();
        let managed_binary = write_provider_fixture(&root).to_string_lossy().to_string();
        let mut profiles = BTreeMap::new();
        for (name, command) in [
            ("managed", managed_binary.clone()),
            ("external", "/bin/cat".to_owned()),
        ] {
            profiles.insert(
                name.to_owned(),
                AcpProfileConfig {
                    command,
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    auth_method: None,
                    api_key_env: None,
                    resident: true,
                },
            );
        }
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: managed_binary,
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: profiles,
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let locks = CliLocks::default();
        let supervisor =
            ConversationSupervisor::new(config, store, ConversationEventHub::default())
                .with_cli_locks(locks.clone());
        assert_eq!(
            supervisor.clis_for(ProviderKind::Codex, None),
            [ManagedCli::Codex]
        );
        assert_eq!(
            supervisor.clis_for(ProviderKind::Acp, Some("managed")),
            [ManagedCli::Codex]
        );
        assert!(supervisor
            .clis_for(ProviderKind::Acp, Some("external"))
            .is_empty());
        // While Codex upgrades, other Agents start; Codex and the ACP profile
        // running its binary wait.
        let upgrade = locks.try_write(ManagedCli::Codex).unwrap();
        drop(
            supervisor
                .start_permits(ProviderKind::ClaudeCode, None)
                .await,
        );
        drop(
            supervisor
                .start_permits(ProviderKind::Acp, Some("external"))
                .await,
        );
        for (provider, profile) in [
            (ProviderKind::Codex, None),
            (ProviderKind::Acp, Some("managed")),
        ] {
            assert!(tokio::time::timeout(
                Duration::from_millis(50),
                supervisor.start_permits(provider, profile)
            )
            .await
            .is_err());
        }
        assert!(supervisor
            .try_discovery_permits(ProviderKind::Codex, None)
            .is_err());
        assert!(supervisor
            .try_discovery_permits(ProviderKind::Pi, None)
            .is_ok());
        drop(upgrade);
        drop(supervisor.start_permits(ProviderKind::Codex, None).await);
        for (id, provider, profile) in [
            ("conv-codex", ProviderKind::Codex, None),
            (
                "conv-acp-managed",
                ProviderKind::Acp,
                Some("managed".to_owned()),
            ),
            (
                "conv-acp-external",
                ProviderKind::Acp,
                Some("external".to_owned()),
            ),
        ] {
            let (cancel, _) = watch::channel(false);
            supervisor.active.insert(
                id.to_owned(),
                ActiveTurn {
                    turn_id: format!("turn-{id}"),
                    cancel,
                    provider,
                    provider_profile: profile,
                },
            );
        }
        assert!(supervisor.has_active_turns_for_cli(ManagedCli::Codex));
        for cli in [
            ManagedCli::Pi,
            ManagedCli::ClaudeCode,
            ManagedCli::GrokBuild,
            ManagedCli::Devin,
            ManagedCli::Opencode,
            ManagedCli::Antigravity,
            ManagedCli::DeepseekHarness,
        ] {
            assert!(!supervisor.has_active_turns_for_cli(cli));
        }
        // The "managed" ACP profile still runs the Codex binary.
        supervisor.active.remove("conv-codex");
        assert!(supervisor.has_active_turns_for_cli(ManagedCli::Codex));
        // The remaining ACP turn runs an unmanaged executable.
        supervisor.active.remove("conv-acp-managed");
        assert!(!supervisor.has_active_turns_for_cli(ManagedCli::Codex));
        supervisor.active.remove("conv-acp-external");
        assert!(!supervisor.has_active_turns());
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
        let idle = store
            .create(ConversationManifest::new(
                ProviderKind::Pi,
                root.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store.append(&idle.id, "provider.runtime", json!({"provider":"pi","scope":"session","runtimeId":"runtime-idle","status":"ready"})).await.unwrap();
        store
            .append(
                &idle.id,
                "permission.requested",
                json!({"permissionId":"idle-dialog","scope":"session","runtimeId":"runtime-idle"}),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get(&idle.id).await.unwrap().status,
            ConversationStatus::Idle
        );
        supervisor.recover_all().await.unwrap();
        assert_eq!(
            store.get(&idle.id).await.unwrap().status,
            ConversationStatus::Idle
        );
        let idle_history = store.complete_history(&idle.id).await.unwrap();
        assert!(idle_history
            .iter()
            .any(|event| event.event_type == "permission.resolved"
                && event.payload["scope"] == "session"
                && event.payload["runtimeId"] == "runtime-idle"));
        assert!(idle_history
            .iter()
            .any(|event| event.event_type == "provider.runtime"
                && event.payload["status"] == "stopped"
                && event.payload["reason"] == "daemon_restarted"));
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
            store.complete_history(&idle.id).await.unwrap().len(),
            idle_history.len()
        );
        assert_eq!(
            store.complete_history(&manifest.id).await.unwrap().len(),
            count
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn restart_reports_background_work_a_finished_turn_left_running() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-recover-background").await;
        let mut waiting = Vec::new();
        for (provider, left_waiting) in [
            (ProviderKind::ClaudeCode, true),
            (ProviderKind::ClaudeCode, false),
            // Pi holds background subagents the same way.
            (ProviderKind::Pi, true),
        ] {
            let manifest = store
                .create(ConversationManifest::new(
                    provider,
                    workspace.clone(),
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
                    "provider.event",
                    json!({ "provider": provider,
                "providerMethod": "background_tasks_pending", "turnId": "t",
                "metadata": { "taskIds": ["b1"] } }),
                )
                .await
                .unwrap();
            store
                .append(&manifest.id, "turn.completed", json!({ "turnId": "t" }))
                .await
                .unwrap();
            if !left_waiting {
                // Claude took the finished task up in a turn of its own.
                store
                    .append(
                        &manifest.id,
                        "turn.started",
                        json!({ "turnId": "u", "trigger": "provider" }),
                    )
                    .await
                    .unwrap();
                store
                    .append(&manifest.id, "turn.completed", json!({ "turnId": "u" }))
                    .await
                    .unwrap();
            }
            waiting.push((manifest.id, left_waiting));
        }
        let ended = |history: &[ConversationEvent]| {
            history
                .iter()
                .filter(|event| event.payload["providerMethod"] == "background_tasks_ended")
                .map(|event| event.payload["metadata"]["reason"].clone())
                .collect::<Vec<_>>()
        };
        supervisor.recover_all().await.unwrap();
        for (id, left_waiting) in &waiting {
            let history = store.complete_history(id).await.unwrap();
            let expected = if *left_waiting {
                vec![json!("daemon_restarted")]
            } else {
                vec![]
            };
            assert_eq!(ended(&history), expected);
            assert_eq!(
                store.get(id).await.unwrap().status,
                ConversationStatus::Idle
            );
        }
        // Reported once.
        supervisor.recover_all().await.unwrap();
        let history = store.complete_history(&waiting[0].0).await.unwrap();
        assert_eq!(ended(&history).len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    /// Replaces the driver for `provider` so a test can script its behavior
    /// while prompt validation still sees the real descriptor.
    fn replace_driver(
        supervisor: &mut ConversationSupervisor,
        provider: ProviderKind,
        driver: impl FnOnce(Arc<dyn ProviderDriver>) -> Arc<dyn ProviderDriver>,
    ) {
        let mut drivers = (*supervisor.registry.drivers).clone();
        let real = drivers.remove(&provider).unwrap();
        drivers.insert(provider, driver(real));
        supervisor.registry = DriverRegistry {
            drivers: Arc::new(drivers),
        };
    }

    /// A driver reported available, so a prewarm reaches it.
    struct AvailableDriver(Arc<dyn ProviderDriver>);

    #[async_trait::async_trait]
    impl ProviderDriver for AvailableDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            let mut descriptor = self.0.descriptor();
            descriptor.available = true;
            descriptor.unavailable_reason = None;
            descriptor
        }

        async fn prewarm(
            &self,
            context: DriverContext,
            settings: DriverPrompt,
            sink: DriverEventSink,
        ) -> Result<super::super::types::ProviderPrewarm, AppError> {
            self.0.prewarm(context, settings, sink).await
        }

        async fn run_turn(
            &self,
            context: DriverContext,
            prompt: DriverPrompt,
            sink: DriverEventSink,
            cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            self.0.run_turn(context, prompt, sink, cancel).await
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn warm_starts_a_resident_agent_without_recording_anything() {
        let (root, store, mut supervisor, workspace) =
            control_fixture_with("todex-warm", true, false).await;
        let manifest = supervisor
            .create_owned(
                "local",
                ProviderKind::Acp,
                workspace.clone(),
                None,
                Some("fixture".to_owned()),
            )
            .await
            .unwrap();
        let before = store.complete_history(&manifest.id).await.unwrap().len();
        assert_eq!(
            supervisor.warm_owned("local", &manifest.id).await.unwrap(),
            None
        );
        assert_eq!(
            supervisor.warm_owned("local", &manifest.id).await.unwrap(),
            Some("already_warm")
        );
        assert_eq!(
            store.complete_history(&manifest.id).await.unwrap().len(),
            before
        );
        let state = store.provider_state(&manifest.id).await.unwrap();
        assert!(state.native_session_id.is_none());
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            manifest.status
        );

        // Another owner's conversation is not found, as for any other command.
        assert!(matches!(
            supervisor.warm_owned("other", &manifest.id).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            supervisor.warm_owned("local", "missing").await,
            Err(AppError::NotFound(_) | AppError::InvalidRequest(_))
        ));

        // The prepared session serves the first turn.
        supervisor
            .prompt_owned(
                "local",
                &manifest.id,
                ConversationPrompt {
                    client_request_id: None,
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
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history
            .iter()
            .any(|event| event.event_type == "turn.completed"));
        assert!(store
            .provider_state(&manifest.id)
            .await
            .unwrap()
            .native_session_id
            .is_some());

        // A running conversation is left alone.
        let (cancel, _) = watch::channel(false);
        supervisor.active.insert(
            manifest.id.clone(),
            ActiveTurn {
                turn_id: "turn-running".to_owned(),
                cancel,
                provider: ProviderKind::Acp,
                provider_profile: Some("fixture".to_owned()),
            },
        );
        assert_eq!(
            supervisor.warm_owned("local", &manifest.id).await.unwrap(),
            Some("running")
        );
        supervisor.active.remove(&manifest.id);

        // A provider that starts a process per turn has nothing to warm.
        replace_driver(&mut supervisor, ProviderKind::Antigravity, |real| {
            Arc::new(AvailableDriver(real))
        });
        let antigravity = supervisor
            .create_owned(
                "local",
                ProviderKind::Antigravity,
                workspace.clone(),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            supervisor
                .warm_owned("local", &antigravity.id)
                .await
                .unwrap(),
            Some("unsupported_provider")
        );

        // A workspace that no longer exists never starts a process.
        let gone = workspace.join("gone");
        fs::create_dir_all(&gone).unwrap();
        let other = supervisor
            .create_owned(
                "local",
                ProviderKind::Acp,
                gone.clone(),
                None,
                Some("fixture".to_owned()),
            )
            .await
            .unwrap();
        fs::remove_dir(&gone).unwrap();
        assert_eq!(
            supervisor.warm_owned("local", &other.id).await.unwrap(),
            Some("workspace_unavailable")
        );

        // Disabled by configuration.
        let mut config = (*supervisor.config).clone();
        config.agent.provider_prewarm = false;
        let disabled = ConversationSupervisor::new(
            Arc::new(config),
            store.clone(),
            ConversationEventHub::default(),
        );
        assert_eq!(
            disabled.warm_owned("local", &other.id).await.unwrap(),
            Some("disabled")
        );
        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    /// Announces its command catalog while warming, as an ACP agent does
    /// once after `session/new`.
    struct AnnouncingDriver(Arc<dyn ProviderDriver>);

    #[async_trait::async_trait]
    impl ProviderDriver for AnnouncingDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            self.0.descriptor()
        }

        async fn prewarm(
            &self,
            context: DriverContext,
            settings: DriverPrompt,
            sink: DriverEventSink,
        ) -> Result<super::super::types::ProviderPrewarm, AppError> {
            sink.emit(
                "provider.commands.updated",
                json!({ "provider": "acp", "commands": [{ "name": "review" }] }),
            )
            .await?;
            self.0.prewarm(context, settings, sink).await
        }

        async fn run_turn(
            &self,
            context: DriverContext,
            prompt: DriverPrompt,
            sink: DriverEventSink,
            cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            self.0.run_turn(context, prompt, sink, cancel).await
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn events_a_prewarm_held_back_are_journalled_by_the_next_turn() {
        let (root, store, mut supervisor, workspace) =
            control_fixture_with("todex-warm-deferred", true, false).await;
        replace_driver(&mut supervisor, ProviderKind::Acp, |real| {
            Arc::new(AnnouncingDriver(real))
        });
        let manifest = supervisor
            .create_owned(
                "local",
                ProviderKind::Acp,
                workspace,
                None,
                Some("fixture".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(
            supervisor.warm_owned("local", &manifest.id).await.unwrap(),
            None
        );
        let commands = |history: &[ConversationEvent]| {
            history
                .iter()
                .filter(|event| event.event_type == "provider.commands.updated")
                .count()
        };
        assert_eq!(
            commands(&store.complete_history(&manifest.id).await.unwrap()),
            0
        );

        supervisor
            .prompt_owned(
                "local",
                &manifest.id,
                ConversationPrompt {
                    client_request_id: None,
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
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(commands(&history), 1);
        let position = |event_type: &str| {
            history
                .iter()
                .position(|event| event.event_type == event_type)
                .unwrap()
        };
        assert!(position("turn.started") < position("provider.commands.updated"));
        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    /// Announces its command catalog while warming, then holds the warm
    /// until `release`, and reports something more once released.
    struct SlowWarmDriver {
        real: Arc<dyn ProviderDriver>,
        announced: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl ProviderDriver for SlowWarmDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            self.real.descriptor()
        }

        async fn prewarm(
            &self,
            _context: DriverContext,
            _settings: DriverPrompt,
            sink: DriverEventSink,
        ) -> Result<super::super::types::ProviderPrewarm, AppError> {
            sink.emit(
                "provider.commands.updated",
                json!({ "provider": "acp", "commands": [{ "name": "review" }] }),
            )
            .await?;
            self.announced.notify_one();
            self.release.notified().await;
            // As a resident actor reports between turns through its last sink.
            sink.emit("provider.event", json!({ "kind": "late" }))
                .await?;
            Ok(super::super::types::ProviderPrewarm::Warmed)
        }

        async fn run_turn(
            &self,
            context: DriverContext,
            prompt: DriverPrompt,
            sink: DriverEventSink,
            cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            self.real.run_turn(context, prompt, sink, cancel).await
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_during_a_warm_takes_its_events_over_and_later_ones_follow_it() {
        let (root, store, mut supervisor, workspace) =
            control_fixture_with("todex-warm-handover", true, false).await;
        let announced = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        replace_driver(&mut supervisor, ProviderKind::Acp, |real| {
            Arc::new(SlowWarmDriver {
                real,
                announced: announced.clone(),
                release: release.clone(),
            })
        });
        let manifest = supervisor
            .create_owned(
                "local",
                ProviderKind::Acp,
                workspace,
                None,
                Some("fixture".to_owned()),
            )
            .await
            .unwrap();
        let warm = tokio::spawn({
            let supervisor = supervisor.clone();
            let id = manifest.id.clone();
            async move { supervisor.warm_owned("local", &id).await }
        });
        announced.notified().await;
        // The warm is still running when the turn starts.
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let count = |history: &[ConversationEvent], event_type: &str| {
            history
                .iter()
                .filter(|event| event.event_type == event_type)
                .count()
        };
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(count(&history, "provider.commands.updated"), 1);
        let position = |event_type: &str| {
            history
                .iter()
                .position(|event| event.event_type == event_type)
                .unwrap()
        };
        assert!(position("turn.started") < position("provider.commands.updated"));

        // What the warm reports after the turn took it over is journalled
        // right away, with that turn, not held for a later one.
        release.notify_one();
        assert_eq!(warm.await.unwrap().unwrap(), None);
        let late = |history: &[ConversationEvent]| {
            history
                .iter()
                .filter(|event| event.payload["kind"] == "late")
                .cloned()
                .collect::<Vec<_>>()
        };
        let history = store.complete_history(&manifest.id).await.unwrap();
        let reported = late(&history);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].payload["turnId"], json!(turn_id));
        assert!(!supervisor.prewarm_sinks.contains_key(&manifest.id));

        supervisor
            .prompt(&manifest.id, "again".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert_eq!(late(&history).len(), 1);
        assert_eq!(count(&history, "provider.commands.updated"), 1);
        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    /// Holds the warm until `release`, then prepares the real session;
    /// records the warm and every session shutdown in order.
    struct GatedWarmDriver {
        real: Arc<dyn ProviderDriver>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ProviderDriver for GatedWarmDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            self.real.descriptor()
        }

        async fn prewarm(
            &self,
            context: DriverContext,
            settings: DriverPrompt,
            sink: DriverEventSink,
        ) -> Result<super::super::types::ProviderPrewarm, AppError> {
            self.started.notify_one();
            self.release.notified().await;
            let outcome = self.real.prewarm(context, settings, sink).await;
            self.log.lock().unwrap().push("prewarmed".to_owned());
            outcome
        }

        async fn shutdown_session_with_reason(&self, conversation_id: &str, reason: &str) {
            self.log.lock().unwrap().push(format!("shutdown:{reason}"));
            self.real
                .shutdown_session_with_reason(conversation_id, reason)
                .await;
        }

        async fn run_turn(
            &self,
            context: DriverContext,
            prompt: DriverPrompt,
            sink: DriverEventSink,
            cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            self.real.run_turn(context, prompt, sink, cancel).await
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_warm_never_revives_a_conversation_deleted_meanwhile() {
        let (root, _store, mut supervisor, workspace) =
            control_fixture_with("todex-warm-deleted", true, false).await;
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        replace_driver(&mut supervisor, ProviderKind::Acp, |real| {
            Arc::new(GatedWarmDriver {
                real,
                started: started.clone(),
                release: release.clone(),
                log: log.clone(),
            })
        });
        let manifest = supervisor
            .create_owned(
                "local",
                ProviderKind::Acp,
                workspace,
                None,
                Some("fixture".to_owned()),
            )
            .await
            .unwrap();
        let warm = tokio::spawn({
            let supervisor = supervisor.clone();
            let id = manifest.id.clone();
            async move { supervisor.warm_owned("local", &id).await }
        });
        started.notified().await;
        supervisor
            .delete_owned("local", &manifest.id)
            .await
            .unwrap();
        release.notify_one();
        assert!(matches!(warm.await.unwrap(), Err(AppError::NotFound(_))));
        // The session the warm started after the deletion is stopped again.
        assert_eq!(
            *log.lock().unwrap(),
            [
                "shutdown:conversation_deleted",
                "prewarmed",
                "shutdown:conversation_deleted"
            ]
        );
        assert!(!supervisor.prewarm_sinks.contains_key(&manifest.id));
        supervisor.shutdown_all().await;
        let _ = fs::remove_dir_all(root);
    }

    struct PanickingDriver(Arc<dyn ProviderDriver>);

    #[async_trait::async_trait]
    impl ProviderDriver for PanickingDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            self.0.descriptor()
        }

        async fn run_turn(
            &self,
            _context: DriverContext,
            _prompt: DriverPrompt,
            _sink: DriverEventSink,
            _cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            panic!("fixture driver panic")
        }
    }

    #[tokio::test]
    async fn panicking_driver_still_fails_the_turn_and_frees_the_slot() {
        let (root, store, mut supervisor, workspace) = control_fixture("todex-driver-panic").await;
        replace_driver(&mut supervisor, ProviderKind::ClaudeCode, |real| {
            Arc::new(PanickingDriver(real))
        });
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        let failed = history
            .iter()
            .filter(|event| event.event_type == "turn.failed")
            .collect::<Vec<_>>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].payload["turnId"], turn_id);
        assert_eq!(failed[0].payload["code"], "PROVIDER_PANIC");
        assert!(failed[0].payload["message"]
            .as_str()
            .unwrap()
            .contains("fixture driver panic"));
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Failed
        );
        assert!(store
            .provider_state(&manifest.id)
            .await
            .unwrap()
            .last_error
            .unwrap()
            .contains("fixture driver panic"));
        fs::remove_dir_all(root).unwrap();
    }

    fn turn_events(history: &[ConversationEvent], turn_id: &str) -> Vec<(String, Value)> {
        history
            .iter()
            .filter(|event| {
                event.event_type.starts_with("turn.") && event.payload["turnId"] == turn_id
            })
            .map(|event| (event.event_type.clone(), event.payload.clone()))
            .collect()
    }

    #[tokio::test]
    async fn silent_provider_is_stopped_by_the_idle_watchdog() {
        let (root, store, mut supervisor, workspace) = control_fixture("todex-idle-watchdog").await;
        supervisor.provider_idle_timeout = Some(Duration::from_millis(300));
        fs::write(root.join("silent-turn"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let events = turn_events(
            &store.complete_history(&manifest.id).await.unwrap(),
            &turn_id,
        );
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[1].0, "turn.failed");
        assert_eq!(events[1].1["code"], "PROVIDER_IDLE_TIMEOUT");
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Failed
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Emits a provider event every 50 ms for half a second, then completes.
    struct ChattyDriver(Arc<dyn ProviderDriver>);

    #[async_trait::async_trait]
    impl ProviderDriver for ChattyDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            self.0.descriptor()
        }

        async fn run_turn(
            &self,
            _context: DriverContext,
            _prompt: DriverPrompt,
            sink: DriverEventSink,
            _cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            for index in 0..10 {
                sleep(Duration::from_millis(50)).await;
                sink.emit("provider.event", json!({ "index": index }))
                    .await?;
            }
            Ok(DriverTurnResult {
                native_session_id: None,
                stop_reason: "end_turn".to_owned(),
                cancelled: false,
            })
        }
    }

    /// Waits on a permission dialog until the turn is cancelled.
    struct PermissionWaitingDriver(Arc<dyn ProviderDriver>);

    #[async_trait::async_trait]
    impl ProviderDriver for PermissionWaitingDriver {
        fn descriptor(&self) -> ProviderDescriptor {
            self.0.descriptor()
        }

        async fn run_turn(
            &self,
            _context: DriverContext,
            _prompt: DriverPrompt,
            sink: DriverEventSink,
            mut cancel: watch::Receiver<bool>,
        ) -> Result<DriverTurnResult, AppError> {
            sink.request_permission(
                "fixture-request".to_owned(),
                "tool",
                "Run fixture tool",
                json!({}),
                Value::Null,
                &mut cancel,
            )
            .await?;
            Err(AppError::Conflict(
                "permission should not resolve".to_owned(),
            ))
        }
    }

    #[tokio::test]
    async fn idle_watchdog_spares_active_providers_and_pending_permissions() {
        let (root, store, mut supervisor, workspace) =
            control_fixture("todex-idle-watchdog-spared").await;
        supervisor.provider_idle_timeout = Some(Duration::from_millis(400));
        replace_driver(&mut supervisor, ProviderKind::ClaudeCode, |real| {
            Arc::new(ChattyDriver(real))
        });
        replace_driver(&mut supervisor, ProviderKind::Pi, |real| {
            Arc::new(PermissionWaitingDriver(real))
        });
        let chatty = supervisor
            .create(ProviderKind::ClaudeCode, workspace.clone(), None, None)
            .await
            .unwrap();
        let waiting = supervisor
            .create(ProviderKind::Pi, workspace, None, None)
            .await
            .unwrap();
        let chatty_turn = supervisor
            .prompt(&chatty.id, "hello".to_owned(), None)
            .await
            .unwrap();
        let waiting_turn = supervisor
            .prompt(&waiting.id, "hello".to_owned(), None)
            .await
            .unwrap();

        let started = Instant::now();
        let events = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let events = turn_events(
                    &store.complete_history(&chatty.id).await.unwrap(),
                    &chatty_turn,
                );
                if events.len() > 1 {
                    return events;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(events[1].0, "turn.completed", "{events:?}");
        // The dialog has now been open for several idle windows.
        sleep(Duration::from_millis(1200).saturating_sub(started.elapsed())).await;
        let events = turn_events(
            &store.complete_history(&waiting.id).await.unwrap(),
            &waiting_turn,
        );
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            store.get(&waiting.id).await.unwrap().status,
            ConversationStatus::WaitingPermission
        );

        supervisor.cancel(&waiting.id).await.unwrap();
        wait_until_idle(&supervisor).await;
        let events = turn_events(
            &store.complete_history(&waiting.id).await.unwrap(),
            &waiting_turn,
        );
        assert_eq!(events.last().unwrap().0, "turn.cancelled", "{events:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn terminal_event_retries_follow_the_backoff_and_report_the_last_error() {
        let delays = [Duration::from_millis(1), Duration::from_millis(2)];
        let mut attempts = Vec::new();
        let recovered = retry_with_backoff(&delays, |attempt| {
            attempts.push(attempt);
            async move {
                if attempt < 2 {
                    Err(AppError::Conflict(format!("attempt {attempt}")))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(recovered.is_ok());
        assert_eq!(attempts, [0, 1, 2]);

        let mut calls = 0;
        let exhausted = retry_with_backoff(&delays, |attempt| {
            calls += 1;
            async move { Err(AppError::Conflict(format!("attempt {attempt}"))) }
        })
        .await;
        assert_eq!(calls, delays.len() + 1);
        assert!(matches!(exhausted, Err(AppError::Conflict(message)) if message == "attempt 2"));
        assert_eq!(TERMINAL_EVENT_RETRY_DELAYS.len(), 3);
    }

    #[tokio::test]
    async fn unparseable_provider_lines_are_reported_without_failing_the_turn() {
        let (root, store, supervisor, workspace) = control_fixture("todex-noisy-stdout").await;
        fs::write(root.join("noisy-stdout"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history.iter().any(
            |event| event.event_type == "turn.completed" && event.payload["turnId"] == turn_id
        ));
        let reports = history
            .iter()
            .filter(|event| {
                event.event_type == "provider.event" && event.payload.get("kind").is_some()
            })
            .map(|event| &event.payload)
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert_eq!(reports[0]["kind"], "invalid_line");
        assert_eq!(reports[0]["preview"], "provider banner: not json");
        assert_eq!(reports[0]["turnId"], turn_id);
        assert_eq!(reports[1]["kind"], "oversized_line");
        assert_eq!(reports[1]["bytes"], 4_194_400);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn an_oversized_tool_result_is_truncated_and_the_turn_completes() {
        let (root, store, supervisor, workspace) = control_fixture("todex-big-tool-result").await;
        fs::write(root.join("big-tool-result"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history.iter().any(
            |event| event.event_type == "turn.completed" && event.payload["turnId"] == turn_id
        ));
        let tool = history
            .iter()
            .find(|event| event.event_type == "tool.completed")
            .expect("the tool result is stored");
        let stored = serde_json::to_vec(&tool.payload).unwrap().len();
        assert!(
            stored <= crate::conversation::MAX_EVENT_PAYLOAD_BYTES,
            "{stored}"
        );
        // The output keeps its head and tail (docs/API.md, `tool.completed`).
        let original = tool.payload["truncated"]["originalBytes"]
            .as_u64()
            .expect("the original output size is recorded");
        assert!(original > 256 * 1024, "{original}");
        assert!(stored < 300 * 1024, "{stored}");
        assert!(serde_json::to_string(&tool.payload)
            .unwrap()
            .contains("bytes of tool output truncated]"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_pending_permission_keeps_the_claude_stream_draining() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-permission-drain").await;
        fs::write(root.join("permission-drain"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();

        // The fixture writes the marker frame after requesting the tool
        // permission and only reads the control_response afterwards, so the
        // marker reaching the journal while the prompt is still open proves
        // the read loop kept draining stdout instead of blocking on it.
        let request = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.requested"
        })
        .await;
        wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "provider.event"
                && event.payload["providerMethod"] == "fixture_marker"
        })
        .await;
        assert!(supervisor.permissions.has_pending(&manifest.id));

        supervisor
            .resolve_permission(
                &manifest.id,
                request.payload["permissionId"].as_str().unwrap(),
                PermissionDecision {
                    outcome: PermissionOutcome::AllowOnce,
                    option_id: Some("allow_once".to_owned()),
                    data: None,
                },
            )
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;

        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history.iter().any(
            |event| event.event_type == "turn.completed" && event.payload["turnId"] == turn_id
        ));
        let position = |predicate: &dyn Fn(&ConversationEvent) -> bool| {
            history.iter().position(predicate).unwrap()
        };
        let drained = position(&|event| {
            event.event_type == "provider.event"
                && event.payload["providerMethod"] == "fixture_marker"
        });
        let resolved = position(&|event| event.event_type == "permission.resolved");
        // The prompt task may emit `permission.requested` after the marker —
        // what matters is the marker was drained before the decision.
        assert!(drained < resolved, "{history:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn claude_without_auto_mode_continues_as_ask_and_reports_it() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-auto-unavailable").await;
        fs::write(root.join("auto-unavailable"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        // Claude's default permission mode is `auto`.
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        let request = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.requested"
        })
        .await;
        supervisor
            .resolve_permission(
                &manifest.id,
                request.payload["permissionId"].as_str().unwrap(),
                PermissionDecision {
                    outcome: PermissionOutcome::AllowOnce,
                    option_id: Some("allow_once".to_owned()),
                    data: None,
                },
            )
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;

        let history = store.complete_history(&manifest.id).await.unwrap();
        let event = history
            .iter()
            .find(|event| {
                event.event_type == "provider.event"
                    && event.payload["providerMethod"] == "auto_mode_unavailable"
            })
            .unwrap_or_else(|| panic!("{history:?}"));
        assert_eq!(
            event.payload,
            json!({
                "provider": "claude-code",
                "providerMethod": "auto_mode_unavailable",
                "metadata": {
                    "requestedMode": "auto",
                    "activeMode": "ask",
                    "model": "claude-haiku-fixture",
                    "reason": "Claude Code did not enable auto mode for this model or account",
                    "cause": "unavailable",
                    "settingsSource": null,
                },
                "turnId": turn_id,
            })
        );
        assert!(
            history
                .iter()
                .any(|event| event.event_type == "turn.completed"
                    && event.payload["turnId"] == turn_id),
            "{history:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn claude_reporting_another_permission_mode_fails_the_turn() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-mode-mismatch").await;
        fs::write(root.join("mode-mismatch"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(
            history.iter().any(|event| event.event_type == "turn.failed"
                && event
                    .payload
                    .to_string()
                    .contains("did not activate requested permission mode")),
            "{history:?}"
        );
        assert!(!history
            .iter()
            .any(|event| event.payload["providerMethod"] == "auto_mode_unavailable"));
        fs::remove_dir_all(root).unwrap();
    }

    /// Runs one Claude turn in `mode` against the `mode-settings` fixture
    /// (see `write_provider_fixture`) and returns the history and stdin.
    async fn claude_mode_settings_turn(
        label: &str,
        marker: &str,
        mode: &str,
    ) -> (Vec<ConversationEvent>, String) {
        let (root, store, supervisor, workspace) = control_fixture(label).await;
        fs::write(root.join("mode-settings"), marker).unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        supervisor
            .prompt_owned(
                "local",
                &manifest.id,
                ConversationPrompt {
                    client_request_id: None,
                    text: "hello".to_owned(),
                    model: None,
                    reasoning_effort: None,
                    skills: Vec::new(),
                    content: Vec::new(),
                    permission_mode: Some(mode.to_owned()),
                    work_mode: None,
                    permission_profile: None,
                    sandbox_mode: None,
                    approval_policy: None,
                },
            )
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let history = store.complete_history(&manifest.id).await.unwrap();
        let stdin = fs::read_to_string(root.join("stdin.log")).unwrap_or_default();
        fs::remove_dir_all(root).unwrap();
        (history, stdin)
    }

    fn provider_events<'a>(
        history: &'a [ConversationEvent],
        method: &str,
    ) -> Vec<&'a ConversationEvent> {
        history
            .iter()
            .filter(|event| {
                event.event_type == "provider.event" && event.payload["providerMethod"] == method
            })
            .collect()
    }

    #[tokio::test]
    async fn claude_refusing_full_access_names_the_settings_layer_and_fails() {
        let (history, stdin) = claude_mode_settings_turn(
            "todex-claude-bypass-refused",
            "auto auto - projectSettings",
            "full-access",
        )
        .await;
        let reason = "Claude Code settings (projectSettings) disable full access (permissions.disableBypassPermissionsMode). Turn that setting off in CLI settings to use full access.";
        let events = provider_events(&history, "bypass_mode_disabled");
        assert_eq!(events.len(), 1, "{history:?}");
        assert_eq!(
            events[0].payload["metadata"],
            json!({
                "requestedMode": "full-access",
                "reportedMode": "auto",
                "settingsSource": "projectSettings",
                "reason": reason,
            })
        );
        assert!(
            history.iter().any(|event| event.event_type == "turn.failed"
                && event.payload.to_string().contains(reason)),
            "{history:?}"
        );
        // Never downgraded: the prompt does not reach the model at all.
        assert!(stdin.contains(r#""subtype":"get_settings""#), "{stdin}");
        assert!(!stdin.contains(r#""type":"user""#), "{stdin}");
        assert!(provider_events(&history, "auto_mode_unavailable").is_empty());
    }

    #[tokio::test]
    async fn claude_auto_disabled_by_settings_continues_in_the_mode_it_fell_back_to() {
        let (history, _) = claude_mode_settings_turn(
            "todex-claude-auto-settings",
            "acceptEdits acceptEdits - localSettings",
            "auto",
        )
        .await;
        let events = provider_events(&history, "auto_mode_unavailable");
        assert_eq!(events.len(), 1, "{history:?}");
        assert_eq!(
            events[0].payload["metadata"],
            json!({
                "requestedMode": "auto",
                "activeMode": "accept-edits",
                "model": "claude-fixture",
                "reason": "Claude Code settings (localSettings) disable auto mode (permissions.disableAutoMode)",
                "cause": "settings",
                "settingsSource": "localSettings",
            })
        );
        assert!(
            history
                .iter()
                .any(|event| event.event_type == "turn.completed"),
            "{history:?}"
        );
    }

    #[tokio::test]
    async fn claude_dropping_auto_mid_turn_reports_it_once_with_its_settings_layer() {
        let (history, stdin) = claude_mode_settings_turn(
            "todex-claude-auto-status",
            "auto auto default policySettings",
            "auto",
        )
        .await;
        let events = provider_events(&history, "auto_mode_unavailable");
        assert_eq!(events.len(), 1, "{history:?}");
        assert_eq!(events[0].payload["metadata"]["activeMode"], "ask");
        assert_eq!(events[0].payload["metadata"]["cause"], "settings");
        assert_eq!(
            events[0].payload["metadata"]["settingsSource"],
            "policySettings"
        );
        assert_eq!(stdin.matches(r#""subtype":"get_settings""#).count(), 1);
        assert!(
            history
                .iter()
                .any(|event| event.event_type == "turn.completed"),
            "{history:?}"
        );
    }

    #[tokio::test]
    async fn claude_auto_disabled_into_plan_fails_the_turn_naming_the_layer() {
        let (history, _) =
            claude_mode_settings_turn("todex-claude-auto-plan", "plan plan - userSettings", "auto")
                .await;
        assert!(
            history.iter().any(|event| event.event_type == "turn.failed"
                && event.payload.to_string().contains(
                    "Claude Code settings (userSettings) disable auto mode (permissions.disableAutoMode) and it fell back to 'plan'"
                )),
            "{history:?}"
        );
        assert!(provider_events(&history, "auto_mode_unavailable").is_empty());
    }

    #[tokio::test]
    async fn a_late_task_notification_keeps_the_claude_turn_open() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-late-notification").await;
        fs::write(root.join("late-notification"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "plan it".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;

        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history.iter().any(
            |event| event.event_type == "turn.completed" && event.payload["turnId"] == turn_id
        ));
        // The last task finished before the `result` that emptied the set;
        // its hand-back is answered only by the follow-up invocation.
        assert!(
            history
                .iter()
                .any(|event| event.payload.to_string().contains("fixture hand-back")),
            "{history:?}"
        );
        // The zero-turn invocation came after the prompt was answered, so the
        // prompt is not resent.
        let stdin = fs::read_to_string(root.join("stdin.log")).unwrap();
        assert_eq!(stdin.matches("plan it").count(), 1, "{stdin}");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn work_claude_starts_after_its_turn_is_journalled_as_a_provider_turn() {
        let (root, store, supervisor, workspace) = control_fixture("todex-claude-own-work").await;
        fs::write(root.join("own-work-after-idle"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let user_turn = supervisor
            .prompt(&manifest.id, "start it".to_owned(), None)
            .await
            .unwrap();
        let started = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "turn.started" && event.payload["trigger"] == "provider"
        })
        .await;
        let provider_turn = started.payload["turnId"].as_str().unwrap().to_owned();
        assert_ne!(provider_turn, user_turn);
        assert_eq!(started.payload["triggerDetail"]["kind"], "background_task");
        assert_eq!(started.payload["triggerDetail"]["taskIds"], json!(["b74"]));
        assert_eq!(
            started.payload["triggerDetail"]["summary"],
            "Background command done (exit code 0)"
        );
        assert_eq!(
            started.payload["effectivePermissions"],
            supervisor_effective_permissions(&store, &manifest.id, &user_turn).await
        );
        wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "turn.completed" && event.payload["turnId"] == provider_turn
        })
        .await;
        wait_until_idle(&supervisor).await;

        let history = store.complete_history(&manifest.id).await.unwrap();
        let of_turn = |turn: &str| {
            history
                .iter()
                .filter(|event| event.payload["turnId"] == turn)
                .map(|event| (event.event_type.clone(), event.payload.to_string()))
                .collect::<Vec<_>>()
        };
        let provider_events = of_turn(&provider_turn);
        assert!(
            provider_events
                .iter()
                .any(|(_, payload)| payload.contains("background done")),
            "{provider_events:?}"
        );
        // No user message opens it.
        assert!(!provider_events
            .iter()
            .any(|(event_type, _)| event_type == "message.created"));
        assert!(!of_turn(&user_turn)
            .iter()
            .any(|(_, payload)| payload.contains("background done")));

        // The same process answers the next prompt.
        let next = supervisor
            .prompt(&manifest.id, "and now".to_owned(), None)
            .await
            .unwrap();
        wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "turn.completed" && event.payload["turnId"] == next
        })
        .await;
        let stdin = fs::read_to_string(root.join("stdin.log")).unwrap();
        assert_eq!(
            stdin.matches(r#""subtype":"initialize""#).count(),
            1,
            "{stdin}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    async fn supervisor_effective_permissions(
        store: &ConversationStore,
        conversation_id: &str,
        turn_id: &str,
    ) -> Value {
        store
            .complete_history(conversation_id)
            .await
            .unwrap()
            .into_iter()
            .find(|event| event.event_type == "turn.started" && event.payload["turnId"] == turn_id)
            .unwrap()
            .payload["effectivePermissions"]
            .clone()
    }

    #[tokio::test]
    async fn a_queued_peer_hand_back_keeps_the_claude_turn_open_until_idle() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-peer-handback").await;
        fs::write(root.join("peer-handback"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "plan it".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;

        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history.iter().any(
            |event| event.event_type == "turn.completed" && event.payload["turnId"] == turn_id
        ));
        // Nothing was pending at the "waiting on the second" `result`; the
        // queued hand-back is answered only because the turn waits for idle.
        assert!(
            history
                .iter()
                .any(|event| event.payload.to_string().contains("fixture hand-back")),
            "{history:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_turn_resolves_an_open_permission_prompt() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-permission-cancel").await;
        fs::write(root.join("permission-drain"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        let request = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.requested"
        })
        .await;

        supervisor.cancel(&manifest.id).await.unwrap();
        wait_until_idle(&supervisor).await;
        // The detached prompt task is released by the closed cancel channel:
        // it withdraws the dialog and drops its now-undeliverable response.
        let resolved = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.resolved"
                && event.payload["permissionId"] == request.payload["permissionId"]
        })
        .await;
        assert_eq!(resolved.payload["outcome"], "cancelled");
        tokio::time::timeout(Duration::from_secs(5), async {
            while supervisor.permissions.has_pending(&manifest.id) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the pending permission is released when the turn ends");

        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(history
            .iter()
            .any(|event| event.event_type == "turn.cancelled"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_permission_claude_withdraws_is_closed_and_never_answered() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-claude-permission-withdrawn").await;
        fs::write(root.join("permission-withdrawn"), "").unwrap();
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace, None, None)
            .await
            .unwrap();
        let turn_id = supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        let request = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.requested"
        })
        .await;
        fs::write(root.join("withdraw"), "").unwrap();
        let resolved = wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.resolved"
                && event.payload["permissionId"] == request.payload["permissionId"]
        })
        .await;
        assert_eq!(resolved.payload["outcome"], "cancelled");
        wait_until_idle(&supervisor).await;

        let history = store.complete_history(&manifest.id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|event| event.event_type == "turn.completed"
                    && event.payload["turnId"] == turn_id),
            "{history:?}"
        );
        assert!(!supervisor.permissions.has_pending(&manifest.id));
        let stdin = fs::read_to_string(root.join("stdin.log")).unwrap();
        assert!(!stdin.contains("perm-w"), "{stdin}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_tool_mode_lasts_only_while_a_turn_runs() {
        let (root, store, supervisor, workspace) = control_fixture("todex-agent-tool-mode").await;
        let ssh = crate::ssh::tests::fixture("Host web\n").await;
        let agent_mcp = crate::agent_mcp::tests::registry(ssh.service.clone(), &root).await;
        let supervisor = supervisor.with_agent_mcp(agent_mcp.clone());
        let mode = |id: &str| agent_mcp.authorizer_state().mode(id);
        let manifest = supervisor
            .create(ProviderKind::ClaudeCode, workspace.clone(), None, None)
            .await
            .unwrap();

        // A turn that cannot start records nothing.
        let moved = root.join("moved-workspace");
        fs::rename(&workspace, &moved).unwrap();
        assert!(supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .is_err());
        assert_eq!(mode(&manifest.id), None);
        fs::rename(&moved, &workspace).unwrap();

        // Recorded while the turn runs (it waits on a permission here),
        // gone once it ends.
        fs::write(root.join("permission-drain"), "").unwrap();
        supervisor
            .prompt(&manifest.id, "hello".to_owned(), None)
            .await
            .unwrap();
        wait_for_journal_event(&store, &manifest.id, |event| {
            event.event_type == "permission.requested"
        })
        .await;
        assert!(mode(&manifest.id).is_some());
        supervisor.cancel(&manifest.id).await.unwrap();
        wait_until_idle(&supervisor).await;
        // No mode: side-effect tools ask (see `AuthorizerState::tool_mode`).
        assert_eq!(mode(&manifest.id), None);
        fs::remove_dir_all(root).unwrap();
    }

    /// Rewrites a manifest's status on disk, as a crash between the journal
    /// append and the manifest write would leave it.
    fn set_manifest_status(root: &Path, conversation_id: &str, status: &str) {
        let path = root
            .join("data/conversations")
            .join(conversation_id)
            .join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["status"] = json!(status);
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn restart_recovery_follows_the_journal_not_the_manifest_status() {
        let (root, store, supervisor, workspace) = control_fixture("todex-recover-journal").await;
        let open = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&open.id, "turn.started", json!({ "turnId": "turn-open" }))
            .await
            .unwrap();
        set_manifest_status(&root, &open.id, "idle");
        let closed = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        for event_type in ["turn.started", "turn.completed"] {
            store
                .append(&closed.id, event_type, json!({ "turnId": "turn-closed" }))
                .await
                .unwrap();
        }
        set_manifest_status(&root, &closed.id, "running");
        let closed_events = store.complete_history(&closed.id).await.unwrap().len();

        supervisor.recover_all().await.unwrap();

        let history = store.complete_history(&open.id).await.unwrap();
        let interrupted = history
            .iter()
            .filter(|event| event.event_type == "conversation.interrupted")
            .collect::<Vec<_>>();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].payload["turnId"], "turn-open");
        assert_eq!(interrupted[0].payload["reason"], "daemon_restarted");
        assert_eq!(
            store.get(&open.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        // The stale "running" manifest is rebuilt from the journal as idle and
        // no interruption is invented for a turn that already completed.
        assert_eq!(
            store.complete_history(&closed.id).await.unwrap().len(),
            closed_events
        );
        assert_eq!(
            store.get(&closed.id).await.unwrap().status,
            ConversationStatus::Idle
        );

        supervisor.recover_all().await.unwrap();
        assert_eq!(
            store.complete_history(&open.id).await.unwrap().len(),
            history.len()
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn journal_path(root: &Path, conversation_id: &str) -> PathBuf {
        root.join("data/conversations")
            .join(conversation_id)
            .join("events.jsonl")
    }

    fn corrupt_copies(root: &Path, conversation_id: &str) -> usize {
        fs::read_dir(root.join("data/conversations").join(conversation_id))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("events.corrupt.")
            })
            .count()
    }

    /// Replace journal line `index` with bytes that do not parse.
    fn corrupt_journal_line(root: &Path, conversation_id: &str, index: usize) -> Vec<u8> {
        let path = journal_path(root, conversation_id);
        let raw = fs::read_to_string(&path).unwrap();
        let mut lines = raw.lines().map(str::to_owned).collect::<Vec<_>>();
        lines[index] = "{\"torn".to_owned();
        let corrupt = format!("{}\n", lines.join("\n")).into_bytes();
        fs::write(&path, &corrupt).unwrap();
        corrupt
    }

    #[tokio::test]
    async fn recover_all_leaves_settled_journals_to_their_first_read() {
        let (root, store, supervisor, workspace) = control_fixture("todex-recover-lazy").await;
        let mut ids = Vec::new();
        for (end, last) in [
            ("turn.completed", "turn.completed"),
            // Informational events after a finished turn (a provider's command
            // catalog, imported history) leave the conversation Idle.
            ("turn.completed", "provider.commands.updated"),
            ("turn.failed", "message.created"),
        ] {
            let manifest = store
                .create(ConversationManifest::new(
                    ProviderKind::Codex,
                    workspace.clone(),
                    None,
                    None,
                ))
                .await
                .unwrap();
            for event_type in ["message.created", "turn.started", end, last] {
                store
                    .append(&manifest.id, event_type, json!({ "turnId": "t" }))
                    .await
                    .unwrap();
            }
            ids.push(manifest.id);
        }
        let (settled, settled_idle, unsettled) = (&ids[0], &ids[1], &ids[2]);
        let settled_bytes = corrupt_journal_line(&root, settled, 1);
        let settled_idle_bytes = corrupt_journal_line(&root, settled_idle, 1);
        corrupt_journal_line(&root, unsettled, 1);

        supervisor.recover_all().await.unwrap();

        // Idle with the manifest in step: startup never read the journal
        // body, whether the tail is terminal or merely informational, so the
        // damage is still there untouched.
        for (id, bytes) in [(settled, settled_bytes), (settled_idle, settled_idle_bytes)] {
            assert_eq!(fs::read(journal_path(&root, id)).unwrap(), bytes);
            assert_eq!(corrupt_copies(&root, id), 0);
            assert_eq!(
                store.get(id).await.unwrap().status,
                ConversationStatus::Idle
            );
        }
        // A failed conversation needs a terminal tail; without one the full
        // scan runs and salvages at startup.
        assert_eq!(corrupt_copies(&root, unsettled), 1);

        // The first read of the settled conversation validates and salvages.
        let replay = supervisor.replay(settled, 0, 100).await.unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(replay.events[1].event_type, "journal.recordLost");
        assert_eq!(corrupt_copies(&root, settled), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recover_all_fully_recovers_a_journal_ahead_of_its_manifest() {
        let (root, store, supervisor, workspace) = control_fixture("todex-recover-ahead").await;
        let manifest = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace,
                None,
                None,
            ))
            .await
            .unwrap();
        for event_type in ["turn.started", "turn.completed"] {
            store
                .append(&manifest.id, event_type, json!({ "turnId": "a" }))
                .await
                .unwrap();
        }
        // Journal lines a crash kept while the manifest never saw them: turn b
        // opens, then a late terminal for turn a leaves it open while the
        // tail still looks terminal.
        let mut raw = fs::read(journal_path(&root, &manifest.id)).unwrap();
        for (sequence, event_type, turn_id) in
            [(3, "turn.started", "b"), (4, "turn.completed", "a")]
        {
            let event = ConversationEvent::new(
                &manifest.id,
                sequence,
                event_type,
                json!({ "turnId": turn_id }),
            );
            raw.extend(serde_json::to_vec(&event).unwrap());
            raw.push(b'\n');
        }
        fs::write(journal_path(&root, &manifest.id), raw).unwrap();
        assert_eq!(store.get(&manifest.id).await.unwrap().last_sequence, 2);

        supervisor.recover_all().await.unwrap();

        let history = store.complete_history(&manifest.id).await.unwrap();
        let interrupted = history
            .iter()
            .filter(|event| event.event_type == "conversation.interrupted")
            .collect::<Vec<_>>();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].sequence, 5);
        assert_eq!(interrupted[0].payload["turnId"], "b");
        let recovered = store.get(&manifest.id).await.unwrap();
        assert_eq!(recovered.last_sequence, 5);
        assert_eq!(recovered.status, ConversationStatus::Interrupted);

        // Now settled: a second startup appends nothing.
        supervisor.recover_all().await.unwrap();
        assert_eq!(
            store.complete_history(&manifest.id).await.unwrap().len(),
            history.len()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recover_all_recovers_a_journal_past_the_prompt_limit() {
        let (root, store, supervisor, workspace) =
            control_fixture("todex-recover-big-journal").await;
        let healthy = store
            .create(ConversationManifest::new(
                ProviderKind::Codex,
                workspace.clone(),
                None,
                None,
            ))
            .await
            .unwrap();
        store
            .append(&healthy.id, "turn.started", json!({ "turnId": "t" }))
            .await
            .unwrap();

        // A journal packed with small strings has nothing compaction can
        // shrink. Over the prompt limit it refuses new turns, but recovery
        // bookkeeping still appends and startup recovers every journal.
        let wedged = ConversationManifest::new(ProviderKind::Codex, workspace, None, None);
        let wedged_id = wedged.id.clone();
        let chunk = "x".repeat(3900);
        let mut events = vec![ConversationEvent::new(
            &wedged_id,
            1,
            "permission.requested",
            json!({ "permissionId": "stuck" }),
        )];
        let mut bytes = serde_json::to_vec(&events[0]).unwrap().len() as u64 + 1;
        while bytes <= 60 * 1024 * 1024 {
            let event = ConversationEvent::new(
                &wedged_id,
                events.len() as u64 + 1,
                "provider.event",
                json!({ "items": vec![chunk.clone(); 16] }),
            );
            bytes += serde_json::to_vec(&event).unwrap().len() as u64 + 1;
            events.push(event);
        }
        store
            .create_with_history(wedged, events, None, None)
            .await
            .unwrap();

        supervisor.recover_all().await.unwrap();
        assert_eq!(
            store.get(&healthy.id).await.unwrap().status,
            ConversationStatus::Interrupted
        );
        assert!(store
            .complete_history(&healthy.id)
            .await
            .unwrap()
            .iter()
            .any(|event| event.event_type == "conversation.interrupted"));
        assert_eq!(
            store.get(&wedged_id).await.unwrap().status,
            ConversationStatus::Interrupted
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
                auth_method: None,
                api_key_env: None,
                resident: true,
            },
        );
        let config = Arc::new(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture_text.clone(),
                claude_bin: fixture_text.clone(),
                pi_bin: fixture_text.clone(),
                grok_bin: fixture_text.clone(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: fixture_text.clone(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: fixture_text.clone(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: fixture_text.clone(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: fixture_text,
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: profiles,
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
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

    /// Runs one fixture turn per provider through a wrapper that logs argv
    /// and stdin, and returns both logs.
    #[cfg(unix)]
    async fn logged_fixture_turns(
        supervisor: &ConversationSupervisor,
        workspace: &Path,
        root: &Path,
    ) -> (String, String) {
        let _ = fs::remove_file(root.join("argv.log"));
        let _ = fs::remove_file(root.join("stdin.log"));
        for provider in [
            ProviderKind::Codex,
            ProviderKind::ClaudeCode,
            ProviderKind::Acp,
        ] {
            let profile = (provider == ProviderKind::Acp).then(|| "fixture".to_owned());
            let manifest = supervisor
                .create(provider, workspace.to_path_buf(), None, profile)
                .await
                .unwrap();
            supervisor
                .prompt(&manifest.id, "hello".to_owned(), None)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let replay = supervisor.replay(&manifest.id, 0, 100).await.unwrap();
                    if let Some(event) = replay.events.iter().find(|event| {
                        matches!(event.event_type.as_str(), "turn.completed" | "turn.failed")
                    }) {
                        assert_eq!(
                            event.event_type,
                            "turn.completed",
                            "{}: {:?}",
                            provider.as_str(),
                            event.payload
                        );
                        break;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture provider turn should finish");
        }
        supervisor.shutdown_all().await;
        (
            fs::read_to_string(root.join("argv.log")).unwrap(),
            fs::read_to_string(root.join("stdin.log")).unwrap(),
        )
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agent_ssh_tools_are_injected_only_while_a_host_is_enabled() {
        use std::os::unix::fs::PermissionsExt;

        let (root, _store, supervisor, workspace) =
            control_fixture_with("todex-agent-mcp-injection", true, false).await;
        let fixture = root.join("provider-fixture.sh");
        let wrapper = root.join("logging-provider.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{root}/argv.log'\ntee -a '{root}/stdin.log' | '{fixture}' \"$@\"\n",
                root = root.display(),
                fixture = fixture.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = (*supervisor.config).clone();
        let wrapper_text = wrapper.display().to_string();
        config.agent.codex_bin = wrapper_text.clone();
        config.agent.claude_bin = wrapper_text.clone();
        config
            .agent
            .acp_profiles
            .get_mut("fixture")
            .unwrap()
            .command = wrapper_text;
        let ssh = crate::ssh::tests::fixture("Host web\n").await;
        let agent_mcp = crate::agent_mcp::tests::registry(ssh.service.clone(), &root).await;
        let supervisor = ConversationSupervisor::new(
            Arc::new(config),
            supervisor.store.clone(),
            ConversationEventHub::default(),
        )
        .with_agent_mcp(agent_mcp);

        // Without an agent-enabled host the launches are exactly as before.
        let (argv, stdin) = logged_fixture_turns(&supervisor, &workspace, &root).await;
        assert!(!argv.contains("--mcp-config"), "{argv}");
        assert!(!stdin.contains("todex_ssh"), "{stdin}");
        assert!(stdin.contains(r#""mcpServers":[]"#), "{stdin}");

        ssh.service.set_agent_access("web", true).await.unwrap();
        let (argv, stdin) = logged_fixture_turns(&supervisor, &workspace, &root).await;
        let lines: Vec<Value> = stdin
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let request = |method: &str| {
            lines
                .iter()
                .find(|line| line["method"] == method)
                .unwrap_or_else(|| panic!("no {method} in {stdin}"))
        };

        let codex = &request("thread/start")["params"]["config"]["mcp_servers.todex_ssh"];
        assert_eq!(codex["command"], "/opt/todex/todex-agentd");
        assert_eq!(codex["args"], json!(["agent-mcp-bridge"]));
        assert_eq!(
            codex["env"]["TODEX_AGENT_MCP_URL"],
            "http://127.0.0.1:7345/internal/agent-mcp/ssh"
        );
        assert_eq!(
            codex["env"]["TODEX_AGENT_MCP_TOKEN"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(codex["default_tools_approval_mode"], "approve");

        let claude = argv
            .lines()
            .find(|line| line.starts_with("-p "))
            .unwrap_or_else(|| panic!("no claude launch in {argv}"));
        assert!(claude.contains("--allowedTools=mcp__todex_ssh"), "{claude}");
        let config_path = claude
            .split("--mcp-config=")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .unwrap();
        let config: Value =
            serde_json::from_str(&fs::read_to_string(config_path).unwrap()).unwrap();
        let entry = &config["mcpServers"]["todex_ssh"];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["args"], json!(["agent-mcp-bridge"]));
        assert!(entry["env"]["TODEX_AGENT_MCP_TOKEN"].is_string());
        assert_eq!(
            fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let acp = &request("session/new")["params"]["mcpServers"];
        assert_eq!(acp[0]["name"], "todex_ssh");
        assert_eq!(acp[0]["command"], "/opt/todex/todex-agentd");
        assert_eq!(acp[0]["env"][1]["name"], "TODEX_AGENT_MCP_TOKEN");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pi_keeps_full_auto_argv_and_reopens_after_workspace_cancel() {
        let root = temp_dir("todex-pi-launch-contract");
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
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "pi".to_owned(),
                codex_bin: fixture_text.clone(),
                claude_bin: fixture_text.clone(),
                pi_bin: fixture_text.clone(),
                grok_bin: fixture_text,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
        });
        let store = ConversationStore::new(config.data_dir.clone())
            .await
            .unwrap();
        let supervisor =
            ConversationSupervisor::new(config, store.clone(), ConversationEventHub::default());
        let manifest = supervisor
            .create(ProviderKind::Pi, workspace.clone(), None, None)
            .await
            .unwrap();

        supervisor
            .models_live(ProviderKind::Pi, &workspace)
            .await
            .unwrap();
        supervisor
            .commands_live(ProviderKind::Pi, &workspace)
            .await
            .unwrap();
        supervisor
            .prompt(&manifest.id, "start".to_owned(), None)
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
        wait_until_idle(&supervisor).await;
        let catalog = supervisor
            .conversation_commands_owned("local", &manifest.id)
            .await
            .unwrap();
        assert_eq!(catalog["catalogSource"], "session");
        assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 3);
        assert!(matches!(
            supervisor
                .conversation_commands_owned("other", &manifest.id)
                .await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            supervisor.stop_runtime_owned("other", &manifest.id).await,
            Err(AppError::NotFound(_))
        ));
        supervisor
            .cancel_owned("local", &manifest.id, None)
            .await
            .unwrap();
        assert_eq!(
            supervisor
                .conversation_commands_owned("local", &manifest.id)
                .await
                .unwrap()["runtimeId"],
            catalog["runtimeId"]
        );
        assert_eq!(
            supervisor
                .cancel_workspace_owned("local", &workspace)
                .await
                .unwrap(),
            0
        );
        assert!(store
            .complete_history(&manifest.id)
            .await
            .unwrap()
            .iter()
            .any(|event| event.event_type == "provider.runtime"
                && event.payload["reason"] == "workspace_access_revoked"));
        supervisor
            .prompt(&manifest.id, "reopen".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        let reopened = supervisor
            .conversation_commands_owned("local", &manifest.id)
            .await
            .unwrap();
        assert_ne!(catalog["runtimeId"], reopened["runtimeId"]);
        let stopped = supervisor
            .stop_runtime_owned("local", &manifest.id)
            .await
            .unwrap();
        assert_eq!(stopped["status"], "stopped");
        assert_eq!(
            store.get(&manifest.id).await.unwrap().status,
            ConversationStatus::Idle
        );
        assert!(store
            .provider_state(&manifest.id)
            .await
            .unwrap()
            .native_session_id
            .is_some());
        supervisor
            .prompt(&manifest.id, "reopen before delete".to_owned(), None)
            .await
            .unwrap();
        wait_until_idle(&supervisor).await;
        supervisor
            .delete_owned("local", &manifest.id)
            .await
            .unwrap();
        assert!(supervisor
            .registry
            .driver(ProviderKind::Pi)
            .unwrap()
            .session_commands(&manifest.id)
            .await
            .unwrap()
            .is_none());
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
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture.to_string_lossy().to_string(),
                claude_bin: fixture.to_string_lossy().to_string(),
                pi_bin: fixture.to_string_lossy().to_string(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
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
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: fixture.to_string_lossy().to_string(),
                claude_bin: fixture.to_string_lossy().to_string(),
                pi_bin: fixture.to_string_lossy().to_string(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
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

if [ "$mode" = "--no-auto-update" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"_meta":{"modelState":{"currentModelId":"grok-fixture","availableModels":[{"modelId":"grok-fixture","name":"Grok Fixture","_meta":{"supportsReasoningEffort":true}}]}}}}\n' "$id"
        ;;
      *'"method":"session/new"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"grok-native","models":{"currentModelId":"grok-fixture","availableModels":[{"modelId":"grok-fixture","name":"Grok Fixture","_meta":{"supportsReasoningEffort":true}}]}}}\n' "$id"
        ;;
      *'"method":"session/load"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"models":{"currentModelId":"grok-fixture","availableModels":[{"modelId":"grok-fixture","name":"Grok Fixture","_meta":{"supportsReasoningEffort":true}}]}}}\n' "$id"
        ;;
      *'"method":"session/prompt"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
        ;;
    esac
  done
elif [ "$mode" = "acp" ] || [ "$mode" = "--profile" ]; then
  while IFS= read -r line; do
    case "$line" in
      *'"method":"initialize"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"fork":{}}},"authMethods":[{"id":"fixture-key","name":"API key"}]}}\n' "$id"
        ;;
      *'"method":"authenticate"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{}}\n' "$id"
        ;;
      *'"method":"session/new"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"acp-native","configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"fixture-model","options":[{"value":"fixture-model","name":"Fixture"}]},{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"build","name":"build"},{"value":"plan","name":"plan"}]}]}}\n' "$id"
        ;;
      *'"method":"session/load"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"fixture-model","options":[{"value":"fixture-model","name":"Fixture"}]},{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"build","name":"build"},{"value":"plan","name":"plan"}]}]}}\n' "$id"
        ;;
      *'"method":"session/set_config_option"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"fixture-model","options":[{"value":"fixture-model","name":"Fixture"}]},{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"build","options":[{"value":"build","name":"build"},{"value":"plan","name":"plan"}]}]}}\n' "$id"
        ;;
      *'"method":"session/fork"'*)
        id=$(extract_id "$line")
        printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"acp-fork-native"}}\n' "$id"
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
elif [ "$mode" = "--input-format" ]; then
  # Antigravity print mode: one prompt in, then like agy it exits only once
  # stdin closes.
  IFS= read -r line
  printf '{"event":"init","conversation_id":"agy-native","init":{"permission_mode":"request-review"}}\n'
  printf '{"event":"step_update","step_update":{"conversation_id":"agy-native","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"agy fixture"}}\n'
  printf '{"event":"result","result":{"conversation_id":"agy-native","status":"SUCCESS","response":"agy fixture","num_turns":1}}\n'
  cat > /dev/null
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
elif [ -f "$(dirname "$0")/late-notification" ]; then
  # Frame order captured from Claude Code: a subagent finishes while the main
  # invocation is still answering, so its notification precedes that
  # invocation's `result` and the hand-back is answered by a follow-up
  # invocation. A redundant zero-turn invocation (the notification paired
  # with an already-delivered hand-back) comes first. Frames are written from
  # a subshell so every stdin line, including a resent prompt, is logged.
  while IFS= read -r line; do
    printf '%s\n' "$line" >> "$(dirname "$0")/stdin.log"
    case "$line" in
      *'"subtype":"initialize"'*)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{}}}\n'
        continue
        ;;
    esac
    [ -n "$started" ] && continue
    started=1
    (
      printf '{"type":"system","subtype":"task_started","task_id":"bg-1","is_backgrounded":true}\n'
      printf '{"type":"system","subtype":"task_started","task_id":"bg-2","is_backgrounded":true}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"launched"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"claude-native","result":"launched"}\n'
      printf '{"type":"system","subtype":"task_notification","task_id":"bg-1","status":"completed"}\n'
      printf '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bg-2"}]}\n'
      printf '{"type":"system","subtype":"init"}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":0,"session_id":"claude-native","result":""}\n'
      sleep 0.3
      printf '{"type":"system","subtype":"init"}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"one is back"}}}\n'
      printf '{"type":"system","subtype":"task_notification","task_id":"bg-2","status":"completed"}\n'
      printf '{"type":"system","subtype":"background_tasks_changed","tasks":[]}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"claude-native","result":"one is back"}\n'
      printf '{"type":"system","subtype":"init"}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"fixture hand-back"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"claude-native","result":"fixture hand-back"}\n'
    ) &
  done
elif [ -f "$(dirname "$0")/own-work-after-idle" ]; then
  # Frame order captured from Claude Code 2.1.299: a backgrounded command is
  # still running at `idle`; when it finishes, Claude starts an invocation of
  # its own. Later prompts are answered normally.
  while IFS= read -r line; do
    printf '%s\n' "$line" >> "$(dirname "$0")/stdin.log"
    case "$line" in
      *'"subtype":"initialize"'*)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{}}}\n'
        continue
        ;;
    esac
    printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
    printf '%s\n' "$line"
    if [ -n "$started" ]; then
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"answered"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"answered"}\n'
      printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
      continue
    fi
    started=1
    printf '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b74","task_type":"local_bash"}]}\n'
    printf '{"type":"system","subtype":"task_started","task_id":"b74","is_backgrounded":true,"task_type":"local_bash"}\n'
    printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"started"}}}\n'
    printf '{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"started"}\n'
    printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
    (
      sleep 0.3
      printf '{"type":"system","subtype":"task_updated","task_id":"b74","patch":{"status":"completed"}}\n'
      printf '{"type":"system","subtype":"task_notification","task_id":"b74","status":"completed","summary":"Background command done (exit code 0)"}\n'
      printf '{"type":"system","subtype":"background_tasks_changed","tasks":[]}\n'
      printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
      printf '{"type":"system","subtype":"init"}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"background done"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"background done"}\n'
      printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
    ) &
  done
elif [ -f "$(dirname "$0")/peer-handback" ]; then
  # Background subagents that report through `SubagentHandback`: their
  # hand-backs reach the main session as queued `peer` messages with no
  # `task_notification`, so nothing is pending at the second `result` even
  # though the second hand-back is still queued. Only `idle` ends the turn.
  while IFS= read -r line; do
    case "$line" in
      *'"subtype":"initialize"'*)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{}}}\n'
        continue
        ;;
    esac
    [ -n "$started" ] && continue
    started=1
    (
      printf '{"type":"system","subtype":"session_state_changed","state":"running"}\n'
      printf '{"type":"system","subtype":"task_started","task_id":"a-1","is_backgrounded":true}\n'
      printf '{"type":"system","subtype":"task_started","task_id":"a-2","is_backgrounded":true}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"launched"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"claude-native","result":"launched"}\n'
      printf '{"type":"system","subtype":"background_tasks_changed","tasks":[]}\n'
      printf '{"type":"system","subtype":"init"}\n'
      printf '{"type":"user","origin":{"kind":"peer","senderTaskId":"a-1"},"message":{"role":"user","content":"report one"}}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"waiting on the second"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"claude-native","result":"waiting on the second"}\n'
      sleep 0.3
      printf '{"type":"system","subtype":"init"}\n'
      printf '{"type":"user","origin":{"kind":"peer","senderTaskId":"a-2"},"message":{"role":"user","content":"report two"}}\n'
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"fixture hand-back"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"claude-native","result":"fixture hand-back"}\n'
      printf '{"type":"system","subtype":"session_state_changed","state":"idle"}\n'
    ) &
  done
elif [ -f "$(dirname "$0")/mode-settings" ]; then
  # Claude replacing a requested permission mode. The marker holds the mode
  # initialize reports, the init frame's mode, a mid-turn status frame's
  # mode (or -), and the settings layer get_settings names.
  read -r reported init_mode status_mode layer < "$(dirname "$0")/mode-settings"
  settings_answer() {
    printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-get-settings","response":{"effective":{},"sources":[{"source":"userSettings","settings":{"permissions":{"defaultMode":"auto"}}},{"source":"%s","settings":{"permissions":{"disableAutoMode":"disable","disableBypassPermissionsMode":"disable"}}}]}}}\n' "$layer"
  }
  while IFS= read -r line; do
    printf '%s\n' "$line" >> "$(dirname "$0")/stdin.log"
    case "$line" in
      *'"subtype":"initialize"'*)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{"current_permission_mode":"%s"}}}\n' "$reported"
        continue
        ;;
      *'"subtype":"get_settings"'*)
        settings_answer
        continue
        ;;
    esac
    printf '{"type":"system","subtype":"init","permissionMode":"%s","model":"claude-fixture"}\n' "$init_mode"
    if [ "$status_mode" != "-" ]; then
      printf '{"type":"system","subtype":"status","permissionMode":"%s"}\n' "$status_mode"
      # The turn asks for the settings before it reports the change.
      IFS= read -r probe
      printf '%s\n' "$probe" >> "$(dirname "$0")/stdin.log"
      settings_answer
    fi
    printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"claude fixture"}}}\n'
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"claude-native","result":"ok"}\n'
  done
else
  while IFS= read -r line; do
    if [ -f "$(dirname "$0")/permission-withdrawn" ]; then
      printf '%s\n' "$line" >> "$(dirname "$0")/stdin.log"
    fi
    case "$line" in
      *'"subtype":"initialize"'*)
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"todex-initialize","response":{}}}\n'
        continue
        ;;
      *'"type":"control_response"'*)
        if [ -f "$(dirname "$0")/permission-withdrawn" ]; then
          continue
        fi
        ;;
    esac
    if [ -f "$(dirname "$0")/silent-turn" ]; then
      continue
    fi
    if [ -f "$(dirname "$0")/noisy-stdout" ]; then
      printf 'provider banner: not json\n'
      head -c 4194400 /dev/zero | tr '\0' x
      printf '\n'
    fi
    if [ -f "$(dirname "$0")/big-tool-result" ]; then
      printf '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_big","name":"Bash","input":{"command":"cat big.log"}}]}}\n'
      printf '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_big","is_error":false,"content":[{"type":"text","text":"'
      head -c 2097152 /dev/zero | tr '\0' y
      printf '"}]}]}}\n'
    fi
    if [ -f "$(dirname "$0")/mode-mismatch" ]; then
      printf '{"type":"system","subtype":"init","permissionMode":"plan","model":"claude-fixture"}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"session_id":"claude-native","result":"ok"}\n'
      continue
    fi
    if [ -f "$(dirname "$0")/auto-unavailable" ]; then
      # Claude Code without auto mode for the model: it reports `default`
      # and asks for every tool through can_use_tool.
      printf '{"type":"system","subtype":"init","permissionMode":"default","model":"claude-haiku-fixture"}\n'
      printf '{"type":"control_request","request_id":"perm-auto","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}\n'
      IFS= read -r reply
      case "$reply" in
        *'"request_id":"perm-auto"'*'"behavior":"allow"'*)
          printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"claude fixture"}}}\n'
          printf '{"type":"result","subtype":"success","is_error":false,"session_id":"claude-native","result":"allowed"}\n'
          ;;
        *)
          printf '{"type":"result","subtype":"success","is_error":true,"session_id":"claude-native","result":"not allowed"}\n'
          ;;
      esac
      continue
    fi
    if [ -f "$(dirname "$0")/permission-withdrawn" ]; then
      # Claude withdraws the prompt (an interrupt while it was open) and
      # goes on; it reads nothing more for it.
      printf '{"type":"control_request","request_id":"perm-w","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}\n'
      while [ ! -f "$(dirname "$0")/withdraw" ]; do sleep 0.02; done
      printf '{"type":"control_cancel_request","request_id":"perm-w"}\n'
      sleep 0.3
      printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"claude fixture"}}}\n'
      printf '{"type":"result","subtype":"success","is_error":false,"session_id":"claude-native","result":"ok"}\n'
      continue
    fi
    if [ -f "$(dirname "$0")/permission-drain" ]; then
      printf '{"type":"control_request","request_id":"perm-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}\n'
      # Written while the prompt is still open; the turn completes only if
      # this frame was drained and the control_response arrives on stdin.
      printf '{"type":"stream_event","event":{"type":"fixture_marker"}}\n'
      IFS= read -r reply
      case "$reply" in
        *'"request_id":"perm-1"'*)
          printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"claude fixture"}}}\n'
          printf '{"type":"result","subtype":"success","is_error":false,"session_id":"claude-native","result":"ok"}\n'
          ;;
        *)
          printf '{"type":"result","subtype":"success","is_error":true,"session_id":"claude-native","result":"missing control response"}\n'
          ;;
      esac
      continue
    fi
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
      id=$(extract_id "$line")
      printf '{{"id":"%s","type":"response","success":true,"data":{{"commands":[]}}}}\n' "$id"
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
    async fn prompt_content_may_name_uploaded_files() {
        let root = temp_dir("todex-prompt-uploads");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let uploads = crate::prompt_uploads::upload_root();
        let file = crate::prompt_uploads::write_chunk(&uploads, None, "notes.md", 0, b"# notes")
            .await
            .unwrap();
        let image = crate::prompt_uploads::write_chunk(&uploads, None, "shot.png", 0, b"png")
            .await
            .unwrap();

        let (text, content) = prepare_prompt_content(
            ProviderKind::ClaudeCode,
            &workspace,
            vec![
                PromptContentRef::File {
                    path: file.path.clone(),
                    name: None,
                },
                PromptContentRef::LocalImage {
                    path: image.path.clone(),
                },
            ],
        )
        .await
        .unwrap();
        let file_path = fs::canonicalize(&file.path).unwrap();
        let image_path = fs::canonicalize(&image.path).unwrap();
        assert_eq!(
            text,
            format!(
                "Attached file: @{}\n\nAttached image: {}",
                file_path.display(),
                image_path.display()
            )
        );
        assert!(matches!(
            content.as_slice(),
            [
                DriverPromptContent::File { .. },
                DriverPromptContent::Image { path: Some(_), .. }
            ]
        ));

        // A path that climbs back out of the upload directory is refused.
        let error = prepare_prompt_content(
            ProviderKind::Codex,
            &workspace,
            vec![PromptContentRef::File {
                path: uploads.join(&file.upload_id).join("..").join(".."),
                name: None,
            }],
        )
        .await
        .expect_err("paths that leave the upload directory must be rejected");
        assert!(error.to_string().contains("trusted workspace"));

        let _ = fs::remove_dir_all(uploads.join(&file.upload_id));
        let _ = fs::remove_dir_all(uploads.join(&image.upload_id));
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
            .expect("typed image input passes through to every provider");
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
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: executable.clone(),
                claude_bin: executable.clone(),
                pi_bin: executable.clone(),
                grok_bin: executable,
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
            deployment: Default::default(),
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
