//! Bounded execution of short-lived external tools (git, gh, ssh, ssh-keygen).
//!
//! Every run has a deadline and an output cap, and the child is placed in its
//! own process group so helpers that inherit its pipes (git hooks, ssh
//! `ProxyCommand`s) are killed with it on timeout, overflow, or cancellation.

use std::{
    ffi::{OsStr, OsString},
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::mpsc,
    time::timeout,
};

/// Environment kept for external tools: user configuration, locale, network
/// certificates and SSH agent access. Daemon configuration (`TODEX_AGENTD_*`)
/// and tool-specific override variables are deliberately not inherited.
const INHERITED_ENV: &[&str] = &[
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
];

#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExternalCommandError {
    #[error("executable not found")]
    NotFound,
    #[error("process could not be started: {0}")]
    Spawn(io::Error),
    #[error("timed out")]
    TimedOut,
    #[error("output exceeded the {0} byte limit")]
    OutputLimit(usize),
    #[error("{0}")]
    Io(io::Error),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommandLimits {
    pub timeout: Duration,
    pub output_limit: usize,
}

/// Output of [`run_streaming`]: each stream holds at most the output limit.
#[derive(Debug)]
pub(crate) struct TruncatedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether stdout or stderr produced more than the output limit.
    pub truncated: bool,
}

/// What happens when a stream exceeds [`CommandLimits::output_limit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overflow {
    /// Kill the process and fail with [`ExternalCommandError::OutputLimit`].
    Fail,
    /// Keep the first `output_limit` bytes, drain and discard the rest.
    Truncate,
}

/// A `Command` for `program` with a cleared environment plus [`INHERITED_ENV`].
pub(crate) fn secure_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    command.envs(inherited_env());
    command
}

/// The daemon's values for [`INHERITED_ENV`], for launchers other than
/// `tokio::process::Command` (for example PTY command builders).
pub(crate) fn inherited_env() -> Vec<(&'static str, OsString)> {
    INHERITED_ENV
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (*key, value)))
        .collect()
}

