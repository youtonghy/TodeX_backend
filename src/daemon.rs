use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::config::Config;
use crate::server_runner::ManagedServer;
use crate::transport_crypto::PairingKeys;

const PID_FILE_NAME: &str = "daemon.json";
const LOG_FILE_NAME: &str = "todex-agentd-daemon.log";
const START_TIMEOUT: Duration = Duration::from_secs(8);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonProcess {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub workspace_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub executable: PathBuf,
}

impl DaemonProcess {
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub async fn start(config: Config) -> Result<DaemonProcess> {
    if let Some(process) = status(&config)? {
        return Ok(process);
    }

    fs::create_dir_all(log_dir(&config.data_dir)).with_context(|| {
        format!(
            "failed to create log directory {}",
            log_dir(&config.data_dir).display()
        )
    })?;

    let log_path = log_file_path(&config.data_dir);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log {}", log_path.display()))?;
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(&executable);
    command
        .arg("daemon-run")
        .arg("--host")
        .arg(&config.host)
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--data-dir")
        .arg(&config.data_dir)
        .arg("--workspace-root")
        .arg(&config.workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().with_context(|| {
            format!("failed to clone daemon log {}", log_path.display())
        })?))
        .stderr(Stdio::from(log));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn daemon {}", executable.display()))?;

    let started = Instant::now();
    loop {
        if let Some(exit_status) = child.try_wait().context("failed to poll daemon startup")? {
            bail!(
                "daemon exited during startup with status {exit_status}; see {}",
                log_path.display()
            );
        }

        if let Some(process) = status(&config)? {
            return Ok(process);
        }

        if started.elapsed() >= START_TIMEOUT {
            bail!(
                "daemon did not become ready within {:?}; see {}",
                START_TIMEOUT,
                log_path.display()
            );
        }

        sleep(STATUS_POLL_INTERVAL).await;
    }
}

pub async fn stop(config: &Config) -> Result<Option<DaemonProcess>> {
    let Some(process) = status(config)? else {
        return Ok(None);
    };

    terminate_process(process.pid)?;

    let started = Instant::now();
    while process_is_running(process.pid) || daemon_health_check(&process) {
        if started.elapsed() >= STOP_TIMEOUT {
            bail!(
                "daemon pid {} did not stop within {:?}",
                process.pid,
                STOP_TIMEOUT
            );
        }
        sleep(STATUS_POLL_INTERVAL).await;
    }

    remove_pid_file(&config.data_dir)?;
    Ok(Some(process))
}

pub async fn restart(config: Config) -> Result<DaemonProcess> {
    let _ = stop(&config).await?;
    start(config).await
}

pub fn status(config: &Config) -> Result<Option<DaemonProcess>> {
    let Some(process) = read_pid_file(&config.data_dir)? else {
        return Ok(None);
    };

    if process_is_running(process.pid) || daemon_health_check(&process) {
        return Ok(Some(process));
    }

    Ok(None)
}

pub async fn run(config: Config) -> Result<()> {
    let server = ManagedServer::start(config).await?;
    let process = write_pid_file(server.config(), server.addr().port())?;
    let _guard = PidFileGuard {
        data_dir: server.config().data_dir.clone(),
        pid: process.pid,
    };

    tracing::info!(
        pid = process.pid,
        listen = %process.listen_addr(),
        pid_file = %pid_file_path(&process.data_dir).display(),
        "todex-agentd daemon ready"
    );

    wait_for_shutdown_or_server_exit(server).await
}

pub async fn pairing_qr_payloads(config: &Config, port: u16) -> Result<Vec<String>> {
    let keys = PairingKeys::load_or_generate(&config.data_dir).await?;
    Ok(keys.pairing_qr_payloads(config, port, config.pairing_encryption)?)
}

pub fn pid_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PID_FILE_NAME)
}

pub fn log_file_path(data_dir: &Path) -> PathBuf {
    log_dir(data_dir).join(LOG_FILE_NAME)
}

fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

fn read_pid_file(data_dir: &Path) -> Result<Option<DaemonProcess>> {
    let path = pid_file_path(data_dir);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()))
        }
    };

    let process = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(process))
}

fn write_pid_file(config: &Config, port: u16) -> Result<DaemonProcess> {
    fs::create_dir_all(&config.data_dir).with_context(|| {
        format!(
            "failed to create data directory {}",
            config.data_dir.display()
        )
    })?;
    let path = pid_file_path(&config.data_dir);
    let tmp_path = path.with_extension("json.tmp");
    let process = DaemonProcess {
        pid: std::process::id(),
        host: config.host.clone(),
        port,
        data_dir: config.data_dir.clone(),
        workspace_root: config.workspace_root.clone(),
        started_at: Utc::now(),
        executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("todex-agentd")),
    };
    let raw = serde_json::to_string_pretty(&process)?;
    fs::write(&tmp_path, raw).with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(process)
}

