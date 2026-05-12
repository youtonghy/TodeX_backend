use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

use crate::app_state::AppState;
use crate::config::Config;
use crate::config::PairingEncryption;
use crate::event::EventRecord;
use crate::server;

pub struct ManagedServer {
    config: Config,
    addr: SocketAddr,
    state: AppState,
    started_at: Instant,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<()>>,
}

impl ManagedServer {
    pub async fn start(config: Config) -> Result<Self> {
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
            started_at: Instant::now(),
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

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn active_codex_adapters(&self) -> usize {
        self.state.codex_local_adapters.len()
    }

    pub fn websocket_connection_count(&self) -> usize {
        self.state.websocket_connection_count()
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<EventRecord> {
        self.state.events.subscribe()
    }

    pub fn pairing_qr_text(&self, preferred_encryption: PairingEncryption) -> Result<String> {
        Ok(self.state.pairing_keys.pairing_qr_text(
            &self.config,
            self.addr.port(),
            preferred_encryption,
        )?)
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub async fn stop(mut self) -> Result<()> {
        self.state.codex_local_adapters.shutdown_all().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.handle.await.context("server task join failed")?
    }

    pub async fn wait(self) -> Result<()> {
        self.handle.await.context("server task join failed")?
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

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
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: None,
            },
        };

        let server = ManagedServer::start(config).await.expect("start server");
        assert_eq!(server.active_codex_adapters(), 0);
        server.stop().await.expect("stop server");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn managed_server_serves_encrypted_pairing_payload() {
        let root = env::temp_dir().join(format!("todex-pairing-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let data_dir = root.join("data");
        let workspace_root = root.join("workspace");
        fs::create_dir_all(&workspace_root).expect("create workspace root");

        let server = ManagedServer::start(test_config(data_dir, workspace_root))
            .await
            .expect("start server");
        let (status, body) = http_get(server.addr().port(), "/v1/pairing", Some("token")).await;

        assert_eq!(status, 200);
        assert!(body.contains("\"id\":\"x25519\""));
        assert!(body.contains("\"id\":\"ml-kem-768\""));
        assert!(body.contains("\"preferredEncryption\":\"ml-kem-768\""));
        assert!(body.contains("\"authToken\":\"token\""));

        server.stop().await.expect("stop server");
        let _ = fs::remove_dir_all(root);
    }

    fn test_config(data_dir: std::path::PathBuf, workspace_root: std::path::PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_root,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                auth_token: Some("token".to_owned()),
            },
        }
    }

    async fn http_get(port: u16, path: &str, token: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect server");
        let authorization = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{authorization}Connection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let response = String::from_utf8_lossy(&response);
        let status = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        (status, body)
    }
}
