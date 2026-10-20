//! Manual port forwards (`ssh -L` / `-R` / `-D`).
//!
//! Each running forward is one dedicated ssh process that neither uses nor
//! becomes a shared master, so stopping it never affects other sessions.
//! Readiness comes from ssh's own debug log: the listener line for local and
//! dynamic forwards, the server's confirmation for remote ones. Forwards
//! never start on their own; runtime state lives in memory only.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use uuid::Uuid;

use super::{
    classify_failure,
    tunnel::{self, GroupChild, StderrTail},
    SshFailureKind,
};
use crate::{error::AppError, remote_fs::askpass::AskpassSecret};

/// Forwards running at the same time.
pub(crate) const MAX_RUNNING_FORWARDS: usize = 32;
/// Forwards configured in total.
pub(crate) const MAX_FORWARDS: usize = 256;
const MAX_LABEL_CHARS: usize = 80;
const MAX_HOST_LEN: usize = 253;
/// How long `start` waits for a forward to become ready.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Without a listener line, a local forward is probed this long after ssh
/// authenticated (other OpenSSH versions may word their log differently).
const PROBE_AFTER_AUTH: Duration = Duration::from_secs(2);

const LOCAL_READY: &str = "local forwarding listening on";
const REMOTE_READY: &str = "remote forward success for:";
const AUTHENTICATED: &str = "authenticated to ";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ForwardKind {
    Local,
    Remote,
    Dynamic,
}

/// Body of `POST /v2/ssh/forwards` and `PUT /v2/ssh/forwards/{id}`. Ports
/// are wide integers so out-of-range values get a clear 400.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ForwardInput {
    pub alias: String,
    pub kind: ForwardKind,
    #[serde(default)]
    pub bind_address: Option<String>,
    pub bind_port: i64,
    #[serde(default)]
    pub target_host: Option<String>,
    #[serde(default)]
    pub target_port: Option<i64>,
    #[serde(default)]
    pub label: Option<String>,
}

/// A saved forward, as stored in `store.json` and returned by the API.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForwardSpec {
    pub id: String,
    pub alias: String,
    pub kind: ForwardKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    pub bind_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ForwardStatus {
    #[default]
    Stopped,
    Starting,
    Running,
    Failed,
}

/// Runtime state of one forward.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForwardRunInfo {
    pub status: ForwardStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<SshFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

/// `SshForward` of the API.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForwardView {
    #[serde(flatten)]
    pub spec: ForwardSpec,
    #[serde(flatten)]
    pub run: ForwardRunInfo,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::InvalidRequest(message.into())
}

fn port(value: i64, field: &str) -> Result<u16, AppError> {
    u16::try_from(value)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| invalid(format!("{field} must be 1-65535")))
}

/// Host names and IP literals: letters, digits and `.-_:%`, not starting
/// with `-` (so ssh cannot read it as an option).
fn valid_host(host: &str, allow_wildcard: bool) -> bool {
    !host.is_empty()
        && host.len() <= MAX_HOST_LEN
        && !host.starts_with('-')
        && (host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'))
            || (allow_wildcard && host == "*"))
}

/// Local listeners only ever bind loopback.
fn loopback_bind(value: &str) -> bool {
    matches!(value, "127.0.0.1" | "::1" | "localhost")
}

