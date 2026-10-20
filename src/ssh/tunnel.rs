//! Long-running ssh children for port forwards and the IP pool: each runs in
//! a process group of its own (so a `ProxyCommand` dies with it), is killed
//! when its handle drops, and keeps a short tail of its stderr for errors.

use std::{
    collections::VecDeque,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

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

/// A spawned ssh child in its own process group.
pub(crate) struct GroupChild {
    child: Child,
    pgid: Option<u32>,
    reaped: bool,
}

impl GroupChild {
    /// Spawns `command` with null stdin/stdout unless the caller configured
    /// them, piped stderr, `kill_on_drop` and a process group of its own.
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let child = command.spawn()?;
        let pgid = child.id();
        Ok(Self {
            child,
            pgid,
            reaped: false,
        })
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.reaped = true;
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
            self.reaped = true;
        }
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
