//! Long-running ssh children for port forwards and the IP pool: each runs in
//! a process group of its own (so a `ProxyCommand` dies with it), is killed
//! when its handle drops, and keeps a short tail of its stderr for errors.
//! Their pids are recorded in an owner-only file, so the next daemon can end
//! the ones a killed daemon left behind.

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::mpsc,
};

/// Lines of stderr kept for error messages.
const TAIL_LINES: usize = 12;
/// Longest stderr line kept; longer lines are cut.
const MAX_LINE_BYTES: usize = 512;
/// Longest error summary reported to clients.
pub(crate) const MAX_ERROR_LEN: usize = 400;

/// Arguments that only TodeX's long-running children carry: forwards
/// (`ExitOnForwardFailure=yes`) and pool masters (`ControlMaster=yes`).
#[cfg(unix)]
pub(crate) const TRACKED_MARKERS: &[&str] = &["ExitOnForwardFailure=yes", "ControlMaster=yes"];

/// One recorded child.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ChildRecord {
    pid: u32,
    /// Process start time where the system reports it (Linux), so a reused
    /// pid is never mistaken for the child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_time: Option<u64>,
}

/// Pids of running long-lived children, mirrored to an owner-only file.
pub(crate) struct ChildRegistry {
    path: Option<PathBuf>,
    children: Mutex<BTreeMap<u32, ChildRecord>>,
}

impl ChildRegistry {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            children: Mutex::new(BTreeMap::new()),
        }
    }

    fn add(&self, pid: u32) {
        let mut children = self.children.lock().unwrap();
        children.insert(
            pid,
            ChildRecord {
                pid,
                start_time: process_start_time(pid),
            },
        );
        self.write(&children);
    }

    fn remove(&self, pid: u32) {
        let mut children = self.children.lock().unwrap();
        if children.remove(&pid).is_some() {
            self.write(&children);
        }
    }

    fn write(&self, children: &BTreeMap<u32, ChildRecord>) {
        let Some(path) = &self.path else { return };
        let records: Vec<&ChildRecord> = children.values().collect();
        let result = serde_json::to_vec(&records)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::secure_fs::write_owner_only_atomic(path, &bytes));
        if let Err(error) = result {
            tracing::warn!(%error, "cannot record ssh child processes");
        }
    }

    /// Ends children recorded by an earlier daemon that are still running
    /// with `wrapper` and one of [`TRACKED_MARKERS`] in their arguments, then
    /// empties the file. Blocking.
    pub(crate) fn reap_stale(path: &Path, wrapper: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let records: Vec<ChildRecord> = serde_json::from_slice(&bytes).unwrap_or_default();
        for record in records {
            if is_stale_child(record, wrapper) {
                tracing::info!(
                    pid = record.pid,
                    "ending ssh child left by an earlier daemon"
                );
                kill_group_blocking(record.pid);
            }
        }
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22, counted after the parenthesized command name.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// The arguments of `pid` joined by spaces, if it still runs.
#[cfg(unix)]
fn process_command_line(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(
            String::from_utf8_lossy(&raw)
                .split('\0')
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-ww", "-o", "command=", "-p"])
            .arg(pid.to_string())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
}

#[cfg(unix)]
fn is_stale_child(record: ChildRecord, wrapper: &Path) -> bool {
    if record.pid <= 1 {
        return false;
    }
    if let (Some(recorded), Some(now)) = (record.start_time, process_start_time(record.pid)) {
        if recorded != now {
            return false;
        }
    }
    let Some(command) = process_command_line(record.pid) else {
        return false;
    };
    let program = command.split(' ').next().unwrap_or_default();
    Path::new(program)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains("ssh"))
        && command.contains(&format!("-F {}", wrapper.display()))
        && TRACKED_MARKERS
            .iter()
            .any(|marker| command.contains(marker))
}

#[cfg(not(unix))]
fn is_stale_child(_record: ChildRecord, _wrapper: &Path) -> bool {
    false
}

/// SIGTERM to the group `pid` leads (or `pid` alone), SIGKILL after a second.
#[cfg(unix)]
fn kill_group_blocking(pid: u32) {
    let pid = pid as i32;
    // SAFETY: plain syscalls on a verified process.
    unsafe {
        let target = if libc::getpgid(pid) == pid { -pid } else { pid };
        libc::kill(target, libc::SIGTERM);
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(100));
            if libc::kill(pid, 0) != 0 {
                return;
            }
        }
        libc::kill(target, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group_blocking(_pid: u32) {}

/// A spawned ssh child in its own process group.
pub(crate) struct GroupChild {
    child: Child,
    pgid: Option<u32>,
    reaped: bool,
    registry: Option<Arc<ChildRegistry>>,
}

impl GroupChild {
    /// Spawns `command` with null stdin/stdout unless the caller configured
    /// them, piped stderr, `kill_on_drop` and a process group of its own.
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
        Self::spawn_tracked(command, None)
    }

    /// [`Self::spawn`], recording the pid in `registry` while it runs.
    pub(crate) fn spawn_tracked(
        command: &mut Command,
        registry: Option<Arc<ChildRegistry>>,
    ) -> std::io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let child = command.spawn()?;
        let pgid = child.id();
        if let (Some(registry), Some(pid)) = (&registry, pgid) {
            registry.add(pid);
        }
        Ok(Self {
            child,
            pgid,
            reaped: false,
            registry,
        })
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn mark_reaped(&mut self) {
        self.reaped = true;
        if let (Some(registry), Some(pid)) = (&self.registry, self.pgid) {
            registry.remove(pid);
        }
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.mark_reaped();
        }
        status
    }

    /// SIGTERM to the group, SIGKILL after a grace period.
    pub(crate) async fn terminate(&mut self) {
        if self.reaped {
            return;
        }
        self.signal_group(Signal::Term);
        if tokio::time::timeout(Duration::from_secs(2), self.wait())
            .await
            .is_err()
        {
            self.signal_group(Signal::Kill);
            let _ = self.child.kill().await;
            self.mark_reaped();
        }
    }

    /// Kills what is left of the group after the leader exited (helpers
    /// that still hold its pipes). Only right after reaping, while the
    /// group id cannot have been reused.
    pub(crate) fn kill_leftovers(&self) {
        self.signal_group(Signal::Kill);
    }

    fn signal_group(&self, signal: Signal) {
        #[cfg(unix)]
        if let Some(pid) = self.pgid {
            let signal = match signal {
                Signal::Term => libc::SIGTERM,
                Signal::Kill => libc::SIGKILL,
            };
            // SAFETY: plain syscall on the group this child leads.
            unsafe {
                libc::kill(-(pid as i32), signal);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (signal, self.pgid);
        }
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.signal_group(Signal::Kill);
            self.mark_reaped();
        }
    }
}

