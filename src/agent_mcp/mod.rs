//! MCP tools the daemon hosts for the agents it launches.
//!
//! The daemon serves a Streamable HTTP MCP endpoint on its own listener
//! ([`ROUTE`], outside device auth). Providers reach it through a tiny stdio
//! bridge (`todex-agentd ssh-mcp-bridge`) injected into their MCP config. The
//! endpoint only accepts loopback peers presenting a per-conversation bearer
//! token, so every tool call is attributed to one conversation. Tokens live in
//! memory only: a daemon restart invalidates them along with every provider
//! process that carried them.
//!
//! Injection happens only while at least one SSH host has agent access, so
//! provider arguments stay unchanged for everyone who never enables it.

mod bridge;
mod server;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{error::AppError, secure_fs, ssh::SshService};

pub(crate) use bridge::run_bridge;
pub(crate) use server::routes;

/// MCP server name the agents see; tools appear as e.g. `todex_ssh.ssh_exec`.
pub(crate) const SERVER_NAME: &str = "todex_ssh";
pub(crate) const ROUTE: &str = "/internal/agent-mcp/ssh";
/// Hidden subcommand that bridges stdio MCP to [`ROUTE`].
pub(crate) const BRIDGE_SUBCOMMAND: &str = "ssh-mcp-bridge";
/// Not `TODEX_AGENTD_*`: provider launches strip that prefix from their env.
pub(crate) const URL_ENV: &str = "TODEX_SSH_MCP_URL";
pub(crate) const TOKEN_ENV: &str = "TODEX_SSH_MCP_TOKEN";
/// Codex and Claude stop waiting for a tool call after their own timeout;
/// leave room for the longest `ssh_exec` plus connection setup.
const PROVIDER_TOOL_TIMEOUT_SECONDS: u64 = server::MAX_TIMEOUT_SECONDS + 60;
const STATE_DIR: &str = "agent-mcp";

/// Per-conversation tokens and the endpoint agents connect to.
#[derive(Clone)]
pub struct AgentMcp {
    inner: Arc<Inner>,
}

struct Inner {
    ssh: SshService,
    /// conversation id → bearer token.
    tokens: Mutex<HashMap<String, String>>,
    /// `http://<loopback>:<port>/internal/agent-mcp/ssh`, set once bound.
    endpoint: OnceLock<String>,
    /// The bridge binary; `None` disables injection.
    bridge_command: Option<PathBuf>,
    state_dir: PathBuf,
}

