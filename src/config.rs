use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::secure_fs;

#[derive(Debug, Clone, Default, Args)]
pub struct ServeArgs {
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub workspace_root: Vec<PathBuf>,
    #[arg(long)]
    pub history_retention_days: Option<u64>,
    /// Serve the external API (API key auth) on its own port.
    #[arg(long)]
    pub enable_api: bool,
    #[arg(long)]
    pub api_host: Option<String>,
    #[arg(long)]
    pub api_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// `network_enabled = false` serves clients over IPC only: `host`/`port`
    /// are not bound, and a loopback port on a random number serves just the
    /// agent MCP routes. Needs `[ipc]` enabled.
    pub network_enabled: bool,
    pub pairing_encryption: PairingEncryption,
    pub data_dir: PathBuf,
    pub workspace_roots: Vec<PathBuf>,
    pub history_retention_days: Option<u64>,
    /// Active journal files at least this large are sealed early — after a
    /// turn, when the conversation goes idle, or before their DEK expires —
    /// so they are compressed while the DEK is still in memory; `0` turns
    /// that off (docs/history-encryption.md §4.1).
    pub history_seal_min_bytes: u64,
    pub agent: AgentConfig,
    pub security: SecurityConfig,
    pub api: ApiConfig,
    /// How this daemon is deployed and reached (`[server]`
    /// `advertise_url`/`deployment`, `agent.image_cli_dirs`,
    /// `[[workspace.path_mappings]]`). All of it is off by default.
    pub deployment: DeploymentConfig,
    /// The same-user local IPC listener (`[ipc]`).
    pub ipc: IpcConfig,
    /// `[update]`: self-update preferences.
    pub update: UpdateConfig,
}

/// `[ipc]`: a Unix socket (named pipe on Windows) that only the daemon's
/// own user can open. It serves the normal router plus the local admin API
/// (docs/API.md, "Local IPC").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcConfig {
    pub enabled: bool,
    /// Overrides the default path (`crate::ipc::default_path`).
    pub path: Option<PathBuf>,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }
}

/// `[update]`. `auto_update = false` turns off the automatic updates at
/// launch and in a running daemon; `TODEX_AUTO_UPDATE=0` does too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateConfig {
    pub auto_update: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { auto_update: true }
    }
}

/// Where the daemon runs. `container` marks a daemon shipped in a container
/// image: the image is the update unit, so self-update is off.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Deployment {
    #[default]
    Host,
    Container,
}

impl Deployment {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "host" => Some(Self::Host),
            "container" => Some(Self::Container),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Container => "container",
        }
    }
}

/// One client-side directory and the path it has on this backend (a
/// container bind mount at another path). Clients use it to translate local
/// paths; the backend itself only publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PathMapping {
    pub host_path: String,
    pub backend_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentConfig {
    pub kind: Deployment,
    /// The `http(s)://host[:port]` clients should use, shown in the startup
    /// log and the TUI pairing link instead of guessed interface addresses.
    pub advertise_url: Option<String>,
    /// A managed CLI whose resolved binary lies under one of these
    /// directories ships with the image and is not installed or upgraded
    /// by TodeX.
    pub image_cli_dirs: Vec<PathBuf>,
    pub path_mappings: Vec<PathMapping>,
}

/// Set once any configuration with `deployment = container` was loaded in
/// this process; see [`container_deployment`].
static CONTAINER_DEPLOYMENT: AtomicBool = AtomicBool::new(false);

/// Whether this process loaded a container deployment's configuration. The
/// updater reads it, since it runs without a [`Config`] at hand.
pub fn container_deployment() -> bool {
    CONTAINER_DEPLOYMENT.load(Ordering::Relaxed)
}

/// `[update] auto_update` of the configuration this process loaded last
/// (`true` before any load); see [`auto_update_configured`].
static AUTO_UPDATE_CONFIGURED: AtomicBool = AtomicBool::new(true);

/// Whether the loaded configuration allows automatic updates. The updater
/// reads it, since it runs without a [`Config`] at hand.
pub fn auto_update_configured() -> bool {
    AUTO_UPDATE_CONFIGURED.load(Ordering::Relaxed)
}

/// Applies a changed `[update] auto_update` to this process right away (the
/// local admin API), so a running daemon honours it without a restart.
pub fn set_auto_update_configured(enabled: bool) {
    AUTO_UPDATE_CONFIGURED.store(enabled, Ordering::Relaxed);
}

/// The external API listener (`[api]`): REST + SSE authenticated with API
/// keys, on its own port (docs/API.md, "外部 API").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// A non-loopback API host sends bearer keys over plaintext HTTP; it is
    /// refused unless this is set (terminate TLS in front of it).
    pub allow_plaintext_remote: bool,
}

pub const DEFAULT_API_PORT: u16 = 7346;

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_owned(),
            port: DEFAULT_API_PORT,
            allow_plaintext_remote: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
pub enum PairingEncryption {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "x25519")]
    X25519,
    #[serde(rename = "ml-kem-768")]
    MlKem768,
}

impl PairingEncryption {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "disabled" => Some(Self::None),
            "x25519" | "x-25519" => Some(Self::X25519),
            "ml-kem-768" | "mlkem768" | "post-quantum" | "pq" => Some(Self::MlKem768),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::X25519 => "x25519",
            Self::MlKem768 => "ml-kem-768",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::None => Self::MlKem768,
            Self::MlKem768 => Self::X25519,
            Self::X25519 => Self::None,
        }
    }
}

