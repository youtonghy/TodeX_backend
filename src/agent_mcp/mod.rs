//! MCP tools the daemon hosts for the agents it launches.
//!
//! The daemon serves Streamable HTTP MCP endpoints on its own listener (one
//! route per server, outside device auth). Providers reach them through a
//! tiny stdio bridge (`todex-agentd agent-mcp-bridge`) injected into their
//! MCP config, once per server. The endpoints only accept loopback peers
//! presenting a per-conversation bearer token, so every tool call is
//! attributed to one conversation. Tokens live in memory only: a daemon
//! restart invalidates them along with every provider process that carried
//! them.
//!
//! Each server is injected only while its feature is enabled (`todex_ssh`:
//! at least one SSH host has agent access; `todex_desktop`: desktop tools
//! are switched on), so provider arguments stay unchanged for everyone who
//! never enables one.
//!
//! Tools are declared in a [`registry`]; every prompt they raise goes
//! through the [`authorizer`], which also applies the conversation's
//! permission mode to tools with side effects. The token only separates
//! conversations: software running as the same OS user can read it from
//! the provider's environment, so per-conversation grants are a guard
//! against agents acting unasked, not a security boundary.

mod agy_hook;
mod authorizer;
mod bridge;
mod desktop_computer;
mod desktop_server;
mod registry;
mod server;

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{
    agent_desktop::AgentDesktop, error::AppError, provider::ConversationSupervisor, secure_fs,
    ssh::SshService,
};

pub(crate) use agy_hook::run_hook as run_agy_hook;
use authorizer::{Authorizer, AuthorizerState, ToolMode};
pub(crate) use bridge::run_bridge;

/// Every agent MCP endpoint, behind the same loopback + token guard.
pub(crate) fn routes(
    state: &crate::app_state::AppState,
) -> axum::Router<crate::app_state::AppState> {
    server::routes(state)
        .merge(desktop_server::routes(state))
        .merge(agy_hook::routes())
}

/// MCP server name the agents see; tools appear as e.g. `todex_ssh.ssh_exec`.
pub(crate) const SSH_SERVER: &str = "todex_ssh";
pub(crate) const SSH_ROUTE: &str = "/internal/agent-mcp/ssh";
/// Hidden subcommand that bridges stdio MCP to one server's route.
pub(crate) const BRIDGE_SUBCOMMAND: &str = "agent-mcp-bridge";
/// Earlier name of [`BRIDGE_SUBCOMMAND`], kept as an alias.
pub(crate) const LEGACY_BRIDGE_SUBCOMMAND: &str = "ssh-mcp-bridge";
/// Not `TODEX_AGENTD_*`: provider launches strip that prefix from their env.
pub(crate) const URL_ENV: &str = "TODEX_AGENT_MCP_URL";
pub(crate) const TOKEN_ENV: &str = "TODEX_AGENT_MCP_TOKEN";
/// Names [`LEGACY_BRIDGE_SUBCOMMAND`] was launched with.
pub(crate) const LEGACY_URL_ENV: &str = "TODEX_SSH_MCP_URL";
pub(crate) const LEGACY_TOKEN_ENV: &str = "TODEX_SSH_MCP_TOKEN";
/// Codex and Claude stop waiting for a tool call after their own timeout;
/// leave room for an approval (ask mode), the longest `ssh_exec` and
/// connection setup.
const SSH_TOOL_TIMEOUT_SECONDS: u64 =
    authorizer::CONFIRM_TIMEOUT.as_secs() + server::MAX_TIMEOUT_SECONDS + 60;
/// For providers whose MCP servers and hooks come from one static global
/// config (Antigravity): the daemon's base URL and the routes enabled for
/// the turn, passed in the provider's environment; each static entry names
/// its route with `--route`.
pub(crate) const ENDPOINT_ENV: &str = "TODEX_AGENT_MCP_ENDPOINT";
pub(crate) const ROUTES_ENV: &str = "TODEX_AGENT_MCP_ROUTES";
/// Bridge argument naming the route of a static config entry.
pub(crate) const ROUTE_ARG: &str = "--route";
/// Hidden subcommand an Antigravity `PreToolUse` hook runs; see [`agy_hook`].
pub(crate) const AGY_HOOK_SUBCOMMAND: &str = "agy-hook";
pub(crate) const AGY_HOOK_ROUTE: &str = "/internal/agent-mcp/agy-permission";
pub(crate) const DESKTOP_SERVER: &str = "todex_desktop";
pub(crate) const DESKTOP_ROUTE: &str = "/internal/agent-mcp/desktop";
const STATE_DIR: &str = "agent-mcp";

/// Per-conversation tokens and the endpoint agents connect to.
#[derive(Clone)]
pub struct AgentMcp {
    inner: Arc<Inner>,
}

