use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table};
use uuid::Uuid;

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub workspace_root: Option<PathBuf>,
    #[arg(long)]
    pub history_retention_days: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub pairing_encryption: PairingEncryption,
    pub data_dir: PathBuf,
    pub workspace_root: PathBuf,
    pub history_retention_days: Option<u64>,
    pub agent: AgentConfig,
    pub security: SecurityConfig,
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
    pub acp_profiles: BTreeMap<String, AcpProfileConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AcpProfileConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecurityConfig {
    pub enable_auth: bool,
    pub enable_tls: bool,
    pub auth_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    host: Option<String>,
    port: Option<u16>,
    pairing_encryption: Option<PairingEncryption>,
    data_dir: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    history_retention_days: Option<u64>,
    agent: Option<PartialAgentConfig>,
    security: Option<PartialSecurityConfig>,
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
    acp_profiles: Option<BTreeMap<String, AcpProfileConfig>>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialSecurityConfig {
    enable_auth: Option<bool>,
    enable_tls: Option<bool>,
    auth_token: Option<String>,
}

impl Config {
    pub fn load(args: ServeArgs) -> anyhow::Result<Self> {
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
        let workspace_root = coalesce_path(
            args.workspace_root,
            env_path("TODEX_AGENTD_WORKSPACE_ROOT"),
            file_config.workspace_root,
            defaults.workspace_root,
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
        let pairing_encryption = coalesce(
            None,
            env::var("TODEX_AGENTD_PAIRING_ENCRYPTION")
                .ok()
                .and_then(|value| PairingEncryption::parse(&value)),
            file_config.pairing_encryption,
            defaults.pairing_encryption,
        );

        let agent_file = file_config.agent.unwrap_or_default();
        let security_file = file_config.security.unwrap_or_default();
        let enable_auth = coalesce(
            None,
            env_bool("TODEX_AGENTD_ENABLE_AUTH"),
            security_file.enable_auth,
            defaults.security.enable_auth,
        );
        let auth_token = if enable_auth {
            let configured = optional_non_empty(env::var("TODEX_AGENTD_AUTH_TOKEN").ok())
                .or_else(|| optional_non_empty(security_file.auth_token))
                .or(defaults.security.auth_token);
            match configured {
                Some(token) => Some(token),
                None => {
                    let token = generate_auth_token();
                    Self::save_auth_token(data_dir.clone(), &token)?;
                    Some(token)
                }
            }
        } else {
            None
        };

        Ok(Config {
            host,
            port,
            pairing_encryption,
            data_dir,
            workspace_root: expand_home(workspace_root),
            history_retention_days,
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
                acp_profiles: agent_file
                    .acp_profiles
                    .unwrap_or(defaults.agent.acp_profiles),
            },
            security: SecurityConfig {
                enable_auth,
                enable_tls: coalesce(
                    None,
                    env_bool("TODEX_AGENTD_ENABLE_TLS"),
                    security_file.enable_tls,
                    defaults.security.enable_tls,
                ),
                auth_token,
            },
        })
    }

    pub fn save_tui_settings(
        data_dir: PathBuf,
        host: &str,
        port: u16,
        pairing_encryption: PairingEncryption,
        workspace_root: &Path,
    ) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        document["host"] = value(host);
        document["port"] = value(i64::from(port));
        document["pairing_encryption"] = value(pairing_encryption.as_str());
        document["workspace_root"] = value(workspace_root.display().to_string());
        write_config_document(&data_dir, &document)?;
        Ok(())
    }

    pub fn save_auth_token(data_dir: PathBuf, auth_token: &str) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        document["security"]["auth_token"] = value(auth_token);
        write_config_document(&data_dir, &document)?;
        Ok(())
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

    pub fn reset_auth_token(data_dir: PathBuf) -> anyhow::Result<String> {
        let token = generate_auth_token();
        Self::save_auth_token(data_dir, &token)?;
        Ok(token)
    }
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
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_root,
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: default_grok_env_allowlist(),
                acp_profiles: BTreeMap::new(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: None,
            },
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
    replace_some!(base.data_dir, overlay.data_dir);
    replace_some!(base.workspace_root, overlay.workspace_root);
    replace_some!(base.history_retention_days, overlay.history_retention_days);

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
        replace_some!(base_agent.acp_profiles, overlay_agent.acp_profiles);
    }
    if let Some(overlay_security) = overlay.security {
        let base_security = base
            .security
            .get_or_insert_with(PartialSecurityConfig::default);
        replace_some!(base_security.enable_auth, overlay_security.enable_auth);
        replace_some!(base_security.enable_tls, overlay_security.enable_tls);
        replace_some!(base_security.auth_token, overlay_security.auth_token);
    }
    base
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