impl Default for PairingEncryption {
    fn default() -> Self {
        Self::MlKem768
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub default_agent: String,
    pub codex_bin: String,
    pub claude_bin: String,
    pub pi_bin: String,
    pub grok_bin: String,
    pub grok_auth_method: Option<String>,
    pub grok_env_allowlist: Vec<String>,
    pub devin_bin: String,
    pub devin_auth_method: Option<String>,
    pub devin_api_key_env: Option<String>,
    pub devin_env_allowlist: Vec<String>,
    pub opencode_bin: String,
    pub opencode_env_allowlist: Vec<String>,
    pub antigravity_bin: String,
    pub antigravity_env_allowlist: Vec<String>,
    pub dsh_bin: String,
    pub dsh_env_allowlist: Vec<String>,
    pub acp_profiles: BTreeMap<String, AcpProfileConfig>,
    /// OpenSSH client used for remote hosts; `ssh-add` is looked up next to it.
    pub ssh_bin: String,
    /// A running turn whose provider produces no output for this long is
    /// stopped and failed. 0 disables the watchdog.
    pub provider_idle_timeout_minutes: u64,
    /// How long a resident provider process (Codex, Pi, OpenCode, Grok,
    /// Devin, DeepSeek Harness, resident ACP profiles) is kept without a
    /// turn before it is stopped. Read it through
    /// [`AgentConfig::provider_keepalive`], which applies the minimum.
    pub provider_keepalive_seconds: u64,
    /// Whether `conversation.warm` may start a conversation's provider
    /// process before its first prompt.
    pub provider_prewarm: bool,
}

pub const DEFAULT_PROVIDER_IDLE_TIMEOUT_MINUTES: u64 = 60;
pub const DEFAULT_PROVIDER_KEEPALIVE_SECONDS: u64 = 300;

/// Default of `history_seal_min_bytes` / `TODEX_AGENTD_HISTORY_SEAL_MIN_BYTES`.
pub const DEFAULT_HISTORY_SEAL_MIN_BYTES: u64 = 2 * 1024 * 1024;
/// Shorter keepalives are raised to this; a process that stops between
/// quick follow-up turns would pay its whole startup again.
pub const MIN_PROVIDER_KEEPALIVE_SECONDS: u64 = 30;
/// Longer keepalives are lowered to this (a week): deadlines are computed as
/// `Instant::now() + keepalive`, which a value near `u64::MAX` overflows.
pub const MAX_PROVIDER_KEEPALIVE_SECONDS: u64 = 7 * 24 * 60 * 60;

/// The keepalive of [`Config::load`], for runtimes built without the config.
static PROVIDER_KEEPALIVE_SECONDS: AtomicU64 = AtomicU64::new(DEFAULT_PROVIDER_KEEPALIVE_SECONDS);

impl AgentConfig {
    /// The idle keepalive of resident provider processes, clamped to
    /// [`MIN_PROVIDER_KEEPALIVE_SECONDS`]..=[`MAX_PROVIDER_KEEPALIVE_SECONDS`].
    pub fn provider_keepalive(&self) -> Duration {
        keepalive_duration(self.provider_keepalive_seconds)
    }
}

/// The provider keepalive of the configuration the daemon loaded (the
/// default before [`Config::load`]); for runtimes constructed without an
/// [`AgentConfig`].
pub fn provider_keepalive() -> Duration {
    keepalive_duration(PROVIDER_KEEPALIVE_SECONDS.load(Ordering::Relaxed))
}

fn keepalive_duration(seconds: u64) -> Duration {
    Duration::from_secs(seconds.clamp(
        MIN_PROVIDER_KEEPALIVE_SECONDS,
        MAX_PROVIDER_KEEPALIVE_SECONDS,
    ))
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcpProfileConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Keep one agent process per conversation between turns (the default);
    /// `false` starts a process for every turn.
    #[serde(default = "default_true")]
    pub resident: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecurityConfig {
    pub enable_auth: bool,
    pub enable_tls: bool,
    /// Reverse proxies on this machine whose `X-Forwarded-For` names the
    /// real client (its rightmost entry); see `listen_addrs::forwarded_peer`.
    /// Requests that arrive through any proxy are never treated as local.
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Origins allowed to start a device pairing (`Origin` header); `None`
    /// allows any, marking browser origins in the approval.
    #[serde(default)]
    pub pairing_origins: Option<Vec<String>>,
    /// Web origins, beyond the desktop app's and loopback pages, whose pages
    /// may read the daemon's responses (CORS), e.g. a self-hosted web client
    /// that connects directly.
    #[serde(default)]
    pub cors_origins: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enable_auth: true,
            enable_tls: false,
            trusted_proxies: Vec::new(),
            pairing_origins: None,
            cors_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    host: Option<String>,
    port: Option<u16>,
    network_enabled: Option<bool>,
    pairing_encryption: Option<PairingEncryption>,
    /// Retired: history is always end-to-end encrypted. Any value (even an
    /// invalid one) is accepted and only logged; see
    /// [`warn_retired_history_encryption`].
    history_encryption: Option<toml::Value>,
    data_dir: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    workspace_roots: Option<Vec<PathBuf>>,
    history_retention_days: Option<u64>,
    history_seal_min_bytes: Option<u64>,
    agent: Option<PartialAgentConfig>,
    security: Option<PartialSecurityConfig>,
    api: Option<PartialApiConfig>,
    server: Option<PartialServerConfig>,
    workspace: Option<PartialWorkspaceConfig>,
    ipc: Option<PartialIpcConfig>,
    update: Option<PartialUpdateConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialIpcConfig {
    enabled: Option<bool>,
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialUpdateConfig {
    auto_update: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialServerConfig {
    advertise_url: Option<String>,
    deployment: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialWorkspaceConfig {
    path_mappings: Option<Vec<FilePathMapping>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePathMapping {
    host_path: String,
    backend_path: String,
}

#[derive(Debug, Default, Deserialize)]
struct PartialApiConfig {
    enabled: Option<bool>,
    host: Option<String>,
    port: Option<u16>,
    allow_plaintext_remote: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialAgentConfig {
    default_agent: Option<String>,
    codex_bin: Option<String>,
    claude_bin: Option<String>,
    pi_bin: Option<String>,
    grok_bin: Option<String>,
    grok_auth_method: Option<String>,
    grok_env_allowlist: Option<Vec<String>>,
    devin_bin: Option<String>,
    devin_auth_method: Option<String>,
    devin_api_key_env: Option<String>,
    devin_env_allowlist: Option<Vec<String>>,
    opencode_bin: Option<String>,
    opencode_env_allowlist: Option<Vec<String>>,
    antigravity_bin: Option<String>,
    antigravity_env_allowlist: Option<Vec<String>>,
    dsh_bin: Option<String>,
    dsh_env_allowlist: Option<Vec<String>>,
    acp_profiles: Option<BTreeMap<String, AcpProfileConfig>>,
    ssh_bin: Option<String>,
    provider_idle_timeout_minutes: Option<u64>,
    provider_keepalive_seconds: Option<u64>,
    provider_prewarm: Option<bool>,
    image_cli_dirs: Option<Vec<PathBuf>>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialSecurityConfig {
    enable_auth: Option<bool>,
    enable_tls: Option<bool>,
    /// Legacy bearer token. Parsed only to warn that it is ignored; device
    /// signature auth replaced it.
    auth_token: Option<String>,
    trusted_proxies: Option<Vec<std::net::IpAddr>>,
    pairing_origins: Option<Vec<String>>,
    cors_origins: Option<Vec<String>>,
}

impl Config {
    /// Loads the configuration a daemon runs with; it also becomes the
    /// process-wide [`provider_keepalive`].
    pub fn load(args: ServeArgs) -> anyhow::Result<Self> {
        let config = Self::load_read_only(args)?;
        PROVIDER_KEEPALIVE_SECONDS
            .store(config.agent.provider_keepalive_seconds, Ordering::Relaxed);
        Ok(config)
    }

    /// Loads the configuration without writing anything; it still sets the
    /// process-wide deployment and auto-update flags the updater reads.
    pub fn load_read_only(args: ServeArgs) -> anyhow::Result<Self> {
        let config = Self::load_internal(args)?;
        if config.deployment.kind == Deployment::Container {
            CONTAINER_DEPLOYMENT.store(true, Ordering::Relaxed);
        }
        AUTO_UPDATE_CONFIGURED.store(config.update.auto_update, Ordering::Relaxed);
        Ok(config)
    }

    /// The configuration a fresh start on `data_dir` would use (config file
    /// and environment), without touching any process-wide state. The local
    /// admin API compares it with the running one.
    pub fn load_saved(data_dir: PathBuf) -> anyhow::Result<Self> {
        Self::load_internal(ServeArgs {
            data_dir: Some(data_dir),
            ..Default::default()
        })
    }

    fn load_internal(args: ServeArgs) -> anyhow::Result<Self> {
        let defaults = Config::default();
        let env_data_dir = env_path("TODEX_AGENTD_DATA_DIR");
        let bootstrap_data_dir = expand_home(
            args.data_dir
                .clone()
                .or(env_data_dir.clone())
                .unwrap_or_else(|| defaults.data_dir.clone()),
        );
        let data_dir_is_explicit = args.data_dir.is_some() || env_data_dir.is_some();
        let (data_dir, file_config) = load_file_config(&bootstrap_data_dir, !data_dir_is_explicit)?;
        let workspace_roots = resolve_workspace_roots(
            args.workspace_root,
            env_workspace_roots(),
            file_workspace_roots(&file_config),
            defaults.workspace_roots,
        );
        let history_retention_days = args
            .history_retention_days
            .or_else(|| {
                env::var("TODEX_AGENTD_HISTORY_RETENTION_DAYS")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .or(file_config.history_retention_days)
            .or(defaults.history_retention_days);
        let history_seal_min_bytes = coalesce(
            None,
            env::var("TODEX_AGENTD_HISTORY_SEAL_MIN_BYTES")
                .ok()
                .and_then(|value| value.trim().parse().ok()),
            file_config.history_seal_min_bytes,
            defaults.history_seal_min_bytes,
        );
        let host = coalesce(
            args.host,
            env::var("TODEX_AGENTD_HOST").ok(),
            file_config.host,
            defaults.host,
        );
        let port = coalesce(
            args.port,
            env::var("TODEX_AGENTD_PORT")
                .ok()
                .and_then(|value| value.parse().ok()),
            file_config.port,
            defaults.port,
        );
        let network_enabled = coalesce(
            None,
            env_bool("TODEX_AGENTD_NETWORK_ENABLED"),
            file_config.network_enabled,
            defaults.network_enabled,
        );
        let pairing_encryption = coalesce(
            None,
            env::var("TODEX_AGENTD_PAIRING_ENCRYPTION")
                .ok()
                .and_then(|value| PairingEncryption::parse(&value)),
            file_config.pairing_encryption,
            defaults.pairing_encryption,
        );

        let agent_file = file_config.agent.unwrap_or_default();
        let deployment = resolve_deployment_config(
            &|key| env::var_os(key),
            file_config.server.unwrap_or_default(),
            agent_file.image_cli_dirs.clone(),
            file_config.workspace.unwrap_or_default(),
            &workspace_roots,
        )?;
        let ipc_file = file_config.ipc.unwrap_or_default();
        let ipc = IpcConfig {
            enabled: coalesce(
                None,
                env_bool("TODEX_AGENTD_IPC_ENABLED"),
                ipc_file.enabled,
                defaults.ipc.enabled,
            ),
            path: env_path("TODEX_AGENTD_IPC_PATH")
                .filter(|path| !path.as_os_str().is_empty())
                .or(ipc_file.path.filter(|path| !path.as_os_str().is_empty()))
                .map(expand_home),
        };
        let update = UpdateConfig {
            auto_update: file_config
                .update
                .and_then(|update| update.auto_update)
                .unwrap_or(defaults.update.auto_update),
        };
        let security_file = file_config.security.unwrap_or_default();
        let api_file = file_config.api.unwrap_or_default();
        let api = ApiConfig {
            enabled: coalesce(
                args.enable_api.then_some(true),
                env_bool("TODEX_AGENTD_API_ENABLED"),
                api_file.enabled,
                defaults.api.enabled,
            ),
            host: coalesce(
                args.api_host,
                env::var("TODEX_AGENTD_API_HOST").ok(),
                api_file.host,
                defaults.api.host,
            ),
            port: coalesce(
                args.api_port,
                env::var("TODEX_AGENTD_API_PORT")
                    .ok()
                    .and_then(|value| value.parse().ok()),
                api_file.port,
                defaults.api.port,
            ),
            allow_plaintext_remote: coalesce(
                None,
                env_bool("TODEX_AGENTD_API_ALLOW_PLAINTEXT_REMOTE"),
                api_file.allow_plaintext_remote,
                defaults.api.allow_plaintext_remote,
            ),
        };
        let enable_auth = coalesce(
            None,
            env_bool("TODEX_AGENTD_ENABLE_AUTH"),
            security_file.enable_auth,
            defaults.security.enable_auth,
        );
        let trusted_proxies = match env_list("TODEX_AGENTD_TRUSTED_PROXIES") {
            Some(list) => list
                .iter()
                .map(|item| {
                    item.parse::<std::net::IpAddr>().with_context(|| {
                        format!("TODEX_AGENTD_TRUSTED_PROXIES: {item} is not an IP address")
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
            None => security_file
                .trusted_proxies
                .clone()
                .unwrap_or_else(|| defaults.security.trusted_proxies.clone()),
        };
        let cors_origins = env_list("TODEX_AGENTD_CORS_ORIGINS")
            .or_else(|| security_file.cors_origins.clone())
            .unwrap_or_else(|| defaults.security.cors_origins.clone());
        let pairing_origins = env_list("TODEX_AGENTD_PAIRING_ORIGINS")
            .or_else(|| security_file.pairing_origins.clone())
            .or_else(|| defaults.security.pairing_origins.clone());
        if optional_non_empty(env::var("TODEX_AGENTD_AUTH_TOKEN").ok()).is_some()
            || optional_non_empty(security_file.auth_token).is_some()
        {
            tracing::warn!(
                "security.auth_token / TODEX_AGENTD_AUTH_TOKEN is ignored; clients \
                 authenticate with paired device keys"
            );
        }

        Ok(Config {
            host,
            port,
            network_enabled,
            pairing_encryption,
            data_dir,
            workspace_roots,
            history_retention_days,
            history_seal_min_bytes,
            agent: AgentConfig {
                default_agent: coalesce(
                    None,
                    env::var("TODEX_AGENTD_DEFAULT_AGENT").ok(),
                    agent_file.default_agent,
                    defaults.agent.default_agent,
                ),
                codex_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_CODEX_BIN").ok(),
                    agent_file.codex_bin,
                    defaults.agent.codex_bin,
                ),
                claude_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_CLAUDE_BIN").ok(),
                    agent_file.claude_bin,
                    defaults.agent.claude_bin,
                ),
                pi_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_PI_BIN").ok(),
                    agent_file.pi_bin,
                    defaults.agent.pi_bin,
                ),
                grok_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_GROK_BIN").ok(),
                    agent_file.grok_bin,
                    defaults.agent.grok_bin,
                ),
                grok_auth_method: optional_non_empty(
                    env::var("TODEX_AGENTD_GROK_AUTH_METHOD").ok(),
                )
                .or_else(|| optional_non_empty(agent_file.grok_auth_method))
                .or(defaults.agent.grok_auth_method),
                grok_env_allowlist: env_list("TODEX_AGENTD_GROK_ENV_ALLOWLIST")
                    .or(agent_file.grok_env_allowlist)
                    .unwrap_or(defaults.agent.grok_env_allowlist),
                devin_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_DEVIN_BIN").ok(),
                    agent_file.devin_bin,
                    defaults.agent.devin_bin,
                ),
                devin_auth_method: optional_non_empty(
                    env::var("TODEX_AGENTD_DEVIN_AUTH_METHOD").ok(),
                )
                .or_else(|| optional_non_empty(agent_file.devin_auth_method))
                .or(defaults.agent.devin_auth_method),
                devin_api_key_env: optional_non_empty(
                    env::var("TODEX_AGENTD_DEVIN_API_KEY_ENV").ok(),
                )
                .or_else(|| optional_non_empty(agent_file.devin_api_key_env))
                .or(defaults.agent.devin_api_key_env),
                devin_env_allowlist: env_list("TODEX_AGENTD_DEVIN_ENV_ALLOWLIST")
                    .or(agent_file.devin_env_allowlist)
                    .unwrap_or(defaults.agent.devin_env_allowlist),
                opencode_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_OPENCODE_BIN").ok(),
                    agent_file.opencode_bin,
                    defaults.agent.opencode_bin,
                ),
                ssh_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_SSH_BIN").ok(),
                    agent_file.ssh_bin,
                    defaults.agent.ssh_bin,
                ),
                opencode_env_allowlist: env_list("TODEX_AGENTD_OPENCODE_ENV_ALLOWLIST")
                    .or(agent_file.opencode_env_allowlist)
                    .unwrap_or(defaults.agent.opencode_env_allowlist),
                antigravity_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_ANTIGRAVITY_BIN").ok(),
                    agent_file.antigravity_bin,
                    defaults.agent.antigravity_bin,
                ),
                antigravity_env_allowlist: env_list("TODEX_AGENTD_ANTIGRAVITY_ENV_ALLOWLIST")
                    .or(agent_file.antigravity_env_allowlist)
                    .unwrap_or(defaults.agent.antigravity_env_allowlist),
                dsh_bin: coalesce(
                    None,
                    env::var("TODEX_AGENTD_DSH_BIN").ok(),
                    agent_file.dsh_bin,
                    defaults.agent.dsh_bin,
                ),
                dsh_env_allowlist: env_list("TODEX_AGENTD_DSH_ENV_ALLOWLIST")
                    .or(agent_file.dsh_env_allowlist)
                    .unwrap_or(defaults.agent.dsh_env_allowlist),
                acp_profiles: agent_file
                    .acp_profiles
                    .unwrap_or(defaults.agent.acp_profiles),
                provider_idle_timeout_minutes: coalesce(
                    None,
                    env::var("TODEX_AGENTD_PROVIDER_IDLE_TIMEOUT_MINUTES")
                        .ok()
                        .and_then(|value| value.trim().parse().ok()),
                    agent_file.provider_idle_timeout_minutes,
                    defaults.agent.provider_idle_timeout_minutes,
                ),
                provider_keepalive_seconds: coalesce(
                    None,
                    env::var("TODEX_AGENTD_PROVIDER_KEEPALIVE_SECONDS")
                        .ok()
                        .and_then(|value| value.trim().parse().ok()),
                    agent_file.provider_keepalive_seconds,
                    defaults.agent.provider_keepalive_seconds,
                ),
                provider_prewarm: coalesce(
                    None,
                    env_bool("TODEX_AGENTD_PROVIDER_PREWARM"),
                    agent_file.provider_prewarm,
                    defaults.agent.provider_prewarm,
                ),
            },
            security: SecurityConfig {
                enable_auth,
                enable_tls: coalesce(
                    None,
                    env_bool("TODEX_AGENTD_ENABLE_TLS"),
                    security_file.enable_tls,
                    defaults.security.enable_tls,
                ),
                trusted_proxies,
                pairing_origins,
                cors_origins,
            },
            api,
            deployment,
            ipc,
            update,
        })
    }

    /// The API listener must not share the main port, and a non-loopback
    /// API host needs `api.allow_plaintext_remote`.
    pub fn ensure_api_listener_is_valid(&self) -> anyhow::Result<()> {
        if !self.api.enabled {
            return Ok(());
        }
        if self.api.port == self.port && self.api.port != 0 {
            anyhow::bail!(
                "api.port {} is the main listener's port; choose another port for the API",
                self.api.port
            );
        }
        if !crate::listen_addrs::is_loopback_host(&self.api.host)
            && !self.api.allow_plaintext_remote
        {
            anyhow::bail!(
                "the API listener {} is not a loopback address and would send API keys over \
                 plaintext HTTP; bind it to 127.0.0.1 or ::1, or set api.allow_plaintext_remote \
                 = true behind a TLS-terminating proxy",
                self.api.host
            );
        }
        Ok(())
    }

    /// With the network off only IPC reaches the daemon, so IPC must be on.
    pub fn ensure_network_off_has_ipc(&self) -> anyhow::Result<()> {
        if !self.network_enabled && !self.ipc.enabled {
            anyhow::bail!(
                "network_enabled = false serves clients over IPC only, but [ipc] is disabled; \
                 enable [ipc] or set network_enabled = true"
            );
        }
        Ok(())
    }

    /// Saves top-level `network_enabled`; the rest of the file is kept.
    pub fn save_network_enabled(data_dir: PathBuf, enabled: bool) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        document["network_enabled"] = value(enabled);
        write_config_document(&data_dir, &document)
    }

    /// Saves the external API listener's `[api]` `enabled` and `port`,
    /// each only when given; the rest of the file is kept.
    pub fn save_api_settings(
        data_dir: PathBuf,
        enabled: Option<bool>,
        port: Option<u16>,
    ) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        if !document.get("api").is_some_and(Item::is_table) {
            document.insert("api", Item::Table(Table::new()));
        }
        if let Some(enabled) = enabled {
            document["api"]["enabled"] = value(enabled);
        }
        if let Some(port) = port {
            document["api"]["port"] = value(i64::from(port));
        }
        write_config_document(&data_dir, &document)
    }

    /// Without device authentication only this machine may reach the daemon,
    /// so a non-loopback listener is refused instead of serving every network
    /// peer anonymously. Non-loopback peers must use transport v2, which
    /// needs a pairing key, so such a listener also needs
    /// `pairing_encryption` other than `none`.
    pub fn ensure_listener_matches_auth(&self) -> anyhow::Result<()> {
        if !self.network_enabled {
            return Ok(());
        }
        ensure_listener_matches_auth(
            &self.host,
            self.security.enable_auth,
            self.pairing_encryption,
        )
        .map_err(anyhow::Error::msg)
    }

    /// `<data_dir>/workspace`, parent of the per-conversation folders created
    /// for conversations started without a workspace.
    pub fn scratch_workspace_root(&self) -> PathBuf {
        crate::workspace_paths::scratch_workspace_root(&self.data_dir)
    }

    /// Roots a conversation may work in: the configured workspace roots plus
    /// the scratch root. Directory browsing and the workspace registry keep
    /// using `workspace_roots` alone.
    pub fn workspace_access_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.workspace_roots.clone();
        roots.push(self.scratch_workspace_root());
        roots
    }

    /// The first configured root; `workspace_roots` is never empty after load.
    pub fn primary_workspace_root(&self) -> &Path {
        self.workspace_roots
            .first()
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new(""))
    }

    pub fn save_tui_settings(
        data_dir: PathBuf,
        host: &str,
        port: u16,
        pairing_encryption: PairingEncryption,
        workspace_roots: &[PathBuf],
    ) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        document["host"] = value(host);
        document["port"] = value(i64::from(port));
        document["pairing_encryption"] = value(pairing_encryption.as_str());
        if let Some(primary) = workspace_roots.first() {
            document["workspace_root"] = value(primary.display().to_string());
        }
        let mut roots = toml_edit::Array::new();
        for root in workspace_roots {
            roots.push(root.display().to_string());
        }
        document["workspace_roots"] = value(roots);
        write_config_document(&data_dir, &document)?;
        Ok(())
    }

    /// Saves `[update] auto_update`; the rest of the file is kept.
    pub fn save_update_settings(data_dir: PathBuf, auto_update: bool) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        if !document.get("update").is_some_and(Item::is_table) {
            document.insert("update", Item::Table(Table::new()));
        }
        document["update"]["auto_update"] = value(auto_update);
        write_config_document(&data_dir, &document)
    }

    pub fn load_tui_language(data_dir: &Path) -> anyhow::Result<Option<String>> {
        let document = load_config_document(&data_dir.to_path_buf())?;
        Ok(document
            .get("tui")
            .and_then(|item| item.get("language"))
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned))
    }