/// Checks and normalizes API input. `id` is kept for updates.
pub(crate) fn normalize(id: Option<String>, input: ForwardInput) -> Result<ForwardSpec, AppError> {
    let alias = input.alias.trim().to_owned();
    if !super::config_file::valid_alias(&alias) {
        return Err(invalid(format!("invalid ssh host: {alias}")));
    }
    let bind_port = port(input.bind_port, "bindPort")?;
    let bind_address = input
        .bind_address
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let bind_address = match input.kind {
        ForwardKind::Local | ForwardKind::Dynamic => {
            let value = bind_address.unwrap_or_else(|| "127.0.0.1".to_owned());
            if !loopback_bind(&value) {
                return Err(invalid(
                    "bindAddress must be 127.0.0.1, ::1 or localhost: local listeners stay on \
                     this machine's loopback interface",
                ));
            }
            Some(value)
        }
        ForwardKind::Remote => {
            if let Some(value) = &bind_address {
                if !valid_host(value, true) {
                    return Err(invalid(
                        "bindAddress must be a host name or IP address (or *)",
                    ));
                }
            }
            bind_address
        }
    };
    let (target_host, target_port) = match input.kind {
        ForwardKind::Dynamic => {
            if input.target_host.is_some() || input.target_port.is_some() {
                return Err(invalid(
                    "targetHost and targetPort are not used by dynamic forwards",
                ));
            }
            (None, None)
        }
        ForwardKind::Local | ForwardKind::Remote => {
            let host = input
                .target_host
                .map(|value| value.trim().to_owned())
                .unwrap_or_default();
            if !valid_host(&host, false) {
                return Err(invalid(format!(
                    "targetHost must be a host name or IP address of at most {MAX_HOST_LEN} \
                     characters, without whitespace and not starting with '-'"
                )));
            }
            let target_port = input
                .target_port
                .ok_or_else(|| invalid("targetPort is required"))?;
            (Some(host), Some(port(target_port, "targetPort")?))
        }
    };
    let label = input
        .label
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if label.as_ref().is_some_and(|label| {
        label.chars().count() > MAX_LABEL_CHARS || label.chars().any(char::is_control)
    }) {
        return Err(invalid(format!(
            "label must be at most {MAX_LABEL_CHARS} printable characters"
        )));
    }
    Ok(ForwardSpec {
        id: id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        alias,
        kind: input.kind,
        bind_address,
        bind_port,
        target_host,
        target_port,
        label,
    })
}

/// A stored forward from any daemon version: re-checked, invalid ones are
/// dropped with a warning.
pub(crate) fn from_stored(value: serde_json::Value) -> Option<ForwardSpec> {
    let spec: ForwardSpec = match serde_json::from_value(value) {
        Ok(spec) => spec,
        Err(error) => {
            tracing::warn!(%error, "ignoring unreadable stored ssh forward");
            return None;
        }
    };
    let input = ForwardInput {
        alias: spec.alias.clone(),
        kind: spec.kind,
        bind_address: spec.bind_address.clone(),
        bind_port: spec.bind_port.into(),
        target_host: spec.target_host.clone(),
        target_port: spec.target_port.map(Into::into),
        label: spec.label.clone(),
    };
    match normalize(Some(spec.id.clone()), input) {
        Ok(checked) if !spec.id.is_empty() && spec.id.len() <= 64 => Some(checked),
        Ok(_) => {
            tracing::warn!("ignoring stored ssh forward with an invalid id");
            None
        }
        Err(error) => {
            tracing::warn!(id = spec.id, %error, "ignoring invalid stored ssh forward");
            None
        }
    }
}

fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// The ssh forwarding option and its argument for `spec`.
pub(crate) fn forward_args(spec: &ForwardSpec) -> [String; 2] {
    let target = || {
        format!(
            "{}:{}",
            bracket(spec.target_host.as_deref().unwrap_or_default()),
            spec.target_port.unwrap_or_default()
        )
    };
    let bind = |address: &Option<String>| match address {
        Some(address) => format!("{}:{}", bracket(address), spec.bind_port),
        None => spec.bind_port.to_string(),
    };
    match spec.kind {
        ForwardKind::Local => [
            "-L".into(),
            format!("{}:{}", bind(&spec.bind_address), target()),
        ],
        ForwardKind::Remote => [
            "-R".into(),
            format!("{}:{}", bind(&spec.bind_address), target()),
        ],
        ForwardKind::Dynamic => ["-D".into(), bind(&spec.bind_address)],
    }
}

/// Loopback addresses a local or dynamic forward listens on.
fn local_addresses(spec: &ForwardSpec) -> Vec<IpAddr> {
    match spec.kind {
        ForwardKind::Remote => Vec::new(),
        ForwardKind::Local | ForwardKind::Dynamic => match spec.bind_address.as_deref() {
            Some("::1") => vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
            Some("localhost") => vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ],
            _ => vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        },
    }
}

/// A listener of the daemon itself that forwards must not take over.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReservedListener {
    /// `None` for a wildcard address (every interface, loopback included).
    pub address: Option<IpAddr>,
    pub port: u16,
}

struct Entry {
    run: ForwardRunInfo,
    generation: u64,
    /// Local listener addresses while starting or running.
    listens: Vec<IpAddr>,
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    next_generation: u64,
}

#[derive(Clone, Default)]
pub(crate) struct ForwardRuntime {
    state: Arc<Mutex<State>>,
    reserved: Arc<Mutex<Vec<ReservedListener>>>,
}

fn active(run: &ForwardRunInfo) -> bool {
    matches!(run.status, ForwardStatus::Starting | ForwardStatus::Running)
}

