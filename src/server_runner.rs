use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
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
    /// Never sent; its sender drops when the server task ends.
    exited: watch::Receiver<()>,
    retention_task: Option<JoinHandle<()>>,
    /// Applies the tenants' saved history cleanup policies.
    history_cleanup_task: Option<JoinHandle<()>>,
    /// Removes expired prompt uploads.
    upload_cleanup_task: Option<JoinHandle<()>>,
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
    /// Held until the server is dropped: see [`lock_data_dir`].
    _data_dir_lock: std::fs::File,
    /// The local IPC listener (`[ipc]`), when enabled and bound.
    ipc: Option<IpcServer>,
}

/// How long a stopping listener waits for its open requests. API event
/// streams (and slow clients) never end on their own, and the shutdown steps
/// after the wait must still run before `daemon stop` kills the process.
const LISTENER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits for a listener task after its graceful shutdown was signalled, at
/// most [`LISTENER_DRAIN_TIMEOUT`]; then aborts it, closing what is left
/// open, and returns `None`.
async fn drain_listener(
    mut handle: JoinHandle<Result<()>>,
    name: &str,
) -> Option<std::result::Result<Result<()>, tokio::task::JoinError>> {
    match tokio::time::timeout(LISTENER_DRAIN_TIMEOUT, &mut handle).await {
        Ok(joined) => Some(joined),
        Err(_) => {
            tracing::warn!(
                listener = name,
                timeout_ms = LISTENER_DRAIN_TIMEOUT.as_millis() as u64,
                "listener still had open requests at shutdown; closing them"
            );
            handle.abort();
            None
        }
    }
}

/// Longest time a client may take to send a request's headers; also how
/// long an idle keep-alive connection may wait for its next request. Before
/// this, hyper's default had no timer behind it, so a peer that never
/// finished its headers held a connection for good.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Connections a listener serves at once; more wait in the kernel's accept
/// queue until one closes.
const MAX_CONNECTIONS: usize = 1024;

/// What one listener allows its connections.
#[derive(Clone, Copy, Debug)]
struct ConnectionLimits {
    header_read_timeout: std::time::Duration,
    max_connections: usize,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
        }
    }
}

/// `axum::serve` with `ConnectInfo<SocketAddr>`, plus [`ConnectionLimits`].
/// HTTP/1 with upgrades (WebSockets). After `shutdown` it stops
/// accepting and waits for open connections to finish; the connection tasks
/// belong to this future, so aborting it closes them too.
async fn serve_listener(
    listener: TcpListener,
    app: axum::Router,
    limits: ConnectionLimits,
    shutdown: impl std::future::Future<Output = ()>,
) {
    use axum::extract::connect_info::ConnectInfo;
    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;
    use tower::ServiceExt;

    // HTTP/1 only, like `axum::serve` with axum's default features.
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    // Tells open connections to finish their current request and close.
    let (closing, _) = tokio::sync::watch::channel(false);
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(limits.max_connections));
    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
            permit = std::sync::Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let (stream, remote) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    // As `axum::serve`: a failed handshake is the peer's
                    // problem; anything else (out of descriptors) backs off.
                    if !matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionReset
                    ) {
                        tracing::warn!(error = %error, "accepting a connection failed");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    continue;
                }
            },
        };
        let _ = stream.set_nodelay(true);
        let service = app.clone().map_request(
            move |mut request: axum::http::Request<hyper::body::Incoming>| {
                request.extensions_mut().insert(ConnectInfo(remote));
                request
            },
        );
        let connection = builder
            .serve_connection(TokioIo::new(stream), TowerToHyperService::new(service))
            .with_upgrades();
        let mut close = closing.subscribe();
        connections.spawn(async move {
            tokio::pin!(connection);
            let result = tokio::select! {
                result = connection.as_mut() => result,
                () = async {
                    // Drop the guard `wait_for` returns before awaiting on.
                    let _ = close.wait_for(|closing| *closing).await.map(drop);
                } => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = result {
                tracing::debug!(error = %error, "connection ended with an error");
            }
            drop(permit);
        });
    }
    drop(listener);
    let _ = closing.send(true);
    while connections.join_next().await.is_some() {}
}

