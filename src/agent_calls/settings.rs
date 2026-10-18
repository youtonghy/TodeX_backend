//! `$DATA_DIR/agent-calling.json`: which agents and models agent calls may
//! use (both entries: the external API and the `todex_agents` MCP server),
//! and the MCP entry's switch and approval policy. The external API
//! listener itself stays in `config.toml` `[api]`; this store also tracks
//! the listener running in this process and the `[api]` values saved since,
//! so clients can tell that a restart is pending.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};

use crate::api_keys::ApprovalPolicy;
use crate::config::{ApiConfig, Config};
use crate::conversation::ProviderKind;
use crate::error::AppError;
use crate::provider::types::ProviderModelDescriptor;
use crate::secure_fs;

const SETTINGS_FILE: &str = "agent-calling.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_MODELS_PER_AGENT: usize = 256;
const MAX_MODEL_ID_CHARS: usize = 200;

/// Agents and models agent calls may use. Every caller's own scope (an API
/// key's, for instance) applies on top of it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentScope {
    /// `None` allows every agent; a list allows only those (may be empty).
    #[serde(default)]
    pub agents: Option<Vec<ProviderKind>>,
    /// Allowed model ids per agent; an agent absent from the map may use
    /// all of its models. Lists are never empty.
    #[serde(default)]
    pub models: BTreeMap<ProviderKind, Vec<String>>,
}

impl AgentScope {
    pub(crate) fn allows_agent(&self, agent: ProviderKind) -> bool {
        self.agents
            .as_ref()
            .is_none_or(|agents| agents.contains(&agent))
    }

    /// The models `agent` is limited to, or `None` when all are allowed.
    pub(crate) fn allowed_models(&self, agent: ProviderKind) -> Option<&[String]> {
        self.models.get(&agent).map(Vec::as_slice)
    }

    pub(crate) fn allows_model(&self, agent: ProviderKind, model: &str) -> bool {
        self.allowed_models(agent)
            .is_none_or(|models| models.iter().any(|allowed| allowed == model))
    }

    /// At least one agent may be called.
    pub(crate) fn any_agent_allowed(&self) -> bool {
        self.agents.as_ref().is_none_or(|agents| !agents.is_empty())
    }

    /// The model a turn of `agent` runs: the requested one if allowed;
    /// without one, the provider default when all models are allowed
    /// (`None`), else the catalog default if allowed, else the first
    /// allowed model. `catalog` is the provider's live model list, when it
    /// could be read.
    pub(crate) fn resolve_model(
        &self,
        agent: ProviderKind,
        requested: Option<&str>,
        catalog: Option<&[ProviderModelDescriptor]>,
    ) -> Result<Option<String>, AppError> {
        if let Some(model) = requested {
            if !self.allows_model(agent, model) {
                return Err(AppError::Unauthorized(format!(
                    "model {model} of {} is not allowed for agent calling",
                    agent.as_str()
                )));
            }
            return Ok(Some(model.to_owned()));
        }
        let Some(allowed) = self.allowed_models(agent) else {
            return Ok(None);
        };
        let default = catalog
            .and_then(|models| models.iter().find(|model| model.is_default))
            .map(|model| model.id.as_str())
            .filter(|id| allowed.iter().any(|allowed| allowed == id));
        Ok(default
            .or(allowed.first().map(String::as_str))
            .map(str::to_owned))
    }

    fn validate(&self) -> Result<(), AppError> {
        for (agent, models) in &self.models {
            if models.is_empty() {
                return Err(AppError::InvalidRequest(format!(
                    "scope.models.{} must list at least one model; omit the agent to allow all",
                    agent.as_str()
                )));
            }
            if models.len() > MAX_MODELS_PER_AGENT {
                return Err(AppError::InvalidRequest(format!(
                    "scope.models.{} may list at most {MAX_MODELS_PER_AGENT} models",
                    agent.as_str()
                )));
            }
            if models.iter().any(|model| {
                model.trim().is_empty()
                    || model.trim() != model
                    || model.chars().count() > MAX_MODEL_ID_CHARS
            }) {
                return Err(AppError::InvalidRequest(format!(
                    "scope.models.{} holds an empty or malformed model id",
                    agent.as_str()
                )));
            }
        }
        Ok(())
    }

    fn normalized(mut self) -> Self {
        if let Some(agents) = &mut self.agents {
            agents.sort();
            agents.dedup();
        }
        for models in self.models.values_mut() {
            let mut seen = std::collections::HashSet::new();
            models.retain(|model| seen.insert(model.clone()));
        }
        self
    }
}

