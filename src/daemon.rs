use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
const STOP_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_FORCE_AFTER: Duration = Duration::from_secs(8);
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
    remove_stale_pid_file(&config.data_dir)?;

    fs::create_dir_all(log_dir(&config.data_dir)).with_context(|| {
        format!(
            "failed to create log directory {}",
            log_dir(&config.data_dir).display()
        )
    })?;
    set_owner_only_directory(&config.data_dir)?;
    set_owner_only_directory(&log_dir(&config.data_dir))?;

    let log_path = log_file_path(&config.data_dir);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log {}", log_path.display()))?;
    set_owner_only_file(&log_path)?;
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
            reap_daemon_child(child);
            return Ok(process);
        }

        if started.elapsed() >= START_TIMEOUT {
            terminate_spawned_child(&mut child).await;
            bail!(
                "daemon did not become ready within {:?}; see {}",
                START_TIMEOUT,
                log_path.display()
            );
        }

        sleep(STATUS_POLL_INTERVAL).await;
    }
}

fn reap_daemon_child(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

async fn terminate_spawned_child(child: &mut Child) {
    #[cfg(unix)]
    let pid = child.id();
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    #[cfg(windows)]
    let _ = child.kill();

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub async fn stop(config: &Config) -> Result<Option<DaemonProcess>> {
    let Some(process) = read_pid_file(&config.data_dir)? else {
        return Ok(None);
    };

    if process_has_exited(process.pid) && !daemon_health_check(&process) {
        remove_pid_file(&config.data_dir)?;
        return Ok(None);
    }
    if !process_matches_record(&process) {
        bail!(
            "refusing to stop pid {} because it does not match the recorded daemon executable {}",
            process.pid,
            process.executable.display()
        );
    }

    continue_process(process.pid)?;
    terminate_process(process.pid)?;

    let started = Instant::now();
    let mut forced = false;
    loop {
        let liveness = process_liveness(process.pid);
        if liveness.has_exited() && !daemon_health_check(&process) {
            break;
        }

        if liveness.is_stopped() {
            continue_process(process.pid)?;
        }

        if !forced && !liveness.has_exited() && started.elapsed() >= STOP_FORCE_AFTER {
            force_kill_process(process.pid)?;
            forced = true;
        }

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

    if process_matches_record(&process)
        && (process_liveness(process.pid).is_running() || daemon_health_check(&process))
    {
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
    set_owner_only_directory(&config.data_dir)?;
    let path = pid_file_path(&config.data_dir);
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
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to exclusively create {}", path.display()))?;
    file.write_all(raw.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    set_owner_only_file(&path)?;
    Ok(process)
}

fn remove_stale_pid_file(data_dir: &Path) -> Result<()> {
    let Some(process) = read_pid_file(data_dir)? else {
        return Ok(());
    };
    if process_has_exited(process.pid) || !process_matches_record(&process) {
        remove_pid_file(data_dir)?;
    }
    Ok(())
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

fn set_owner_only_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_owner_only_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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

fn continue_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(pid as i32), libc::SIGCONT) };
        if result != 0 && !process_has_exited(pid) {
            bail!("failed to send SIGCONT to daemon pid {pid}");
        }
    }
    #[cfg(not(unix))]
    let _ = pid;

    Ok(())
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        if result != 0 && process_is_running(pid) {
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

fn force_kill_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if result != 0 && !process_has_exited(pid) {
            bail!("failed to send SIGKILL to daemon pid {pid}");
        }
    }

    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .with_context(|| format!("failed to force stop daemon pid {pid}"))?;
        if !status.success() && !process_has_exited(pid) {
            bail!("failed to force stop daemon pid {pid}");
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessLiveness {
    Missing,
    Running,
    #[cfg(unix)]
    Stopped,
    #[cfg(unix)]
    Zombie,
}

impl ProcessLiveness {
    fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    fn is_stopped(self) -> bool {
        #[cfg(unix)]
        {
            matches!(self, Self::Stopped)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    fn has_exited(self) -> bool {
        #[cfg(unix)]
        {
            matches!(self, Self::Missing | Self::Zombie)
        }
        #[cfg(not(unix))]
        {
            matches!(self, Self::Missing)
        }
    }
}

fn process_has_exited(pid: u32) -> bool {
    process_liveness(pid).has_exited()
}

fn process_is_running(pid: u32) -> bool {
    let liveness = process_liveness(pid);
    liveness.is_running() || liveness.is_stopped()
}

fn process_matches_record(process: &DaemonProcess) -> bool {
    let Some(actual) = process_executable(process.pid) else {
        return false;
    };
    canonical_or_original(&actual) == canonical_or_original(&process.executable)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    fs::read_link(Path::new("/proc").join(pid.to_string()).join("exe")).ok()
}

#[cfg(target_os = "macos")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

#[cfg(windows)]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid} -ErrorAction Stop).Path"),
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

fn process_liveness(pid: u32) -> ProcessLiveness {
    #[cfg(target_os = "linux")]
    {
        let status_path = Path::new("/proc").join(pid.to_string()).join("status");
        let raw = match fs::read_to_string(status_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ProcessLiveness::Missing;
            }
            Err(_) => return ProcessLiveness::Running,
        };
        let state = raw
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .and_then(|value| value.trim().chars().next());
        match state {
            Some('T') | Some('t') => ProcessLiveness::Stopped,
            Some('Z') => ProcessLiveness::Zombie,
            Some(_) => ProcessLiveness::Running,
            None => ProcessLiveness::Running,
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "state="])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                match String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .chars()
                    .next()
                {
                    Some('T') | Some('t') => ProcessLiveness::Stopped,
                    Some('Z') => ProcessLiveness::Zombie,
                    Some(_) => ProcessLiveness::Running,
                    None => ProcessLiveness::Missing,
                }
            }
            _ => ProcessLiveness::Missing,
        }
    }

    #[cfg(windows)]
    {
        if Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}")])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
        {
            ProcessLiveness::Running
        } else {
            ProcessLiveness::Missing
        }
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
    let request = format!("GET /v2/version HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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

    #[cfg(unix)]
    use std::{process::Command, thread, time::Duration};

    #[cfg(target_os = "linux")]
    use super::process_has_exited;
    use super::{pid_file_path, process_is_running, process_matches_record, status, DaemonProcess};
    #[cfg(unix)]
    use super::{process_liveness, ProcessLiveness};
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

    #[test]
    fn daemon_identity_rejects_a_reused_pid_with_another_executable() {
        let process = DaemonProcess {
            pid: std::process::id(),
            host: "127.0.0.1".to_owned(),
            port: 0,
            data_dir: std::env::temp_dir(),
            workspace_root: std::env::temp_dir(),
            started_at: Utc::now(),
            executable: std::env::temp_dir().join("definitely-not-todex-agentd"),
        };
        assert!(!process_matches_record(&process));
    }

    #[cfg(unix)]
    #[test]
    fn status_ignores_stopped_pid_file() {
        let root = unique_tmp_dir("todex-daemon-status-stopped");
        let config = test_config(root.clone());
        fs::create_dir_all(&config.data_dir).expect("create data dir");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("spawn sleeping process");
        let pid = child.id();
        send_signal("-STOP", pid);
        if process_liveness(pid) != ProcessLiveness::Stopped {
            send_signal("-CONT", pid);
            send_signal("-TERM", pid);
            let _ = child.wait();
            let _ = fs::remove_dir_all(root);
            return;
        }
        wait_for_liveness(pid, ProcessLiveness::Stopped);
        let process = DaemonProcess {
            pid,
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

        send_signal("-CONT", pid);
        send_signal("-TERM", pid);
        let _ = child.wait();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_has_exited_treats_zombies_as_exited() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn exiting process");
        let pid = child.id();
        wait_for_liveness(pid, ProcessLiveness::Zombie);

        assert!(process_has_exited(pid));

        let _ = child.wait();
    }

    #[cfg(unix)]
    fn send_signal(signal: &str, pid: u32) {
        let status = Command::new("kill")
            .arg(signal)
            .arg(pid.to_string())
            .status()
            .expect("send signal");
        assert!(status.success(), "failed to send {signal} to pid {pid}");
    }

    #[cfg(any(unix, target_os = "linux"))]
    fn wait_for_liveness(pid: u32, expected: ProcessLiveness) {
        for _ in 0..50 {
            if process_liveness(pid) == expected {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(process_liveness(pid), expected);
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
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                acp_profiles: Default::default(),
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