    pub fn save_tui_language(data_dir: PathBuf, language: &str) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        if !document.get("tui").is_some_and(Item::is_table) {
            document.insert("tui", Item::Table(Table::new()));
        }
        document["tui"]["language"] = value(language);
        write_config_document(&data_dir, &document)
    }
}

/// Shared by startup and the TUI host editor; see
/// [`Config::ensure_listener_matches_auth`].
pub fn ensure_listener_matches_auth(
    host: &str,
    enable_auth: bool,
    pairing_encryption: PairingEncryption,
) -> Result<(), String> {
    if crate::listen_addrs::is_loopback_host(host) {
        return Ok(());
    }
    if !enable_auth {
        return Err(format!(
            "device authentication is disabled (security.enable_auth = false) but the listener \
             {host} is not a loopback address; bind to 127.0.0.1 or ::1, or enable device \
             authentication"
        ));
    }
    if pairing_encryption == PairingEncryption::None {
        return Err(format!(
            "transport encryption is disabled (pairing_encryption = \"none\") but the listener \
             {host} is not a loopback address; remote clients must use encrypted transport \
             v2, so choose x25519 or ml-kem-768, or bind to 127.0.0.1 or ::1"
        ));
    }
    Ok(())
}

/// A listen host as the TUI and the local admin API accept it: an IP literal.
pub fn validate_host(value: &str) -> Result<String, String> {
    let host = value.trim();
    if host.is_empty() {
        return Err("host cannot be empty".to_owned());
    }
    host.parse::<std::net::IpAddr>()
        .map(|_| host.to_owned())
        .map_err(|_| "host must be a valid IPv4 or IPv6 address".to_owned())
}

