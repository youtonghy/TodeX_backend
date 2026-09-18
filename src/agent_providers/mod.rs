//! Managed agent provider accounts (cc-switch model).
//!
//! The store at `$DATA_DIR/agent-providers.json` is the source of truth; each
//! agent's native config file is a projection written on save (additive
//! agents: Pi, OpenCode) or on activate (exclusive agents: Claude, Codex).
//! Writes use atomic owner-only files plus a content-revision check so edits
//! made outside TodeX surface as conflicts rather than being overwritten.

mod claude;
mod codex;
mod files;
mod model_fetch;
mod opencode;
mod pi;
mod store;

pub use store::AgentProviderProfile;
use store::{AgentProviderBucket, AgentProviderStore};

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::conversation::ProviderKind;
use crate::error::AppError;

pub(crate) const AGENT_PROVIDERS_FILE: &str = "agent-providers.json";
pub(crate) const MAX_PROVIDER_SETTINGS_BYTES: usize = 256 * 1024;
const MAX_PROVIDERS_PER_AGENT: usize = 100;
const MAX_PROVIDER_NAME_CHARS: usize = 120;
const MAX_PROVIDER_ID_CHARS: usize = 64;

/// Agents that support managed providers in this first pass.
pub const SUPPORTED_AGENTS: [ProviderKind; 4] = [
    ProviderKind::Codex,
    ProviderKind::ClaudeCode,
    ProviderKind::Pi,
    ProviderKind::Opencode,
];

/// Sentinel written into responses in place of secret values. A write that
/// carries this value keeps the previously stored secret.
pub const MASKED_SECRET: &str = "__TODEX_MASKED__";