/// The last lines a child wrote to stderr.
#[derive(Clone, Default)]
pub(crate) struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// A short summary for clients: the latest lines that are not progress
    /// output (or the latest lines when there are no others).
    pub(crate) fn summary(&self) -> String {
        let lines = self.lines.lock().unwrap();
        let plain: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            // Debug output and the sign-in notice are progress, not causes.
            .filter(|line| !line.starts_with("debug") && !line.starts_with("Authenticated to"))
            .collect();
        let chosen: Vec<&str> = if plain.is_empty() {
            lines
                .iter()
                .rev()
                .take(2)
                .rev()
                .map(String::as_str)
                .collect()
        } else {
            plain.iter().rev().take(4).rev().copied().collect()
        };
        cut(&chosen.join("; "), MAX_ERROR_LEN)
    }

    /// All kept lines, for failure classification.
    pub(crate) fn text(&self) -> String {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Reads `stderr` to its end, keeping the tail and sending every line to
/// `lines` (while the receiver lives).
pub(crate) fn drain_stderr(
    stderr: impl AsyncRead + Unpin + Send + 'static,
    tail: StderrTail,
    lines: Option<mpsc::UnboundedSender<String>>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&buffer);
            let line = cut(text.trim_end_matches(['\r', '\n']).trim(), MAX_LINE_BYTES);
            if line.is_empty() {
                continue;
            }
            if let Some(sender) = &lines {
                let _ = sender.send(line.clone());
            }
            tail.push(line);
        }
    });
}

pub(crate) fn piped_stderr(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
}

/// `text` cut to at most `limit` bytes at a character boundary.
pub(crate) fn cut(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