/// The `todex_agents` MCP entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct McpSettings {
    /// Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// How permission requests of agent-call child conversations are
    /// answered.
    #[serde(default)]
    pub approval: ApprovalPolicy,
}

/// `agent-calling.json`. A missing file means these defaults: every agent
/// and model allowed (so existing external API users keep working), MCP
/// off. Clients' updates are strict ([`AgentCallingUpdate`]); the file is
/// read leniently ([`StoredSettings`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentCallingSettings {
    #[serde(default)]
    pub scope: AgentScope,
    #[serde(default)]
    pub mcp: McpSettings,
}

impl AgentCallingSettings {
    /// The MCP server would be injected now.
    pub(crate) fn mcp_injected(&self) -> bool {
        self.mcp.enabled && self.scope.any_agent_allowed()
    }
}

/// `PUT /v2/agent-calling`: absent fields keep their value.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AgentCallingUpdate {
    #[serde(default)]
    pub scope: Option<AgentScope>,
    #[serde(default)]
    pub mcp: Option<McpUpdate>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct McpUpdate {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub approval: Option<ApprovalPolicy>,
}

/// `agent-calling.json` as read at startup: unknown fields are ignored and
/// unknown agent ids dropped (a newer TodeX may have written them), so only
/// a broken file is refused.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSettings {
    #[serde(default)]
    scope: StoredScope,
    #[serde(default)]
    mcp: StoredMcp,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredScope {
    #[serde(default)]
    agents: Option<Vec<String>>,
    #[serde(default)]
    models: BTreeMap<String, Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredMcp {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    approval: Option<String>,
}

/// An agent id as the file spells it (`"codex"`, `"claude-code"`), or
/// `None` for one this TodeX does not know.
fn stored_agent(id: &str) -> Option<ProviderKind> {
    serde_json::from_value(serde_json::Value::String(id.to_owned())).ok()
}

impl StoredSettings {
    /// The settings the file holds, or why they cannot be used (never
    /// quoting the file).
    fn parse(bytes: &[u8]) -> Result<AgentCallingSettings, String> {
        let stored: Self = serde_json::from_slice(bytes).map_err(|error| {
            format!(
                "not valid agent calling settings ({:?} error at line {}, column {})",
                error.classify(),
                error.line(),
                error.column()
            )
        })?;
        let approval = match stored.mcp.approval {
            None => ApprovalPolicy::default(),
            // An approval this TodeX does not know: the default (asking the
            // user) rather than a guess.
            Some(approval) => {
                serde_json::from_value(serde_json::Value::String(approval)).unwrap_or_default()
            }
        };
        let scope = AgentScope {
            agents: stored.scope.agents.map(|agents| {
                agents
                    .iter()
                    .filter_map(|agent| stored_agent(agent))
                    .collect()
            }),
            models: stored
                .scope
                .models
                .into_iter()
                .filter_map(|(agent, models)| Some((stored_agent(&agent)?, models)))
                .collect(),
        };
        scope
            .validate()
            .map_err(|error| format!("invalid scope: {error}"))?;
        Ok(AgentCallingSettings {
            scope: scope.normalized(),
            mcp: McpSettings {
                enabled: stored.mcp.enabled,
                approval,
            },
        })
    }
}

/// The `[api]` listener: what runs in this process and what is saved.
#[derive(Clone, Debug)]
struct ListenerState {
    /// The address the running listener is bound to.
    running: Option<SocketAddr>,
    /// `[api]` as this process started with it.
    started: (bool, u16),
    /// `[api]` as saved since (equal to `started` until a client saves).
    saved: (bool, u16),
}

/// What clients see of the external API listener.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiListenerView {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub listening: bool,
    pub base_url: Option<String>,
    pub pending_restart: bool,
}

/// Daemon-wide agent calling settings.
#[derive(Clone)]
pub(crate) struct AgentCallingStore {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    data_dir: PathBuf,
    settings: RwLock<AgentCallingSettings>,
    listener: RwLock<ListenerState>,
    /// Serializes writes of `agent-calling.json` and `config.toml`.
    write: tokio::sync::Mutex<()>,
}