/// Read one env value from the live Claude `settings.json`, so model
/// discovery follows the currently activated provider rather than only the
/// daemon's own process environment.
pub(crate) fn claude_live_env(key: &str) -> Option<String> {
    let dirs = AgentDirs::detect();
    claude::read_live(&dirs)
        .ok()
        .flatten()
        .and_then(|settings| {
            settings
                .get("env")
                .and_then(|env| env.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|value| !value.trim().is_empty())
}

pub fn supported_agent(value: &str) -> Result<ProviderKind, AppError> {
    let kind = ProviderKind::from_str(value).map_err(AppError::InvalidRequest)?;
    if SUPPORTED_AGENTS.contains(&kind) {
        Ok(kind)
    } else {
        Err(AppError::Unsupported(format!(
            "{value} does not support managed providers"
        )))
    }
}

fn is_additive(agent: ProviderKind) -> bool {
    matches!(agent, ProviderKind::Pi | ProviderKind::Opencode)
}

/// Per-agent config directories, resolved per call so environment overrides
/// (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `PI_CODING_AGENT_DIR`, `XDG_CONFIG_HOME`)
/// picked up after daemon start still apply.
#[derive(Clone, Debug)]
pub(crate) struct AgentDirs {
    codex_home: PathBuf,
    claude_dir: PathBuf,
    pi_dir: PathBuf,
    opencode_dir: PathBuf,
}

impl AgentDirs {
    pub(crate) fn detect() -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let env_dir = |key: &str, fallback: PathBuf| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or(fallback)
        };
        Self {
            codex_home: env_dir("CODEX_HOME", home.join(".codex")),
            claude_dir: env_dir("CLAUDE_CONFIG_DIR", home.join(".claude")),
            pi_dir: env_dir("PI_CODING_AGENT_DIR", home.join(".pi").join("agent")),
            opencode_dir: env_dir("XDG_CONFIG_HOME", home.join(".config")).join("opencode"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProviderInput {
    pub name: String,
    #[serde(default)]
    pub settings_config: Value,
    #[serde(default)]
    pub website_url: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_color: Option<String>,
    #[serde(default)]
    pub sort_index: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivateInput {
    #[serde(default)]
    pub model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportLiveInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Provider store plus the live-file writes, serialized per agent.
#[derive(Clone)]
pub struct AgentProviderService {
    store: AgentProviderStore,
    locks: [std::sync::Arc<Mutex<()>>; 4],
    dirs_override: Option<AgentDirs>,
}

impl AgentProviderService {
    pub async fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        Self::with_dirs(data_dir, None).await
    }

    async fn with_dirs(data_dir: PathBuf, dirs: Option<AgentDirs>) -> Result<Self, AppError> {
        Ok(Self {
            store: AgentProviderStore::new(data_dir).await?,
            locks: [
                std::sync::Arc::new(Mutex::new(())),
                std::sync::Arc::new(Mutex::new(())),
                std::sync::Arc::new(Mutex::new(())),
                std::sync::Arc::new(Mutex::new(())),
            ],
            dirs_override: dirs,
        })
    }

    fn dirs(&self) -> AgentDirs {
        self.dirs_override.clone().unwrap_or_else(AgentDirs::detect)
    }

    fn lock_for(&self, agent: ProviderKind) -> &Mutex<()> {
        let index = SUPPORTED_AGENTS
            .iter()
            .position(|kind| *kind == agent)
            .expect("agent gated by supported_agent");
        &self.locks[index]
    }

    /// `{agents: {<id>: block}}` for one agent or all supported agents.
    pub async fn snapshot(&self, agent: Option<ProviderKind>) -> Result<Value, AppError> {
        let dirs = self.dirs();
        let snapshot = self.store.snapshot().await;
        let agents = match agent {
            Some(kind) => vec![kind],
            None => SUPPORTED_AGENTS.to_vec(),
        };
        let mut out = Map::new();
        for kind in agents {
            let bucket = snapshot
                .agents
                .get(kind.as_str())
                .cloned()
                .unwrap_or_default();
            out.insert(
                kind.as_str().to_owned(),
                self.agent_block(&dirs, kind, &bucket)?,
            );
        }
        Ok(json!({ "agents": out, "updatedAt": snapshot.updated_at }))
    }

    /// One agent block: `{agent, mode, currentProviderId, providers[], live}`.
    fn agent_block(
        &self,
        dirs: &AgentDirs,
        agent: ProviderKind,
        bucket: &AgentProviderBucket,
    ) -> Result<Value, AppError> {
        let mut providers: Vec<&AgentProviderProfile> = bucket.providers.values().collect();
        providers.sort_by(|a, b| {
            a.sort_index
                .unwrap_or(i64::MAX)
                .cmp(&b.sort_index.unwrap_or(i64::MAX))
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        let providers: Vec<Value> = providers
            .iter()
            .map(|profile| masked_profile(agent, profile))
            .collect();
        Ok(json!({
            "agent": agent.as_str(),
            "mode": if is_additive(agent) { "additive" } else { "exclusive" },
            "currentProviderId": bucket.current_provider_id,
            "providers": providers,
            "live": self.live_block(dirs, agent, bucket)?,
        }))
    }

    fn live_block(
        &self,
        dirs: &AgentDirs,
        agent: ProviderKind,
        bucket: &AgentProviderBucket,
    ) -> Result<Value, AppError> {
        if is_additive(agent) {
            let nodes = additive_nodes(dirs, agent)?;
            let unmanaged: Vec<String> = nodes
                .keys()
                .filter(|id| !bucket.providers.contains_key(id.as_str()))
                .cloned()
                .collect();
            let masked_nodes: Map<String, Value> = nodes
                .into_iter()
                .map(|(id, node)| (id, mask_agent_settings(agent, &node)))
                .collect();
            let selection = additive_selection(dirs, agent)?;
            return Ok(json!({
                "kind": "additive",
                "providers": masked_nodes,
                "selection": selection,
                "unmanagedProviders": unmanaged,
            }));
        }
        let live = read_live(dirs, agent)?;
        let current = bucket
            .current_provider_id
            .as_deref()
            .and_then(|id| bucket.providers.get(id));
        Ok(json!({
            "kind": "exclusive",
            "configured": live.is_some(),
            "config": live.as_ref().map(|value| mask_agent_settings(agent, value)),
            "matchesCurrent": match (&live, current) {
                (Some(live), Some(current)) => live_matches(agent, live, &current.settings_config),
                _ => false,
            },
        }))
    }

    /// The normalized live state for `GET …/live` (masked).
    pub async fn live(&self, agent: ProviderKind) -> Result<Value, AppError> {
        let dirs = self.dirs();
        let bucket = self
            .store
            .snapshot()
            .await
            .agents
            .get(agent.as_str())
            .cloned()
            .unwrap_or_default();
        self.live_block(&dirs, agent, &bucket)
    }

    /// Create or update a profile. Additive agents project the node into the
    /// live file immediately; exclusive agents only rewrite live when the
    /// edited profile is the current one.
    pub async fn upsert(
        &self,
        agent: ProviderKind,
        id: &str,
        input: AgentProviderInput,
    ) -> Result<Value, AppError> {
        validate_provider_id(id)?;
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > MAX_PROVIDER_NAME_CHARS {
            return Err(AppError::InvalidRequest(format!(
                "provider name must be 1-{MAX_PROVIDER_NAME_CHARS} characters"
            )));
        }
        if !input.settings_config.is_object() {
            return Err(AppError::InvalidRequest(
                "settingsConfig must be a JSON object".to_owned(),
            ));
        }
        if serde_json::to_vec(&input.settings_config)?.len() > MAX_PROVIDER_SETTINGS_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "settingsConfig exceeds the {} byte limit",
                MAX_PROVIDER_SETTINGS_BYTES
            )));
        }

        let _guard = self.lock_for(agent).lock().await;
        let existing = self.store.profile(agent, id).await;
        let mut settings_config = input.settings_config.clone();
        restore_masked(
            agent,
            &mut settings_config,
            existing.as_ref().map(|p| &p.settings_config),
        );
        if contains_masked(&settings_config)
            || (agent == ProviderKind::Codex && codex::config_text_has_mask(&settings_config))
        {
            return Err(AppError::InvalidRequest(
                "masked secret has no stored value to restore".to_owned(),
            ));
        }

        let now = now_millis();
        let profile = AgentProviderProfile {
            id: id.to_owned(),
            name: name.to_owned(),
            settings_config,
            website_url: optional_trimmed(input.website_url),
            category: optional_trimmed(input.category),
            notes: input.notes,
            icon: optional_trimmed(input.icon),
            icon_color: optional_trimmed(input.icon_color),
            sort_index: input.sort_index,
            created_at: existing.as_ref().map(|p| p.created_at).unwrap_or(now),
            updated_at: now,
        };

        let dirs = self.dirs();
        let snapshot = self.store.snapshot().await;
        let is_current = snapshot
            .agents
            .get(agent.as_str())
            .and_then(|bucket| bucket.current_provider_id.as_deref())
            == Some(id);

        self.enforce_limit(agent, id).await?;
        // Live first: a conflict there must not leave the store ahead of disk.
        if is_additive(agent) {
            additive_upsert(&dirs, agent, id, &profile.settings_config)?;
        } else if is_current {
            write_live(&dirs, agent, &profile.settings_config, false)?;
        }
        self.store.upsert(agent, profile).await?;
        self.agent_block(&dirs, agent, &self.bucket(agent).await)
    }

    async fn enforce_limit(&self, agent: ProviderKind, id: &str) -> Result<(), AppError> {
        let bucket = self.bucket(agent).await;
        if !bucket.providers.contains_key(id) && bucket.providers.len() >= MAX_PROVIDERS_PER_AGENT {
            return Err(AppError::ResourceExhausted(format!(
                "{} provider limit of {MAX_PROVIDERS_PER_AGENT} reached",
                agent.as_str()
            )));
        }
        Ok(())
    }

    async fn bucket(&self, agent: ProviderKind) -> AgentProviderBucket {
        self.store
            .snapshot()
            .await
            .agents
            .get(agent.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub async fn delete(&self, agent: ProviderKind, id: &str) -> Result<(), AppError> {
        let _guard = self.lock_for(agent).lock().await;
        let dirs = self.dirs();
        if self.store.profile(agent, id).await.is_none() {
            return Err(AppError::NotFound(format!(
                "provider {id} does not exist for {}",
                agent.as_str()
            )));
        }
        if is_additive(agent) {
            additive_remove(&dirs, agent, id)?;
        }
        // Exclusive agents keep the live file as-is; deleting the current
        // profile only clears the pointer, like cc-switch.
        self.store.remove(agent, id).await.map(|_| ())
    }

    /// Switch the agent to a provider. Exclusive mode backfills the live config
    /// into the outgoing profile before overwriting it; additive mode only
    /// moves the native default selection.
    pub async fn activate(
        &self,
        agent: ProviderKind,
        id: &str,
        model_id: Option<String>,
    ) -> Result<Value, AppError> {
        let _guard = self.lock_for(agent).lock().await;
        let dirs = self.dirs();
        let bucket = self.bucket(agent).await;
        let profile = bucket.providers.get(id).cloned().ok_or_else(|| {
            AppError::NotFound(format!(
                "provider {id} does not exist for {}",
                agent.as_str()
            ))
        })?;

        if is_additive(agent) {
            additive_upsert(&dirs, agent, id, &profile.settings_config)?;
            let model = model_id
                .filter(|value| !value.trim().is_empty())
                .or_else(|| additive_first_model(agent, &profile.settings_config))
                .ok_or_else(|| {
                    AppError::InvalidRequest(format!(
                        "provider {id} declares no models; pass modelId"
                    ))
                })?;
            additive_set_default(&dirs, agent, id, &model)?;
            self.store.set_current(agent, Some(id.to_owned())).await?;
        } else {
            let current = bucket.current_provider_id.as_deref();
            if current != Some(id) {
                // Backfill: capture the live config into the outgoing profile so
                // external edits — and credentials like Codex auth.json — stay
                // recoverable before we overwrite the live files.
                let live = read_live(&dirs, agent)?;
                let mut backfilled = false;
                if let (Some(old_id), Some(live)) = (current, live) {
                    if old_id != id {
                        backfilled = self.store.backfill_settings(agent, old_id, live).await?;
                    }
                }
                let remove_auth = agent == ProviderKind::Codex
                    && !codex::has_auth(&profile.settings_config)
                    && backfilled;
                write_live(&dirs, agent, &profile.settings_config, remove_auth)?;
                self.store.set_current(agent, Some(id.to_owned())).await?;
            } else {
                // Re-activating the current provider self-heals the live file.
                write_live(&dirs, agent, &profile.settings_config, false)?;
            }
        }
        self.agent_block(&dirs, agent, &self.bucket(agent).await)
    }

    /// Capture the current live config as a stored profile.
    pub async fn import_live(
        &self,
        agent: ProviderKind,
        input: ImportLiveInput,
    ) -> Result<Value, AppError> {
        let id = input.id.trim();
        validate_provider_id(id)?;
        let _guard = self.lock_for(agent).lock().await;
        let dirs = self.dirs();
        if self.store.profile(agent, id).await.is_some() {
            return Err(AppError::Conflict(format!(
                "provider {id} already exists for {}",
                agent.as_str()
            )));
        }

        let (settings_config, is_live) = if is_additive(agent) {
            let nodes = additive_nodes(&dirs, agent)?;
            let node = nodes.get(id).cloned().ok_or_else(|| {
                AppError::NotFound(format!(
                    "no {id} provider in the live {} config",
                    agent.as_str()
                ))
            })?;
            let selected = additive_selection(&dirs, agent)?
                .get("providerId")
                .and_then(Value::as_str)
                == Some(id);
            (node, selected)
        } else {
            let live = read_live(&dirs, agent)?.ok_or_else(|| {
                AppError::NotFound(format!("no live {} config to import", agent.as_str()))
            })?;
            (live, true)
        };

        let name = input
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_owned());
        if name.chars().count() > MAX_PROVIDER_NAME_CHARS {
            return Err(AppError::InvalidRequest(format!(
                "provider name must be 1-{MAX_PROVIDER_NAME_CHARS} characters"
            )));
        }
        let now = now_millis();
        self.enforce_limit(agent, id).await?;
        self.store
            .upsert(
                agent,
                AgentProviderProfile {
                    id: id.to_owned(),
                    name,
                    settings_config,
                    website_url: None,
                    category: None,
                    notes: None,
                    icon: None,
                    icon_color: None,
                    sort_index: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .await?;
        if is_live {
            self.store.set_current(agent, Some(id.to_owned())).await?;
        }
        self.agent_block(&dirs, agent, &self.bucket(agent).await)
    }

    /// Fetch the model catalog through the stored credentials (server-side).
    pub async fn models(&self, agent: ProviderKind, id: &str) -> Result<Value, AppError> {
        let profile = self.store.profile(agent, id).await.ok_or_else(|| {
            AppError::NotFound(format!(
                "provider {id} does not exist for {}",
                agent.as_str()
            ))
        })?;
        model_fetch::fetch_models(agent, &profile.settings_config).await
    }

    /// Fetch the model catalog for a proposed settingsConfig — the provider
    /// editor posts its current form before saving. Masked secrets resolve
    /// against the stored profile or the live additive node of the same id.
    pub async fn preview_models(
        &self,
        agent: ProviderKind,
        id: &str,
        settings_config: Value,
    ) -> Result<Value, AppError> {
        if !settings_config.is_object() {
            return Err(AppError::InvalidRequest(
                "settingsConfig must be a JSON object".to_owned(),
            ));
        }
        if serde_json::to_vec(&settings_config)?.len() > MAX_PROVIDER_SETTINGS_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "settingsConfig exceeds the {} byte limit",
                MAX_PROVIDER_SETTINGS_BYTES
            )));
        }
        let existing = match self.store.profile(agent, id).await {
            Some(profile) => Some(profile.settings_config),
            None if is_additive(agent) => additive_nodes(&self.dirs(), agent)?.get(id).cloned(),
            None => None,
        };
        let mut settings_config = settings_config;
        restore_masked(agent, &mut settings_config, existing.as_ref());
        model_fetch::fetch_models(agent, &settings_config).await
    }
}

// ---------- per-agent live dispatch ----------

fn read_live(dirs: &AgentDirs, agent: ProviderKind) -> Result<Option<Value>, AppError> {
    match agent {
        ProviderKind::ClaudeCode => claude::read_live(dirs),
        ProviderKind::Codex => codex::read_live(dirs),
        _ => Err(AppError::Unsupported(format!(
            "{} is not an exclusive-mode agent",
            agent.as_str()
        ))),
    }
}

fn write_live(
    dirs: &AgentDirs,
    agent: ProviderKind,
    settings: &Value,
    remove_auth: bool,
) -> Result<(), AppError> {
    match agent {
        ProviderKind::ClaudeCode => claude::write_live(dirs, settings),
        ProviderKind::Codex => codex::write_live(dirs, settings, remove_auth),
        _ => Err(AppError::Unsupported(format!(
            "{} is not an exclusive-mode agent",
            agent.as_str()
        ))),
    }
}

fn additive_nodes(dirs: &AgentDirs, agent: ProviderKind) -> Result<Map<String, Value>, AppError> {
    match agent {
        ProviderKind::Opencode => opencode::provider_nodes(dirs),
        ProviderKind::Pi => pi::provider_nodes(dirs),
        _ => Err(AppError::Unsupported(format!(
            "{} is not an additive-mode agent",
            agent.as_str()
        ))),
    }
}

fn additive_upsert(
    dirs: &AgentDirs,
    agent: ProviderKind,
    id: &str,
    node: &Value,
) -> Result<(), AppError> {
    match agent {
        ProviderKind::Opencode => opencode::upsert_provider(dirs, id, node),
        ProviderKind::Pi => pi::upsert_provider(dirs, id, node),
        _ => Err(AppError::Unsupported(format!(
            "{} is not an additive-mode agent",
            agent.as_str()
        ))),
    }
}

fn additive_remove(dirs: &AgentDirs, agent: ProviderKind, id: &str) -> Result<(), AppError> {
    match agent {
        ProviderKind::Opencode => opencode::remove_provider(dirs, id),
        ProviderKind::Pi => pi::remove_provider(dirs, id),
        _ => Err(AppError::Unsupported(format!(
            "{} is not an additive-mode agent",
            agent.as_str()
        ))),
    }
}

fn additive_set_default(
    dirs: &AgentDirs,
    agent: ProviderKind,
    id: &str,
    model_id: &str,
) -> Result<(), AppError> {
    match agent {
        ProviderKind::Opencode => opencode::set_default(dirs, id, model_id),
        ProviderKind::Pi => pi::set_default(dirs, id, model_id),
        _ => Err(AppError::Unsupported(format!(
            "{} is not an additive-mode agent",
            agent.as_str()
        ))),
    }
}

/// `{providerId, modelId}` of the native default selection.
fn additive_selection(dirs: &AgentDirs, agent: ProviderKind) -> Result<Value, AppError> {
    match agent {
        ProviderKind::Opencode => {
            let selection = opencode::default_selection(dirs)?;
            Ok(match selection {
                Some((provider, model)) => {
                    json!({ "providerId": provider, "modelId": model })
                }
                None => Value::Null,
            })
        }
        ProviderKind::Pi => {
            let (provider, model) = pi::default_selection(dirs)?;
            Ok(match provider {
                Some(provider) => json!({ "providerId": provider, "modelId": model }),
                None => Value::Null,
            })
        }
        _ => Err(AppError::Unsupported(format!(
            "{} is not an additive-mode agent",
            agent.as_str()
        ))),
    }
}

fn additive_first_model(agent: ProviderKind, node: &Value) -> Option<String> {
    match agent {
        ProviderKind::Opencode => opencode::first_model_id(node),
        ProviderKind::Pi => pi::first_model_id(node),
        _ => None,
    }
}

/// Whether the exclusive live config still matches the current profile.
fn live_matches(agent: ProviderKind, live: &Value, settings: &Value) -> bool {
    match agent {
        ProviderKind::ClaudeCode => live == settings,
        ProviderKind::Codex => {
            let auth_eq = live.get("auth") == settings.get("auth")
                || (live.get("auth").is_some_and(Value::is_null)
                    && settings.get("auth").is_none_or(Value::is_null));
            let toml_eq = match (
                live.get("config").and_then(Value::as_str),
                settings.get("config").and_then(Value::as_str),
            ) {
                (Some(a), Some(b)) => match (a.parse::<toml::Value>(), b.parse::<toml::Value>()) {
                    (Ok(a), Ok(b)) => a == b,
                    _ => a.trim() == b.trim(),
                },
                (a, b) => a == b,
            };
            auth_eq && toml_eq
        }
        _ => false,
    }
}

// ---------- secret masking ----------

/// Keys whose *values* are credentials. `env_key`-style fields hold variable
/// names rather than values, so a bare `key` suffix is deliberately excluded.
fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
        "bearer",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

pub(crate) fn mask_json_secrets(mut value: Value) -> Value {
    mask_json_secrets_in_place(&mut value);
    value
}

fn mask_json_secrets_in_place(item: &mut Value) {
    match item {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_sensitive_key(key) && value.is_string() {
                    *value = json!(MASKED_SECRET);
                } else {
                    mask_json_secrets_in_place(value);
                }
            }
        }
        Value::Array(items) => {
            for value in items {
                mask_json_secrets_in_place(value);
            }
        }
        _ => {}
    }
}