impl AgentMcp {
    pub async fn new(data_dir: &std::path::Path, ssh: SshService) -> Result<Self, AppError> {
        let bridge_command = match std::env::current_exe() {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(%error, "agent SSH tools disabled: cannot locate the daemon executable");
                None
            }
        };
        Self::with_bridge_command(data_dir, ssh, bridge_command).await
    }

    pub(crate) async fn with_bridge_command(
        data_dir: &std::path::Path,
        ssh: SshService,
        bridge_command: Option<PathBuf>,
    ) -> Result<Self, AppError> {
        let state_dir = data_dir.join(STATE_DIR);
        // Configs written for a previous daemon carry dead tokens.
        match tokio::fs::remove_dir_all(&state_dir).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        {
            let state_dir = state_dir.clone();
            tokio::task::spawn_blocking(move || secure_fs::ensure_owner_only_dir(&state_dir))
                .await
                .map_err(|error| AppError::Anyhow(error.into()))??;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                ssh,
                tokens: Mutex::new(HashMap::new()),
                endpoint: OnceLock::new(),
                bridge_command,
                state_dir,
            }),
        })
    }

    pub(crate) fn ssh(&self) -> &SshService {
        &self.inner.ssh
    }

    /// Records the bound listener. Agents connect over loopback; a listener
    /// bound to one non-loopback interface is unreachable that way, so the
    /// tools stay disabled.
    pub fn set_listen_addr(&self, addr: SocketAddr) {
        let ip = match addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip if ip.is_loopback() => ip,
            ip => {
                tracing::warn!(%ip, "agent SSH tools disabled: the listener is not reachable over loopback");
                return;
            }
        };
        let endpoint = format!("http://{}{ROUTE}", SocketAddr::new(ip, addr.port()));
        if self.inner.endpoint.set(endpoint).is_err() {
            tracing::warn!("agent MCP listen address was already set");
        }
    }

    /// The MCP server to inject for `conversation_id`, or `None` when no host
    /// has agent access (or the endpoint is unavailable).
    pub(crate) async fn launch(&self, conversation_id: &str) -> Option<AgentMcpServer> {
        if !self.inner.ssh.has_agent_hosts().await {
            return None;
        }
        let endpoint = self.inner.endpoint.get()?;
        let command = self.inner.bridge_command.clone()?;
        let token = self.token_for(conversation_id);
        Some(AgentMcpServer {
            command,
            env: vec![
                (URL_ENV.to_owned(), endpoint.clone()),
                (TOKEN_ENV.to_owned(), token),
            ],
            claude_config_path: self
                .inner
                .state_dir
                .join(format!("claude-{}.json", file_key(conversation_id))),
        })
    }

    /// The conversation's token, created on first use. It stays stable for
    /// the daemon's lifetime because long-lived providers (Codex app-server,
    /// ACP agents) keep the MCP config of their first turn.
    fn token_for(&self, conversation_id: &str) -> String {
        let mut tokens = self.inner.tokens.lock().expect("agent MCP token lock");
        tokens
            .entry(conversation_id.to_owned())
            .or_insert_with(new_token)
            .clone()
    }

    /// The conversation a presented token belongs to. Every stored token is
    /// compared in constant time, without stopping at a match.
    pub(crate) fn authenticate(&self, presented: &str) -> Option<String> {
        let tokens = self.inner.tokens.lock().expect("agent MCP token lock");
        let mut found = None;
        for (conversation_id, token) in tokens.iter() {
            if bool::from(token.as_bytes().ct_eq(presented.as_bytes())) {
                found = Some(conversation_id.clone());
            }
        }
        found
    }

    /// Invalidates the conversation's token (conversation deleted/expired).
    pub(crate) async fn revoke(&self, conversation_id: &str) {
        self.inner
            .tokens
            .lock()
            .expect("agent MCP token lock")
            .remove(conversation_id);
        let path = self
            .inner
            .state_dir
            .join(format!("claude-{}.json", file_key(conversation_id)));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to remove agent MCP config")
            }
        }
    }
}

fn new_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Conversation ids are client-influenced; never use them as file names.
fn file_key(conversation_id: &str) -> String {
    hex(&Sha256::digest(conversation_id.as_bytes())[..16])
}

/// How a provider launches the `todex_ssh` stdio bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMcpServer {
    pub command: PathBuf,
    pub env: Vec<(String, String)>,
    claude_config_path: PathBuf,
}

impl AgentMcpServer {
    pub(crate) fn args(&self) -> Vec<String> {
        vec![BRIDGE_SUBCOMMAND.to_owned()]
    }

    /// Codex `config` override (dotted key, so the user's own
    /// `mcp_servers` table is merged rather than replaced). Tools are
    /// pre-approved: SSH access is granted per host in TodeX instead.
    pub(crate) fn codex_config(&self) -> (String, Value) {
        let env: serde_json::Map<String, Value> = self
            .env
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        (
            format!("mcp_servers.{SERVER_NAME}"),
            json!({
                "command": self.command,
                "args": self.args(),
                "env": env,
                "default_tools_approval_mode": "approve",
                "tool_timeout_sec": PROVIDER_TOOL_TIMEOUT_SECONDS,
            }),
        )
    }

