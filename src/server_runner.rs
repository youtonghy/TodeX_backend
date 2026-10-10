use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::interval;
use tracing::info;

use crate::app_state::AppState;
use crate::config::Config;
use crate::listen_addrs;
use crate::server;

pub struct ManagedServer {
    config: Config,
    addr: SocketAddr,
    state: AppState,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<()>>,
    retention_task: Option<JoinHandle<()>>,
    /// Runs due kanban task schedules.
    kanban_schedule_task: Option<JoinHandle<()>>,
    migration_task: Option<JoinHandle<()>>,
    /// Journal segment conversion, v2 migration and backup cleanup.
    maintenance_task: Option<JoinHandle<()>>,
    legacy_scan_task: Option<JoinHandle<()>>,
    /// Pushes history key changes made by other processes (the TUI).
    history_watch_task: Option<JoinHandle<()>>,
    /// The external API listener (`[api]`), when enabled.
    api: Option<ApiListener>,
}

struct ApiListener {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<()>>,
    /// Cancels turns of keys revoked or expired outside the daemon.
    revocation_task: JoinHandle<()>,
}

impl ApiListener {
    async fn stop(mut self) {
        self.revocation_task.abort();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match self.handle.await {
            Ok(Err(error)) => tracing::warn!(error = %error, "API listener failed"),
            Err(error) => tracing::warn!(error = %error, "API listener task failed"),
            Ok(Ok(())) => {}
        }
    }
}

/// Whether the server records the provider processes it spawns and reaps the
/// ones a crashed predecessor left behind. The registry is process-wide, so
/// only the real daemon enables it; in-process test servers must not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderProcessTracking {
    Enabled,
    #[cfg_attr(not(test), allow(dead_code))]
    Disabled,
}

