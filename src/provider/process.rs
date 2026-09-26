use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{timeout, timeout_at, Duration, Instant};

use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

const MAX_PROTOCOL_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of an unparseable provider line kept for diagnostics.
const UNPARSED_LINE_PREVIEW_BYTES: usize = 512;
const MAX_STDERR_BYTES: usize = 64 * 1024;
pub(super) fn control_timeout() -> Result<Duration, AppError> {
    configured_timeout("TODEX_AGENTD_PROVIDER_CONTROL_TIMEOUT_SECONDS", 30, 3600)
}
pub(super) fn cancel_timeout() -> Result<Duration, AppError> {
    configured_timeout("TODEX_AGENTD_PROVIDER_CANCEL_TIMEOUT_SECONDS", 10, 3600)
}
pub(super) fn compact_timeout() -> Result<Duration, AppError> {
    configured_timeout("TODEX_AGENTD_PROVIDER_COMPACT_TIMEOUT_SECONDS", 300, 86400)
}
fn write_timeout() -> Result<Duration, AppError> {
    configured_timeout("TODEX_AGENTD_PROVIDER_WRITE_TIMEOUT_SECONDS", 10, 3600)
}
fn configured_timeout(name: &str, default: u64, maximum: u64) -> Result<Duration, AppError> {
    match std::env::var(name) {
        Ok(value) => parse_timeout_value(name, &value, maximum),
        Err(std::env::VarError::NotPresent) => Ok(Duration::from_secs(default)),
        Err(_) => Err(AppError::InvalidRequest(format!(
            "{name} must contain a positive integer number of seconds"
        ))),
    }
}
fn parse_timeout_value(name: &str, value: &str, maximum: u64) -> Result<Duration, AppError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|seconds| (1..=maximum).contains(seconds))
        .map(Duration::from_secs)
        .ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "{name} must be an integer between 1 and {maximum} seconds"
            ))
        })
}
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(3);
// How much stderr travels with a failure message. The buffer holds up to
// MAX_STDERR_BYTES, which is more than a user can read and more than an error
// payload should carry, but the first line alone is often just a stack frame.
const STDERR_EXCERPT_CHARS: usize = 2000;

pub struct BoundedCommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: BTreeMap::new(),
        }
    }
}

/// One line of provider stdout.
///
/// Providers print banners, progress text and occasionally huge frames on the
/// protocol stream. A line that cannot be decoded is reported instead of
/// failing the exchange, so one stray print does not end a turn.
#[derive(Debug)]
pub enum ProviderRead {
    /// A JSON frame; `Value::Null` for a blank line.
    Frame(Value),
    /// A line that is not JSON. `preview` is redacted and at most
    /// UNPARSED_LINE_PREVIEW_BYTES long.
    Invalid { preview: String },
    /// A line above MAX_PROTOCOL_LINE_BYTES, discarded without buffering it.
    Oversized { bytes: usize },
}

impl ProviderRead {
    /// The frame, or `Value::Null` after logging a line nobody reports.
    pub fn into_logged_frame(self) -> Value {
        match self {
            Self::Frame(value) => value,
            Self::Invalid { preview } => {
                tracing::warn!(preview = %preview, "skipping provider stdout line that is not JSON");
                Value::Null
            }
            Self::Oversized { bytes } => {
                tracing::warn!(bytes, "skipping oversized provider stdout line");
                Value::Null
            }
        }
    }
}

enum BoundedLine {
    Line(Vec<u8>),
    Oversized(usize),
}

pub struct JsonLineProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stdout_pending: Vec<u8>,
    /// Bytes skipped so far of an oversized line whose end has not arrived.
    stdout_discarding: Option<usize>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    pid: Option<u32>,
}