fn mask_agent_settings(agent: ProviderKind, settings: &Value) -> Value {
    match agent {
        ProviderKind::Codex => codex::masked_settings(settings),
        _ => mask_json_secrets(settings.clone()),
    }
}

fn masked_profile(agent: ProviderKind, profile: &AgentProviderProfile) -> Value {
    let mut value = serde_json::to_value(profile).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        if let Some(settings) = object.get_mut("settingsConfig") {
            *settings = mask_agent_settings(agent, settings);
        }
    }
    value
}

/// A write that still carries the mask sentinel is rejected unless the value
/// was restored from the previously stored profile.
fn contains_masked(value: &Value) -> bool {
    match value {
        Value::String(text) => text == MASKED_SECRET,
        Value::Array(items) => items.iter().any(contains_masked),
        Value::Object(map) => map.values().any(contains_masked),
        _ => false,
    }
}

fn restore_masked(agent: ProviderKind, new: &mut Value, old: Option<&Value>) {
    if let (Some(new_config), Some(old_config)) = (
        new.get("config").and_then(Value::as_str),
        old.and_then(|old| old.get("config"))
            .and_then(Value::as_str),
    ) {
        if agent == ProviderKind::Codex && new_config.contains(MASKED_SECRET) {
            let restored = codex::restore_config_text(new_config, old_config);
            if let Some(object) = new.as_object_mut() {
                object.insert("config".to_owned(), json!(restored));
            }
        }
    }
    if let Some(old) = old {
        restore_masked_in_place(new, old);
    }
}