    /// Claude Code arguments. The config goes to an owner-only file rather
    /// than the command line, where other local users could read the token.
    pub(crate) async fn claude_args(&self) -> Result<Vec<String>, AppError> {
        let env: serde_json::Map<String, Value> = self
            .env
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        let config = serde_json::to_vec(&json!({
            "mcpServers": {
                SERVER_NAME: {
                    "type": "stdio",
                    "command": self.command,
                    "args": self.args(),
                    "env": env,
                }
            }
        }))?;
        let path = self.claude_config_path.clone();
        let written = path.clone();
        tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&written, &config))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        // `=` form: both options are variadic and would otherwise swallow
        // any argument that follows them.
        Ok(vec![
            format!("--mcp-config={}", path.display()),
            // A server-level permission rule: every todex_ssh tool runs
            // without a prompt.
            format!("--allowedTools=mcp__{SERVER_NAME}"),
        ])
    }

    /// ACP `McpServerStdio` entry for `session/new|load|resume`.
    pub(crate) fn acp_server(&self) -> Value {
        json!({
            "name": SERVER_NAME,
            "command": self.command,
            "args": self.args(),
            "env": self
                .env
                .iter()
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An `AgentMcp` on a fixture ssh service with a loopback endpoint.
    pub(crate) async fn registry(ssh: SshService, root: &std::path::Path) -> AgentMcp {
        let mcp = AgentMcp::with_bridge_command(
            &root.join("data"),
            ssh,
            Some(PathBuf::from("/opt/todex/todex-agentd")),
        )
        .await
        .unwrap();
        mcp.set_listen_addr("0.0.0.0:7345".parse().unwrap());
        mcp
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokens_are_per_conversation_stable_and_revocable() {
        let fixture = crate::ssh::tests::fixture("Host web\n").await;
        let mcp = registry(fixture.service.clone(), &fixture.root).await;
        // No host has agent access yet: nothing is injected.
        assert!(mcp.launch("conv_a").await.is_none());
        fixture.service.set_agent_access("web", true).await.unwrap();

        let a = mcp.launch("conv_a").await.unwrap();
        let b = mcp.launch("conv_b").await.unwrap();
        assert_eq!(
            a.env[0],
            (
                URL_ENV.to_owned(),
                "http://127.0.0.1:7345/internal/agent-mcp/ssh".to_owned()
            )
        );
        let token_a = a.env[1].1.clone();
        assert_eq!(token_a.len(), 64);
        assert_ne!(token_a, b.env[1].1);
        assert_eq!(mcp.launch("conv_a").await.unwrap().env[1].1, token_a);

        assert_eq!(mcp.authenticate(&token_a).as_deref(), Some("conv_a"));
        assert_eq!(mcp.authenticate(""), None);
        assert_eq!(mcp.authenticate(&token_a[..63]), None);
        assert_eq!(mcp.authenticate(&format!("{token_a}0")), None);

        let args = a.claude_args().await.unwrap();
        let config_path = PathBuf::from(args[0].strip_prefix("--mcp-config=").unwrap());
        assert!(config_path.is_file());
        mcp.revoke("conv_a").await;
        assert_eq!(mcp.authenticate(&token_a), None);
        assert!(!config_path.exists());
        assert!(mcp.authenticate(&b.env[1].1).is_some());
    }

    #[test]
    fn launch_formats_match_each_provider_schema() {
        let server = AgentMcpServer {
            command: PathBuf::from("/opt/todex/todex-agentd"),
            env: vec![
                (URL_ENV.to_owned(), "http://127.0.0.1:1/x".to_owned()),
                (TOKEN_ENV.to_owned(), "t".to_owned()),
            ],
            claude_config_path: PathBuf::from("/tmp/x.json"),
        };
        let (key, codex) = server.codex_config();
        assert_eq!(key, "mcp_servers.todex_ssh");
        assert_eq!(codex["args"], json!(["ssh-mcp-bridge"]));
        assert_eq!(codex["env"][TOKEN_ENV], "t");
        assert_eq!(codex["default_tools_approval_mode"], "approve");
        let acp = server.acp_server();
        assert_eq!(acp["name"], "todex_ssh");
        assert_eq!(acp["env"][1], json!({ "name": TOKEN_ENV, "value": "t" }));
        // The typed ACP schema accepts the entry as a stdio server.
        let parsed: agent_client_protocol::schema::v1::McpServer =
            serde_json::from_value(acp).unwrap();
        assert!(matches!(
            parsed,
            agent_client_protocol::schema::v1::McpServer::Stdio(_)
        ));
    }

    #[tokio::test]
    async fn non_loopback_listener_disables_injection() {
        let root = std::env::temp_dir().join(format!("todex-agent-mcp-{}", uuid::Uuid::new_v4()));
        let ssh = SshService::with_home(&root.join("data"), "ssh".into(), None)
            .await
            .unwrap();
        let mcp = AgentMcp::with_bridge_command(&root.join("data"), ssh, Some("x".into()))
            .await
            .unwrap();
        mcp.set_listen_addr("192.168.1.20:7345".parse().unwrap());
        assert!(mcp.inner.endpoint.get().is_none());
        mcp.set_listen_addr("[::]:7345".parse().unwrap());
        assert_eq!(
            mcp.inner.endpoint.get().unwrap(),
            "http://[::1]:7345/internal/agent-mcp/ssh"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