impl JsonLineProcess {
    pub async fn spawn(spec: &CommandSpec) -> Result<Self, AppError> {
        if !spec.cwd.is_absolute() {
            return Err(AppError::InvalidRequest(
                "provider working directory must be absolute".to_owned(),
            ));
        }
        let mut command = secure_command(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }

        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::ProviderUnavailable(format!(
                    "provider executable '{}' was not found",
                    spec.program
                ))
            } else {
                AppError::Io(error)
            }
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            AppError::ProviderUnavailable("provider process did not expose stdin".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppError::ProviderUnavailable("provider process did not expose stdout".to_owned())
        })?;
        let stderr_reader = child.stderr.take().ok_or_else(|| {
            AppError::ProviderUnavailable("provider process did not expose stderr".to_owned())
        })?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr_reader, stderr.clone()));

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stdout_pending: Vec::new(),
            stdout_discarding: None,
            stderr,
            stderr_task,
            pid,
        })
    }

    pub async fn spawn_trusted(
        spec: &CommandSpec,
        permit: WorkspaceTrustPermit,
    ) -> Result<Self, AppError> {
        let process = Self::spawn(spec).await?;
        drop(permit);
        Ok(process)
    }

    pub async fn send(&mut self, value: &Value) -> Result<(), AppError> {
        self.send_with_timeout(value, write_timeout()?).await
    }

    async fn send_with_timeout(
        &mut self,
        value: &Value,
        deadline: Duration,
    ) -> Result<(), AppError> {
        let mut bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_PROTOCOL_LINE_BYTES {
            return Err(AppError::InvalidRequest(
                "provider protocol frame is too large".to_owned(),
            ));
        }
        bytes.push(b'\n');
        timeout(deadline, async {
            self.stdin.write_all(&bytes).await?;
            self.stdin.flush().await
        })
        .await
        .map_err(|_| {
            AppError::ProviderUnavailable("provider protocol write timed out".to_owned())
        })??;
        Ok(())
    }

    /// A single control exchange keeps the same deadline across unrelated notifications.
    /// Unparseable lines are logged and surface as `Value::Null`.
    pub async fn read_control_until(
        &mut self,
        deadline: Instant,
    ) -> Result<Option<Value>, AppError> {
        Ok(self
            .read_frame_until(deadline)
            .await?
            .map(ProviderRead::into_logged_frame))
    }

    /// [`Self::read_control_until`] for loops that report unparseable lines.
    pub async fn read_frame_until(
        &mut self,
        deadline: Instant,
    ) -> Result<Option<ProviderRead>, AppError> {
        if Instant::now() >= deadline {
            return Err(AppError::ProviderUnavailable(
                "provider control response timed out".to_owned(),
            ));
        }
        timeout_at(deadline, self.read_frame()).await.map_err(|_| {
            AppError::ProviderUnavailable("provider control response timed out".to_owned())
        })?
    }

    /// The next frame; unparseable lines are logged and surface as
    /// `Value::Null`, which every reader already skips like a blank line.
    pub async fn read(&mut self) -> Result<Option<Value>, AppError> {
        Ok(self
            .read_frame()
            .await?
            .map(ProviderRead::into_logged_frame))
    }

    /// The next stdout line, classified. `None` is end of stream. Cancel-safe:
    /// partial lines and oversized-line progress stay with the process.
    pub async fn read_frame(&mut self) -> Result<Option<ProviderRead>, AppError> {
        let line = read_bounded_line(
            &mut self.stdout,
            &mut self.stdout_pending,
            &mut self.stdout_discarding,
            MAX_PROTOCOL_LINE_BYTES,
        )
        .await?;
        Ok(line.map(classify_line))
    }

    pub async fn terminate(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        #[cfg(unix)]
        signal_process_group(pid, libc::SIGTERM);
        #[cfg(not(unix))]
        {
            let _ = pid;
            let _ = self.child.start_kill();
        }

        if timeout(GRACEFUL_STOP_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            #[cfg(unix)]
            signal_process_group(pid, libc::SIGKILL);
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        self.stderr_task.abort();
    }
}

impl Drop for JsonLineProcess {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            #[cfg(unix)]
            signal_process_group(pid, libc::SIGKILL);
            #[cfg(not(unix))]
            let _ = pid;
            let _ = self.child.start_kill();
        }
        self.stderr_task.abort();
    }
}