/// A listen port typed into the TUI: 1..=65535.
pub fn validate_port(value: &str) -> Result<u16, String> {
    let port = value
        .trim()
        .parse::<u16>()
        .map_err(|_| "port must be a number from 1 to 65535".to_owned())?;
    validate_listen_port(port, None)
}

/// 1..=65535, and not the API listener's port when `api_port` is given
/// (the API is enabled).
pub fn validate_listen_port(port: u16, api_port: Option<u16>) -> Result<u16, String> {
    if port == 0 {
        return Err("port must be a number from 1 to 65535".to_owned());
    }
    if api_port == Some(port) {
        return Err(format!("port {port} is the API listener's port"));
    }
    Ok(port)
}

impl Default for Config {
    fn default() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        let data_dir = home
            .clone()
            .map(|home| home.join(".todex-agent"))
            .unwrap_or_else(|| PathBuf::from(".todex-agent"));
        let workspace_root = home
            .map(|home| home.join("projects"))
            .unwrap_or_else(|| PathBuf::from("projects"));

        Config {
            host: "127.0.0.1".to_owned(),
            port: 7345,
            network_enabled: true,
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: DEFAULT_HISTORY_SEAL_MIN_BYTES,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: default_grok_env_allowlist(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: default_devin_env_allowlist(),
                opencode_bin: "opencode".to_owned(),
                ssh_bin: "ssh".to_owned(),
                opencode_env_allowlist: default_opencode_env_allowlist(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: default_antigravity_env_allowlist(),
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: default_dsh_env_allowlist(),
                acp_profiles: BTreeMap::new(),
                provider_idle_timeout_minutes: DEFAULT_PROVIDER_IDLE_TIMEOUT_MINUTES,
                provider_keepalive_seconds: DEFAULT_PROVIDER_KEEPALIVE_SECONDS,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                ..Default::default()
            },
            api: ApiConfig::default(),
            deployment: DeploymentConfig::default(),
            ipc: IpcConfig::default(),
            update: UpdateConfig::default(),
        }
    }
}

fn read_config_file(path: PathBuf) -> anyhow::Result<FileConfig> {
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(toml::from_str(&contents)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(err) => Err(err.into()),
    }
}

fn load_file_config(
    bootstrap_data_dir: &Path,
    allow_redirect: bool,
) -> anyhow::Result<(PathBuf, FileConfig)> {
    let bootstrap_config = read_config_file(bootstrap_data_dir.join("config.toml"))?;
    let data_dir = if allow_redirect {
        bootstrap_config
            .data_dir
            .as_ref()
            .map(|path| resolve_config_data_dir(bootstrap_data_dir, path))
            .unwrap_or_else(|| bootstrap_data_dir.to_path_buf())
    } else {
        bootstrap_data_dir.to_path_buf()
    };
    if data_dir == bootstrap_data_dir {
        return Ok((data_dir, bootstrap_config));
    }

    let effective_config = read_config_file(data_dir.join("config.toml"))?;
    if let Some(next) = effective_config.data_dir.as_ref() {
        let next = resolve_config_data_dir(&data_dir, next);
        if next != data_dir {
            anyhow::bail!(
                "config data_dir may redirect only once ({} -> {} -> {})",
                bootstrap_data_dir.display(),
                data_dir.display(),
                next.display()
            );
        }
    }
    Ok((
        data_dir,
        merge_file_config(bootstrap_config, effective_config),
    ))
}

fn resolve_config_data_dir(base: &Path, configured: &Path) -> PathBuf {
    let configured = expand_home(configured.to_path_buf());
    if configured.is_absolute() {
        configured
    } else {
        base.join(configured)
    }
}

fn merge_file_config(mut base: FileConfig, overlay: FileConfig) -> FileConfig {
    macro_rules! replace_some {
        ($target:expr, $value:expr) => {
            if $value.is_some() {
                $target = $value;
            }
        };
    }

    replace_some!(base.host, overlay.host);
    replace_some!(base.port, overlay.port);
    replace_some!(base.pairing_encryption, overlay.pairing_encryption);
    replace_some!(base.history_encryption, overlay.history_encryption);
    replace_some!(base.data_dir, overlay.data_dir);
    replace_some!(base.workspace_root, overlay.workspace_root);
    replace_some!(base.workspace_roots, overlay.workspace_roots);
    replace_some!(base.history_retention_days, overlay.history_retention_days);
    replace_some!(base.history_seal_min_bytes, overlay.history_seal_min_bytes);

    if let Some(overlay_agent) = overlay.agent {
        let base_agent = base.agent.get_or_insert_with(PartialAgentConfig::default);
        replace_some!(base_agent.default_agent, overlay_agent.default_agent);
        replace_some!(base_agent.codex_bin, overlay_agent.codex_bin);
        replace_some!(base_agent.claude_bin, overlay_agent.claude_bin);
        replace_some!(base_agent.pi_bin, overlay_agent.pi_bin);
        replace_some!(base_agent.grok_bin, overlay_agent.grok_bin);
        replace_some!(base_agent.grok_auth_method, overlay_agent.grok_auth_method);
        replace_some!(
            base_agent.grok_env_allowlist,
            overlay_agent.grok_env_allowlist
        );
        replace_some!(base_agent.devin_bin, overlay_agent.devin_bin);
        replace_some!(
            base_agent.devin_auth_method,
            overlay_agent.devin_auth_method
        );
        replace_some!(
            base_agent.devin_api_key_env,
            overlay_agent.devin_api_key_env
        );
        replace_some!(
            base_agent.devin_env_allowlist,
            overlay_agent.devin_env_allowlist
        );
        replace_some!(base_agent.opencode_bin, overlay_agent.opencode_bin);
        replace_some!(base_agent.ssh_bin, overlay_agent.ssh_bin);
        replace_some!(
            base_agent.opencode_env_allowlist,
            overlay_agent.opencode_env_allowlist
        );
        replace_some!(base_agent.antigravity_bin, overlay_agent.antigravity_bin);
        replace_some!(
            base_agent.antigravity_env_allowlist,
            overlay_agent.antigravity_env_allowlist
        );
        replace_some!(base_agent.dsh_bin, overlay_agent.dsh_bin);
        replace_some!(
            base_agent.dsh_env_allowlist,
            overlay_agent.dsh_env_allowlist
        );
        replace_some!(base_agent.acp_profiles, overlay_agent.acp_profiles);
        replace_some!(
            base_agent.provider_idle_timeout_minutes,
            overlay_agent.provider_idle_timeout_minutes
        );
        replace_some!(
            base_agent.provider_keepalive_seconds,
            overlay_agent.provider_keepalive_seconds
        );
        replace_some!(base_agent.provider_prewarm, overlay_agent.provider_prewarm);
        replace_some!(base_agent.image_cli_dirs, overlay_agent.image_cli_dirs);
    }
    if let Some(overlay_security) = overlay.security {
        let base_security = base
            .security
            .get_or_insert_with(PartialSecurityConfig::default);
        replace_some!(base_security.enable_auth, overlay_security.enable_auth);
        replace_some!(base_security.enable_tls, overlay_security.enable_tls);
        replace_some!(base_security.auth_token, overlay_security.auth_token);
    }
    if let Some(overlay_api) = overlay.api {
        let base_api = base.api.get_or_insert_with(PartialApiConfig::default);
        replace_some!(base_api.enabled, overlay_api.enabled);
        replace_some!(base_api.host, overlay_api.host);
        replace_some!(base_api.port, overlay_api.port);
        replace_some!(
            base_api.allow_plaintext_remote,
            overlay_api.allow_plaintext_remote
        );
    }
    if let Some(overlay_server) = overlay.server {
        let base_server = base.server.get_or_insert_with(PartialServerConfig::default);
        replace_some!(base_server.advertise_url, overlay_server.advertise_url);
        replace_some!(base_server.deployment, overlay_server.deployment);
    }
    if let Some(overlay_ipc) = overlay.ipc {
        let base_ipc = base.ipc.get_or_insert_with(PartialIpcConfig::default);
        replace_some!(base_ipc.enabled, overlay_ipc.enabled);
        replace_some!(base_ipc.path, overlay_ipc.path);
    }
    if let Some(overlay_update) = overlay.update {
        let base_update = base.update.get_or_insert_with(PartialUpdateConfig::default);
        replace_some!(base_update.auto_update, overlay_update.auto_update);
    }
    if let Some(overlay_workspace) = overlay.workspace {
        let base_workspace = base
            .workspace
            .get_or_insert_with(PartialWorkspaceConfig::default);
        replace_some!(
            base_workspace.path_mappings,
            overlay_workspace.path_mappings
        );
    }
    base
}