/// Pipes stdout/stderr, enables `kill_on_drop`, and isolates the child in its
/// own process group. stdin is null unless the caller pipes it first.
pub(crate) fn prepare_captured(command: &mut Command, piped_stdin: bool) {
    command
        .stdin(if piped_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

/// Runs a command prepared with [`prepare_captured`]. `stdin`, when given, is
/// written and closed before waiting; it requires `piped_stdin = true`.
/// Output beyond the limit kills the process and fails the run.
pub(crate) async fn run(
    command: Command,
    stdin: Option<Vec<u8>>,
    limits: CommandLimits,
) -> Result<CommandOutput, ExternalCommandError> {
    let output = run_with(command, stdin, limits, Overflow::Fail, None).await?;
    Ok(CommandOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Like [`run`], but output beyond the limit is dropped instead of failing
/// the run: the process keeps running until it exits or times out.
/// Which pipe an [`OutputChunk`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

/// Bytes as they were read from the child, before any truncation.
#[derive(Debug)]
pub(crate) struct OutputChunk {
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
}

/// Runs a command whose output beyond the limit is dropped (and reported as
/// truncated) instead of failing the run, and sends the kept stdout and stderr
/// bytes to `tap` as they arrive, so the tap holds at most the output limit
/// per stream. The tap is dropped when the run ends (or is cancelled), so a
/// receiver loop ends with it.
pub(crate) async fn run_streaming(
    command: Command,
    stdin: Option<Vec<u8>>,
    limits: CommandLimits,
    tap: mpsc::UnboundedSender<OutputChunk>,
) -> Result<TruncatedOutput, ExternalCommandError> {
    run_with(command, stdin, limits, Overflow::Truncate, Some(tap)).await
}

async fn run_with(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    limits: CommandLimits,
    overflow: Overflow,
    tap: Option<mpsc::UnboundedSender<OutputChunk>>,
) -> Result<TruncatedOutput, ExternalCommandError> {
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ExternalCommandError::NotFound
        } else {
            ExternalCommandError::Spawn(error)
        }
    })?;
    let process_group_id = child.id();
    let mut process_group_guard = ProcessGroupGuard::new(process_group_id);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExternalCommandError::Io(io::Error::other("stdout pipe unavailable")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ExternalCommandError::Io(io::Error::other("stderr pipe unavailable")))?;
    let stdin_pipe = child.stdin.take();

    let result = timeout(limits.timeout, async {
        let feed = async {
            if let (Some(mut pipe), Some(bytes)) = (stdin_pipe, stdin) {
                // A remote command may exit without reading its input; a
                // broken pipe then is not an error of the run itself.
                match pipe.write_all(&bytes).await {
                    Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
                        return Err(LimitedReadError::Io(error))
                    }
                    _ => {}
                }
                drop(pipe);
            }
            Ok(())
        };
        let wait = async { child.wait().await.map_err(LimitedReadError::Io) };
        tokio::try_join!(
            read_limited(
                stdout,
                limits.output_limit,
                overflow,
                tap.as_ref().map(|tap| (tap, OutputStream::Stdout)),
            ),
            read_limited(
                stderr,
                limits.output_limit,
                overflow,
                tap.as_ref().map(|tap| (tap, OutputStream::Stderr)),
            ),
            feed,
            wait,
        )
    })
    .await;

    let failure = match result {
        Ok(Ok(((stdout, stdout_cut), (stderr, stderr_cut), (), status))) => {
            process_group_guard.disarm();
            return Ok(TruncatedOutput {
                status,
                stdout,
                stderr,
                truncated: stdout_cut || stderr_cut,
            });
        }
        Err(_elapsed) => ExternalCommandError::TimedOut,
        Ok(Err(LimitedReadError::Limit)) => ExternalCommandError::OutputLimit(limits.output_limit),
        Ok(Err(LimitedReadError::Io(error))) => ExternalCommandError::Io(error),
    };
    terminate_child(&mut child, process_group_id).await;
    process_group_guard.disarm();
    Err(failure)
}