pub async fn run_bounded_command(
    spec: &CommandSpec,
    max_stdout_bytes: usize,
    timeout_duration: Duration,
) -> Result<BoundedCommandOutput, AppError> {
    if !spec.cwd.is_absolute() {
        return Err(AppError::InvalidRequest(
            "provider working directory must be absolute".to_owned(),
        ));
    }
    let mut command = secure_command(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::ProviderUnavailable(format!(
                "provider executable '{}' was not found",
                spec.program
            ))
        } else {
            AppError::Io(error)
        }
    })?;
    let pid = child.id();
    #[cfg(not(unix))]
    let _ = pid;
    let stdout = child.stdout.take().ok_or_else(|| {
        AppError::ProviderUnavailable("provider process did not expose stdout".to_owned())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        AppError::ProviderUnavailable("provider process did not expose stderr".to_owned())
    })?;
    let mut stdout_task = tokio::spawn(drain_bounded(stdout, max_stdout_bytes));
    let mut stderr_task = tokio::spawn(drain_bounded(stderr, MAX_STDERR_BYTES));

    let started = Instant::now();
    let status = match timeout(timeout_duration, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                signal_process_group(pid, libc::SIGKILL);
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(AppError::ProviderUnavailable(
                "provider diagnostic command timed out".to_owned(),
            ));
        }
    };
    let remaining = timeout_duration.saturating_sub(started.elapsed());
    let drains = timeout(remaining, async {
        let stdout = (&mut stdout_task)
            .await
            .map_err(|error| AppError::Anyhow(error.into()))?;
        let stderr = (&mut stderr_task)
            .await
            .map_err(|error| AppError::Anyhow(error.into()))?;
        Ok::<_, AppError>((stdout?, stderr?))
    })
    .await;
    let ((stdout, stdout_exceeded), (stderr, _)) = match drains {
        Ok(result) => result?,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                signal_process_group(pid, libc::SIGKILL);
            }
            stdout_task.abort();
            stderr_task.abort();
            return Err(AppError::ProviderUnavailable(
                "provider diagnostic command timed out".to_owned(),
            ));
        }
    };
    if stdout_exceeded {
        return Err(AppError::InvalidRequest(
            "provider diagnostic output is too large".to_owned(),
        ));
    }
    Ok(BoundedCommandOutput {
        stdout,
        stderr,
        success: status.success(),
    })
}

pub async fn provider_exit_error(process: &JsonLineProcess, message: &str) -> AppError {
    let excerpt = {
        let buffer = process.stderr.lock().await;
        stderr_excerpt(&buffer)
    };
    match excerpt {
        Some(excerpt) => AppError::ProviderUnavailable(format!("{message}: {excerpt}")),
        None => AppError::ProviderUnavailable(message.to_owned()),
    }
}

/// The tail of a provider's stderr, for attaching to a failure message.
///
/// The reason a provider died — a missing API key, an unknown model, an expired
/// login — is almost always in what it printed, so reporting only a byte count
/// leaves the user with nothing to act on. `drain_stderr` already keeps the last
/// MAX_STDERR_BYTES, so the tail is the part worth showing.
fn stderr_excerpt(buffer: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(buffer);
    // Providers pad their diagnostics with blank lines and progress spinners.
    let collapsed = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if collapsed.is_empty() {
        return None;
    }
    // Count characters, not bytes: truncating a UTF-8 sequence mid-way would
    // panic on a str slice, and provider output is frequently non-ASCII.
    let collapsed = redact_sensitive_text(&collapsed);
    if collapsed.chars().count() <= STDERR_EXCERPT_CHARS {
        return Some(collapsed);
    }
    let kept = collapsed
        .chars()
        .skip(collapsed.chars().count() - STDERR_EXCERPT_CHARS)
        .collect::<String>();
    Some(format!("...{kept}"))
}

pub(super) fn redact_sensitive_text(input: &str) -> String {
    let mut output = input.to_owned();
    for prefix in ["xai-", "Bearer "] {
        let mut search_from = 0;
        while let Some(relative) = output[search_from..].find(prefix) {
            let start = search_from + relative;
            let value_start = start + prefix.len();
            let value_end = output[value_start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | '}' | ']'))
                .map_or(output.len(), |offset| value_start + offset);
            output.replace_range(start..value_end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }
    for key in ["api_key", "access_token", "refresh_token", "authorization"] {
        redact_json_string_value(&mut output, key);
    }
    output
}

fn redact_json_string_value(output: &mut String, key: &str) {
    let marker = format!("\"{key}\"");
    let mut search_from = 0;
    while let Some(relative) = output[search_from..].find(&marker) {
        let marker_start = search_from + relative;
        let Some(colon_offset) = output[marker_start + marker.len()..].find(':') else {
            break;
        };
        let after_colon = marker_start + marker.len() + colon_offset + 1;
        let Some(quote_offset) = output[after_colon..].find('"') else {
            break;
        };
        let value_start = after_colon + quote_offset + 1;
        let Some(end_offset) = output[value_start..].find('"') else {
            break;
        };
        let value_end = value_start + end_offset;
        output.replace_range(value_start..value_end, "[REDACTED]");
        search_from = value_start + "[REDACTED]".len();
    }
}

pub fn executable_available(program: &str) -> bool {
    resolve_executable(program).is_some()
}

pub(crate) fn same_executable(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (Some(a), Some(b)) = (resolve_executable(a), resolve_executable(b)) else {
        return false;
    };
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    canonical(a) == canonical(b)
}

fn resolve_executable(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        if executable_file(path) {
            return Some(path.to_owned());
        }
        #[cfg(windows)]
        if path.extension().is_none() {
            let directory = path.parent().unwrap_or_else(|| Path::new("."));
            let file_name = path.file_name()?.to_str()?;
            return executable_with_platform_extension(directory, file_name);
        }
        return None;
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .find_map(|directory| executable_in_directory(&directory, program))
    })
}