/// `history_encryption` (config file) and `TODEX_AGENTD_HISTORY_ENCRYPTION`
/// are retired: conversation history is always end-to-end encrypted
/// (docs/history-encryption.md). A leftover setting is ignored with one
/// warning each, never a startup failure.
pub fn warn_retired_history_encryption(data_dir: &Path) {
    if let Ok(value) = env::var("TODEX_AGENTD_HISTORY_ENCRYPTION") {
        tracing::warn!(
            value = %value,
            "TODEX_AGENTD_HISTORY_ENCRYPTION is retired and ignored: conversation history is always end-to-end encrypted"
        );
    }
    match load_file_config(data_dir, false) {
        Ok((_, file_config)) => {
            if let Some(value) = file_config.history_encryption {
                tracing::warn!(
                    value = %value,
                    "config history_encryption is retired and ignored: conversation history is always end-to-end encrypted"
                );
            }
        }
        // `Config::load` already read (and reported) this file.
        Err(error) => {
            tracing::debug!(error = %error, "config file unreadable while checking retired settings")
        }
    }
}

/// `[server]`, `agent.image_cli_dirs` and `[[workspace.path_mappings]]`,
/// each overridden by its environment variable. Compose files pass these
/// variables even when left blank, so an empty value counts as unset.
fn resolve_deployment_config(
    env: &dyn Fn(&str) -> Option<OsString>,
    server: PartialServerConfig,
    file_image_cli_dirs: Option<Vec<PathBuf>>,
    workspace: PartialWorkspaceConfig,
    workspace_roots: &[PathBuf],
) -> anyhow::Result<DeploymentConfig> {
    let var =
        |key: &str| optional_non_empty(env(key).map(|value| value.to_string_lossy().into_owned()));
    let kind = match var("TODEX_AGENTD_DEPLOYMENT")
        .map(|value| ("TODEX_AGENTD_DEPLOYMENT", value))
        .or_else(|| optional_non_empty(server.deployment).map(|value| ("server.deployment", value)))
    {
        Some((source, value)) => Deployment::parse(&value).ok_or_else(|| {
            anyhow::anyhow!("{source} must be \"host\" or \"container\", got {value:?}")
        })?,
        None => Deployment::Host,
    };
    let advertise_url = match var("TODEX_AGENTD_ADVERTISE_URL")
        .map(|value| ("TODEX_AGENTD_ADVERTISE_URL", value))
        .or_else(|| {
            optional_non_empty(server.advertise_url).map(|value| ("server.advertise_url", value))
        }) {
        Some((source, value)) => {
            Some(validate_advertise_url(&value).with_context(|| format!("invalid {source}"))?)
        }
        None => None,
    };
    let image_cli_dirs = match env("TODEX_AGENTD_IMAGE_CLI_DIR")
        .filter(|value| !value.to_string_lossy().trim().is_empty())
    {
        Some(value) => validate_image_cli_dirs(
            env::split_paths(&value).collect(),
            "TODEX_AGENTD_IMAGE_CLI_DIR",
        )?,
        None => validate_image_cli_dirs(
            file_image_cli_dirs.unwrap_or_default(),
            "agent.image_cli_dirs",
        )?,
    };
    let path_mappings = match var("TODEX_AGENTD_PATH_MAPPINGS") {
        Some(value) => {
            let mappings: Vec<PathMapping> = serde_json::from_str(&value).context(
                "TODEX_AGENTD_PATH_MAPPINGS must be a JSON array of {\"hostPath\", \"backendPath\"}",
            )?;
            validate_path_mappings(mappings, workspace_roots)
                .context("invalid TODEX_AGENTD_PATH_MAPPINGS")?
        }
        None => validate_path_mappings(
            workspace
                .path_mappings
                .unwrap_or_default()
                .into_iter()
                .map(|mapping| PathMapping {
                    host_path: mapping.host_path,
                    backend_path: mapping.backend_path,
                })
                .collect(),
            workspace_roots,
        )
        .context("invalid workspace.path_mappings")?,
    };
    Ok(DeploymentConfig {
        kind,
        advertise_url,
        image_cli_dirs,
        path_mappings,
    })
}

/// An `http(s)` URL with a host and no credentials, query or fragment,
/// returned without a trailing `/`.
fn validate_advertise_url(value: &str) -> anyhow::Result<String> {
    // Never echo a value that may carry credentials.
    let url = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("not an absolute http(s) URL ({error})"))?;
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("the advertised URL must not contain credentials");
    }
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("{value:?} must use http or https");
    }
    if url.host_str().is_none_or(str::is_empty) {
        anyhow::bail!("{value:?} has no host");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("{value:?} must not contain a query or fragment");
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn validate_image_cli_dirs(dirs: Vec<PathBuf>, source: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut resolved: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let dir = expand_home(dir);
        if !dir.is_absolute() {
            anyhow::bail!("{source} entry {} is not an absolute path", dir.display());
        }
        if !resolved.contains(&dir) {
            resolved.push(dir);
        }
    }
    Ok(resolved)
}

/// Each `backendPath` is absolute, free of `..` and inside a workspace root
/// (checked lexically: roots may be mounted after the daemon starts); no
/// `hostPath` or `backendPath` appears twice.
fn validate_path_mappings(
    mappings: Vec<PathMapping>,
    workspace_roots: &[PathBuf],
) -> anyhow::Result<Vec<PathMapping>> {
    let mut validated: Vec<PathMapping> = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let host_path = mapping.host_path.trim().to_owned();
        let backend_path = mapping.backend_path.trim().to_owned();
        if host_path.is_empty() {
            anyhow::bail!("hostPath must not be empty");
        }
        let backend = Path::new(&backend_path);
        if !backend.is_absolute() {
            anyhow::bail!("backendPath {backend_path:?} must be an absolute path");
        }
        if backend
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            anyhow::bail!("backendPath {backend_path:?} must not contain \"..\"");
        }
        if !workspace_roots.iter().any(|root| backend.starts_with(root)) {
            anyhow::bail!("backendPath {backend_path:?} is not inside a workspace root");
        }
        if validated.iter().any(|other| other.host_path == host_path) {
            anyhow::bail!("hostPath {host_path:?} is mapped more than once");
        }
        if validated
            .iter()
            .any(|other| Path::new(&other.backend_path) == backend)
        {
            anyhow::bail!("backendPath {backend_path:?} is mapped more than once");
        }
        validated.push(PathMapping {
            host_path,
            backend_path,
        });
    }
    Ok(validated)
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).map(PathBuf::from)
}

fn env_bool(key: &str) -> Option<bool> {
    env::var(key)
        .ok()
        .and_then(|value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
}

fn env_list(key: &str) -> Option<Vec<String>> {
    env::var(key).ok().map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    })
}