struct IpcServer {
    path: std::path::PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<()>>,
}

impl IpcServer {
    /// Ends the listener; the socket file goes with it.
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        match drain_listener(self.handle, "ipc").await {
            Some(Ok(Err(error))) => tracing::warn!(error = %error, "IPC listener failed"),
            Some(Err(error)) if !error.is_cancelled() => {
                tracing::warn!(error = %error, "IPC listener task failed")
            }
            _ => {}
        }
    }
}

/// `<data_dir>/daemon.lock`, locked for a server's lifetime.
const DATA_DIR_LOCK_FILE: &str = "daemon.lock";

/// Claims the data directory for this server. Two servers on one directory
/// would both recover "interrupted" turns and append to the same journals,
/// so a second one fails to start instead. The OS releases the lock when the
/// process exits, however it exits.
fn lock_data_dir(data_dir: &Path) -> Result<std::fs::File> {
    let path = data_dir.join(DATA_DIR_LOCK_FILE);
    crate::secure_fs::lock_exclusive(&path, std::time::Duration::ZERO).map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            anyhow::anyhow!(
                "another TodeX server is already using the data directory {}",
                data_dir.display()
            )
        } else {
            anyhow::Error::new(error).context(format!("failed to lock {}", path.display()))
        }
    })
}

/// The IPC socket, bound before the server state exists and served once it
/// does.
#[cfg(any(unix, windows))]
type BoundIpc = (std::path::PathBuf, crate::ipc::IpcListener);
#[cfg(not(any(unix, windows)))]
type BoundIpc = (std::path::PathBuf, std::convert::Infallible);

/// Binds the IPC listener. With the network on, failing to bind is not
/// fatal: the daemon keeps serving TCP only. With it off nothing else could
/// reach the daemon, so the start fails.
#[cfg(any(unix, windows))]
fn bind_ipc(config: &Config) -> Result<Option<BoundIpc>> {
    if !config.ipc.enabled {
        return Ok(None);
    }
    let bound = crate::ipc::resolve_path(config)
        .and_then(|path| crate::ipc::IpcListener::bind(&path).map(|listener| (path, listener)));
    match bound {
        Ok(bound) => Ok(Some(bound)),
        Err(error) if config.network_enabled => {
            tracing::warn!(error = %error, "local IPC listener unavailable; serving TCP only");
            Ok(None)
        }
        Err(error) => Err(anyhow::Error::new(error)
            .context("network_enabled = false needs the local IPC listener, which failed to bind")),
    }
}

#[cfg(not(any(unix, windows)))]
fn bind_ipc(config: &Config) -> Result<Option<BoundIpc>> {
    if config.network_enabled {
        return Ok(None);
    }
    anyhow::bail!("network_enabled = false needs local IPC, which this platform does not have")
}

/// Serves a bound IPC listener.
#[cfg(any(unix, windows))]
fn serve_ipc(
    (path, listener): BoundIpc,
    state: &AppState,
    info: impl FnOnce(String) -> server::LocalAdminInfo,
) -> IpcServer {
    let app = server::ipc_router(state.clone(), info(path.display().to_string()));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<crate::ipc::LocalPeer>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .context("IPC server failed")
    });
    info!(path = %path.display(), "todex-agentd IPC listening");
    IpcServer {
        path,
        shutdown: Some(shutdown_tx),
        handle,
    }
}

#[cfg(not(any(unix, windows)))]
fn serve_ipc(
    (_, never): BoundIpc,
    _state: &AppState,
    _info: impl FnOnce(String) -> server::LocalAdminInfo,
) -> IpcServer {
    match never {}
}

struct ApiListener {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<()>>,
    /// Cancels turns of keys revoked or expired outside the daemon.
    revocation_task: JoinHandle<()>,
}

