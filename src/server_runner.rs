use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

use crate::app_state::AppState;
use crate::config::Config;
use crate::server;

pub struct ManagedServer {
    config: Config,
    addr: SocketAddr,
    state: AppState,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<()>>,
}

impl ManagedServer {
    pub async fn start(config: Config) -> Result<Self> {
        if config.security.enable_tls {
            anyhow::bail!(
                "TLS is configured but this build has no certificate/key listener; terminate TLS at a trusted reverse proxy or disable enable_tls"
            );
        }
        let addr = bind_addr(&config)?;
        let state = AppState::new(config.clone()).await?;
        let app = server::router(state.clone());
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind {addr}"))?;
        let addr = listener
            .local_addr()
            .context("failed to read bound address")?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        info!(
            host = %config.host,
            port = config.port,
            data_dir = %config.data_dir.display(),
            workspace_root = %config.workspace_root.display(),
            "todex-agentd listening"
        );
        if !config.security.enable_tls && config.host != "127.0.0.1" && config.host != "::1" {
            tracing::warn!(
                host = %config.host,
                "non-loopback listener is using plaintext HTTP; pairing encryption protects websocket frames but not bearer headers"
            );
        }

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .context("server failed")
        });

        Ok(Self {
            config,
            addr,
            state,
            shutdown: Some(shutdown_tx),
            handle,
        })
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

    pub async fn stop(mut self) -> Result<()> {
        self.state.conversations.shutdown_all().await;
        self.state.codex_local_adapters.shutdown_all().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.await.context("server task join failed")?
    }

    pub async fn wait(self) -> Result<()> {
        let result = self.handle.await.context("server task join failed")?;
        self.state.conversations.shutdown_all().await;
        self.state.codex_local_adapters.shutdown_all().await;
        result
    }
}

pub fn bind_addr(config: &Config) -> Result<SocketAddr> {
    format!("{}:{}", config.host, config.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", config.host, config.port))
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::ManagedServer;
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};

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
            workspace_root,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                acp_profiles: Default::default(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: None,
            },
        };

        let server = ManagedServer::start(config).await.expect("start server");
        assert!(server.addr().port() > 0);
        server.stop().await.expect("stop server");

        let _ = fs::remove_dir_all(root);
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
            workspace_root,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                acp_profiles: Default::default(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: true,
                auth_token: Some("token".to_owned()),
            },
        };
        assert!(ManagedServer::start(config).await.is_err());
        let _ = fs::remove_dir_all(root);
    }
}