impl AgentCallingStore {
    /// Reads `agent-calling.json`; a missing file is the defaults (and is
    /// not written until a client saves). Settings never keep the daemon
    /// from starting: a file that cannot be used (unreadable, too large,
    /// malformed or invalid) is logged without its contents and the
    /// defaults apply, with the MCP entry off. A malformed or invalid file
    /// is moved aside to `agent-calling.json.invalid-<unix time>` (kept for
    /// inspection, never overwritten by the next save), so the defaults are
    /// also what the next start reads until a client saves.
    pub(crate) async fn load(data_dir: &Path, api: &ApiConfig) -> Self {
        let path = data_dir.join(SETTINGS_FILE);
        let settings = match read_settings(&path).await {
            Ok(settings) => settings,
            Err(Unusable { reason, set_aside }) => {
                tracing::warn!(
                    path = %path.display(),
                    reason,
                    "ignoring the agent calling settings; using the defaults with agent calling over MCP off"
                );
                if set_aside {
                    set_aside_invalid(&path).await;
                }
                AgentCallingSettings::default()
            }
        };
        let started = (api.enabled, api.port);
        Self {
            inner: Arc::new(Inner {
                path,
                data_dir: data_dir.to_owned(),
                settings: RwLock::new(settings),
                listener: RwLock::new(ListenerState {
                    running: None,
                    started,
                    saved: started,
                }),
                write: tokio::sync::Mutex::new(()),
            }),
        }
    }

    pub(crate) fn settings(&self) -> AgentCallingSettings {
        self.inner
            .settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn scope(&self) -> AgentScope {
        self.settings().scope
    }

    /// Validates, persists (owner-only, atomic) and applies `update`.
    pub(crate) async fn update(
        &self,
        update: AgentCallingUpdate,
    ) -> Result<AgentCallingSettings, AppError> {
        let _write = self.inner.write.lock().await;
        let mut next = self.settings();
        if let Some(scope) = update.scope {
            scope.validate()?;
            next.scope = scope.normalized();
        }
        if let Some(mcp) = update.mcp {
            next.mcp.enabled = mcp.enabled.unwrap_or(next.mcp.enabled);
            next.mcp.approval = mcp.approval.unwrap_or(next.mcp.approval);
        }
        let bytes = serde_json::to_vec_pretty(&next)?;
        let path = self.inner.path.clone();
        tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&path, &bytes))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        *self
            .inner
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next.clone();
        Ok(next)
    }

    /// Records the external API listener this process serves.
    pub(crate) fn set_running_api_listener(&self, addr: Option<SocketAddr>) {
        self.inner
            .listener
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .running = addr;
    }

    pub(crate) fn api_listener(&self, config: &Config) -> ApiListenerView {
        let listener = self
            .inner
            .listener
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let (enabled, port) = listener.saved;
        let pending_restart =
            enabled != listener.running.is_some() || (enabled && port != listener.started.1);
        ApiListenerView {
            enabled,
            host: config.api.host.clone(),
            port,
            listening: listener.running.is_some(),
            base_url: listener
                .running
                .map(|addr| api_base_url(&config.api.host, addr.port())),
            pending_restart,
        }
    }

    /// Saves `[api]` `enabled`/`port` in `config.toml`; the listener changes
    /// on the next daemon start. The port must be 1..=65535 and differ from
    /// the main listener's, and an enabled listener must be one the daemon
    /// starts with ([`Config::ensure_api_listener_is_valid`]). Only the
    /// fields given are written: the other one may come from a command-line
    /// or environment override (`--enable-api`, `TODEX_AGENTD_API_*`),
    /// which must not end up in `config.toml`.
    pub(crate) async fn save_api_listener(
        &self,
        config: &Config,
        enabled: Option<bool>,
        port: Option<u64>,
    ) -> Result<ApiListenerView, AppError> {
        let _write = self.inner.write.lock().await;
        let (saved_enabled, saved_port) = self
            .inner
            .listener
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .saved;
        let requested_port = match port {
            None => None,
            Some(port) => {
                let port = u16::try_from(port)
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| {
                        AppError::InvalidRequest("port must be between 1 and 65535".to_owned())
                    })?;
                if port == config.port {
                    return Err(AppError::InvalidRequest(format!(
                        "API port {port} is the main listener's port; choose another port"
                    )));
                }
                Some(port)
            }
        };
        let port = requested_port.unwrap_or(saved_port);
        let requested_enabled = enabled;
        let enabled = enabled.unwrap_or(saved_enabled);
        let mut candidate = config.clone();
        candidate.api.enabled = enabled;
        candidate.api.port = port;
        candidate
            .ensure_api_listener_is_valid()
            .map_err(|error| AppError::InvalidRequest(error.to_string()))?;
        let data_dir = self.inner.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            Config::save_api_settings(data_dir, requested_enabled, requested_port)
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?
        .map_err(AppError::Anyhow)?;
        self.inner
            .listener
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .saved = (enabled, port);
        Ok(self.api_listener(config))
    }
}