/// UTF-8 (lossy) text of at most `limit` bytes, cut at a character boundary.
pub(crate) fn bounded_text(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= limit {
        return text.into_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

struct ProcessGroupGuard {
    process_group_id: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(process_group_id: Option<u32>) -> Self {
        Self { process_group_id }
    }

    fn disarm(&mut self) {
        self.process_group_id = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process_group_id {
            // This guard also runs when an outer request deadline cancels the
            // future before the normal async cleanup path can finish.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

enum LimitedReadError {
    Limit,
    Io(io::Error),
}

impl From<io::Error> for LimitedReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads a stream to its end, returning the bytes and whether any were cut.
async fn read_limited<R>(
    mut reader: R,
    limit: usize,
    overflow: Overflow,
    tap: Option<(&mpsc::UnboundedSender<OutputChunk>, OutputStream)>,
) -> Result<(Vec<u8>, bool), LimitedReadError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    // Heap buffer: an inline array would make every future that awaits a run
    // (and every caller's future in turn) 16 KiB larger.
    let mut buffer = vec![0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if let Some((tap, stream)) = tap {
            // Only the kept bytes are sent, so a slow receiver queues at most
            // `limit` per stream however much the child prints. A receiver
            // that went away only stops observation, not the run.
            let kept = read.min(limit - output.len());
            if kept > 0 {
                let _ = tap.send(OutputChunk {
                    stream,
                    bytes: buffer[..kept].to_vec(),
                });
            }
        }
        if output.len().saturating_add(read) > limit {
            if overflow == Overflow::Fail {
                return Err(LimitedReadError::Limit);
            }
            // Keep draining so the child never blocks on a full pipe.
            let room = limit - output.len();
            output.extend_from_slice(&buffer[..room]);
            truncated = true;
            continue;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

async fn terminate_child(child: &mut Child, process_group_id: Option<u32>) {
    // `Child::kill` waits for the process as well as sending SIGKILL, which
    // avoids leaving a zombie after a timeout or output-limit violation.
    #[cfg(unix)]
    if let Some(pid) = process_group_id {
        // Hooks, credential helpers and proxy commands can inherit the pipes.
        // The child is isolated in its own process group so they die too.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = process_group_id;
    let _ = child.kill().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn limits(seconds: u64, output_limit: usize) -> CommandLimits {
        CommandLimits {
            timeout: Duration::from_secs(seconds),
            output_limit,
        }
    }

    #[tokio::test]
    async fn captures_output_and_feeds_stdin() {
        let mut command = secure_command("/bin/sh");
        command.args(["-c", "cat; echo err >&2; exit 3"]);
        prepare_captured(&mut command, true);
        let output = run(command, Some(b"hello".to_vec()), limits(5, 1024))
            .await
            .unwrap();
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.stderr, b"err\n");
        assert_eq!(output.status.code(), Some(3));
    }

    #[tokio::test]
    async fn enforces_timeout_and_output_limit() {
        let mut command = secure_command("/bin/sh");
        command.args(["-c", "sleep 5"]);
        prepare_captured(&mut command, false);
        let error = run(
            command,
            None,
            CommandLimits {
                timeout: Duration::from_millis(100),
                output_limit: 16,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ExternalCommandError::TimedOut));

        let mut command = secure_command("/bin/sh");
        command.args(["-c", "yes | head -c 4096"]);
        prepare_captured(&mut command, false);
        let error = run(command, None, limits(5, 16)).await.unwrap_err();
        assert!(matches!(error, ExternalCommandError::OutputLimit(16)));
    }

    #[tokio::test]
    async fn streaming_run_truncates_the_result_and_taps_only_kept_output() {
        let mut command = secure_command("/bin/sh");
        command.args(["-c", "yes | head -c 100000; printf ab >&2; exit 4"]);
        prepare_captured(&mut command, false);
        let (tap, mut chunks) = mpsc::unbounded_channel();
        let output = run_streaming(command, None, limits(5, 16), tap)
            .await
            .unwrap();
        assert_eq!(output.stdout, b"y\ny\ny\ny\ny\ny\ny\ny\n");
        assert_eq!(output.stderr, b"ab");
        assert!(output.truncated);
        assert_eq!(output.status.code(), Some(4));
        let (mut stdout, mut stderr) = (0, Vec::new());
        while let Some(chunk) = chunks.recv().await {
            match chunk.stream {
                OutputStream::Stdout => stdout += chunk.bytes.len(),
                OutputStream::Stderr => stderr.extend(chunk.bytes),
            }
        }
        assert_eq!(stdout, 16, "the tap stops at the output limit");
        assert_eq!(stderr, b"ab");

        let mut command = secure_command("/bin/sh");
        command.args(["-c", "printf ok"]);
        prepare_captured(&mut command, false);
        let (tap, _chunks) = mpsc::unbounded_channel();
        let output = run_streaming(command, None, limits(5, 16), tap)
            .await
            .unwrap();
        assert_eq!(output.stdout, b"ok");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn does_not_inherit_daemon_environment() {
        std::env::set_var("TODEX_AGENTD_EXTERNAL_COMMAND_TEST", "leak");
        let mut command = secure_command("/bin/sh");
        command.args(["-c", "printf %s \"$TODEX_AGENTD_EXTERNAL_COMMAND_TEST\""]);
        prepare_captured(&mut command, false);
        let output = run(command, None, limits(5, 1024)).await.unwrap();
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn bounded_text_cuts_at_char_boundary() {
        assert_eq!(bounded_text("héllo".as_bytes(), 2), "h");
        assert_eq!(bounded_text(b"abc", 8), "abc");
    }
}
