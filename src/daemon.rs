use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::{sleep, MissedTickBehavior};

use crate::config::Config;
use crate::server_runner::{ManagedServer, ProviderProcessTracking};
use crate::update;

const PID_FILE_NAME: &str = "daemon.json";
const LOG_FILE_NAME: &str = "todex-agentd-daemon.log";
const START_TIMEOUT: Duration = Duration::from_secs(120);
const STOP_TIMEOUT: Duration = Duration::from_secs(12);
const STOP_FORCE_AFTER: Duration = Duration::from_secs(8);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(200);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// A found update restarts the daemon only after Agents were idle this long.
const UPDATE_RESTART_IDLE_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonProcess {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    #[serde(default)]
    pub workspace_roots: Vec<PathBuf>,
    pub started_at: DateTime<Utc>,
    pub executable: PathBuf,
    /// The external API listener (`host:port`), when enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_listen: Option<String>,
    /// Set by [`status`] when the executable on disk was replaced or removed
    /// after this daemon started, so it should be restarted to run it.
    #[serde(skip)]
    pub outdated: bool,
}

impl DaemonProcess {
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub async fn start(config: Config) -> Result<DaemonProcess> {
    if let Some(process) = recover(&config)? {
        return Ok(process);
    }
    // The child would refuse the same configuration; fail here with the
    // reason instead of a generic "exited during startup".
    config.ensure_listener_matches_auth()?;
    remove_stale_pid_file(&config.data_dir)?;
    if port_is_listening(&config.host, config.port) {
        bail!(
            "cannot start daemon because {}:{} is already in use by an unmanaged process; stop that service or choose another port",
            config.host,
            config.port
        );
    }
    config.ensure_api_listener_is_valid()?;
    if config.api.enabled && port_is_listening(&config.api.host, config.api.port) {
        bail!(
            "cannot start daemon because the API port {}:{} is already in use; stop that service or choose another api.port",
            config.api.host,
            config.api.port
        );
    }

    fs::create_dir_all(log_dir(&config.data_dir)).with_context(|| {
        format!(
            "failed to create log directory {}",
            log_dir(&config.data_dir).display()
        )
    })?;
    set_owner_only_directory(&config.data_dir)?;
    set_owner_only_directory(&log_dir(&config.data_dir))?;

    let log_path = log_file_path(&config.data_dir);
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(&executable);
    command.arg("daemon-run");
    detach_with_config(&mut command, &config)?;

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
                "daemon did not become ready within {:?}; check conversation recovery progress in {}",
                START_TIMEOUT,
                log_path.display()
            );
        }

        sleep(STATUS_POLL_INTERVAL).await;
    }
}

/// The `daemon-run` arguments every supervised launch receives; `daemon
/// start` and the `autostart` login registration must agree on them.
pub fn daemon_run_args(config: &Config) -> Vec<std::ffi::OsString> {
    let mut args = vec![
        "--host".into(),
        config.host.clone().into(),
        "--port".into(),
        config.port.to_string().into(),
        "--data-dir".into(),
        config.data_dir.clone().into_os_string(),
    ];
    for root in &config.workspace_roots {
        args.push("--workspace-root".into());
        args.push(root.clone().into_os_string());
    }
    if config.api.enabled {
        args.extend([
            "--enable-api".into(),
            "--api-host".into(),
            config.api.host.clone().into(),
            "--api-port".into(),
            config.api.port.to_string().into(),
        ]);
    }
    args
}

/// Adds the resolved configuration, sends output to the daemon log, and puts
/// the child in its own process group so it outlives its parent.
fn detach_with_config(command: &mut Command, config: &Config) -> Result<()> {
    let log_path = log_file_path(&config.data_dir);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log {}", log_path.display()))?;
    set_owner_only_file(&log_path)?;
    command.args(daemon_run_args(config));
    command
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
    Ok(())
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
    if read_pid_file(&config.data_dir)?.is_none() {
        recover(config)?;
    }
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
        let outdated = executable_is_outdated(&process);
        return Ok(Some(DaemonProcess {
            outdated,
            ..process
        }));
    }

    Ok(None)
}