fn executable_in_directory(directory: &Path, program: &str) -> Option<PathBuf> {
    let direct = directory.join(program);
    if executable_file(&direct) {
        return Some(direct);
    }
    #[cfg(windows)]
    {
        if Path::new(program).extension().is_none() {
            return executable_with_platform_extension(directory, program);
        }
    }
    None
}

#[cfg(windows)]
fn executable_with_platform_extension(directory: &Path, program: &str) -> Option<PathBuf> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| directory.join(format!("{program}{extension}")))
        .find(|candidate| executable_file(candidate))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn secure_command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let resolved = program
        .to_str()
        .and_then(resolve_executable)
        .unwrap_or_else(|| PathBuf::from(program));
    let mut command = Command::new(resolved);
    command.env_clear();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LANG",
        "LC_ALL",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "CODEX_HOME",
        "PI_CODING_AGENT_DIR",
        "CLAUDE_CONFIG_DIR",
        "SSH_AUTH_SOCK",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "REQUESTS_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SYSTEMROOT",
        "COMSPEC",
        "PATHEXT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TODEX_AGENTD_") {
            command.env_remove(key);
        }
    }
    command
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, destination: Arc<Mutex<Vec<u8>>>) {
    let mut reader = stderr;
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let mut destination = destination.lock().await;
                destination.extend_from_slice(&chunk[..count]);
                if destination.len() > MAX_STDERR_BYTES {
                    let excess = destination.len() - MAX_STDERR_BYTES;
                    destination.drain(..excess);
                }
            }
        }
    }
}

async fn drain_bounded<R>(mut reader: R, max_bytes: usize) -> Result<(Vec<u8>, bool), AppError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut exceeded = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        output.extend_from_slice(&chunk[..count.min(remaining)]);
        exceeded |= count > remaining;
    }
    Ok((output, exceeded))
}

fn classify_line(line: BoundedLine) -> ProviderRead {
    let mut bytes = match line {
        BoundedLine::Line(bytes) => bytes,
        BoundedLine::Oversized(bytes) => return ProviderRead::Oversized { bytes },
    };
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return ProviderRead::Frame(Value::Null);
    }
    match serde_json::from_slice(&bytes) {
        Ok(value) => ProviderRead::Frame(value),
        Err(_) => ProviderRead::Invalid {
            preview: unparsed_line_preview(&bytes),
        },
    }
}

/// A redacted, char-boundary-safe prefix of an unparseable line.
fn unparsed_line_preview(bytes: &[u8]) -> String {
    let head = &bytes[..bytes.len().min(UNPARSED_LINE_PREVIEW_BYTES)];
    let mut preview = redact_sensitive_text(&String::from_utf8_lossy(head));
    if preview.len() > UNPARSED_LINE_PREVIEW_BYTES {
        let mut end = UNPARSED_LINE_PREVIEW_BYTES;
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
    }
    preview
}