fn default_grok_env_allowlist() -> Vec<String> {
    [
        "GROK_HOME",
        "GROK_CONFIG",
        "GROK_CONFIG_PATH",
        "GROK_OIDC_ISSUER",
        "GROK_OIDC_CLIENT_ID",
        "GROK_CLI_CHAT_PROXY_BASE_URL",
        "GROK_EXTRA_CA_BUNDLE",
        "XAI_API_KEY",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_devin_env_allowlist() -> Vec<String> {
    [
        "DEVIN_API_KEY",
        "DEVIN_MODEL",
        "WINDSURF_API_KEY",
        "DEVIN_EXTRA_CA_BUNDLE",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_opencode_env_allowlist() -> Vec<String> {
    [
        "OPENCODE_API_KEY",
        "OPENCODE_AUTH_CONTENT",
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_CONTENT",
        "OPENCODE_CONFIG_DIR",
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "OPENCODE_DISABLE_EXTERNAL_SKILLS",
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        "OPENCODE_EXPERIMENTAL",
        "OPENCODE_PERMISSION",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_dsh_env_allowlist() -> Vec<String> {
    [
        "DSH_HOME",
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_BASE_URL",
        "DEEPSEEK_SEARCH_BASE_URL",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn default_antigravity_env_allowlist() -> Vec<String> {
    [
        "GEMINI_API_KEY",
        "GOOGLE_GEMINI_BASE_URL",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn coalesce<T>(cli: Option<T>, env: Option<T>, file: Option<T>, default: T) -> T {
    cli.or(env).or(file).unwrap_or(default)
}

fn optional_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn load_config_document(data_dir: &PathBuf) -> anyhow::Result<DocumentMut> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create config directory {}", data_dir.display()))?;
    let path = data_dir.join("config.toml");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    if contents.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        contents
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse {}", path.display()))
    }
}

fn write_config_document(data_dir: &PathBuf, document: &DocumentMut) -> anyhow::Result<()> {
    secure_fs::ensure_owner_only_dir(data_dir)
        .with_context(|| format!("failed to create config directory {}", data_dir.display()))?;
    let path = data_dir.join("config.toml");
    secure_fs::write_owner_only_atomic(&path, document.to_string().as_bytes())
        .with_context(|| format!("failed to replace {}", path.display()))
}

fn env_workspace_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = env::var_os("TODEX_AGENTD_WORKSPACE_ROOTS")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(root) = env_path("TODEX_AGENTD_WORKSPACE_ROOT") {
        roots.insert(0, root);
    }
    roots
}

fn file_workspace_roots(file_config: &FileConfig) -> Vec<PathBuf> {
    let mut roots = file_config.workspace_roots.clone().unwrap_or_default();
    if let Some(root) = file_config.workspace_root.clone() {
        roots.insert(0, root);
    }
    roots
}

fn resolve_workspace_roots(
    cli: Vec<PathBuf>,
    env: Vec<PathBuf>,
    file: Vec<PathBuf>,
    defaults: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let roots = if !cli.is_empty() {
        cli
    } else if !env.is_empty() {
        env
    } else if !file.is_empty() {
        file
    } else {
        defaults
    };
    let mut resolved: Vec<PathBuf> = Vec::new();
    for root in roots {
        let root = expand_home(root);
        if !resolved.contains(&root) {
            resolved.push(root);
        }
    }
    resolved
}

pub(crate) fn expand_home(path: PathBuf) -> PathBuf {
    expand_home_with_home(path, env::var_os("HOME"))
}

pub(crate) fn expand_home_with_home(path: PathBuf, home: Option<OsString>) -> PathBuf {
    let Some(path_str) = path.to_str() else {
        return path;
    };

    if path_str == "~" {
        return home.map(PathBuf::from).unwrap_or(path);
    }

    if let Some(rest) = path_str.strip_prefix("~/") {
        if let Some(home) = home {
            return PathBuf::from(home).join(rest);
        }
    }

    path
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };

    use super::{
        expand_home_with_home, load_file_config, optional_non_empty,
        warn_retired_history_encryption, AgentConfig, Config, PairingEncryption, ServeArgs,
        DEFAULT_PROVIDER_IDLE_TIMEOUT_MINUTES, DEFAULT_PROVIDER_KEEPALIVE_SECONDS,
        MIN_PROVIDER_KEEPALIVE_SECONDS,
    };
    use uuid::Uuid;

    #[test]
    fn default_grok_config_is_safe() {
        let config = Config::default();
        assert_eq!(config.agent.grok_bin, "grok");
        assert!(config.agent.grok_auth_method.is_none());
        assert!(config
            .agent
            .grok_env_allowlist
            .contains(&"GROK_HOME".to_owned()));
        assert!(config
            .agent
            .grok_env_allowlist
            .contains(&"XAI_API_KEY".to_owned()));
        assert!(config
            .agent
            .grok_env_allowlist
            .iter()
            .all(|name| !name.starts_with("TODEX_AGENTD_")));
        assert_eq!(
            optional_non_empty(Some(" cached_token ".to_owned())).as_deref(),
            Some("cached_token")
        );
    }

    #[test]
    fn default_config_requires_auth() {
        let config = Config::default();

        assert!(config.security.enable_auth);
    }

    #[test]
    fn disabled_auth_requires_a_loopback_listener() {
        let none = PairingEncryption::None;
        let x25519 = PairingEncryption::X25519;
        for host in ["127.0.0.1", "::1", "[::1]"] {
            for (auth, encryption) in [(false, none), (true, none), (false, x25519)] {
                assert!(
                    super::ensure_listener_matches_auth(host, auth, encryption).is_ok(),
                    "{host}"
                );
            }
        }
        for host in ["0.0.0.0", "::", "192.168.1.20"] {
            let error = super::ensure_listener_matches_auth(host, false, x25519).unwrap_err();
            assert!(error.contains("enable_auth = false"), "{error}");
            // Remote peers must use transport v2, which needs a pairing key.
            let error = super::ensure_listener_matches_auth(host, true, none).unwrap_err();
            assert!(error.contains("pairing_encryption"), "{error}");
            for encryption in [x25519, PairingEncryption::MlKem768] {
                assert!(
                    super::ensure_listener_matches_auth(host, true, encryption).is_ok(),
                    "{host}"
                );
            }
        }
        let config = Config {
            host: "0.0.0.0".to_owned(),
            security: super::SecurityConfig {
                enable_auth: false,
                enable_tls: false,
                ..Default::default()
            },
            ..Config::default()
        };
        assert!(config.ensure_listener_matches_auth().is_err());
    }

    #[test]
    fn loaded_config_requires_auth_by_default() {
        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: None,
            workspace_root: Vec::new(),
            history_retention_days: None,
            ..Default::default()
        })
        .expect("load default config");

        assert!(config.security.enable_auth);
    }

    #[test]
    fn loaded_config_does_not_write_files() {
        let root = env::temp_dir().join(format!("todex-config-load-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: Vec::new(),
            history_retention_days: None,
            ..Default::default()
        })
        .expect("load config");

        assert!(config.security.enable_auth);
        assert!(!root.join("config.toml").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_only_config_load_does_not_generate_or_persist_auth_token() {
        let root = env::temp_dir().join(format!(
            "todex-config-read-only-test-{}",
            Uuid::new_v4().simple()
        ));

        let config = Config::load_read_only(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: Vec::new(),
            history_retention_days: None,
            ..Default::default()
        })
        .expect("load config without writes");

        assert!(config.security.enable_auth);
        assert!(!root.exists());
    }

    #[test]
    fn file_data_dir_redirect_loads_effective_config_once() {
        let root =
            env::temp_dir().join(format!("todex-config-redirect-{}", Uuid::new_v4().simple()));
        let bootstrap = root.join("bootstrap");
        let effective = bootstrap.join("effective");
        fs::create_dir_all(&effective).unwrap();
        fs::write(
            bootstrap.join("config.toml"),
            "host = \"127.0.0.2\"\ndata_dir = \"effective\"\n",
        )
        .unwrap();
        fs::write(
            effective.join("config.toml"),
            "port = 8123\n[security]\nauth_token = \"persisted\"\n",
        )
        .unwrap();

        let (resolved, config) = load_file_config(&bootstrap, true).unwrap();
        assert_eq!(resolved, effective);
        assert_eq!(config.host.as_deref(), Some("127.0.0.2"));
        assert_eq!(config.port, Some(8123));
        assert_eq!(
            config.security.unwrap().auth_token.as_deref(),
            Some("persisted")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_idle_timeout_defaults_to_an_hour_and_can_be_disabled() {
        assert_eq!(
            Config::default().agent.provider_idle_timeout_minutes,
            DEFAULT_PROVIDER_IDLE_TIMEOUT_MINUTES
        );
        let root = env::temp_dir().join(format!(
            "todex-config-idle-timeout-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.toml"),
            "[agent]\nprovider_idle_timeout_minutes = 0\n",
        )
        .unwrap();
        let config = Config::load_read_only(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: Vec::new(),
            history_retention_days: None,
            ..Default::default()
        })
        .unwrap();
        if env::var_os("TODEX_AGENTD_PROVIDER_IDLE_TIMEOUT_MINUTES").is_none() {
            assert_eq!(config.agent.provider_idle_timeout_minutes, 0);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn history_seal_min_bytes_defaults_and_parses() {
        assert_eq!(
            Config::default().history_seal_min_bytes,
            super::DEFAULT_HISTORY_SEAL_MIN_BYTES
        );
        let root =
            env::temp_dir().join(format!("todex-config-seal-min-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        let load = || {
            Config::load_read_only(ServeArgs {
                data_dir: Some(root.clone()),
                ..Default::default()
            })
            .unwrap()
            .history_seal_min_bytes
        };
        if env::var_os("TODEX_AGENTD_HISTORY_SEAL_MIN_BYTES").is_none() {
            fs::write(root.join("config.toml"), "port = 7400\n").unwrap();
            assert_eq!(load(), 2 * 1024 * 1024);
            fs::write(
                root.join("config.toml"),
                "history_seal_min_bytes = 0\n[agent]\nprovider_prewarm = true\n",
            )
            .unwrap();
            assert_eq!(load(), 0);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_keepalive_and_acp_resident_parse_with_defaults() {
        let defaults = Config::default();
        assert_eq!(
            defaults.agent.provider_keepalive(),
            Duration::from_secs(DEFAULT_PROVIDER_KEEPALIVE_SECONDS)
        );
        let root = env::temp_dir().join(format!(
            "todex-config-keepalive-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.toml"),
            "[agent]\nprovider_keepalive_seconds = 900\nprovider_prewarm = false\n\n\
             [agent.acp_profiles.kept]\ncommand = \"agent\"\n\n\
             [agent.acp_profiles.per_turn]\ncommand = \"agent\"\nresident = false\n",
        )
        .unwrap();
        let config = Config::load_read_only(ServeArgs {
            data_dir: Some(root.clone()),
            ..Default::default()
        })
        .unwrap();
        if env::var_os("TODEX_AGENTD_PROVIDER_KEEPALIVE_SECONDS").is_none() {
            assert_eq!(config.agent.provider_keepalive_seconds, 900);
            assert_eq!(config.agent.provider_keepalive(), Duration::from_secs(900));
            let mut forever = config.agent.clone();
            forever.provider_keepalive_seconds = u64::MAX;
            assert_eq!(
                forever.provider_keepalive(),
                Duration::from_secs(crate::config::MAX_PROVIDER_KEEPALIVE_SECONDS)
            );
        }
        if env::var_os("TODEX_AGENTD_PROVIDER_PREWARM").is_none() {
            assert!(!config.agent.provider_prewarm);
        }
        assert!(defaults.agent.provider_prewarm);
        assert!(config.agent.acp_profiles["kept"].resident);
        assert!(!config.agent.acp_profiles["per_turn"].resident);
        // Too short a keepalive is raised to the minimum.
        let agent = AgentConfig {
            provider_keepalive_seconds: 1,
            ..config.agent
        };
        assert_eq!(
            agent.provider_keepalive(),
            Duration::from_secs(MIN_PROVIDER_KEEPALIVE_SECONDS)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_data_dir_rejects_a_second_redirect() {
        let root = env::temp_dir().join(format!(
            "todex-config-redirect-cycle-{}",
            Uuid::new_v4().simple()
        ));
        let bootstrap = root.join("bootstrap");
        let effective = bootstrap.join("effective");
        fs::create_dir_all(&effective).unwrap();
        fs::write(bootstrap.join("config.toml"), "data_dir = \"effective\"\n").unwrap();
        fs::write(effective.join("config.toml"), "data_dir = \"next\"\n").unwrap();

        let error = load_file_config(&bootstrap, true).unwrap_err();
        assert!(error.to_string().contains("redirect only once"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn api_listener_is_off_by_default_and_loads_from_file_and_flags() {
        let root = env::temp_dir().join(format!("todex-config-api-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        let load = |args: ServeArgs| {
            Config::load(ServeArgs {
                data_dir: Some(root.clone()),
                ..args
            })
            .unwrap()
        };
        let config = load(ServeArgs::default());
        assert_eq!(config.api, super::ApiConfig::default());
        assert!(!config.api.enabled);
        assert_eq!(config.api.port, super::DEFAULT_API_PORT);
        config.ensure_api_listener_is_valid().unwrap();

        Config::save_api_settings(root.clone(), Some(true), Some(9001)).unwrap();
        let config = load(ServeArgs::default());
        assert!(config.api.enabled);
        assert_eq!(config.api.port, 9001);
        let config = load(ServeArgs {
            api_port: Some(9002),
            api_host: Some("::1".to_owned()),
            ..ServeArgs::default()
        });
        assert_eq!((config.api.host.as_str(), config.api.port), ("::1", 9002));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ipc_and_auto_update_load_from_file_with_defaults() {
        let root = env::temp_dir().join(format!("todex-config-ipc-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        let config = Config::load_saved(root.clone()).unwrap();
        assert_eq!(config.ipc, super::IpcConfig::default());
        assert!(config.ipc.enabled);
        assert!(config.update.auto_update);

        fs::write(
            root.join("config.toml"),
            "port = 7400\n[ipc]\nenabled = false\npath = \"/run/todex-ipc/agentd.sock\"\n",
        )
        .unwrap();
        Config::save_update_settings(root.clone(), false).unwrap();
        let config = Config::load_saved(root.clone()).unwrap();
        assert!(!config.ipc.enabled);
        assert_eq!(
            config.ipc.path.as_deref(),
            Some(Path::new("/run/todex-ipc/agentd.sock"))
        );
        assert!(!config.update.auto_update);
        // The rest of the file is kept.
        assert_eq!(config.port, 7400);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn listen_validation_is_shared_by_the_tui_and_the_local_admin_api() {
        assert_eq!(super::validate_host(" ::1 ").unwrap(), "::1");
        assert!(super::validate_host("localhost").is_err());
        assert!(super::validate_port("0").is_err());
        assert!(super::validate_port("70000").is_err());
        assert_eq!(super::validate_port("7345"), Ok(7345));
        assert!(super::validate_listen_port(7346, Some(7346)).is_err());
        assert_eq!(super::validate_listen_port(7346, None), Ok(7346));
    }

    #[test]
    fn api_listener_needs_its_own_port_and_consent_for_remote_plaintext() {
        let mut config = Config {
            port: 7345,
            ..Config::default()
        };
        config.api.enabled = true;
        config.ensure_api_listener_is_valid().unwrap();
        config.api.port = 7345;
        assert!(config.ensure_api_listener_is_valid().is_err());
        config.api.port = 7346;
        config.api.host = "0.0.0.0".to_owned();
        assert!(config.ensure_api_listener_is_valid().is_err());
        config.api.allow_plaintext_remote = true;
        config.ensure_api_listener_is_valid().unwrap();
        // A disabled listener is never checked.
        config.api.enabled = false;
        config.api.allow_plaintext_remote = false;
        config.ensure_api_listener_is_valid().unwrap();
    }

    #[test]
    fn workspace_roots_load_from_array_and_legacy_key() {
        let root = env::temp_dir().join(format!("todex-config-roots-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();

        fs::write(root.join("config.toml"), "workspace_root = \"/srv/one\"\n").unwrap();
        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: Vec::new(),
            history_retention_days: None,
            ..Default::default()
        })
        .expect("load legacy config");
        assert_eq!(config.workspace_roots, vec![PathBuf::from("/srv/one")]);

        fs::write(
            root.join("config.toml"),
            "workspace_root = \"/srv/one\"\nworkspace_roots = [\"/srv/two\", \"/srv/two\", \"/srv/three\"]\n",
        )
        .unwrap();
        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: Vec::new(),
            history_retention_days: None,
            ..Default::default()
        })
        .expect("load multi-root config");
        assert_eq!(
            config.workspace_roots,
            vec![
                PathBuf::from("/srv/one"),
                PathBuf::from("/srv/two"),
                PathBuf::from("/srv/three"),
            ]
        );
        assert_eq!(config.primary_workspace_root(), Path::new("/srv/one"));

        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: vec![PathBuf::from("/cli/only"), PathBuf::from("/cli/other")],
            history_retention_days: None,
            ..Default::default()
        })
        .expect("load cli config");
        assert_eq!(
            config.workspace_roots,
            vec![PathBuf::from("/cli/only"), PathBuf::from("/cli/other")]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn save_tui_settings_preserves_existing_config_sections() {
        let root = env::temp_dir().join(format!("todex-config-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp config dir");
        fs::write(
            root.join("config.toml"),
            r#"workspace_root = "/tmp/workspaces"
custom_value = "kept"

[agent]
default_agent = "codex"
codex_bin = "codex"
"#,
        )
        .expect("write config");

        Config::save_tui_settings(
            root.clone(),
            "0.0.0.0",
            8080,
            PairingEncryption::X25519,
            &[PathBuf::from("/tmp/mobile-workspaces")],
        )
        .expect("save TUI settings");
        let updated = fs::read_to_string(root.join("config.toml")).expect("read updated config");

        assert!(updated.contains("host = \"0.0.0.0\""));
        assert!(updated.contains("port = 8080"));
        assert!(updated.contains("pairing_encryption = \"x25519\""));
        assert!(updated.contains("workspace_root = \"/tmp/mobile-workspaces\""));
        assert!(updated.contains("workspace_roots = [\"/tmp/mobile-workspaces\"]"));
        assert!(updated.contains("custom_value = \"kept\""));
        assert!(updated.contains("[agent]"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn save_tui_language_round_trips_and_preserves_existing_sections() {
        let root =
            env::temp_dir().join(format!("todex-config-language-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp config dir");
        fs::write(
            root.join("config.toml"),
            "custom_value = \"kept\"\n\n[agent]\ndefault_agent = \"pi\"\n",
        )
        .expect("write config");

        Config::save_tui_language(root.clone(), "zh-CN").expect("save language");

        assert_eq!(
            Config::load_tui_language(&root).expect("load language"),
            Some("zh-CN".to_owned())
        );
        let updated = fs::read_to_string(root.join("config.toml")).expect("read config");
        assert!(updated.contains("custom_value = \"kept\""));
        assert!(updated.contains("[agent]"));
        assert!(updated.contains("[tui]"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retired_history_encryption_setting_is_ignored() {
        let root = env::temp_dir().join(format!(
            "todex-config-history-encryption-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        for value in ["\"off\"", "\"e2e\"", "\"later\"", "false"] {
            fs::write(
                root.join("config.toml"),
                format!("history_encryption = {value}\nport = 7400\n"),
            )
            .unwrap();
            let (_, file_config) = load_file_config(&root, false).unwrap();
            assert!(file_config.history_encryption.is_some(), "{value}");
            assert_eq!(file_config.port, Some(7400));
            warn_retired_history_encryption(&root);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn expand_home_with_home_supports_tilde_paths() {
        let home = OsString::from("/tmp/todex-home");

        assert_eq!(
            expand_home_with_home(PathBuf::from("~"), Some(home.clone())),
            PathBuf::from("/tmp/todex-home")
        );
        assert_eq!(
            expand_home_with_home(PathBuf::from("~/github/TodeX"), Some(home)),
            PathBuf::from("/tmp/todex-home/github/TodeX")
        );
    }

    fn deployment_from(
        vars: &[(&str, &str)],
        file: &str,
        roots: &[&str],
    ) -> anyhow::Result<super::DeploymentConfig> {
        let file: super::FileConfig = toml::from_str(file).unwrap();
        let vars: std::collections::HashMap<String, OsString> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect();
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        super::resolve_deployment_config(
            &|key| vars.get(key).cloned(),
            file.server.unwrap_or_default(),
            file.agent.unwrap_or_default().image_cli_dirs,
            file.workspace.unwrap_or_default(),
            &roots,
        )
    }

    #[test]
    fn deployment_defaults_to_host_and_blank_variables_count_as_unset() {
        let defaults = deployment_from(&[], "", &["/srv"]).unwrap();
        assert_eq!(defaults, super::DeploymentConfig::default());
        assert_eq!(defaults.kind, super::Deployment::Host);
        assert_eq!(Config::default().deployment, defaults);
        // Compose passes variables the user left blank.
        let blank = deployment_from(
            &[
                ("TODEX_AGENTD_DEPLOYMENT", " "),
                ("TODEX_AGENTD_ADVERTISE_URL", ""),
                ("TODEX_AGENTD_IMAGE_CLI_DIR", "  "),
                ("TODEX_AGENTD_PATH_MAPPINGS", ""),
            ],
            "[server]\ndeployment = \"container\"\n",
            &["/srv"],
        )
        .unwrap();
        assert_eq!(blank.kind, super::Deployment::Container);
        assert_eq!(blank.advertise_url, None);
        assert!(blank.image_cli_dirs.is_empty() && blank.path_mappings.is_empty());
    }

    #[test]
    fn deployment_parses_from_env_over_file_and_rejects_unknown_values() {
        let env_wins = deployment_from(
            &[("TODEX_AGENTD_DEPLOYMENT", "Container")],
            "[server]\ndeployment = \"host\"\n",
            &[],
        )
        .unwrap();
        assert_eq!(env_wins.kind, super::Deployment::Container);
        assert_eq!(env_wins.kind.as_str(), "container");
        for (vars, file) in [
            (vec![("TODEX_AGENTD_DEPLOYMENT", "docker")], ""),
            (vec![], "[server]\ndeployment = \"vm\"\n"),
        ] {
            let error = deployment_from(&vars, file, &[]).unwrap_err().to_string();
            assert!(error.contains("\"host\" or \"container\""), "{error}");
        }
    }

    #[test]
    fn advertise_url_accepts_plain_http_urls_only() {
        for (value, expected) in [
            ("http://127.0.0.1:7345", "http://127.0.0.1:7345"),
            ("https://todex.example/", "https://todex.example"),
            ("http://[::1]:7345", "http://[::1]:7345"),
        ] {
            let config =
                deployment_from(&[("TODEX_AGENTD_ADVERTISE_URL", value)], "", &[]).unwrap();
            assert_eq!(config.advertise_url.as_deref(), Some(expected), "{value}");
        }
        let file = deployment_from(
            &[],
            "[server]\nadvertise_url = \"http://192.168.1.20:7345\"\n",
            &[],
        )
        .unwrap();
        assert_eq!(
            file.advertise_url.as_deref(),
            Some("http://192.168.1.20:7345")
        );
        for value in [
            "127.0.0.1:7345",
            "ws://127.0.0.1:7345",
            "http://user:secret@127.0.0.1:7345",
            "http://127.0.0.1:7345/?token=1",
            "http://127.0.0.1:7345/#frag",
            "file:///tmp/x",
        ] {
            let error =
                deployment_from(&[("TODEX_AGENTD_ADVERTISE_URL", value)], "", &[]).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("TODEX_AGENTD_ADVERTISE_URL"), "{message}");
            assert!(!message.contains("secret"), "{message}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn image_cli_dirs_load_from_env_list_or_file_and_must_be_absolute() {
        let env = deployment_from(
            &[(
                "TODEX_AGENTD_IMAGE_CLI_DIR",
                "/opt/todex/agents:/opt/other:",
            )],
            "[agent]\nimage_cli_dirs = [\"/ignored\"]\n",
            &[],
        )
        .unwrap();
        assert_eq!(
            env.image_cli_dirs,
            [
                PathBuf::from("/opt/todex/agents"),
                PathBuf::from("/opt/other")
            ]
        );
        let file = deployment_from(&[], "[agent]\nimage_cli_dirs = [\"/opt/a\"]\n", &[]).unwrap();
        assert_eq!(file.image_cli_dirs, [PathBuf::from("/opt/a")]);
        assert!(
            deployment_from(&[("TODEX_AGENTD_IMAGE_CLI_DIR", "relative/bin")], "", &[]).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_mappings_are_validated_against_workspace_roots() {
        let roots = ["/workspace"];
        let env = deployment_from(
            &[(
                "TODEX_AGENTD_PATH_MAPPINGS",
                r#"[{"hostPath":"C:\\Users\\me\\code","backendPath":"/workspace/code"}]"#,
            )],
            "[[workspace.path_mappings]]\nhost_path = \"/ignored\"\nbackend_path = \"/workspace/x\"\n",
            &roots,
        )
        .unwrap();
        assert_eq!(
            env.path_mappings,
            [super::PathMapping {
                host_path: r"C:\Users\me\code".to_owned(),
                backend_path: "/workspace/code".to_owned(),
            }]
        );
        let file = deployment_from(
            &[],
            "[[workspace.path_mappings]]\nhost_path = \"/Users/me/a\"\nbackend_path = \"/workspace/a\"\n\n\
             [[workspace.path_mappings]]\nhost_path = \"/Users/me/b\"\nbackend_path = \"/workspace\"\n",
            &roots,
        )
        .unwrap();
        assert_eq!(file.path_mappings.len(), 2);

        for (mappings, expected) in [
            (
                r#"{"hostPath":"a","backendPath":"/workspace/a"}"#,
                "JSON array",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"/workspace/a","x":1}]"#,
                "JSON array",
            ),
            (
                r#"[{"hostPath":" ","backendPath":"/workspace/a"}]"#,
                "hostPath",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"workspace/a"}]"#,
                "absolute",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"/srv/a"}]"#,
                "workspace root",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"/workspace/../etc"}]"#,
                "..",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"/workspaces/a"}]"#,
                "workspace root",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"/workspace/a"},{"hostPath":"a","backendPath":"/workspace/b"}]"#,
                "hostPath",
            ),
            (
                r#"[{"hostPath":"a","backendPath":"/workspace/a"},{"hostPath":"b","backendPath":"/workspace/a/"}]"#,
                "backendPath",
            ),
        ] {
            let error = deployment_from(&[("TODEX_AGENTD_PATH_MAPPINGS", mappings)], "", &roots)
                .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(expected), "{mappings}: {message}");
        }
        assert!(deployment_from(
            &[],
            "[[workspace.path_mappings]]\nhost_path = \"a\"\nbackend_path = \"/elsewhere\"\n",
            &roots,
        )
        .is_err());
    }

    #[test]
    fn loaded_config_reads_the_server_and_workspace_sections() {
        let root = env::temp_dir().join(format!("todex-config-deploy-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace-root");
        fs::write(
            root.join("config.toml"),
            format!(
                "workspace_roots = [{workspace:?}]\n[server]\nadvertise_url = \"http://127.0.0.1:7345\"\n\n\
                 [[workspace.path_mappings]]\nhost_path = \"/host/code\"\nbackend_path = {:?}\n",
                workspace.join("code").display().to_string(),
                workspace = workspace.display().to_string(),
            ),
        )
        .unwrap();
        let config = Config::load_read_only(ServeArgs {
            data_dir: Some(root.clone()),
            ..Default::default()
        });
        let unset = |key| env::var(key).map_or(true, |value| value.trim().is_empty());
        if unset("TODEX_AGENTD_ADVERTISE_URL")
            && unset("TODEX_AGENTD_PATH_MAPPINGS")
            && unset("TODEX_AGENTD_WORKSPACE_ROOTS")
            && unset("TODEX_AGENTD_WORKSPACE_ROOT")
        {
            let config = config.unwrap();
            assert_eq!(
                config.deployment.advertise_url.as_deref(),
                Some("http://127.0.0.1:7345")
            );
            assert_eq!(config.deployment.path_mappings.len(), 1);
        }
        let _ = fs::remove_dir_all(root);
    }
}