/// Like [`status`], but when the pid file is missing it also looks for a
/// running daemon on the same data directory and records it again. Older
/// builds deleted the pid file of a daemon whose executable had been
/// replaced, leaving it running unmanaged and holding its port.
pub fn recover(config: &Config) -> Result<Option<DaemonProcess>> {
    if let Some(process) = status(config)? {
        return Ok(Some(process));
    }
    if read_pid_file(&config.data_dir)?.is_some() {
        return Ok(None);
    }
    let Some(process) = find_unrecorded_daemon(config) else {
        return Ok(None);
    };
    match write_pid_record(&process) {
        Ok(()) => {
            tracing::info!(
                pid = process.pid,
                listen = %process.listen_addr(),
                "recovered running daemon without a pid file"
            );
        }
        // Another caller recorded a daemon first; report whatever it wrote.
        Err(error) if read_pid_file(&config.data_dir)?.is_none() => return Err(error),
        Err(_) => {}
    }
    status(config)
}

pub async fn run(config: Config) -> Result<()> {
    // A supervisor (launchd/systemd) may restart this process after a crash
    // that left the pid file behind; clear it when the recorded process is
    // gone so the restart can claim it.
    remove_stale_pid_file(&config.data_dir)?;
    let server = ManagedServer::start(config, ProviderProcessTracking::Enabled).await?;
    let api_listen = server
        .api_addr()
        .map(|addr| format!("{}:{}", server.config().api.host, addr.port()));
    let process = write_pid_file(server.config(), server.addr().port(), api_listen)?;
    let pid_file = PidFileGuard {
        data_dir: server.config().data_dir.clone(),
        pid: process.pid,
    };
    let mut restart_config = server.config().clone();
    restart_config.port = process.port;

    tracing::info!(
        pid = process.pid,
        listen = %process.listen_addr(),
        pid_file = %pid_file_path(&process.data_dir).display(),
        "todex-agentd daemon ready"
    );

    let (update_tx, update_rx) = watch::channel(None);
    let update_checks = update::enabled().then(|| tokio::spawn(watch_for_updates(update_tx)));
    let result = wait_for_shutdown_or_server_exit(server, update_rx).await;
    if let Some(checks) = update_checks {
        checks.abort();
    }
    let Some(version) = result? else {
        return Ok(());
    };
    // Remove the pid file first, so the replacement's `daemon start` does not
    // find this daemon and return it as already running.
    drop(pid_file);
    spawn_update_restart(&restart_config, &version)
}

async fn watch_for_updates(available: watch::Sender<Option<String>>) {
    let mut checks = tokio::time::interval_at(
        tokio::time::Instant::now() + UPDATE_CHECK_INTERVAL,
        UPDATE_CHECK_INTERVAL,
    );
    // After sleep or suspend, check once instead of catching up.
    checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        checks.tick().await;
        if let Some(version) = update::check_for_daemon().await {
            tracing::info!(
                version,
                idle_minutes = UPDATE_RESTART_IDLE_AFTER.as_secs() / 60,
                "backend update available; restarting once no Agent has run for the idle period"
            );
            available.send_replace(Some(version));
        }
    }
}

/// Starts the replacement through `daemon start`, which installs the update
/// before launching it. Startup failures land in the daemon log.
fn spawn_update_restart(config: &Config, version: &str) -> Result<()> {
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(&executable);
    command.arg("daemon").arg("start");
    detach_with_config(&mut command, config)?;
    update::prepare_handoff(&mut command, version);
    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn update restart {}", executable.display()))?;
    tracing::info!(
        pid = child.id(),
        version,
        log = %log_file_path(&config.data_dir).display(),
        "daemon stopped for update; replacement is starting"
    );
    Ok(())
}

/// Measures how long the daemon has continuously had no Agent work.
#[derive(Debug, Default)]
struct IdleTimer {
    since: Option<Instant>,
}

impl IdleTimer {
    fn observe(&mut self, idle: bool, now: Instant) -> Duration {
        if !idle {
            self.since = None;
            return Duration::ZERO;
        }
        now.saturating_duration_since(*self.since.get_or_insert(now))
    }
}

/// The address-only pairing link shown as a QR code.
pub fn pairing_qr_payload(config: &Config, port: u16) -> Result<String> {
    Ok(crate::transport_crypto::pairing_link_json(config, port)?)
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

fn write_pid_file(config: &Config, port: u16, api_listen: Option<String>) -> Result<DaemonProcess> {
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
        workspace_roots: config.workspace_roots.clone(),
        started_at: Utc::now(),
        executable: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("todex-agentd")),
        api_listen,
        outdated: false,
    };
    match write_pid_record(&process) {
        // `recover` may have recorded this daemon between its listener
        // coming up and this write; take the record over.
        Err(_)
            if read_pid_file(&config.data_dir)?
                .is_some_and(|recorded| recorded.pid == process.pid) =>
        {
            fs::write(&path, serde_json::to_string_pretty(&process)?)
                .with_context(|| format!("failed to write {}", path.display()))?;
            Ok(process)
        }
        result => result.map(|()| process),
    }
}