impl ForwardRuntime {
    pub(crate) fn set_reserved(&self, reserved: Vec<ReservedListener>) {
        *self.reserved.lock().unwrap() = reserved;
    }

    pub(crate) fn info(&self, id: &str) -> ForwardRunInfo {
        self.state
            .lock()
            .unwrap()
            .entries
            .get(id)
            .map(|entry| entry.run.clone())
            .unwrap_or_default()
    }

    pub(crate) fn is_active(&self, id: &str) -> bool {
        active(&self.info(id))
    }

    /// Marks `spec` as starting after checking the running limit and local
    /// port clashes. Returns the generation for [`Self::run`], or `None`
    /// when it is already starting or running.
    pub(crate) fn begin(&self, spec: &ForwardSpec) -> Result<Option<u64>, AppError> {
        let mut state = self.state.lock().unwrap();
        if state.entries.get(&spec.id).is_some_and(|e| active(&e.run)) {
            return Ok(None);
        }
        let running = state.entries.values().filter(|e| active(&e.run)).count();
        if running >= MAX_RUNNING_FORWARDS {
            return Err(AppError::Conflict(format!(
                "at most {MAX_RUNNING_FORWARDS} forwards can run at the same time"
            )));
        }
        let listens = local_addresses(spec);
        if !listens.is_empty() {
            let clash = state.entries.iter().any(|(id, entry)| {
                id != &spec.id
                    && active(&entry.run)
                    && entry.port == spec.bind_port
                    && entry.listens.iter().any(|ip| listens.contains(ip))
            });
            if clash {
                return Err(AppError::Conflict(format!(
                    "another running forward already listens on port {}",
                    spec.bind_port
                )));
            }
            let reserved = self.reserved.lock().unwrap().iter().any(|listener| {
                listener.port == spec.bind_port
                    && listener
                        .address
                        .is_none_or(|address| listens.contains(&address))
            });
            if reserved {
                return Err(AppError::Conflict(format!(
                    "port {} is used by the TodeX backend itself",
                    spec.bind_port
                )));
            }
        }
        state.next_generation += 1;
        let generation = state.next_generation;
        let previous = state.entries.remove(&spec.id);
        state.entries.insert(
            spec.id.clone(),
            Entry {
                run: ForwardRunInfo {
                    status: ForwardStatus::Starting,
                    ..ForwardRunInfo::default()
                },
                generation,
                listens,
                port: spec.bind_port,
                stop: None,
                task: None,
            },
        );
        drop(state);
        // A finished task of an earlier run has nothing left to stop.
        drop(previous);
        Ok(Some(generation))
    }

    /// Ends a start that failed before ssh ran.
    pub(crate) fn fail(&self, id: &str, generation: u64, failure: SshFailureKind, error: String) {
        self.update(id, generation, |run| {
            *run = ForwardRunInfo {
                status: ForwardStatus::Failed,
                failure: Some(failure),
                error: Some(error),
                started_at: None,
            }
        });
    }