fn remove_pid_file(data_dir: &Path) -> Result<()> {
    match fs::remove_file(pid_file_path(data_dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove daemon pid file {}",
                pid_file_path(data_dir).display()
            )
        }),
    }
}

async fn wait_for_shutdown_or_server_exit(server: ManagedServer) -> Result<()> {
    loop {
        if server.is_finished() {
            return server.wait().await;
        }

        tokio::select! {
            signal = shutdown_signal() => {
                signal?;
                return server.stop().await;
            }
            _ = sleep(STATUS_POLL_INTERVAL) => {}
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate =
            signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("failed to listen for Ctrl-C")?,
            _ = terminate.recv() => {}
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C")?;
        Ok(())
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .with_context(|| format!("failed to send SIGTERM to daemon pid {pid}"))?;
        if !status.success() && process_is_running(pid) {
            bail!("failed to send SIGTERM to daemon pid {pid}");
        }
    }

    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status()
            .with_context(|| format!("failed to stop daemon pid {pid}"))?;
        if !status.success() && process_is_running(pid) {
            bail!("failed to stop daemon pid {pid}");
        }
    }

    Ok(())
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

fn daemon_health_check(process: &DaemonProcess) -> bool {
    let Some(addr) = health_addr(&process.host, process.port) else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, HEALTH_TIMEOUT) else {
        return false;
    };

    let _ = stream.set_read_timeout(Some(HEALTH_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HEALTH_TIMEOUT));
    let request = format!("GET /v1/version HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut response = String::new();
    match stream.read_to_string(&mut response) {
        Ok(_) => response.contains(" 200 ") && response.contains(r#""name":"todex-agentd""#),
        Err(_) => false,
    }
}

fn health_addr(host: &str, port: u16) -> Option<SocketAddr> {
    let host = match host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        value => value,
    };
    let addr = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    addr.parse().ok()
}

struct PidFileGuard {
    data_dir: PathBuf,
    pid: u32,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let should_remove = read_pid_file(&self.data_dir)
            .ok()
            .flatten()
            .map(|process| process.pid == self.pid)
            .unwrap_or(false);
        if should_remove {
            let _ = remove_pid_file(&self.data_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use super::{pid_file_path, process_is_running, status, DaemonProcess};
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use chrono::Utc;

    #[test]
    fn status_reports_current_process_from_pid_file() {
        let root = unique_tmp_dir("todex-daemon-status-running");
        let config = test_config(root.clone());
        fs::create_dir_all(&config.data_dir).expect("create data dir");
        let process = DaemonProcess {
            pid: std::process::id(),
            host: config.host.clone(),
            port: config.port,
            data_dir: config.data_dir.clone(),
            workspace_root: config.workspace_root.clone(),
            started_at: Utc::now(),
            executable: env::current_exe().unwrap(),
        };
        fs::write(
            pid_file_path(&config.data_dir),
            serde_json::to_string_pretty(&process).unwrap(),
        )
        .expect("write pid file");

        let detected = status(&config).expect("read daemon status").unwrap();

        assert_eq!(detected.pid, std::process::id());
        assert!(process_is_running(detected.pid));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_ignores_stale_pid_file_without_deleting_it() {
        let root = unique_tmp_dir("todex-daemon-status-stale");
        let config = test_config(root.clone());
        fs::create_dir_all(&config.data_dir).expect("create data dir");
        let process = DaemonProcess {
            pid: u32::MAX,
            host: config.host.clone(),
            port: config.port,
            data_dir: config.data_dir.clone(),
            workspace_root: config.workspace_root.clone(),
            started_at: Utc::now(),
            executable: env::current_exe().unwrap(),
        };
        fs::write(
            pid_file_path(&config.data_dir),
            serde_json::to_string_pretty(&process).unwrap(),
        )
        .expect("write pid file");

        assert!(status(&config).expect("read daemon status").is_none());
        assert!(pid_file_path(&config.data_dir).exists());
        let _ = fs::remove_dir_all(root);
    }

    fn test_config(root: std::path::PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_owned(),
            port: 7345,
            pairing_encryption: PairingEncryption::default(),
            data_dir: root.join("data"),
            workspace_root: root.join("workspace"),
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

    fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
        env::temp_dir().join(format!("{label}-{}", std::process::id()))
    }
}