fn generate_auth_token() -> String {
    format!("todex_{}", Uuid::new_v4().simple())
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
    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create config directory {}", data_dir.display()))?;
    set_owner_only(data_dir, true)?;
    let path = data_dir.join("config.toml");
    let temporary = data_dir.join(format!(".config.{}.tmp", Uuid::new_v4().simple()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(document.to_string().as_bytes())
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.display()))?;
    drop(file);
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to replace {}", path.display()))?;
    }
    fs::rename(&temporary, &path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    set_owner_only(&path, false)?;
    Ok(())
}

fn set_owner_only(path: &Path, directory: bool) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o700 } else { 0o600 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}

fn coalesce_path(
    cli: Option<PathBuf>,
    env: Option<PathBuf>,
    file: Option<PathBuf>,
    default: PathBuf,
) -> PathBuf {
    coalesce(cli, env, file, default)
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
    };

    use super::{
        expand_home_with_home, load_file_config, optional_non_empty, Config, PairingEncryption,
        ServeArgs,
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
        assert!(config.security.auth_token.is_none());
    }

    #[test]
    fn loaded_config_requires_auth_by_default() {
        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: None,
            workspace_root: None,
            history_retention_days: None,
        })
        .expect("load default config");

        assert!(config.security.enable_auth);
    }

    #[test]
    fn loaded_config_generates_and_persists_auth_token() {
        let root = env::temp_dir().join(format!("todex-config-token-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let config = Config::load(ServeArgs {
            host: None,
            port: None,
            data_dir: Some(root.clone()),
            workspace_root: None,
            history_retention_days: None,
        })
        .expect("load config and generate auth token");

        let token = config
            .security
            .auth_token
            .as_ref()
            .expect("generated token should be present");
        assert!(token.starts_with("todex_"));

        let updated = fs::read_to_string(root.join("config.toml")).expect("read config");
        assert!(updated.contains("auth_token"));
        assert!(updated.contains(token));

        let _ = fs::remove_dir_all(root);
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
    fn reset_auth_token_replaces_persisted_token() {
        let root = env::temp_dir().join(format!(
            "todex-config-reset-token-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);

        let first = Config::reset_auth_token(root.clone()).expect("write first token");
        let second = Config::reset_auth_token(root.clone()).expect("write second token");

        assert!(first.starts_with("todex_"));
        assert!(second.starts_with("todex_"));
        assert_ne!(first, second);

        let updated = fs::read_to_string(root.join("config.toml")).expect("read config");
        assert!(updated.contains(&second));
        assert!(!updated.contains(&first));

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
            Path::new("/tmp/mobile-workspaces"),
        )
        .expect("save TUI settings");
        let updated = fs::read_to_string(root.join("config.toml")).expect("read updated config");

        assert!(updated.contains("host = \"0.0.0.0\""));
        assert!(updated.contains("port = 8080"));
        assert!(updated.contains("pairing_encryption = \"x25519\""));
        assert!(updated.contains("workspace_root = \"/tmp/mobile-workspaces\""));
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
}