    fn update(&self, id: &str, generation: u64, change: impl FnOnce(&mut ForwardRunInfo)) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(id) {
            if entry.generation == generation {
                change(&mut entry.run);
                if !active(&entry.run) {
                    entry.listens.clear();
                }
            }
        }
    }

    /// Runs `command` (already carrying the ssh options, forward and alias)
    /// and waits until the forward is ready, failed, or the wait ran out.
    pub(crate) async fn run(
        &self,
        spec: &ForwardSpec,
        generation: u64,
        mut command: Command,
        secret: Option<AskpassSecret>,
    ) -> ForwardRunInfo {
        if let Some(error) = local_port_in_use(spec).await {
            self.fail(&spec.id, generation, SshFailureKind::Other, error);
            return self.info(&spec.id);
        }
        tunnel::piped_stderr(&mut command);
        let mut child = match GroupChild::spawn(&mut command) {
            Ok(child) => child,
            Err(error) => {
                let message = if error.kind() == std::io::ErrorKind::NotFound {
                    "ssh executable not found".to_owned()
                } else {
                    format!("cannot start ssh: {error}")
                };
                self.fail(&spec.id, generation, SshFailureKind::Other, message);
                return self.info(&spec.id);
            }
        };
        let tail = StderrTail::default();
        let (lines_tx, lines_rx) = mpsc::unbounded_channel();
        if let Some(stderr) = child.child_mut().stderr.take() {
            tunnel::drain_stderr(stderr, tail.clone(), Some(lines_tx));
        }
        let (ready_tx, ready_rx) = oneshot::channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = tokio::spawn(supervise(Supervision {
            runtime: self.clone(),
            id: spec.id.clone(),
            generation,
            kind: spec.kind,
            probe: local_addresses(spec)
                .first()
                .map(|ip| SocketAddr::new(*ip, spec.bind_port)),
            child,
            tail,
            lines: lines_rx,
            ready: Some(ready_tx),
            stop: stop_rx,
            secret,
        }));
        {
            let mut state = self.state.lock().unwrap();
            match state.entries.get_mut(&spec.id) {
                Some(entry) if entry.generation == generation => {
                    entry.stop = Some(stop_tx);
                    entry.task = Some(task);
                }
                // Stopped or deleted meanwhile.
                _ => {
                    let _ = stop_tx.send(());
                }
            }
        }
        let _ = tokio::time::timeout(READY_TIMEOUT + Duration::from_secs(5), ready_rx).await;
        self.info(&spec.id)
    }

    /// Stops a starting or running forward and waits for its process.
    pub(crate) async fn stop(&self, id: &str) {
        let (stop, task) = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(id) else {
                return;
            };
            entry.run = ForwardRunInfo::default();
            entry.listens.clear();
            entry.generation = u64::MAX;
            (entry.stop.take(), entry.task.take())
        };
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        if let Some(task) = task {
            if tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
            {
                tracing::warn!(id, "ssh forward did not stop in time");
            }
        }
    }

    /// Stops `id` and drops its runtime state.
    pub(crate) async fn forget(&self, id: &str) {
        self.stop(id).await;
        self.state.lock().unwrap().entries.remove(id);
    }

    pub(crate) async fn stop_all(&self) {
        let ids: Vec<String> = self.state.lock().unwrap().entries.keys().cloned().collect();
        futures_util::future::join_all(ids.iter().map(|id| self.stop(id))).await;
    }
}

/// Fails fast, before signing in, when a local listener address is taken.
async fn local_port_in_use(spec: &ForwardSpec) -> Option<String> {
    for ip in local_addresses(spec) {
        let socket = match ip {
            IpAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
            IpAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
        };
        let Ok(socket) = socket else { continue };
        let _ = socket.set_reuseaddr(true);
        let address = SocketAddr::new(ip, spec.bind_port);
        let result = socket.bind(address).and_then(|()| socket.listen(1));
        match result {
            Ok(_listener) => {}
            // `localhost` may have no IPv6 loopback; ssh skips it too.
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrNotAvailable
                    && spec.bind_address.as_deref() == Some("localhost") => {}
            Err(error) => return Some(format!("cannot listen on {address}: {error}")),
        }
    }
    None
}

struct Supervision {
    runtime: ForwardRuntime,
    id: String,
    generation: u64,
    kind: ForwardKind,
    probe: Option<SocketAddr>,
    child: GroupChild,
    tail: StderrTail,
    lines: mpsc::UnboundedReceiver<String>,
    ready: Option<oneshot::Sender<()>>,
    stop: oneshot::Receiver<()>,
    /// Kept until ssh is ready or gone; the helper deletes it once used.
    secret: Option<AskpassSecret>,
}