fn restore_masked_in_place(new: &mut Value, old: &Value) {
    match (new, old) {
        (Value::Object(new_map), Value::Object(old_map)) => {
            for (key, value) in new_map.iter_mut() {
                if let Some(old_value) = old_map.get(key) {
                    restore_masked_in_place(value, old_value);
                }
            }
        }
        (Value::Array(new_items), Value::Array(old_items)) => {
            for (value, old_value) in new_items.iter_mut().zip(old_items.iter()) {
                restore_masked_in_place(value, old_value);
            }
        }
        (Value::String(new_text), Value::String(old_text)) if new_text == MASKED_SECRET => {
            *new_text = old_text.clone();
        }
        _ => {}
    }
}

// ---------- validation / helpers ----------

fn validate_provider_id(id: &str) -> Result<(), AppError> {
    let valid = !id.is_empty()
        && id.chars().count() <= MAX_PROVIDER_ID_CHARS
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ':'));
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "provider id must be 1-{MAX_PROVIDER_ID_CHARS} characters of [A-Za-z0-9._-:]"
        )))
    }
}

fn optional_trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "todex-agent-providers-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dirs_in(root: &Path) -> AgentDirs {
        AgentDirs {
            codex_home: root.join("codex"),
            claude_dir: root.join("claude"),
            pi_dir: root.join("pi"),
            opencode_dir: root.join("opencode"),
        }
    }

    async fn service_in(root: &Path) -> AgentProviderService {
        AgentProviderService::with_dirs(root.join("data"), Some(dirs_in(root)))
            .await
            .expect("service")
    }

    fn input(name: &str, settings_config: Value) -> AgentProviderInput {
        AgentProviderInput {
            name: name.to_owned(),
            settings_config,
            website_url: None,
            category: None,
            notes: None,
            icon: None,
            icon_color: None,
            sort_index: None,
        }
    }

    fn claude_settings(base: &str, token: &str) -> Value {
        json!({
            "env": {
                "ANTHROPIC_BASE_URL": base,
                "ANTHROPIC_AUTH_TOKEN": token,
                "ANTHROPIC_MODEL": "claude-opus-4-1"
            },
            "permissions": {"allow": ["Bash"]}
        })
    }

    fn codex_settings(key: Option<&str>, base: &str) -> Value {
        let auth = key.map(|key| json!({"OPENAI_API_KEY": key}));
        json!({
            "auth": auth,
            "config": format!(
                "model_provider = \"custom\"\nmodel = \"gpt-5\"\n\n[model_providers.custom]\nname = \"Custom\"\nbase_url = \"{base}\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
            )
        })
    }

    fn providers_of(block: &Value) -> &Vec<Value> {
        block["providers"].as_array().expect("providers array")
    }

    #[tokio::test]
    async fn claude_switch_backfills_live_config_into_outgoing_profile() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());

        service
            .upsert(
                ProviderKind::ClaudeCode,
                "a",
                input("A", claude_settings("https://a.example.com", "key-a")),
            )
            .await
            .unwrap();
        service
            .upsert(
                ProviderKind::ClaudeCode,
                "b",
                input("B", claude_settings("https://b.example.com", "key-b")),
            )
            .await
            .unwrap();

        // Non-current profile edits never touch the live file.
        assert!(!dirs.claude_dir.join("settings.json").exists());

        service
            .activate(ProviderKind::ClaudeCode, "a", None)
            .await
            .unwrap();
        let live = claude::read_live(&dirs).unwrap().unwrap();
        assert_eq!(live["env"]["ANTHROPIC_BASE_URL"], "https://a.example.com");

        // An external edit to the live file is captured on switch-away.
        let mut live = live;
        live["env"]["ANTHROPIC_MODEL"] = json!("claude-sonnet-4-5");
        claude::write_live(&dirs, &live).unwrap();

        service
            .activate(ProviderKind::ClaudeCode, "b", None)
            .await
            .unwrap();
        let stored_a = service
            .store
            .profile(ProviderKind::ClaudeCode, "a")
            .await
            .unwrap();
        assert_eq!(
            stored_a.settings_config["env"]["ANTHROPIC_MODEL"],
            "claude-sonnet-4-5"
        );
        let live = claude::read_live(&dirs).unwrap().unwrap();
        assert_eq!(live["env"]["ANTHROPIC_BASE_URL"], "https://b.example.com");
    }

    #[tokio::test]
    async fn codex_switch_removes_auth_json_only_after_backfill() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());

        // A carries an auth.json; B is config-only.
        service
            .upsert(
                ProviderKind::Codex,
                "a",
                input(
                    "A",
                    codex_settings(Some("sk-a"), "https://a.example.com/v1"),
                ),
            )
            .await
            .unwrap();
        service
            .upsert(
                ProviderKind::Codex,
                "b",
                input("B", codex_settings(None, "https://b.example.com/v1")),
            )
            .await
            .unwrap();

        service
            .activate(ProviderKind::Codex, "a", None)
            .await
            .unwrap();
        assert!(dirs.codex_home.join("auth.json").exists());
        let toml = std::fs::read_to_string(dirs.codex_home.join("config.toml")).unwrap();
        assert!(toml.contains("a.example.com"));

        // Switching to B deletes auth.json, but only after A's blob captured it.
        service
            .activate(ProviderKind::Codex, "b", None)
            .await
            .unwrap();
        assert!(!dirs.codex_home.join("auth.json").exists());
        let stored_a = service
            .store
            .profile(ProviderKind::Codex, "a")
            .await
            .unwrap();
        assert_eq!(stored_a.settings_config["auth"]["OPENAI_API_KEY"], "sk-a");
    }

    #[tokio::test]
    async fn codex_activate_without_backfill_preserves_auth_json() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());

        // A live auth.json that no profile owns must not be destroyed.
        std::fs::create_dir_all(&dirs.codex_home).unwrap();
        std::fs::write(
            dirs.codex_home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"chatgpt-session"}"#,
        )
        .unwrap();
        service
            .upsert(
                ProviderKind::Codex,
                "b",
                input("B", codex_settings(None, "https://b.example.com/v1")),
            )
            .await
            .unwrap();
        service
            .activate(ProviderKind::Codex, "b", None)
            .await
            .unwrap();
        let auth = std::fs::read_to_string(dirs.codex_home.join("auth.json")).unwrap();
        assert!(auth.contains("chatgpt-session"));
    }

    #[tokio::test]
    async fn opencode_additive_sync_and_selection() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());
        let config = dirs.opencode_dir.join("opencode.json");

        let node = json!({
            "name": "Acme",
            "options": {"baseURL": "https://acme.example.com/v1", "apiKey": "k-acme"},
            "models": {"m-pro": {"name": "Pro"}, "m-mini": {"name": "Mini"}}
        });
        service
            .upsert(ProviderKind::Opencode, "acme", input("Acme", node.clone()))
            .await
            .unwrap();
        // Additive providers are written to the live file on save.
        let saved: Value = json5::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(saved["provider"]["acme"]["options"]["apiKey"], "k-acme");

        service
            .activate(ProviderKind::Opencode, "acme", Some("m-mini".to_owned()))
            .await
            .unwrap();
        let saved: Value = json5::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(saved["model"], "acme/m-mini");

        // External provider nodes stay put and surface as unmanaged.
        std::fs::write(
            &config,
            r#"{"provider":{"external":{"options":{"apiKey":"x"}}},"model":"acme/m-mini"}"#,
        )
        .unwrap();
        let block = service
            .snapshot(Some(ProviderKind::Opencode))
            .await
            .unwrap();
        let unmanaged = block["agents"]["opencode"]["live"]["unmanagedProviders"]
            .as_array()
            .unwrap();
        assert_eq!(unmanaged, &vec![json!("external")]);

        service
            .delete(ProviderKind::Opencode, "acme")
            .await
            .unwrap();
        let saved: Value = json5::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(saved["provider"].get("acme").is_none());
        assert!(saved["provider"].get("external").is_some());
        assert!(saved.get("model").is_none());
    }

    #[tokio::test]
    async fn pi_additive_sync_defaults_and_cleanup() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());

        let node = json!({
            "name": "Pi Co",
            "baseUrl": "https://pi.example.com/v1",
            "api": "openai-completions",
            "apiKey": "k-pi",
            "models": [{"id": "pi-pro"}, {"id": "pi-fast"}]
        });
        service
            .upsert(ProviderKind::Pi, "pico", input("Pi Co", node))
            .await
            .unwrap();
        // First declared model becomes the default when modelId is omitted.
        service
            .activate(ProviderKind::Pi, "pico", None)
            .await
            .unwrap();
        let settings: Value =
            json5::from_str(&std::fs::read_to_string(dirs.pi_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["defaultProvider"], "pico");
        assert_eq!(settings["defaultModel"], "pi-pro");

        service.delete(ProviderKind::Pi, "pico").await.unwrap();
        let models: Value =
            json5::from_str(&std::fs::read_to_string(dirs.pi_dir.join("models.json")).unwrap())
                .unwrap();
        assert!(models["providers"].get("pico").is_none());
        let settings: Value =
            json5::from_str(&std::fs::read_to_string(dirs.pi_dir.join("settings.json")).unwrap())
                .unwrap();
        assert!(settings.get("defaultProvider").is_none());
    }

    #[tokio::test]
    async fn masked_roundtrip_keeps_stored_secret() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;

        service
            .upsert(
                ProviderKind::ClaudeCode,
                "a",
                input("A", claude_settings("https://a.example.com", "real-key")),
            )
            .await
            .unwrap();
        let block = service
            .snapshot(Some(ProviderKind::ClaudeCode))
            .await
            .unwrap();
        let listed = &providers_of(&block["agents"]["claude-code"])[0];
        assert_eq!(
            listed["settingsConfig"]["env"]["ANTHROPIC_AUTH_TOKEN"],
            MASKED_SECRET
        );

        // Writing the masked value back restores the stored secret.
        let mut update = listed["settingsConfig"].clone();
        update["env"]["ANTHROPIC_BASE_URL"] = json!("https://a2.example.com");
        service
            .upsert(ProviderKind::ClaudeCode, "a", input("A2", update))
            .await
            .unwrap();
        let stored = service
            .store
            .profile(ProviderKind::ClaudeCode, "a")
            .await
            .unwrap();
        assert_eq!(
            stored.settings_config["env"]["ANTHROPIC_AUTH_TOKEN"],
            "real-key"
        );
        assert_eq!(
            stored.settings_config["env"]["ANTHROPIC_BASE_URL"],
            "https://a2.example.com"
        );

        // A masked secret on a brand-new profile has nothing to restore to.
        let error = service
            .upsert(
                ProviderKind::ClaudeCode,
                "new",
                input(
                    "New",
                    claude_settings("https://n.example.com", MASKED_SECRET),
                ),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn import_live_captures_exclusive_and_adopts_additive() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());

        // Exclusive: whatever is live becomes the profile and the current one.
        claude::write_live(
            &dirs,
            &claude_settings("https://live.example.com", "live-key"),
        )
        .unwrap();
        service
            .import_live(
                ProviderKind::ClaudeCode,
                ImportLiveInput {
                    id: "imported".to_owned(),
                    name: Some("Imported".to_owned()),
                },
            )
            .await
            .unwrap();
        let stored = service
            .store
            .profile(ProviderKind::ClaudeCode, "imported")
            .await
            .unwrap();
        assert_eq!(
            stored.settings_config["env"]["ANTHROPIC_BASE_URL"],
            "https://live.example.com"
        );
        let block = service
            .snapshot(Some(ProviderKind::ClaudeCode))
            .await
            .unwrap();
        assert_eq!(
            block["agents"]["claude-code"]["currentProviderId"],
            "imported"
        );
        assert_eq!(
            block["agents"]["claude-code"]["live"]["matchesCurrent"],
            true
        );

        // Additive: adopt an unmanaged live node by id.
        opencode::upsert_provider(
            &dirs,
            "external",
            &json!({"options": {"apiKey": "x"}, "models": {"m": {}}}),
        )
        .unwrap();
        service
            .import_live(
                ProviderKind::Opencode,
                ImportLiveInput {
                    id: "external".to_owned(),
                    name: None,
                },
            )
            .await
            .unwrap();
        let block = service
            .snapshot(Some(ProviderKind::Opencode))
            .await
            .unwrap();
        assert!(block["agents"]["opencode"]["live"]["unmanagedProviders"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn pi_preview_models_restores_live_node_secret() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let dirs = dirs_in(temp.path());

        // Stub catalog endpoint returning one model; captures the request.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let read = socket.read(&mut buf).await.unwrap();
            let _ = tx.send(String::from_utf8_lossy(&buf[..read]).to_string());
            let body = r#"{"data":[{"id":"m-live","name":"Live"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        // The unmanaged live node holds the real key; the editor posts the mask.
        pi::upsert_provider(
            &dirs,
            "pico",
            &json!({
                "baseUrl": format!("http://{addr}/v1"),
                "api": "openai-completions",
                "apiKey": "k-live",
                "models": []
            }),
        )
        .unwrap();

        let preview = json!({
            "baseUrl": format!("http://{addr}/v1"),
            "api": "openai-completions",
            "apiKey": MASKED_SECRET,
            "models": []
        });
        let result = service
            .preview_models(ProviderKind::Pi, "pico", preview)
            .await
            .unwrap();
        assert_eq!(result["models"][0]["id"], "m-live");

        let request = rx.await.unwrap();
        assert!(request.starts_with("GET /v1/models "));
        assert!(request.contains("authorization: Bearer k-live"));
    }

    #[tokio::test]
    async fn preview_models_rejects_non_object_config() {
        let temp = TestRoot::new();
        let service = service_in(temp.path()).await;
        let error = service
            .preview_models(ProviderKind::Pi, "pico", json!("nope"))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::InvalidRequest(_)));
    }

    #[test]
    fn codex_toml_masking_preserves_env_key_names() {
        let settings = json!({
            "auth": {"OPENAI_API_KEY": "sk-live"},
            "config": "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://x/v1\"\nenv_key = \"MY_VAR\"\nexperimental_bearer_token = \"tok-secret\"\n\n[model_providers.custom.http_headers]\nX-Key = \"hdr-secret\"\n\n[model_providers.custom.env_http_headers]\nX-Name = \"ENV_VAR_NAME\"\n"
        });
        let masked = codex::masked_settings(&settings);
        let config = masked["config"].as_str().unwrap();
        assert!(config.contains("env_key = \"MY_VAR\""));
        assert!(config.contains("X-Name = \"ENV_VAR_NAME\""));
        assert!(!config.contains("tok-secret"));
        assert!(!config.contains("hdr-secret"));
        assert_eq!(masked["auth"]["OPENAI_API_KEY"], MASKED_SECRET);

        // The masked text restores real values against the stored profile.
        let restored = codex::restore_config_text(config, settings["config"].as_str().unwrap());
        assert!(restored.contains("tok-secret"));
        assert!(restored.contains("hdr-secret"));
        assert!(restored.contains("env_key = \"MY_VAR\""));
    }

    #[test]
    fn sensitive_key_detection() {
        assert!(is_sensitive_key("OPENAI_API_KEY"));
        assert!(is_sensitive_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(is_sensitive_key("apiKey"));
        assert!(is_sensitive_key("experimental_bearer_token"));
        assert!(!is_sensitive_key("env_key"));
        assert!(!is_sensitive_key("baseUrl"));
    }
}