/// Why `agent-calling.json` cannot be used; `set_aside` when its contents
/// are the problem (rather than reading it).
struct Unusable {
    reason: String,
    set_aside: bool,
}

/// The settings `path` holds; the defaults when it does not exist.
async fn read_settings(path: &Path) -> Result<AgentCallingSettings, Unusable> {
    let unreadable = |error: std::io::Error| Unusable {
        reason: format!("cannot be read: {error}"),
        set_aside: false,
    };
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.len() > MAX_FILE_BYTES => {
            return Err(Unusable {
                reason: format!("larger than {MAX_FILE_BYTES} bytes"),
                set_aside: true,
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentCallingSettings::default());
        }
        Err(error) => return Err(unreadable(error)),
    }
    let bytes = tokio::fs::read(path).await.map_err(unreadable)?;
    StoredSettings::parse(&bytes).map_err(|reason| Unusable {
        reason,
        set_aside: true,
    })
}

/// Moves an unusable settings file out of the way, keeping it.
async fn set_aside_invalid(path: &Path) {
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".invalid-{}", chrono::Utc::now().timestamp()));
    let aside = PathBuf::from(aside);
    match tokio::fs::rename(path, &aside).await {
        Ok(()) => {
            tracing::warn!(path = %aside.display(), "moved the unusable agent calling settings aside")
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot move the unusable agent calling settings aside")
        }
    }
}