async fn supervise(mut s: Supervision) {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    let mut authenticated_at: Option<tokio::time::Instant> = None;
    let mut probe_tick = tokio::time::interval(Duration::from_millis(250));
    let mut lines_open = true;
    let mut ready = false;
    let mark_ready = |s: &mut Supervision| {
        s.runtime.update(&s.id, s.generation, |run| {
            run.status = ForwardStatus::Running;
            run.started_at = Some(Utc::now());
        });
        s.secret = None;
        if let Some(ready) = s.ready.take() {
            let _ = ready.send(());
        }
    };
    loop {
        tokio::select! {
            line = s.lines.recv(), if lines_open => {
                let Some(line) = line else {
                    lines_open = false;
                    continue;
                };
                if ready {
                    continue;
                }
                let lower = line.to_ascii_lowercase();
                if lower.contains(AUTHENTICATED) && authenticated_at.is_none() {
                    authenticated_at = Some(tokio::time::Instant::now());
                }
                let is_ready = match s.kind {
                    ForwardKind::Local | ForwardKind::Dynamic => lower.contains(LOCAL_READY),
                    ForwardKind::Remote => lower.contains(REMOTE_READY),
                };
                if is_ready {
                    ready = true;
                    mark_ready(&mut s);
                }
            }
            status = s.child.wait() => {
                // Let the reader catch the last lines.
                tokio::time::sleep(Duration::from_millis(50)).await;
                while let Ok(_line) = s.lines.try_recv() {}
                let summary = s.tail.summary();
                let text = s.tail.text();
                let error = if summary.is_empty() {
                    match status {
                        Ok(status) => format!("ssh exited ({status})"),
                        Err(error) => format!("ssh failed: {error}"),
                    }
                } else {
                    summary
                };
                let failure = classify_failure(&text);
                s.runtime.update(&s.id, s.generation, |run| {
                    *run = ForwardRunInfo {
                        status: ForwardStatus::Failed,
                        failure: Some(failure),
                        error: Some(error),
                        started_at: None,
                    }
                });
                break;
            }
            _ = &mut s.stop => {
                s.child.terminate().await;
                break;
            }
            _ = tokio::time::sleep_until(deadline), if !ready => {
                if authenticated_at.is_some() {
                    // Signed in and still running: other OpenSSH versions
                    // may log readiness differently.
                    ready = true;
                    mark_ready(&mut s);
                } else {
                    s.child.terminate().await;
                    s.runtime.update(&s.id, s.generation, |run| {
                        *run = ForwardRunInfo {
                            status: ForwardStatus::Failed,
                            failure: Some(SshFailureKind::TimedOut),
                            error: Some("ssh did not sign in within 20 seconds".to_owned()),
                            started_at: None,
                        }
                    });
                    break;
                }
            }
            _ = probe_tick.tick(), if !ready && s.probe.is_some() && authenticated_at
                .is_some_and(|at| at.elapsed() >= PROBE_AFTER_AUTH) => {
                if let Some(address) = s.probe {
                    if tokio::net::TcpStream::connect(address).await.is_ok() {
                        ready = true;
                        mark_ready(&mut s);
                    }
                }
            }
        }
    }
    drop(s.secret.take());
    if let Some(ready) = s.ready.take() {
        let _ = ready.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: ForwardKind) -> ForwardInput {
        ForwardInput {
            alias: "db".into(),
            kind,
            bind_address: None,
            bind_port: 8080,
            target_host: Some("localhost".into()),
            target_port: Some(80),
            label: None,
        }
    }

    #[test]
    fn local_and_dynamic_forwards_bind_loopback_only() {
        for kind in [ForwardKind::Local, ForwardKind::Dynamic] {
            let base = if kind == ForwardKind::Dynamic {
                ForwardInput {
                    target_host: None,
                    target_port: None,
                    ..input(kind)
                }
            } else {
                input(kind)
            };
            let spec = normalize(None, base.clone()).unwrap();
            assert_eq!(spec.bind_address.as_deref(), Some("127.0.0.1"));
            for ok in ["127.0.0.1", "::1", "localhost", " localhost "] {
                let spec = normalize(
                    None,
                    ForwardInput {
                        bind_address: Some(ok.into()),
                        ..base.clone()
                    },
                )
                .unwrap();
                assert_eq!(spec.bind_address.as_deref(), Some(ok.trim()));
            }
            for bad in [
                "0.0.0.0",
                "*",
                "::",
                "192.168.1.2",
                "127.0.0.2",
                "LOCALHOST",
            ] {
                assert!(
                    normalize(
                        None,
                        ForwardInput {
                            bind_address: Some(bad.into()),
                            ..base.clone()
                        }
                    )
                    .is_err(),
                    "{bad}"
                );
            }
        }
        // Remote binds name an address on the server.
        let spec = normalize(
            None,
            ForwardInput {
                bind_address: Some("0.0.0.0".into()),
                ..input(ForwardKind::Remote)
            },
        )
        .unwrap();
        assert_eq!(spec.bind_address.as_deref(), Some("0.0.0.0"));
        let spec = normalize(None, input(ForwardKind::Remote)).unwrap();
        assert_eq!(spec.bind_address, None);
        assert!(normalize(
            None,
            ForwardInput {
                bind_address: Some("a b".into()),
                ..input(ForwardKind::Remote)
            }
        )
        .is_err());
    }

    #[test]
    fn target_and_port_rules() {
        for bad_host in ["", "-oProxyCommand=x", "a b", "a\nb", "a/b", "[::1]"] {
            assert!(
                normalize(
                    None,
                    ForwardInput {
                        target_host: Some(bad_host.into()),
                        ..input(ForwardKind::Local)
                    }
                )
                .is_err(),
                "{bad_host:?}"
            );
        }
        let long = "a".repeat(254);
        assert!(normalize(
            None,
            ForwardInput {
                target_host: Some(long),
                ..input(ForwardKind::Local)
            }
        )
        .is_err());
        assert!(normalize(
            None,
            ForwardInput {
                target_host: None,
                ..input(ForwardKind::Remote)
            }
        )
        .is_err());
        assert!(normalize(
            None,
            ForwardInput {
                target_port: None,
                ..input(ForwardKind::Local)
            }
        )
        .is_err());
        // Dynamic forwards take no target.
        assert!(normalize(None, input(ForwardKind::Dynamic)).is_err());
        for bad_port in [0, -1, 65536] {
            assert!(normalize(
                None,
                ForwardInput {
                    bind_port: bad_port,
                    ..input(ForwardKind::Local)
                }
            )
            .is_err());
            assert!(normalize(
                None,
                ForwardInput {
                    target_port: Some(bad_port),
                    ..input(ForwardKind::Local)
                }
            )
            .is_err());
        }
        let label = "x".repeat(81);
        assert!(normalize(
            None,
            ForwardInput {
                label: Some(label),
                ..input(ForwardKind::Local)
            }
        )
        .is_err());
        assert!(normalize(
            None,
            ForwardInput {
                label: Some("a\tb".into()),
                ..input(ForwardKind::Local)
            }
        )
        .is_err());
        let spec = normalize(
            None,
            ForwardInput {
                label: Some("  ".into()),
                target_host: Some("fe80::1%eth0".into()),
                ..input(ForwardKind::Local)
            },
        )
        .unwrap();
        assert_eq!(spec.label, None);
        assert_eq!(
            forward_args(&spec),
            [
                "-L".to_owned(),
                "127.0.0.1:8080:[fe80::1%eth0]:80".to_owned()
            ]
        );
    }

    #[test]
    fn forward_arguments() {
        let local = normalize(
            None,
            ForwardInput {
                bind_address: Some("::1".into()),
                ..input(ForwardKind::Local)
            },
        )
        .unwrap();
        assert_eq!(forward_args(&local)[1], "[::1]:8080:localhost:80");
        let remote = normalize(None, input(ForwardKind::Remote)).unwrap();
        assert_eq!(
            forward_args(&remote),
            ["-R".to_owned(), "8080:localhost:80".to_owned()]
        );
        let dynamic = normalize(
            None,
            ForwardInput {
                target_host: None,
                target_port: None,
                ..input(ForwardKind::Dynamic)
            },
        )
        .unwrap();
        assert_eq!(
            forward_args(&dynamic),
            ["-D".to_owned(), "127.0.0.1:8080".to_owned()]
        );
    }

    #[test]
    fn running_limit_and_port_clashes() {
        let runtime = ForwardRuntime::default();
        let spec = |id: &str, port: i64, bind: &str| ForwardSpec {
            id: id.into(),
            ..normalize(
                None,
                ForwardInput {
                    bind_port: port,
                    bind_address: Some(bind.into()),
                    ..input(ForwardKind::Local)
                },
            )
            .unwrap()
        };
        assert!(runtime
            .begin(&spec("a", 9000, "127.0.0.1"))
            .unwrap()
            .is_some());
        // Already starting: nothing new to do.
        assert!(runtime
            .begin(&spec("a", 9000, "127.0.0.1"))
            .unwrap()
            .is_none());
        assert!(matches!(
            runtime.begin(&spec("b", 9000, "localhost")),
            Err(AppError::Conflict(_))
        ));
        assert!(runtime.begin(&spec("c", 9000, "::1")).unwrap().is_some());
        runtime.set_reserved(vec![ReservedListener {
            address: None,
            port: 4000,
        }]);
        assert!(matches!(
            runtime.begin(&spec("d", 4000, "127.0.0.1")),
            Err(AppError::Conflict(_))
        ));
        for n in 0..(MAX_RUNNING_FORWARDS - 2) {
            assert!(runtime
                .begin(&spec(&format!("n{n}"), 10_000 + n as i64, "127.0.0.1"))
                .unwrap()
                .is_some());
        }
        assert!(matches!(
            runtime.begin(&spec("over", 20_000, "127.0.0.1")),
            Err(AppError::Conflict(_))
        ));
    }
}