impl ManagedServer {
    pub async fn start(config: Config, tracking: ProviderProcessTracking) -> Result<Self> {
        if config.security.enable_tls {
            anyhow::bail!(
                "TLS is configured but this build has no certificate/key listener; terminate TLS at a trusted reverse proxy or disable enable_tls"
            );
        }
        config.ensure_listener_matches_auth()?;
        config.ensure_api_listener_is_valid()?;
        let addr = bind_addr(&config)?;
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        let addr = listener
            .local_addr()
            .context("failed to read bound address")?;
        // Bound before any state exists, so a taken API port fails the start
        // as cleanly as a taken main port.
        let api_listener = if config.api.enabled {
            let api_addr = api_bind_addr(&config)?;
            let api_listener = TcpListener::bind(api_addr)
                .await
                .with_context(|| format!("failed to bind the API listener {api_addr}"))?;
            Some(api_listener)
        } else {
            None
        };
        // After the bind, so a second server on the same data directory that
        // cannot listen never kills the running one's providers; before
        // AppState::new, so conversation recovery sees no live orphans.
        if tracking == ProviderProcessTracking::Enabled {
            crate::provider::process_registry::activate(&config.data_dir).await;
        }
        let state = AppState::new(config.clone()).await?;
        state.agent_mcp.set_listen_addr(addr);
        state.agent_desktop.set_daemon_port(addr.port());
        let retention_task = config.history_retention_days.map(|days| {
            let state = state.clone();
            tokio::spawn(async move {
                let mut ticker = interval(std::time::Duration::from_secs(24 * 60 * 60));
                loop {
                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                    match state.conversations.cleanup_expired(cutoff).await {
                        Ok(removed) if !removed.is_empty() => tracing::info!(
                            removed = removed.len(),
                            "conversation retention cleanup removed histories"
                        ),
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(error = %error, "conversation retention cleanup failed")
                        }
                    }
                    ticker.tick().await;
                }
            })
        });
        let app = server::router(state.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        info!(
            host = %config.host,
            port = config.port,
            data_dir = %config.data_dir.display(),
            workspace_roots = ?config.workspace_roots,
            "todex-agentd listening"
        );
        match listen_addrs::connect_addresses(&config.host) {
            Ok(addresses) => {
                for address in addresses {
                    info!(
                        url = %address.ws_url(addr.port()),
                        interface = address.interface.as_deref().unwrap_or("-"),
                        "client connect address"
                    );
                }
            }
            Err(error) => tracing::warn!(
                error = %error,
                "failed to list network interface addresses for the listener"
            ),
        }
        if !config.security.enable_tls && config.host != "127.0.0.1" && config.host != "::1" {
            tracing::warn!(
                host = %config.host,
                "non-loopback listener is using plaintext HTTP; pairing encryption protects websocket frames but not bearer headers"
            );
        }

        let handle = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("server failed")
        });
        let api = match api_listener {
            Some(listener) => {
                let api_addr = listener
                    .local_addr()
                    .context("failed to read the API listener address")?;
                if !listen_addrs::is_loopback_host(&config.api.host) {
                    tracing::warn!(
                        host = %config.api.host,
                        "the API listener is not loopback-only; API keys travel over plaintext HTTP unless a TLS proxy terminates in front of it"
                    );
                }
                info!(addr = %api_addr, "todex-agentd API listening");
                let app = server::api_router(state.clone());
                let (api_shutdown_tx, api_shutdown_rx) = oneshot::channel();
                let handle = tokio::spawn(async move {
                    axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(async {
                        let _ = api_shutdown_rx.await;
                    })
                    .await
                    .context("API server failed")
                });
                Some(ApiListener {
                    addr: api_addr,
                    shutdown: Some(api_shutdown_tx),
                    handle,
                    revocation_task: server::api::spawn_revocation_watch(state.clone()),
                })
            }
            None => None,
        };
        let migration_task = Some(state.spawn_legacy_conversation_migration());
        let maintenance_task = Some(state.spawn_journal_maintenance());
        let legacy_scan_task = state.spawn_legacy_history_scan();
        let history_watch_task = Some(server::spawn_history_watch(state.clone()));
        let kanban_schedule_task = Some(crate::kanban_scheduler::spawn(state.clone()));

        Ok(Self {
            config,
            addr,
            state,
            shutdown: Some(shutdown_tx),
            handle,
            retention_task,
            kanban_schedule_task,
            migration_task,
            maintenance_task,
            legacy_scan_task,
            history_watch_task,
            api,
        })
    }

    /// The bound API listener address, when `[api]` is enabled.
    pub fn api_addr(&self) -> Option<SocketAddr> {
        self.api.as_ref().map(|api| api.addr)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// No Agent turn or local Codex adapter is running, no follow-up queue is
    /// about to start its next item, and nothing holds the CLI execution gate
    /// (an Agent start in progress or a CLI install/upgrade).
    pub fn is_agent_idle(&self) -> bool {
        self.state.cli_execution_gate.try_write().is_ok()
            && !self.state.conversations.has_active_turns()
            && !self.state.conversations.has_pending_follow_ups()
            && !self.state.codex_local_adapters.has_active_adapters()
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(task) = self.migration_task.take() {
            task.abort();
        }
        if let Some(task) = self.maintenance_task.take() {
            task.abort();
        }
        if let Some(task) = self.legacy_scan_task.take() {
            task.abort();
        }
        if let Some(task) = self.history_watch_task.take() {
            task.abort();
        }
        if let Some(task) = self.retention_task.take() {
            task.abort();
        }
        if let Some(task) = self.kanban_schedule_task.take() {
            task.abort();
        }
        if let Some(api) = self.api.take() {
            api.stop().await;
        }
        self.state.conversations.shutdown_all().await;
        self.state.codex_local_adapters.shutdown_all().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.await.context("server task join failed")?
    }

    pub async fn wait(mut self) -> Result<()> {
        if let Some(task) = self.retention_task.take() {
            task.abort();
        }
        if let Some(task) = self.kanban_schedule_task.take() {
            task.abort();
        }
        let result = self.handle.await.context("server task join failed");
        if let Some(api) = self.api.take() {
            api.stop().await;
        }
        if let Some(task) = self.migration_task.take() {
            task.abort();
        }
        if let Some(task) = self.maintenance_task.take() {
            task.abort();
        }
        if let Some(task) = self.legacy_scan_task.take() {
            task.abort();
        }
        if let Some(task) = self.history_watch_task.take() {
            task.abort();
        }
        self.state.conversations.shutdown_all().await;
        self.state.codex_local_adapters.shutdown_all().await;
        result?
    }
}