struct Inner {
    ssh: SshService,
    desktop: AgentDesktop,
    /// conversation id → bearer token.
    tokens: Mutex<HashMap<String, String>>,
    /// conversation id → MCP sessions using its token, for removing the
    /// Claude config file once nothing needs it.
    sessions: Mutex<HashMap<String, Sessions>>,
    authorizer: Arc<AuthorizerState>,
    /// conversation id → the running turn's (permission mode, work mode),
    /// for gates that tell `auto` and `full-access` apart.
    turn_modes: Mutex<HashMap<String, (String, String)>>,
    /// `http://<loopback>:<port>`, set once bound.
    endpoint: OnceLock<String>,
    /// The bridge binary; `None` disables injection.
    bridge_command: Option<PathBuf>,
    state_dir: PathBuf,
}

impl AgentMcp {
    pub async fn new(
        data_dir: &std::path::Path,
        ssh: SshService,
        desktop: AgentDesktop,
    ) -> Result<Self, AppError> {
        let bridge_command = match std::env::current_exe() {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(%error, "agent SSH tools disabled: cannot locate the daemon executable");
                None
            }
        };
        Self::with_bridge_command(data_dir, ssh, desktop, bridge_command).await
    }

    pub(crate) async fn with_bridge_command(
        data_dir: &std::path::Path,
        ssh: SshService,
        desktop: AgentDesktop,
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
                desktop,
                tokens: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                authorizer: Arc::new(AuthorizerState::default()),
                turn_modes: Mutex::new(HashMap::new()),
                endpoint: OnceLock::new(),
                bridge_command,
                state_dir,
            }),
        })
    }

    pub(crate) fn ssh(&self) -> &SshService {
        &self.inner.ssh
    }

    pub(crate) fn desktop(&self) -> &AgentDesktop {
        &self.inner.desktop
    }

    /// Takes back a conversation's Computer Use: grant, approved apps and
    /// screen lease (stopping the calls running under it), and voids the
    /// host answers still waiting for it. Every user-facing revoke (stop
    /// button, REST, settings) goes through here. Returns the grant and
    /// whether a screen session ended.
    pub(crate) fn revoke_computer(
        &self,
        conversation_id: &str,
    ) -> (Option<crate::agent_desktop::Grant>, bool) {
        let revoked = self.inner.desktop.revoke_computer(conversation_id);
        self.inner.authorizer.forget_host_answers(conversation_id);
        revoked
    }

    /// Persists the desktop switches; turning one off revokes what it
    /// covered (see [`AgentDesktop::update_settings`]) and voids the revoked
    /// conversations' pending host answers.
    pub(crate) async fn update_desktop_settings(
        &self,
        enabled: Option<bool>,
        computer_enabled: Option<bool>,
    ) -> Result<crate::agent_desktop::SettingsChange, AppError> {
        let change = self
            .inner
            .desktop
            .update_settings(enabled, computer_enabled)
            .await?;
        for conversation_id in &change.computer_revoked {
            self.inner.authorizer.forget_host_answers(conversation_id);
        }
        Ok(change)
    }

    /// The prompts of one tool call.
    fn authorizer<'a>(&'a self, conversations: &'a ConversationSupervisor) -> Authorizer<'a> {
        Authorizer {
            state: &self.inner.authorizer,
            conversations,
            desktop: &self.inner.desktop,
        }
    }

    /// The effective permission mode of the conversation's turn that just
    /// started. Tools with side effects follow it until the turn ends:
    /// refused while planning, approved per call in `ask`, free in `auto` /
    /// `full-access`.
    pub(crate) fn record_turn_mode(
        &self,
        conversation_id: &str,
        permission_mode: &str,
        work_mode: &str,
    ) {
        self.inner.authorizer.set_mode(
            conversation_id,
            ToolMode::from_turn(permission_mode, work_mode),
        );
        self.turn_modes().insert(
            conversation_id.to_owned(),
            (permission_mode.to_owned(), work_mode.to_owned()),
        );
    }

    /// The running turn's (permission mode, work mode); `None` between turns.
    pub(crate) fn turn_mode(&self, conversation_id: &str) -> Option<(String, String)> {
        self.turn_modes().get(conversation_id).cloned()
    }

    fn turn_modes(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, String)>> {
        self.inner
            .turn_modes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The conversation's turn ended (completed, failed or cancelled): its
    /// side-effect tools ask again until the next turn records its mode.
    pub(crate) fn end_turn_mode(&self, conversation_id: &str) {
        self.inner.authorizer.clear_mode(conversation_id);
        self.turn_modes().remove(conversation_id);
    }

    #[cfg(test)]
    pub(crate) fn clear_declines_for_tests(&self, conversation_id: &str) {
        self.inner.authorizer.clear_declines(conversation_id);
    }

    #[cfg(test)]
    pub(crate) fn authorizer_state(&self) -> &Arc<AuthorizerState> {
        &self.inner.authorizer
    }

    fn claude_config_path(&self, conversation_id: &str) -> PathBuf {
        self.inner
            .state_dir
            .join(format!("claude-{}.json", file_key(conversation_id)))
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sessions>> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An agent's bridge opened an MCP session with the conversation's
    /// token (Claude had read its config by then).
    pub(crate) fn session_opened(&self, conversation_id: &str) {
        let mut sessions = self.sessions();
        let entry = sessions.entry(conversation_id.to_owned()).or_default();
        entry.open += 1;
        entry.last_opened = Some(Instant::now());
    }

    /// A bridge closed its MCP session (its agent exited). Once none is
    /// open and no newer provider launch is still to read it, the Claude
    /// config holding the token is removed. Claude Code reads
    /// `--mcp-config` at startup; whether a manual `/mcp reconnect` reads it
    /// again is undocumented, so it is kept while any session is open.
    pub(crate) async fn session_closed(&self, conversation_id: &str) {
        let remove = {
            let mut sessions = self.sessions();
            let Some(entry) = sessions.get_mut(conversation_id) else {
                return;
            };
            entry.open = entry.open.saturating_sub(1);
            entry.open == 0
                && entry
                    .last_opened
                    .is_some_and(|opened| entry.launched.is_none_or(|launched| launched <= opened))
        };
        if remove {
            remove_config(&self.claude_config_path(conversation_id)).await;
        }
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
        let endpoint = format!("http://{}", SocketAddr::new(ip, addr.port()));
        if self.inner.endpoint.set(endpoint).is_err() {
            tracing::warn!("agent MCP listen address was already set");
        }
    }

    /// The MCP servers to inject for `conversation_id`, or `None` when no
    /// server is enabled (or the endpoint is unavailable).
    pub(crate) async fn launch(&self, conversation_id: &str) -> Option<AgentMcpLaunch> {
        let enabled = self.enabled_servers().await;
        if enabled.is_empty() {
            return None;
        }
        self.launch_servers(conversation_id, enabled)
    }

    /// For a provider reading TodeX's servers and approval hook from a static
    /// global config: the enabled servers (possibly none) plus the variables
    /// that point the static entries at this conversation. `None` only when
    /// the endpoint or the daemon executable is unavailable.
    pub(crate) async fn launch_global(&self, conversation_id: &str) -> Option<AgentMcpLaunch> {
        let enabled = self.enabled_servers().await;
        let mut launch = self.launch_servers(conversation_id, enabled)?;
        let endpoint = self.inner.endpoint.get()?.clone();
        let routes = launch
            .servers
            .iter()
            .map(|server| server.route)
            .collect::<Vec<_>>()
            .join(",");
        launch.global = Some(GlobalLaunch {
            daemon: self.inner.bridge_command.clone()?,
            env: vec![
                (ENDPOINT_ENV.to_owned(), endpoint),
                (TOKEN_ENV.to_owned(), self.token_for(conversation_id)),
                (ROUTES_ENV.to_owned(), routes),
            ],
        });
        Some(launch)
    }

    async fn enabled_servers(&self) -> Vec<(&'static str, &'static str, u64)> {
        let mut enabled = Vec::new();
        if self.inner.ssh.has_agent_hosts().await {
            enabled.push((SSH_SERVER, SSH_ROUTE, SSH_TOOL_TIMEOUT_SECONDS));
        }
        if self.inner.desktop.enabled().await {
            enabled.push((
                DESKTOP_SERVER,
                DESKTOP_ROUTE,
                desktop_server::PROVIDER_TOOL_TIMEOUT_SECONDS,
            ));
        }
        enabled
    }

    fn launch_servers(
        &self,
        conversation_id: &str,
        enabled: Vec<(&'static str, &'static str, u64)>,
    ) -> Option<AgentMcpLaunch> {
        let endpoint = self.inner.endpoint.get()?;
        let command = self.inner.bridge_command.clone()?;
        let token = self.token_for(conversation_id);
        self.sessions()
            .entry(conversation_id.to_owned())
            .or_default()
            .launched = Some(Instant::now());
        Some(AgentMcpLaunch {
            servers: enabled
                .into_iter()
                .map(|(name, route, tool_timeout_seconds)| AgentMcpServer {
                    name,
                    route,
                    command: command.clone(),
                    env: vec![
                        (URL_ENV.to_owned(), format!("{endpoint}{route}")),
                        (TOKEN_ENV.to_owned(), token.clone()),
                    ],
                    tool_timeout_seconds,
                })
                .collect(),
            config_file: self.claude_config_path(conversation_id),
            global: None,
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

    /// Invalidates the conversation's token and desktop grant (conversation
    /// deleted/expired).
    pub(crate) async fn revoke(&self, conversation_id: &str) {
        self.inner.desktop.forget(conversation_id).await;
        self.inner.authorizer.forget(conversation_id);
        self.turn_modes().remove(conversation_id);
        self.inner
            .tokens
            .lock()
            .expect("agent MCP token lock")
            .remove(conversation_id);
        self.sessions().remove(conversation_id);
        remove_config(&self.claude_config_path(conversation_id)).await;
    }
}

/// MCP sessions of one conversation's token.
#[derive(Default)]
struct Sessions {
    open: usize,
    last_opened: Option<Instant>,
    /// The latest provider launch, which may still have to read the config.
    launched: Option<Instant>,
}

async fn remove_config(path: &std::path::Path) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to remove agent MCP config")
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

/// How a provider launches one server's stdio bridge. Provider-neutral: each
/// provider adapter renders it in its own config format
/// (`crate::provider::mcp_injection`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMcpServer {
    pub name: &'static str,
    /// The daemon route the bridge relays to.
    pub(crate) route: &'static str,
    pub command: PathBuf,
    pub env: Vec<(String, String)>,
    /// How long the provider waits for one tool call.
    pub(crate) tool_timeout_seconds: u64,
}

impl AgentMcpServer {
    pub(crate) fn args(&self) -> Vec<String> {
        vec![BRIDGE_SUBCOMMAND.to_owned()]
    }
}

/// Every server injected into one conversation's provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMcpLaunch {
    pub servers: Vec<AgentMcpServer>,
    /// Owner-only file for providers that read their MCP config from disk
    /// (removed on [`AgentMcp::revoke`]).
    pub(crate) config_file: PathBuf,
    /// Set by [`AgentMcp::launch_global`].
    pub(crate) global: Option<GlobalLaunch>,
}

/// What a provider with a static global config of TodeX's servers and hook
/// needs per turn: the executable those entries run, and the variables its
/// process must carry so they reach this conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalLaunch {
    pub(crate) daemon: PathBuf,
    pub(crate) env: Vec<(String, String)>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An `AgentMcp` on a fixture ssh service with a loopback endpoint.
    #[cfg(unix)]
    pub(crate) async fn registry(ssh: SshService, root: &std::path::Path) -> AgentMcp {
        let desktop = AgentDesktop::load(&root.join("data")).await.unwrap();
        let mcp = AgentMcp::with_bridge_command(
            &root.join("data"),
            ssh,
            desktop,
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
        assert_eq!(a.servers.len(), 1);
        assert_eq!(
            a.servers[0].env[0],
            (
                URL_ENV.to_owned(),
                "http://127.0.0.1:7345/internal/agent-mcp/ssh".to_owned()
            )
        );
        let token_a = a.servers[0].env[1].1.clone();
        assert_eq!(token_a.len(), 64);
        assert_ne!(token_a, b.servers[0].env[1].1);
        assert_eq!(
            mcp.launch("conv_a").await.unwrap().servers[0].env[1].1,
            token_a
        );

        assert_eq!(mcp.authenticate(&token_a).as_deref(), Some("conv_a"));
        assert_eq!(mcp.authenticate(""), None);
        assert_eq!(mcp.authenticate(&token_a[..63]), None);
        assert_eq!(mcp.authenticate(&format!("{token_a}0")), None);

        let args = crate::provider::mcp_injection::claude_args(&a)
            .await
            .unwrap();
        let config_path = PathBuf::from(args[0].strip_prefix("--mcp-config=").unwrap());
        assert!(config_path.is_file());
        mcp.revoke("conv_a").await;
        assert_eq!(mcp.authenticate(&token_a), None);
        assert!(!config_path.exists());
        assert!(mcp.authenticate(&b.servers[0].env[1].1).is_some());
    }

    #[tokio::test]
    async fn non_loopback_listener_disables_injection() {
        let root = std::env::temp_dir().join(format!("todex-agent-mcp-{}", uuid::Uuid::new_v4()));
        let ssh = SshService::with_home(&root.join("data"), "ssh".into(), None)
            .await
            .unwrap();
        let desktop = AgentDesktop::load(&root.join("data")).await.unwrap();
        let mcp = AgentMcp::with_bridge_command(&root.join("data"), ssh, desktop, Some("x".into()))
            .await
            .unwrap();
        mcp.set_listen_addr("192.168.1.20:7345".parse().unwrap());
        assert!(mcp.inner.endpoint.get().is_none());
        mcp.set_listen_addr("[::]:7345".parse().unwrap());
        assert_eq!(mcp.inner.endpoint.get().unwrap(), "http://[::1]:7345");
        let _ = std::fs::remove_dir_all(root);
    }
}