fn write_pid_record(process: &DaemonProcess) -> Result<()> {
    let path = pid_file_path(&process.data_dir);
    let raw = serde_json::to_string_pretty(process)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to exclusively create {}", path.display()))?;
    file.write_all(raw.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    set_owner_only_file(&path)
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

/// Serves until shutdown. Returns the release to restart into once one is
/// available and no Agent has run for `UPDATE_RESTART_IDLE_AFTER`.
async fn wait_for_shutdown_or_server_exit(
    server: ManagedServer,
    update: watch::Receiver<Option<String>>,
) -> Result<Option<String>> {
    let mut idle = IdleTimer::default();
    loop {
        if server.is_finished() {
            server.wait().await?;
            return Ok(None);
        }

        tokio::select! {
            signal = shutdown_signal() => {
                signal?;
                server.stop().await?;
                return Ok(None);
            }
            _ = sleep(STATUS_POLL_INTERVAL) => {}
        }

        let idle_for = idle.observe(server.is_agent_idle(), Instant::now());
        if idle_for < UPDATE_RESTART_IDLE_AFTER {
            continue;
        }
        let Some(version) = update.borrow().clone() else {
            continue;
        };
        tracing::info!(
            version,
            "no Agent is running; stopping the daemon to install the update"
        );
        server.stop().await?;
        return Ok(Some(version));
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

/// Signal the daemon's process group. Daemons spawned before `process_group(0)`
/// was adopted are not group leaders, so fall back to the single pid on ESRCH.
#[cfg(unix)]
fn signal_process_group_or_self(pid: u32, signal: i32) -> bool {
    if unsafe { libc::kill(-(pid as i32), signal) } == 0 {
        return true;
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return unsafe { libc::kill(pid as i32, signal) } == 0;
    }
    false
}

fn continue_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        if !signal_process_group_or_self(pid, libc::SIGCONT) && !process_has_exited(pid) {
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
        if !signal_process_group_or_self(pid, libc::SIGTERM) && process_is_running(pid) {
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
        if !signal_process_group_or_self(pid, libc::SIGKILL) && !process_has_exited(pid) {
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

/// Whether the daemon's executable path no longer holds the binary it runs:
/// it was rebuilt, updated in place, or deleted.
fn executable_is_outdated(process: &DaemonProcess) -> bool {
    let Ok(on_disk) = fs::metadata(&process.executable) else {
        return true;
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        // `/proc/<pid>/exe` opens the inode the process runs, even unlinked.
        if let Ok(running) = fs::metadata(proc_path(process.pid).join("exe")) {
            return running.dev() != on_disk.dev() || running.ino() != on_disk.ino();
        }
    }
    on_disk
        .modified()
        .is_ok_and(|modified| DateTime::<Utc>::from(modified) > process.started_at)
}

#[cfg(target_os = "linux")]
fn proc_path(pid: u32) -> PathBuf {
    Path::new("/proc").join(pid.to_string())
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let path = fs::read_link(proc_path(pid).join("exe")).ok()?;
    // A replaced or deleted executable reads back as "<path> (deleted)".
    Some(
        path.to_str()
            .and_then(|raw| raw.strip_suffix(" (deleted)"))
            .map(PathBuf::from)
            .unwrap_or(path),
    )
}

/// Finds a healthy `daemon-run` process of this executable family serving
/// `config.data_dir` and rebuilds its pid file record from its arguments.
#[cfg(target_os = "linux")]
fn find_unrecorded_daemon(config: &Config) -> Option<DaemonProcess> {
    let data_dir = canonical_or_original(&config.data_dir);
    let own_pid = std::process::id();
    fs::read_dir("/proc")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != own_pid)
        .find_map(|pid| {
            let executable = process_executable(pid)?;
            if !executable
                .file_name()?
                .to_str()?
                .starts_with("todex-agentd")
            {
                return None;
            }
            let cmdline = fs::read(proc_path(pid).join("cmdline")).ok()?;
            let args = cmdline
                .split(|byte| *byte == 0)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect::<Vec<_>>();
            let process = daemon_record_from_args(pid, executable, &args, config)?;
            // A busy daemon may miss one short health timeout; it is only
            // checked once per recovery, so allow a few attempts.
            if canonical_or_original(&process.data_dir) != data_dir
                || !(0..3).any(|_| daemon_health_check(&process))
            {
                return None;
            }
            Some(DaemonProcess {
                started_at: process_started_at(pid).unwrap_or(process.started_at),
                ..process
            })
        })
}

#[cfg(not(target_os = "linux"))]
fn find_unrecorded_daemon(_config: &Config) -> Option<DaemonProcess> {
    // Elsewhere a replaced executable keeps matching its pid file record, so
    // the record is not lost in the first place.
    None
}

/// Parses `daemon-run` arguments as written by [`daemon_run_args`].
#[cfg(any(target_os = "linux", test))]
fn daemon_record_from_args(
    pid: u32,
    executable: PathBuf,
    args: &[String],
    config: &Config,
) -> Option<DaemonProcess> {
    if !args.iter().any(|arg| arg == "daemon-run") {
        return None;
    }
    let value = |flag: &str| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let port = match value("--port") {
        Some(port) => port.parse::<u16>().ok()?,
        None => config.port,
    };
    if port == 0 {
        return None;
    }
    let workspace_roots = args
        .windows(2)
        .filter(|pair| pair[0] == "--workspace-root")
        .map(|pair| PathBuf::from(&pair[1]))
        .collect();
    let api_listen = value("--api-port").map(|api_port| {
        format!(
            "{}:{api_port}",
            value("--api-host").unwrap_or_else(|| config.api.host.clone())
        )
    });
    Some(DaemonProcess {
        pid,
        host: value("--host").unwrap_or_else(|| config.host.clone()),
        port,
        data_dir: value("--data-dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| config.data_dir.clone()),
        workspace_roots,
        started_at: Utc::now(),
        executable,
        api_listen,
        outdated: false,
    })
}

/// Reads a process start time from `/proc/<pid>/stat` and the boot time.
#[cfg(target_os = "linux")]
fn process_started_at(pid: u32) -> Option<DateTime<Utc>> {
    let stat = fs::read_to_string(proc_path(pid).join("stat")).ok()?;
    // Field 22 (starttime); the fields after the parenthesized name start at 3.
    let start_ticks = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    let boot_time = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse::<i64>()
        .ok()?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let millis = (start_ticks as i64).checked_mul(1000)? / ticks_per_second as i64;
    DateTime::from_timestamp_millis(boot_time.checked_mul(1000)?.checked_add(millis)?)
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

    #[cfg(target_os = "macos")]
    {
        // `ps` costs a fork+exec per call; the TUI polls liveness every tick
        // and each spawn triggers a fresh syspolicyd/XProtect evaluation of
        // this binary. `proc_pidinfo` answers the same question in-process.
        let Ok(pid) = libc::c_int::try_from(pid) else {
            return ProcessLiveness::Missing;
        };
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
            )
        };
        if written != std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int {
            // ESRCH means the pid is gone; anything else (for example EPERM on
            // a process owned by another user) means it exists but cannot be
            // inspected, which must not be reported as "exited".
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => ProcessLiveness::Missing,
                _ => ProcessLiveness::Running,
            };
        }
        match info.pbi_status {
            libc::SSTOP => ProcessLiveness::Stopped,
            libc::SZOMB => ProcessLiveness::Zombie,
            _ => ProcessLiveness::Running,
        }
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
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

fn port_is_listening(host: &str, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    health_addr(host, port)
        .and_then(|addr| TcpStream::connect_timeout(&addr, HEALTH_TIMEOUT).ok())
        .is_some()
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
    use std::{env, fs, net::TcpListener};

    #[cfg(unix)]
    use std::{process::Command, thread};

    use super::{
        daemon_record_from_args, daemon_run_args, pid_file_path, port_is_listening,
        process_is_running, process_matches_record, status, DaemonProcess, IdleTimer,
        UPDATE_RESTART_IDLE_AFTER,
    };
    #[cfg(target_os = "linux")]
    use super::{executable_is_outdated, process_has_exited, process_started_at};
    #[cfg(unix)]
    use super::{process_liveness, ProcessLiveness};
    use crate::config::{AgentConfig, Config, PairingEncryption, SecurityConfig};
    use chrono::Utc;
    use std::time::{Duration, Instant};

    #[test]
    fn idle_timer_restarts_whenever_an_agent_runs() {
        let start = Instant::now();
        let mut idle = IdleTimer::default();
        assert_eq!(idle.observe(true, start), Duration::ZERO);
        assert_eq!(
            idle.observe(true, start + UPDATE_RESTART_IDLE_AFTER),
            UPDATE_RESTART_IDLE_AFTER
        );
        assert_eq!(
            idle.observe(false, start + Duration::from_secs(301)),
            Duration::ZERO
        );
        let resumed = start + Duration::from_secs(302);
        assert_eq!(idle.observe(true, resumed), Duration::ZERO);
        assert_eq!(
            idle.observe(true, resumed + Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn port_preflight_detects_an_existing_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind listener");
        let port = listener.local_addr().expect("listener address").port();

        assert!(port_is_listening("127.0.0.1", port));
        assert!(port_is_listening("0.0.0.0", port));
        assert!(!port_is_listening("127.0.0.1", 0));
    }

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
            workspace_roots: config.workspace_roots.clone(),
            started_at: Utc::now(),
            executable: env::current_exe().unwrap(),
            api_listen: None,
            outdated: false,
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
            workspace_roots: config.workspace_roots.clone(),
            started_at: Utc::now(),
            executable: env::current_exe().unwrap(),
            api_listen: None,
            outdated: false,
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
            workspace_roots: vec![std::env::temp_dir()],
            started_at: Utc::now(),
            executable: std::env::temp_dir().join("definitely-not-todex-agentd"),
            api_listen: None,
            outdated: false,
        };
        assert!(!process_matches_record(&process));
    }

    #[test]
    fn daemon_record_round_trips_daemon_run_args() {
        let root = unique_tmp_dir("todex-daemon-run-args");
        let mut config = test_config(root.clone());
        config.api.enabled = true;
        let mut args = vec!["todex-agentd".to_owned(), "daemon-run".to_owned()];
        args.extend(
            daemon_run_args(&config)
                .into_iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        );

        let record =
            daemon_record_from_args(42, env::current_exe().unwrap(), &args, &config).unwrap();

        assert_eq!(record.pid, 42);
        assert_eq!(record.listen_addr(), "127.0.0.1:7345");
        assert_eq!(record.data_dir, config.data_dir);
        assert_eq!(record.workspace_roots, config.workspace_roots);
        assert_eq!(
            record.api_listen,
            Some(format!("{}:{}", config.api.host, config.api.port))
        );
        assert!(
            daemon_record_from_args(42, env::current_exe().unwrap(), &args[2..], &config).is_none()
        );
    }

    /// Rebuilding or updating the daemon binary must not make the running
    /// daemon look stopped (which used to delete its pid file); it should be
    /// reported as running and outdated instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_executable_still_matches_and_is_outdated() {
        let root = unique_tmp_dir("todex-daemon-replaced-exe");
        fs::create_dir_all(&root).expect("create temp dir");
        let executable = root.join("todex-agentd");
        fs::copy("/bin/sleep", &executable).expect("copy sleep");
        // Concurrent tests forking while the copy was open can briefly keep
        // a write handle alive, which fails exec with ETXTBSY.
        let mut child = (0..50)
            .find_map(|_| match Command::new(&executable).arg("30").spawn() {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    thread::sleep(Duration::from_millis(20));
                    None
                }
                result => Some(result),
            })
            .expect("executable stayed busy")
            .expect("spawn copied executable");
        let process = DaemonProcess {
            pid: child.id(),
            host: "127.0.0.1".to_owned(),
            port: 0,
            data_dir: root.clone(),
            workspace_roots: Vec::new(),
            started_at: Utc::now(),
            executable: executable.clone(),
            api_listen: None,
            outdated: false,
        };
        assert!(process_matches_record(&process));
        assert!(!executable_is_outdated(&process));

        fs::remove_file(&executable).expect("remove executable");
        assert!(process_matches_record(&process));
        assert!(executable_is_outdated(&process));

        fs::copy("/bin/sleep", &executable).expect("replace executable");
        assert!(process_matches_record(&process));
        assert!(executable_is_outdated(&process));
        assert!(process_started_at(process.pid)
            .is_some_and(|started| (Utc::now() - started).num_seconds().abs() < 60));

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(root);
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
            workspace_roots: config.workspace_roots.clone(),
            started_at: Utc::now(),
            executable: env::current_exe().unwrap(),
            api_listen: None,
            outdated: false,
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
            workspace_roots: vec![root.join("workspace")],
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
        }
    }

    fn unique_tmp_dir(label: &str) -> std::path::PathBuf {
        env::temp_dir().join(format!("{label}-{}", std::process::id()))
    }
}