impl ApiListener {
    /// Stops accepting requests; [`Self::finish`] waits for open ones.
    fn begin_stop(&mut self) {
        self.revocation_task.abort();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    async fn finish(self) {
        match drain_listener(self.handle, "API").await {
            Some(Ok(Err(error))) => tracing::warn!(error = %error, "API listener failed"),
            Some(Err(error)) => tracing::warn!(error = %error, "API listener task failed"),
            Some(Ok(Ok(()))) | None => {}
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
        let started_at = chrono::Utc::now();
        if config.security.enable_tls {
            anyhow::bail!(
                "TLS is configured but this build has no certificate/key listener; terminate TLS at a trusted reverse proxy or disable enable_tls"
            );
        }
        config.ensure_listener_matches_auth()?;
        config.ensure_api_listener_is_valid()?;
        config.ensure_network_off_has_ipc()?;
        let addr = listener_addr(&config)?;
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
        // After the binds, so a port conflict still reports itself; before
        // anything reads or recovers the data directory.
        let data_dir_lock = lock_data_dir(&config.data_dir)?;
        // After the lock: binding replaces a stale socket file, which must
        // not be a running server's.
        let bound_ipc = bind_ipc(&config)?;
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
        let history_cleanup_task = Some(crate::history_cleanup::spawn(state.clone()));
        let upload_cleanup_task = Some(crate::prompt_uploads::spawn_cleanup());
        let app = if config.network_enabled {
            server::router(state.clone())
        } else {
            server::agent_router(state.clone())
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        if !config.network_enabled {
            info!(
                agent_addr = %addr,
                data_dir = %config.data_dir.display(),
                workspace_roots = ?config.workspace_roots,
                "todex-agentd network listener off; clients connect over IPC only"
            );
        } else {
            info!(
                host = %config.host,
                port = config.port,
                data_dir = %config.data_dir.display(),
                workspace_roots = ?config.workspace_roots,
                "todex-agentd listening"
            );
            log_connect_addresses(&config, addr);
        }
        let (exited_tx, exited) = watch::channel(());
        let handle = tokio::spawn(async move {
            let _exited = exited_tx;
            serve_listener(listener, app, ConnectionLimits::default(), async {
                let _ = shutdown_rx.await;
            })
            .await;
            Ok(())
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
                state.agent_calling.set_running_api_listener(Some(api_addr));
                let app = server::api_router(state.clone());
                let (api_shutdown_tx, api_shutdown_rx) = oneshot::channel();
                let handle = tokio::spawn(async move {
                    serve_listener(listener, app, ConnectionLimits::default(), async {
                        let _ = api_shutdown_rx.await;
                    })
                    .await;
                    Ok(())
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
        let api_listen = api.as_ref().map(|api| api.addr);
        let ipc = bound_ipc.map(|bound| {
            serve_ipc(bound, &state, |ipc_path| server::LocalAdminInfo {
                started_at,
                port: addr.port(),
                api_listen,
                ipc_path,
            })
        });
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
            exited,
            retention_task,
            history_cleanup_task,
            upload_cleanup_task,
            kanban_schedule_task,
            migration_task,
            maintenance_task,
            legacy_scan_task,
            history_watch_task,
            api,
            _data_dir_lock: data_dir_lock,
            ipc,
        })
    }

    /// The path the IPC listener bound, when it runs.
    pub fn ipc_path(&self) -> Option<&std::path::Path> {
        self.ipc.as_ref().map(|ipc| ipc.path.as_path())
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

    /// Resolves once the server task has ended; `wait` then returns its
    /// result without blocking.
    pub async fn finished(&self) {
        let mut exited = self.exited.clone();
        while exited.changed().await.is_ok() {}
    }

    /// No Agent turn or local Codex adapter is running, no follow-up queue is
    /// about to start its next item, and nothing holds the CLI execution gate
    /// or a CLI's lock (an Agent start in progress or a CLI install/upgrade).
    pub fn is_agent_idle(&self) -> bool {
        self.state.cli_execution_gate.try_write().is_ok()
            && self.state.cli_manager.locks().is_idle()
            && !self.state.conversations.has_active_turns()
            && !self.state.conversations.has_pending_follow_ups()
            && !self.state.codex_local_adapters.has_active_adapters()
    }

    /// Serves until `shutdown` resolves, then stops like the daemon does
    /// (flushing batched audit records and pending device activity), or
    /// until the server ends on its own.
    pub async fn run_until(
        self,
        shutdown: impl std::future::Future<Output = Result<()>>,
    ) -> Result<()> {
        tokio::select! {
            signal = shutdown => {
                let stopped = self.stop().await;
                signal.and(stopped)
            }
            () = self.finished() => self.wait().await,
        }
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
        if let Some(task) = self.history_cleanup_task.take() {
            task.abort();
        }
        if let Some(task) = self.upload_cleanup_task.take() {
            task.abort();
        }
        if let Some(task) = self.kanban_schedule_task.take() {
            task.abort();
        }
        // No new API requests (or turns) while conversations shut down.
        let mut api = self.api.take();
        if let Some(api) = api.as_mut() {
            api.begin_stop();
        }
        self.state.conversations.shutdown_all().await;
        if let Some(api) = api {
            api.finish().await;
        }
        self.state.codex_local_adapters.shutdown_all().await;
        // Port forwards and IP pool masters never outlive the daemon.
        self.state.ssh.shutdown().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(ipc) = self.ipc.take() {
            ipc.stop().await;
        }
        let result = match drain_listener(self.handle, "main").await {
            Some(joined) => joined.context("server task join failed"),
            None => Ok(Ok(())),
        };
        self.state.device_auth.flush_activity();
        self.state.terminal_audit_log.flush();
        result?
    }

    pub async fn wait(mut self) -> Result<()> {
        let result = self.handle.await.context("server task join failed");
        self.state.ssh.shutdown().await;
        // The periodic tasks run for as long as the server does; aborting
        // them before the await stopped them at startup in `serve` mode.
        if let Some(task) = self.retention_task.take() {
            task.abort();
        }
        if let Some(task) = self.history_cleanup_task.take() {
            task.abort();
        }
        if let Some(task) = self.upload_cleanup_task.take() {
            task.abort();
        }
        if let Some(task) = self.kanban_schedule_task.take() {
            task.abort();
        }
        if let Some(mut api) = self.api.take() {
            api.begin_stop();
            api.finish().await;
        }
        if let Some(ipc) = self.ipc.take() {
            ipc.stop().await;
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
        self.state.device_auth.flush_activity();
        self.state.terminal_audit_log.flush();
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

/// Logs where clients reach the main listener, and warns when it serves
/// plaintext beyond loopback.
fn log_connect_addresses(config: &Config, addr: SocketAddr) {
    // The advertised address comes first: inside a container the
    // interface addresses below are not reachable from clients.
    if let Some(url) = &config.deployment.advertise_url {
        info!(url = %url, "advertised client connect address");
    }
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
    if plaintext_listener_needs_warning(config.security.enable_tls, &config.host) {
        tracing::warn!(
            host = %config.host,
            "non-loopback listener is using plaintext HTTP; pairing encryption protects websocket frames but not bearer headers"
        );
    }
}

/// The main listener: `host:port`, or with the network off a loopback port
/// on a random number that serves only the agent MCP routes.
fn listener_addr(config: &Config) -> Result<SocketAddr> {
    if config.network_enabled {
        bind_addr(config)
    } else {
        Ok(SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)))
    }
}

pub fn bind_addr(config: &Config) -> Result<SocketAddr> {
    format!("{}:{}", config.host, config.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", config.host, config.port))
}

/// Whether the main listener serves plaintext HTTP beyond loopback
/// (`localhost`, `127.0.0.0/8` and `::1` count as loopback).
fn plaintext_listener_needs_warning(enable_tls: bool, host: &str) -> bool {
    !enable_tls && !listen_addrs::is_loopback_host(host)
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::{
        drain_listener, plaintext_listener_needs_warning, serve_listener, ConnectionLimits,
        ManagedServer, ProviderProcessTracking, LISTENER_DRAIN_TIMEOUT,
    };
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use anyhow::Result;

    #[test]
    fn plaintext_warning_skips_loopback_hosts_including_localhost() {
        for host in [
            "127.0.0.1",
            "::1",
            "localhost",
            "LOCALHOST",
            "[::1]",
            "127.0.0.2",
        ] {
            assert!(!plaintext_listener_needs_warning(false, host), "{host}");
        }
        for host in ["0.0.0.0", "192.168.1.10", "example.com"] {
            assert!(plaintext_listener_needs_warning(false, host), "{host}");
            assert!(!plaintext_listener_needs_warning(true, host), "{host}");
        }
    }

    /// One HTTP/1.1 request over the IPC socket: the status and JSON body.
    #[cfg(unix)]
    async fn ipc_request(
        path: &std::path::Path,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let request = format!(
            "{method} {uri} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (head, mut payload) = raw.split_once("\r\n\r\n").unwrap();
        let status = head[9..12].parse().unwrap();
        let decoded;
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            let mut out = String::new();
            loop {
                let (size, rest) = payload.split_once("\r\n").unwrap();
                let size = usize::from_str_radix(size.trim(), 16).unwrap();
                if size == 0 {
                    break;
                }
                out.push_str(&rest[..size]);
                payload = &rest[size + 2..];
            }
            decoded = out;
            payload = &decoded;
        }
        let value = if payload.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(payload).unwrap_or(serde_json::Value::String(payload.to_owned()))
        };
        (status, value)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ipc_listener_serves_the_router_and_the_local_admin_api_only_there() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt;

        let root = env::temp_dir().join(format!(
            "todex-ipc-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let config = Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![workspace.clone()],
            ..Config::default()
        };
        let server = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        let path = server.ipc_path().unwrap().to_path_buf();
        assert_eq!(
            path,
            fs::canonicalize(root.join("data"))
                .unwrap()
                .join("agentd.sock")
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let (status, version) = ipc_request(&path, "GET", "/v2/version", None).await;
        assert_eq!(status, 200, "{version}");
        // Never routed on TCP.
        let tcp = reqwest::get(format!("http://{}/v2/local-admin/status", server.addr()))
            .await
            .unwrap();
        assert_eq!(tcp.status(), reqwest::StatusCode::NOT_FOUND);

        let (status, body) = ipc_request(&path, "GET", "/v2/local-admin/status", None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ipcPath"], path.display().to_string());
        assert_eq!(body["listen"]["port"], server.addr().port());
        assert_eq!(body["deployment"], "host");
        assert_eq!(body["configEditable"], true);
        assert_eq!(body["pendingPairingCount"], 0);
        assert!(body["pairingLink"]
            .as_str()
            .unwrap()
            .contains("todex-pairing-link"));
        assert!(body["transportFingerprint"].is_string());

        let (status, body) = ipc_request(&path, "GET", "/v2/local-admin/config", None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["editable"], true);
        for invalid in [
            json!({ "port": 0 }),
            json!({ "port": 70000 }),
            json!({ "host": "not-an-ip" }),
            json!({ "workspaceRoots": [] }),
            json!({ "pairingEncryption": "rot13" }),
            json!({ "unknown": true }),
        ] {
            let (status, body) = ipc_request(
                &path,
                "PUT",
                "/v2/local-admin/config",
                Some(invalid.clone()),
            )
            .await;
            assert_eq!(status, 400, "{invalid}: {body}");
            assert_eq!(body["error"]["code"], "invalid_config", "{body}");
        }
        let (status, body) = ipc_request(
            &path,
            "PUT",
            "/v2/local-admin/config",
            Some(json!({ "port": 7999, "workspaceRoots": [workspace, workspace] })),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["port"], 7999);
        assert_eq!(body["restartRequired"], true);
        assert_eq!(
            body["workspaceRoots"],
            json!([fs::canonicalize(&workspace).unwrap()])
        );
        let saved = fs::read_to_string(root.join("data/config.toml")).unwrap();
        assert!(saved.contains("port = 7999"), "{saved}");

        // A pairing request made over IPC is recorded as coming from "ipc".
        let device_key = ed25519_dalek::SigningKey::from_bytes(&[21; 32])
            .verifying_key()
            .to_bytes();
        let client_public =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([7; 32])).to_bytes();
        let nonce = [5u8; 32];
        let (status, created) = ipc_request(
            &path,
            "POST",
            "/v2/device-pairing/create",
            Some(json!({
                "deviceName": "IPC device",
                "devicePublicKey": URL_SAFE_NO_PAD.encode(device_key),
                "clientCommitment": URL_SAFE_NO_PAD.encode(
                    crate::device_pairing::commitment(&client_public, &nonce)
                ),
                "transportBinding": 1,
                "deviceNameBinding": 1,
            })),
        )
        .await;
        assert_eq!(status, 200, "{created}");
        let request_id = created["requestId"].as_str().unwrap().to_owned();
        let (status, body) = ipc_request(
            &path,
            "POST",
            "/v2/device-pairing/reveal",
            Some(json!({
                "requestId": request_id,
                "clientPublicKey": URL_SAFE_NO_PAD.encode(client_public),
                "clientNonce": URL_SAFE_NO_PAD.encode(nonce),
            })),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, listed) =
            ipc_request(&path, "GET", "/v2/local-admin/pairing-requests", None).await;
        assert_eq!(status, 200, "{listed}");
        assert_eq!(listed[0]["requestId"], request_id.as_str());
        assert_eq!(listed[0]["peerAddress"], "ipc");
        let code = listed[0]["verificationCode"].as_str().unwrap().to_owned();
        let decision = format!("/v2/local-admin/pairing-requests/{request_id}/decision");
        let (status, body) = ipc_request(
            &path,
            "POST",
            &decision,
            Some(json!({ "approved": true, "verificationCode": "AAAAA-AAAAA" })),
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (409, &json!("code_mismatch"))
        );
        let (status, body) = ipc_request(
            &path,
            "POST",
            &format!(
                "/v2/local-admin/pairing-requests/{}/decision",
                uuid::Uuid::new_v4()
            ),
            Some(json!({ "approved": true, "verificationCode": code })),
        )
        .await;
        assert_eq!((status, &body["error"]["code"]), (404, &json!("not_found")));
        let (status, body) = ipc_request(
            &path,
            "POST",
            &decision,
            Some(json!({ "approved": false, "verificationCode": code.to_lowercase().replace('-', "") })),
        )
        .await;
        assert_eq!((status, body), (200, json!({ "ok": true })));

        let (status, body) = ipc_request(&path, "GET", "/v2/local-admin/devices", None).await;
        assert_eq!((status, body), (200, json!([])));
        let (status, body) =
            ipc_request(&path, "DELETE", "/v2/local-admin/devices/dev_missing", None).await;
        assert_eq!((status, &body["error"]["code"]), (404, &json!("not_found")));
        let (status, body) = ipc_request(
            &path,
            "POST",
            "/v2/local-admin/reset",
            Some(json!({ "target": "devices" })),
        )
        .await;
        assert_eq!(
            (status, body),
            (200, json!({ "ok": true, "restartRequired": true }))
        );

        server.stop().await.unwrap();
        assert!(!path.exists(), "the socket is removed on shutdown");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn the_ipc_listener_can_be_disabled() {
        let root = env::temp_dir().join(format!("todex-ipc-off-{}", uuid::Uuid::new_v4()));
        let mut config = Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.ipc.enabled = false;
        let server = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        assert!(server.ipc_path().is_none());
        server.stop().await.unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn with_the_network_off_clients_use_ipc_and_tcp_serves_only_agent_tools() {
        use serde_json::json;

        let root = env::temp_dir().join(format!(
            "todex-netoff-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        // Saved like the running config except for the network, so only it
        // makes a restart required.
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(
            root.join("data/config.toml"),
            format!(
                "host = \"0.0.0.0\"\nport = 0\nworkspace_roots = [{:?}]\n",
                root.join("workspace")
            ),
        )
        .unwrap();
        let config = Config {
            host: "0.0.0.0".to_owned(),
            port: 0,
            network_enabled: false,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspace")],
            ..Config::default()
        };
        let server = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        assert!(server.addr().ip().is_loopback(), "{}", server.addr());
        let tcp = |path: &str| format!("http://{}{path}", server.addr());
        // The daemon health check and the agent MCP routes stay on TCP.
        assert!(reqwest::get(tcp("/v2/version"))
            .await
            .unwrap()
            .status()
            .is_success());
        let mcp = reqwest::Client::new()
            .post(tcp(crate::agent_mcp::SSH_ROUTE))
            .send()
            .await
            .unwrap();
        assert_ne!(mcp.status(), reqwest::StatusCode::NOT_FOUND);
        for client_route in ["/health", "/v2/workspaces", "/v2/device-pairing/create"] {
            let status = reqwest::get(tcp(client_route)).await.unwrap().status();
            assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{client_route}");
        }

        let path = server.ipc_path().unwrap().to_path_buf();
        let (status, body) = ipc_request(&path, "GET", "/v2/local-admin/status", None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["networkEnabled"], false);
        assert_eq!(body["connectUrls"], json!([]));
        assert_eq!(body["pairingLink"], "");
        assert_eq!(body["listen"], json!({ "host": "0.0.0.0", "port": 0 }));

        let (status, body) = ipc_request(&path, "GET", "/v2/local-admin/config", None).await;
        assert_eq!((status, &body["networkEnabled"]), (200, &json!(true)));
        assert_eq!(
            body["restartRequired"], true,
            "the saved config has the network on"
        );
        let (status, body) = ipc_request(
            &path,
            "PUT",
            "/v2/local-admin/config",
            Some(json!({ "networkEnabled": false })),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            (&body["networkEnabled"], &body["restartRequired"]),
            (&json!(false), &json!(false))
        );
        let saved = fs::read_to_string(root.join("data/config.toml")).unwrap();
        assert!(saved.contains("network_enabled = false"), "{saved}");

        server.stop().await.unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn the_network_cannot_be_off_without_ipc() {
        let root = env::temp_dir().join(format!("todex-netoff-noipc-{}", uuid::Uuid::new_v4()));
        let mut config = Config {
            port: 0,
            network_enabled: false,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        config.ipc.enabled = false;
        let error = ManagedServer::start(config.clone(), ProviderProcessTracking::Disabled)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("[ipc] is disabled"), "{error:#}");

        // An IPC listener that cannot bind fails the start instead of
        // leaving a daemon nothing can reach.
        #[cfg(unix)]
        {
            config.ipc.enabled = true;
            fs::create_dir_all(&root).unwrap();
            let not_a_socket = root.join("agentd.sock");
            fs::write(&not_a_socket, b"file").unwrap();
            config.ipc.path = Some(not_a_socket);
            let error = ManagedServer::start(config, ProviderProcessTracking::Disabled)
                .await
                .err()
                .unwrap();
            assert!(
                format!("{error:#}").contains("network_enabled = false"),
                "{error:#}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn silent_connections_time_out_and_waiting_ones_are_served_after() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let limits = ConnectionLimits {
            header_read_timeout: std::time::Duration::from_millis(300),
            max_connections: 1,
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_listener(listener, app, limits, async {
            let _ = stopped.await;
        }));

        // Starts a request and never finishes its headers.
        let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        silent
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let mut waiting = tokio::net::TcpStream::connect(addr).await.unwrap();
        waiting
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            waiting.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        // Served only once the silent one gave up its slot.
        assert!(response.ends_with("ok"), "{response}");
        assert!(started.elapsed() >= std::time::Duration::from_millis(200));
        let mut rest = Vec::new();
        // EOF or a reset: either way the server closed it.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            silent.read_to_end(&mut rest),
        )
        .await
        .expect("the silent connection is closed");

        stop.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn listener_drain_stops_waiting_for_requests_that_never_end() {
        // An API event stream keeps its connection, and so the graceful
        // shutdown, open forever.
        let stuck = tokio::spawn(std::future::pending::<Result<()>>());
        let aborted = stuck.abort_handle();
        let started = std::time::Instant::now();
        assert!(drain_listener(stuck, "test").await.is_none());
        assert!(started.elapsed() < LISTENER_DRAIN_TIMEOUT * 3);
        tokio::task::yield_now().await;
        assert!(aborted.is_finished());

        let finished = tokio::spawn(async { Ok(()) });
        assert!(matches!(
            drain_listener(finished, "test").await,
            Some(Ok(Ok(())))
        ));
    }

    #[tokio::test]
    async fn a_second_server_cannot_share_the_data_directory() {
        let root = env::temp_dir().join(format!("todex-server-lock-{}", uuid::Uuid::new_v4()));
        let config = Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        let first = ManagedServer::start(config.clone(), ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        let error = ManagedServer::start(config.clone(), ProviderProcessTracking::Disabled)
            .await
            .err()
            .expect("a second server on the same data directory must not start");
        assert!(
            error
                .to_string()
                .contains("already using the data directory"),
            "{error}"
        );
        first.stop().await.unwrap();

        // Stopping releases the directory for the next server.
        let next = ManagedServer::start(config, ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        next.stop().await.unwrap();
        let _ = fs::remove_dir_all(root);
    }

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
    async fn a_shutdown_signal_stops_the_server_and_flushes_batched_audit_records() {
        let root = env::temp_dir().join(format!("todex-server-signal-{}", uuid::Uuid::new_v4()));
        let config = Config {
            port: 0,
            data_dir: root.join("data"),
            workspace_roots: vec![root.join("workspaces")],
            ..Config::default()
        };
        let server = ManagedServer::start(config.clone(), ProviderProcessTracking::Disabled)
            .await
            .unwrap();
        let addr = server.addr();
        let record = crate::event::EventRecord::new(
            "terminal.audit",
            None,
            None,
            None,
            serde_json::json!({ "marker": "flushed-on-signal" }),
        );
        server
            .state
            .terminal_audit_log
            .append_batched(&config.data_dir, &record)
            .unwrap();
        // The signal arrives before the batch interval passes.
        server.run_until(async { Ok(()) }).await.unwrap();
        let audit = fs::read_to_string(config.data_dir.join("audit/audit-terminal.jsonl")).unwrap();
        assert!(audit.contains("flushed-on-signal"), "{audit}");
        assert!(tokio::net::TcpStream::connect(addr).await.is_err());
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
            network_enabled: true,
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
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
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                omp_bin: "omp".to_owned(),
                omp_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                ..Default::default()
            },
            api: Default::default(),
            deployment: Default::default(),
            ipc: Default::default(),
            update: Default::default(),
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
        let upgrading = server
            .state
            .cli_manager
            .locks()
            .try_write(crate::provider::ManagedCli::Pi)
            .unwrap();
        assert!(!server.is_agent_idle());
        drop(upgrading);
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
            network_enabled: true,
            pairing_encryption: PairingEncryption::default(),
            data_dir,
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
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
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                omp_bin: "omp".to_owned(),
                omp_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
                ..Default::default()
            },
            api: Default::default(),
            deployment: Default::default(),
            ipc: Default::default(),
            update: Default::default(),
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
            network_enabled: true,
            pairing_encryption: PairingEncryption::default(),
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            history_seal_min_bytes: crate::config::DEFAULT_HISTORY_SEAL_MIN_BYTES,
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
                dsh_bin: "dsh".to_owned(),
                dsh_env_allowlist: Vec::new(),
                omp_bin: "omp".to_owned(),
                omp_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: Default::default(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
                provider_keepalive_seconds: 300,
                provider_prewarm: true,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: true,
                ..Default::default()
            },
            api: Default::default(),
            deployment: Default::default(),
            ipc: Default::default(),
            update: Default::default(),
        };
        assert!(
            ManagedServer::start(config, ProviderProcessTracking::Disabled)
                .await
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }
}