fn api_base_url(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}/api/v1")
    } else {
        format!("http://{host}:{port}/api/v1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, is_default: bool) -> ProviderModelDescriptor {
        ProviderModelDescriptor {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            is_default,
            supported_reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            context_window: None,
            image_input: None,
            family: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "todex-agent-calling-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_missing_file_allows_everything_with_mcp_off() {
        let dir = temp_dir("defaults");
        let store = AgentCallingStore::load(&dir, &ApiConfig::default()).await;
        let settings = store.settings();
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            serde_json::json!({
                "scope": { "agents": null, "models": {} },
                "mcp": { "enabled": false, "approval": "ask" },
            })
        );
        assert!(ProviderKind::ALL
            .iter()
            .all(|agent| settings.scope.allows_agent(*agent)
                && settings.scope.allows_model(*agent, "any-model")));
        assert!(!settings.mcp_injected());
        assert!(
            !dir.join(SETTINGS_FILE).exists(),
            "defaults are not written"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn updates_validate_persist_owner_only_and_reload() {
        let dir = temp_dir("update");
        let store = AgentCallingStore::load(&dir, &ApiConfig::default()).await;
        let empty: AgentCallingUpdate =
            serde_json::from_value(serde_json::json!({ "scope": { "models": { "codex": [] } } }))
                .unwrap();
        assert!(matches!(
            store.update(empty).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert!(!dir.join(SETTINGS_FILE).exists());

        let update: AgentCallingUpdate = serde_json::from_value(serde_json::json!({
            "scope": { "agents": ["codex", "claude-code", "codex"], "models": { "codex": ["gpt-a", "gpt-a", "gpt-b"] } },
            "mcp": { "enabled": true, "approval": "auto-approve" },
        }))
        .unwrap();
        let saved = store.update(update).await.unwrap();
        assert_eq!(
            saved.scope.agents,
            Some(vec![ProviderKind::Codex, ProviderKind::ClaudeCode])
        );
        assert_eq!(
            saved
                .scope
                .allowed_models(ProviderKind::Codex)
                .unwrap()
                .len(),
            2
        );
        assert!(saved.mcp_injected());
        // A partial update keeps the rest.
        let saved = store
            .update(AgentCallingUpdate {
                scope: None,
                mcp: Some(McpUpdate {
                    enabled: None,
                    approval: Some(ApprovalPolicy::Reject),
                }),
            })
            .await
            .unwrap();
        assert!(saved.mcp.enabled);
        assert_eq!(saved.mcp.approval, ApprovalPolicy::Reject);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SETTINGS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reloaded = AgentCallingStore::load(&dir, &ApiConfig::default()).await;
        assert_eq!(reloaded.settings(), saved);

        // No agent allowed: nothing to inject.
        let none: AgentCallingUpdate =
            serde_json::from_value(serde_json::json!({ "scope": { "agents": [] } })).unwrap();
        assert!(!store.update(none).await.unwrap().mcp_injected());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn models_resolve_to_the_allowed_default_or_the_first_allowed() {
        let mut scope = AgentScope::default();
        let catalog = [model("a", false), model("b", true), model("c", false)];
        // Unrestricted: requested as is, or the provider default.
        assert_eq!(
            scope
                .resolve_model(ProviderKind::Codex, Some("zzz"), Some(&catalog))
                .unwrap()
                .as_deref(),
            Some("zzz")
        );
        assert_eq!(
            scope
                .resolve_model(ProviderKind::Codex, None, Some(&catalog))
                .unwrap(),
            None
        );

        scope
            .models
            .insert(ProviderKind::Codex, vec!["c".to_owned(), "b".to_owned()]);
        let resolve = |scope: &AgentScope, requested, catalog| {
            scope.resolve_model(ProviderKind::Codex, requested, catalog)
        };
        assert_eq!(
            resolve(&scope, None, Some(&catalog[..]))
                .unwrap()
                .as_deref(),
            Some("b")
        );
        assert_eq!(
            resolve(&scope, Some("c"), Some(&catalog[..]))
                .unwrap()
                .as_deref(),
            Some("c")
        );
        assert!(matches!(
            resolve(&scope, Some("a"), Some(&catalog[..])),
            Err(AppError::Unauthorized(_))
        ));
        // The default is not allowed, or the catalog is unreadable: the
        // first allowed model.
        scope
            .models
            .insert(ProviderKind::Codex, vec!["c".to_owned(), "a".to_owned()]);
        assert_eq!(
            resolve(&scope, None, Some(&catalog[..]))
                .unwrap()
                .as_deref(),
            Some("c")
        );
        assert_eq!(resolve(&scope, None, None).unwrap().as_deref(), Some("c"));
        // Other agents are unaffected.
        assert_eq!(
            scope
                .resolve_model(ProviderKind::ClaudeCode, None, None)
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn the_api_listener_is_saved_validated_and_marked_pending() {
        let dir = temp_dir("listener");
        let config = Config {
            port: 7345,
            data_dir: dir.clone(),
            ..Config::default()
        };
        let store = AgentCallingStore::load(&dir, &config.api).await;
        let view = store.api_listener(&config);
        assert!(!view.enabled && !view.listening && !view.pending_restart);
        assert_eq!(view.base_url, None);

        for port in [0, 65536, 7345] {
            assert!(
                matches!(
                    store
                        .save_api_listener(&config, Some(true), Some(port))
                        .await,
                    Err(AppError::InvalidRequest(_))
                ),
                "port {port}"
            );
        }
        let view = store
            .save_api_listener(&config, Some(true), Some(7400))
            .await
            .unwrap();
        assert!(view.enabled && view.pending_restart && !view.listening);
        assert_eq!(view.port, 7400);
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("config.toml")).unwrap()).unwrap();
        assert_eq!(saved["api"]["enabled"].as_bool(), Some(true));
        assert_eq!(saved["api"]["port"].as_integer(), Some(7400));

        // A daemon running the saved listener has nothing pending.
        let mut running = config.clone();
        running.api.enabled = true;
        running.api.port = 7400;
        let store = AgentCallingStore::load(&dir, &running.api).await;
        store.set_running_api_listener(Some(SocketAddr::from(([127, 0, 0, 1], 7400))));
        let view = store.api_listener(&running);
        assert!(view.listening && !view.pending_restart);
        assert_eq!(
            view.base_url.as_deref(),
            Some("http://127.0.0.1:7400/api/v1")
        );
        // A non-loopback host without `allow_plaintext_remote` is refused.
        let mut remote = running.clone();
        remote.api.host = "0.0.0.0".to_owned();
        assert!(matches!(
            store.save_api_listener(&remote, None, Some(7401)).await,
            Err(AppError::InvalidRequest(_))
        ));
        assert_eq!(api_base_url("::1", 1), "http://[::1]:1/api/v1");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The files `dir` holds besides `config.toml`.
    fn settings_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(SETTINGS_FILE))
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn an_unusable_file_is_set_aside_for_the_defaults_with_mcp_off() {
        for contents in [
            "{ not json".to_owned(),
            // Valid JSON, invalid scope.
            serde_json::json!({
                "scope": { "models": { "codex": [] } },
                "mcp": { "enabled": true },
            })
            .to_string(),
            serde_json::json!({ "scope": { "agents": "codex" } }).to_string(),
        ] {
            let dir = temp_dir("invalid");
            std::fs::write(dir.join(SETTINGS_FILE), &contents).unwrap();
            let store = AgentCallingStore::load(&dir, &ApiConfig::default()).await;
            assert_eq!(
                store.settings(),
                AgentCallingSettings::default(),
                "{contents}"
            );
            assert!(!store.settings().mcp.enabled);
            let files = settings_files(&dir);
            assert_eq!(files.len(), 1, "{files:?}");
            assert!(
                files[0].starts_with(&format!("{SETTINGS_FILE}.invalid-")),
                "{files:?}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join(&files[0])).unwrap(),
                contents,
                "kept as it was"
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn a_newer_file_is_read_leniently_and_left_alone() {
        let dir = temp_dir("newer");
        let contents = serde_json::json!({
            "version": 2,
            "scope": {
                "agents": ["codex", "future-agent"],
                "models": { "codex": ["gpt-a"], "future-agent": ["x"] },
                "workspaces": ["/somewhere"],
            },
            "mcp": { "enabled": true, "approval": "auto-approve", "depth": 2 },
        })
        .to_string();
        std::fs::write(dir.join(SETTINGS_FILE), &contents).unwrap();
        let settings = AgentCallingStore::load(&dir, &ApiConfig::default())
            .await
            .settings();
        assert_eq!(settings.scope.agents, Some(vec![ProviderKind::Codex]));
        assert_eq!(
            settings.scope.allowed_models(ProviderKind::Codex),
            Some(&["gpt-a".to_owned()][..])
        );
        assert_eq!(settings.scope.models.len(), 1);
        assert!(settings.mcp.enabled);
        assert_eq!(settings.mcp.approval, ApprovalPolicy::AutoApprove);
        assert_eq!(
            std::fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap(),
            contents
        );
        assert_eq!(settings_files(&dir), [SETTINGS_FILE]);

        // An approval this TodeX does not know asks the user.
        std::fs::write(
            dir.join(SETTINGS_FILE),
            serde_json::json!({ "mcp": { "enabled": true, "approval": "ask-twice" } }).to_string(),
        )
        .unwrap();
        let settings = AgentCallingStore::load(&dir, &ApiConfig::default())
            .await
            .settings();
        assert!(settings.mcp.enabled);
        assert_eq!(settings.mcp.approval, ApprovalPolicy::Ask);

        // Clients' updates stay strict.
        for update in [
            serde_json::json!({ "scope": { "agents": ["future-agent"] } }),
            serde_json::json!({ "mcp": { "depth": 2 } }),
        ] {
            assert!(serde_json::from_value::<AgentCallingUpdate>(update).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn saving_the_port_keeps_listener_overrides_out_of_the_config_file() {
        let dir = temp_dir("override");
        // Enabled by `--enable-api` / `TODEX_AGENTD_API_ENABLED`, not by
        // `config.toml`.
        let mut config = Config {
            port: 7345,
            data_dir: dir.clone(),
            ..Config::default()
        };
        config.api.enabled = true;
        let store = AgentCallingStore::load(&dir, &config.api).await;
        store.set_running_api_listener(Some(SocketAddr::from(([127, 0, 0, 1], config.api.port))));
        assert!(!store.api_listener(&config).pending_restart);

        let view = store
            .save_api_listener(&config, None, Some(7400))
            .await
            .unwrap();
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("config.toml")).unwrap()).unwrap();
        assert_eq!(saved["api"]["port"].as_integer(), Some(7400));
        assert!(saved["api"].get("enabled").is_none(), "{saved}");
        // Still compared with the listener that runs.
        assert!(view.enabled && view.listening && view.pending_restart);
        assert_eq!(view.port, 7400);

        let view = store
            .save_api_listener(&config, Some(false), None)
            .await
            .unwrap();
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("config.toml")).unwrap()).unwrap();
        assert_eq!(saved["api"]["enabled"].as_bool(), Some(false));
        assert_eq!(saved["api"]["port"].as_integer(), Some(7400));
        assert!(!view.enabled && view.pending_restart);
        let _ = std::fs::remove_dir_all(dir);
    }
}
