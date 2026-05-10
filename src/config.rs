use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use clap::Args;
use serde::Deserialize;
use toml_edit::{value, DocumentMut};
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
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub workspace_root: PathBuf,
    pub agent: AgentConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub default_agent: String,
    pub codex_bin: String,
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
    data_dir: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    agent: Option<PartialAgentConfig>,
    security: Option<PartialSecurityConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialAgentConfig {
    default_agent: Option<String>,
    codex_bin: Option<String>,
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
        let config_data_dir = args
            .data_dir
            .clone()
            .or(env_data_dir.clone())
            .unwrap_or_else(|| defaults.data_dir.clone());

        let file_config = read_config_file(config_data_dir.join("config.toml"))?;

        let data_dir = coalesce_path(
            args.data_dir,
            env_data_dir,
            file_config.data_dir,
            defaults.data_dir,
        );
        let workspace_root = coalesce_path(
            args.workspace_root,
            env_path("TODEX_AGENTD_WORKSPACE_ROOT"),
            file_config.workspace_root,
            defaults.workspace_root,
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

        let agent_file = file_config.agent.unwrap_or_default();
        let security_file = file_config.security.unwrap_or_default();
        let data_dir = expand_home(data_dir);
        let auth_token = optional_non_empty(env::var("TODEX_AGENTD_AUTH_TOKEN").ok())
            .or_else(|| optional_non_empty(security_file.auth_token))
            .or(defaults.security.auth_token);
        let auth_token = match auth_token {
            Some(token) => Some(token),
            None => {
                let token = generate_auth_token();
                Self::save_auth_token(data_dir.clone(), &token)?;
                Some(token)
            }
        };

        Ok(Config {
            host,
            port,
            data_dir,
            workspace_root: expand_home(workspace_root),
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
            },
            security: SecurityConfig {
                enable_auth: coalesce(
                    None,
                    env_bool("TODEX_AGENTD_ENABLE_AUTH"),
                    security_file.enable_auth,
                    defaults.security.enable_auth,
                ),
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

    pub fn save_host_port(data_dir: PathBuf, host: &str, port: u16) -> anyhow::Result<()> {
        let data_dir = expand_home(data_dir);
        let mut document = load_config_document(&data_dir)?;
        document["host"] = value(host);
        document["port"] = value(i64::from(port));
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
            data_dir,
            workspace_root,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
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

fn coalesce<T>(cli: Option<T>, env: Option<T>, file: Option<T>, default: T) -> T {
    cli.or(env).or(file).unwrap_or(default)
}

fn optional_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(value)
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
    let path = data_dir.join("config.toml");
    fs::write(&path, document.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
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
    use std::{env, ffi::OsString, fs, path::PathBuf};

    use super::{expand_home_with_home, Config, ServeArgs};

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
    fn save_host_port_preserves_existing_config_sections() {
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

        Config::save_host_port(root.clone(), "0.0.0.0", 8080).expect("save host port");
        let updated = fs::read_to_string(root.join("config.toml")).expect("read updated config");

        assert!(updated.contains("host = \"0.0.0.0\""));
        assert!(updated.contains("port = 8080"));
        assert!(updated.contains("workspace_root = \"/tmp/workspaces\""));
        assert!(updated.contains("custom_value = \"kept\""));
        assert!(updated.contains("[agent]"));

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