pub fn api_bind_addr(config: &Config) -> Result<SocketAddr> {
    let host = &config.api.host;
    let port = config.api.port;
    let text = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    text.parse()
        .with_context(|| format!("invalid API bind address {host}:{port}"))
}

pub fn bind_addr(config: &Config) -> Result<SocketAddr> {
    format!("{}:{}", config.host, config.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", config.host, config.port))
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::{ManagedServer, ProviderProcessTracking};
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};

    #[tokio::test]
    async fn managed_server_serves_the_api_listener_only_when_enabled() {
        let root = env::temp_dir().join(format!("todex-server-api-{}", uuid::Uuid::new_v4()));
        let base = Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        let server = ManagedServer::start(base.clone(), ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        assert!(server.api_addr().is_none());
        server.stop().await.unwrap();

        let mut config = base;
        config.api.enabled = true;
        config.api.port = 0;
        let server = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        let api = server.api_addr().unwrap();
        assert_ne!(api.port(), server.addr().port());
        let health: serde_json::Value = reqwest::get(format!("http://{api}/api/v1/health"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["ok"], true);
        let me = reqwest::get(format!("http://{api}/api/v1/me"))
            .await
            .unwrap();
        assert_eq!(me.status(), reqwest::StatusCode::UNAUTHORIZED);
        // The device listener does not serve the API routes.
        let device = reqwest::get(format!("http://{}/api/v1/health", server.addr()))
            .await
            .unwrap();
        assert_eq!(device.status(), reqwest::StatusCode::NOT_FOUND);
        server.stop().await.unwrap();
        assert!(reqwest::get(format!("http://{api}/api/v1/health"))
            .await
            .is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn managed_server_starts_and_stops() {
        let root = env::temp_dir().join(format!("todex-server-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let data_dir = root.join("data");
        let workspace_root = root.join("workspace");
        fs::create_dir_all(&workspace_root).expect("create workspace root");

        let config = Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
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
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
        };

        let server = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .expect("start server");
        assert!(server.addr().port() > 0);
        assert!(server.is_agent_idle());
        // An Agent start or CLI upgrade in progress holds the execution gate.
        let starting = server.state.cli_execution_gate.clone().read_owned().await;
        assert!(!server.is_agent_idle());
        drop(starting);
        assert!(server.is_agent_idle());
        server.stop().await.expect("stop server");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn managed_server_starts_when_legacy_migration_fails() {
        let root = env::temp_dir().join(format!(
            "todex-server-migration-test-{}",
            uuid::Uuid::new_v4()
        ));
        let data_dir = root.join("data");
        let workspace_root = root.join("workspace");
        fs::create_dir_all(data_dir.join("codex_gateway/sessions"))
            .expect("create legacy session root");
        fs::create_dir_all(data_dir.join("migrations")).expect("create migration directory");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        fs::write(
            data_dir.join("migrations/codex-gateway-v1.json"),
            r#"{"schemaVersion":999,"entries":{}}"#,
        )
        .expect("write unsupported migration map");

        let config = Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
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
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
        };

        let mut server = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .expect("start server");
        assert!(server.addr().port() > 0);
        server
            .migration_task
            .take()
            .expect("migration task")
            .await
            .expect("migration task join");
        server.stop().await.expect("stop server");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn managed_server_refuses_anonymous_non_loopback_listener() {
        let root = env::temp_dir().join(format!("todex-server-anon-test-{}", uuid::Uuid::new_v4()));
        let mut config = Config {
            host: "0.0.0.0".to_owned(),
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspace")],
            ..Config::default()
        };
        config.security.enable_auth = false;
        let error = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .err()
            .expect("anonymous non-loopback listener must not start");
        assert!(error.to_string().contains("enable_auth = false"), "{error}");
        // Refused before anything touched the data directory.
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn managed_server_rejects_unimplemented_tls_configuration() {
        let root = env::temp_dir().join(format!("todex-server-tls-test-{}", uuid::Uuid::new_v4()));
        let workspace_root = root.join("workspace");
        fs::create_dir_all(&workspace_root).expect("create workspace root");
        let config = Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::default(),
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
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
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: true,
            },
            api: Default::default(),
        };
        assert!(
            ManagedServer::start(config, ProviderProcessTracking::Disabled)
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }
}