/// Reads one newline-terminated line of at most `max_bytes` (plus the newline).
/// A longer line is consumed to its end without being buffered and reported as
/// `Oversized` with its length excluding the trailing newline.
async fn read_bounded_line<R>(
    reader: &mut R,
    output: &mut Vec<u8>,
    discarding: &mut Option<usize>,
    max_bytes: usize,
) -> Result<Option<BoundedLine>, AppError>
where
    R: AsyncBufRead + Unpin,
{
    // Keep partial frames with the process: cancelling a read to send interrupt must not lose bytes.
    loop {
        let (consumed, found_newline) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if let Some(bytes) = discarding.take() {
                    return Ok(Some(BoundedLine::Oversized(bytes)));
                }
                return if output.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(BoundedLine::Line(std::mem::take(output))))
                };
            }
            let consumed = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            let found_newline = available.get(consumed.saturating_sub(1)) == Some(&b'\n');
            if let Some(skipped) = discarding.as_mut() {
                *skipped = skipped.saturating_add(consumed);
            } else if output.len().saturating_add(consumed) > max_bytes.saturating_add(1) {
                *discarding = Some(output.len().saturating_add(consumed));
                // Release the partial frame's allocation, not just its length.
                *output = Vec::new();
            } else {
                output.extend_from_slice(&available[..consumed]);
            }
            (consumed, found_newline)
        };
        reader.consume(consumed);
        if found_newline {
            if let Some(skipped) = discarding.take() {
                return Ok(Some(BoundedLine::Oversized(skipped.saturating_sub(1))));
            }
            return Ok(Some(BoundedLine::Line(std::mem::take(output))));
        }
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) {
    // The child is spawned into a dedicated process group, so a negative PID targets only it.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;

    use super::*;

    async fn read_all(input: &[u8], capacity: usize, max_bytes: usize) -> Vec<ProviderRead> {
        let mut reader = BufReader::with_capacity(capacity, input);
        let (mut pending, mut discarding) = (Vec::new(), None);
        let mut reads = Vec::new();
        while let Some(line) =
            read_bounded_line(&mut reader, &mut pending, &mut discarding, max_bytes)
                .await
                .unwrap()
        {
            reads.push(classify_line(line));
        }
        reads
    }

    #[tokio::test]
    async fn oversized_provider_line_is_skipped_and_the_next_frame_still_parses() {
        // An 8-byte buffer makes the 25-byte line span several fills, so the
        // discard state has to survive between them.
        let reads = read_all(b"123456789abcdefghijklmnop\n{\"ok\":1}\n", 8, 8).await;
        assert!(matches!(reads[0], ProviderRead::Oversized { bytes: 25 }));
        assert!(
            matches!(&reads[1], ProviderRead::Frame(value) if value == &serde_json::json!({"ok": 1}))
        );
        assert_eq!(reads.len(), 2);
    }

    #[tokio::test]
    async fn oversized_provider_line_at_end_of_stream_is_reported() {
        let reads = read_all(b"123456789", 4, 4).await;
        assert!(matches!(reads[..], [ProviderRead::Oversized { bytes: 9 }]));
    }

    #[tokio::test]
    async fn non_json_provider_line_is_reported_and_reading_continues() {
        let reads = read_all(b"Welcome to provider v1\r\n  \n{\"ok\":true}\n", 64, 128).await;
        assert!(
            matches!(&reads[0], ProviderRead::Invalid { preview } if preview == "Welcome to provider v1")
        );
        assert!(matches!(&reads[1], ProviderRead::Frame(Value::Null)));
        assert!(matches!(&reads[2], ProviderRead::Frame(value) if value["ok"] == true));
    }

    #[test]
    fn unparsed_line_preview_is_bounded_redacted_and_char_safe() {
        let line = format!("Bearer secret-token {}", "错".repeat(400));
        let preview = unparsed_line_preview(line.as_bytes());
        assert!(preview.len() <= UNPARSED_LINE_PREVIEW_BYTES);
        assert!(preview.starts_with("[REDACTED] "));
        assert!(!preview.contains("secret-token"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_read_skips_unparseable_lines_for_control_exchanges() {
        let mut spec = CommandSpec::new("/bin/sh", std::env::temp_dir());
        spec.args = vec![
            "-c".to_owned(),
            "echo banner; head -c 4194400 /dev/zero | tr '\\0' x; echo; echo '{\"id\":1}'"
                .to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await.unwrap();
        assert!(matches!(
            process.read_frame().await.unwrap(),
            Some(ProviderRead::Invalid { .. })
        ));
        assert_eq!(process.read().await.unwrap(), Some(Value::Null));
        assert!(process.stdout_pending.capacity() <= MAX_PROTOCOL_LINE_BYTES + 1);
        assert_eq!(
            process.read().await.unwrap(),
            Some(serde_json::json!({"id": 1}))
        );
        assert_eq!(process.read().await.unwrap(), None);
        process.terminate().await;
    }

    #[test]
    fn stderr_excerpt_reports_content_not_byte_counts() {
        assert_eq!(stderr_excerpt(b""), None);
        assert_eq!(stderr_excerpt(b"   \n \n"), None);
        assert_eq!(
            stderr_excerpt(b"Error: invalid API key\n\n  run `codex login` to authenticate\n"),
            Some("Error: invalid API key | run `codex login` to authenticate".to_owned())
        );
    }

    #[test]
    fn stderr_excerpt_keeps_the_tail_and_survives_multibyte_boundaries() {
        // Repeating a multi-byte character exercises the char-based truncation:
        // a byte-based slice here would panic on a UTF-8 boundary.
        let noise = "错".repeat(STDERR_EXCERPT_CHARS * 2);
        let input = format!("{noise}\nfinal cause");
        let excerpt = stderr_excerpt(input.as_bytes()).unwrap();
        assert!(excerpt.starts_with("..."));
        assert!(excerpt.ends_with("final cause"));
        assert!(excerpt.chars().count() <= STDERR_EXCERPT_CHARS + 3);
    }

    #[test]
    fn sensitive_provider_diagnostics_are_redacted() {
        let redacted = redact_sensitive_text(
            r#"failed api xai-secret and Bearer token-value {"api_key":"raw-secret"}"#,
        );
        assert!(!redacted.contains("xai-secret"));
        assert!(!redacted.contains("token-value"));
        assert!(!redacted.contains("raw-secret"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_rejects_non_executable_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "todex-provider-executable-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("grok");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!executable_available(path.to_str().unwrap()));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_executable(path.to_str().unwrap()),
            Some(path.clone())
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn silent_provider_control_times_out_and_can_be_reaped() {
        let mut spec = CommandSpec::new("/bin/sh", std::env::temp_dir());
        spec.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        let mut process = JsonLineProcess::spawn(&spec).await.unwrap();
        let result = process
            .read_control_until(Instant::now() + Duration::from_millis(20))
            .await;
        assert!(
            matches!(result, Err(AppError::ProviderUnavailable(message)) if message.contains("timed out"))
        );
        process.terminate().await;
        assert!(process.child.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrelated_frames_do_not_reset_control_deadline() {
        let mut spec = CommandSpec::new("/bin/sh", std::env::temp_dir());
        spec.args = vec![
            "-c".to_owned(),
            "while :; do printf '{}\n'; done".to_owned(),
        ];
        let mut process = JsonLineProcess::spawn(&spec).await.unwrap();
        let deadline = Instant::now() + Duration::from_millis(20);
        let mut received = 0;
        loop {
            // timeout_at permits one immediate poll even when expired; enforce the exchange deadline too.
            if Instant::now() >= deadline {
                break;
            }
            match process.read_control_until(deadline).await {
                Ok(Some(_)) => received += 1,
                Err(AppError::ProviderUnavailable(_)) => break,
                other => panic!("unexpected provider result: {other:?}"),
            }
        }
        assert!(received > 0);
        process.terminate().await;
    }
    #[test]
    fn timeout_configuration_is_bounded_and_does_not_echo_invalid_values() {
        for value in ["0", "-1", "3601", "secret-value"] {
            let error =
                parse_timeout_value("TODEX_AGENTD_PROVIDER_WRITE_TIMEOUT_SECONDS", value, 3600)
                    .unwrap_err();
            assert!(!error.to_string().contains("secret-value"));
        }
        assert_eq!(
            parse_timeout_value("timeout", "15", 3600).unwrap(),
            Duration::from_secs(15)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn provider_that_does_not_consume_stdin_has_bounded_writes() {
        let mut spec = CommandSpec::new("/bin/sh", std::env::temp_dir());
        spec.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
        let mut process = JsonLineProcess::spawn(&spec).await.unwrap();
        let result = process
            .send_with_timeout(
                &serde_json::json!({"data": "x".repeat(1024 * 1024)}),
                Duration::from_millis(20),
            )
            .await;
        assert!(
            matches!(result, Err(AppError::ProviderUnavailable(message)) if message.contains("write timed out"))
        );
        process.terminate().await;
        assert!(process.child.try_wait().unwrap().is_some());
    }
    #[tokio::test]
    async fn interrupted_read_preserves_the_partial_protocol_frame() {
        let (mut writer, reader) = tokio::io::duplex(128);
        let mut reader = BufReader::new(reader);
        let (mut pending, mut discarding) = (Vec::new(), None);
        writer.write_all(b"{\"part\":").await.unwrap();
        assert!(timeout(
            Duration::from_millis(10),
            read_bounded_line(&mut reader, &mut pending, &mut discarding, 128)
        )
        .await
        .is_err());
        writer.write_all(b"true}\n").await.unwrap();
        let frame = read_bounded_line(&mut reader, &mut pending, &mut discarding, 128)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            classify_line(frame),
            ProviderRead::Frame(value) if value == serde_json::json!({"part": true})
        ));
        assert!(pending.is_empty());
    }
}
