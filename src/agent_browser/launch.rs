//! Starting a headed Chromium for one browser profile: no startup window
//! (tabs open in their own background windows, so nothing takes focus),
//! no throttling of covered windows (the live view keeps its frame rate),
//! and the DevTools connection on a pipe where the OS allows it.

#[cfg(unix)]
use std::path::PathBuf;
use std::{collections::VecDeque, path::Path, sync::Arc, time::Duration};

use tokio::{
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};

use super::{cdp::Cdp, BrowserError};
use crate::provider::process::{drain_stderr, stderr_excerpt};

const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the X server gets to report its display number.
#[cfg(unix)]
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(10);
/// The part of a process's stderr kept for failure messages.
const STDERR_TAIL_BYTES: usize = 16 * 1024;
/// How long a dying process gets to finish exiting and flush its stderr
/// before a failure message is composed.
const SETTLE: Duration = Duration::from_millis(500);

/// A child process with the tail of its stderr. The drain task runs for the
/// process's whole life: a full pipe would block the child.
struct Supervised {
    name: &'static str,
    child: Child,
    stderr: Arc<Mutex<VecDeque<u8>>>,
    drain: Option<JoinHandle<()>>,
}

impl Supervised {
    fn new(name: &'static str, mut child: Child) -> Self {
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let drain = child
            .stderr
            .take()
            .map(|reader| tokio::spawn(drain_stderr(reader, stderr.clone(), STDERR_TAIL_BYTES)));
        Self {
            name,
            child,
            stderr,
            drain,
        }
    }

    /// Whether the process has exited, and a one-line account of its state
    /// and last stderr output. With `settle`, a process that is going away
    /// (its pipe closed) gets a moment to be reaped.
    async fn report(&mut self, settle: bool) -> (bool, String) {
        let status = match self.child.try_wait() {
            Ok(None) if settle => tokio::time::timeout(SETTLE, self.child.wait())
                .await
                .ok()
                .map(|status| status.map(Some))
                .unwrap_or(Ok(None)),
            other => other,
        };
        let (exited, state) = match status {
            Ok(Some(status)) => (true, format!("exited with {status}")),
            Ok(None) => (false, "is still running".to_owned()),
            Err(error) => (false, format!("has an unknown state ({error})")),
        };
        if exited {
            // The pipe ends when the process (and its children) are gone.
            if let Some(mut drain) = self.drain.take() {
                if tokio::time::timeout(SETTLE, &mut drain).await.is_err() {
                    self.drain = Some(drain);
                }
            }
        }
        let mut report = format!("{} {state}", self.name);
        let mut buffer = self.stderr.lock().await;
        if let Some(excerpt) = stderr_excerpt(buffer.make_contiguous()) {
            report.push_str(&format!("; stderr: {excerpt}"));
        }
        (exited, report)
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        if let Some(drain) = self.drain.take() {
            drain.abort();
        }
    }
}

/// A running Chromium (and its virtual display on headless Linux).
pub(crate) struct Process {
    pub cdp: Cdp,
    browser: Supervised,
    display: Option<Display>,
    #[cfg(windows)]
    _job: job::JobObject,
}

impl Process {
    /// What is known about the browser's state, for a "browser exited"
    /// message: its exit status and the tail of its stderr (secrets
    /// redacted), plus the same for the virtual display if that died.
    // Used by the browser session's exit message.
    #[allow(dead_code)]
    pub(crate) async fn diagnostics(&mut self) -> String {
        let settle = self.cdp.is_closed();
        account(&mut self.browser, self.display.as_mut(), settle).await
    }
}

/// Xvfb and the cookie files its clients authenticate with.
#[cfg(unix)]
struct Display {
    xvfb: Supervised,
    _auth: x11::AuthFiles,
}

#[cfg(not(unix))]
struct Display {
    xvfb: Supervised,
}

pub(crate) async fn launch(executable: &Path, profile_dir: &Path) -> Result<Process, BrowserError> {
    launch_in(executable, profile_dir, true).await
}

/// [`launch`]; `virtual_display: false` skips the Xvfb fallback (tests run
/// a stand-in executable that needs no display).
async fn launch_in(
    executable: &Path,
    profile_dir: &Path,
    virtual_display: bool,
) -> Result<Process, BrowserError> {
    // Cookies and storage of the agent's sessions: owner-only (0700).
    let dir = profile_dir.to_path_buf();
    tokio::task::spawn_blocking(move || crate::secure_fs::ensure_owner_only_dir(&dir))
        .await
        .map_err(|error| {
            BrowserError::failed(format!("cannot create the browser profile: {error}"))
        })?
        .map_err(|error| {
            BrowserError::failed(format!("cannot create the browser profile: {error}"))
        })?;
    let mut command = Command::new(executable);
    command
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--no-startup-window",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
            "--disable-background-timer-throttling",
            "--disable-session-crashed-bubble",
            "--hide-crash-restore-bubble",
            "--disable-sync",
            "--disable-features=Translate,MediaRouter,OptimizationHints",
            "--window-size=1280,800",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Keep agent profiles out of the user's keychain / keyring.
    if cfg!(target_os = "macos") {
        command.arg("--use-mock-keychain");
    }
    if cfg!(target_os = "linux") {
        command.arg("--password-store=basic");
    }
    let mut display = if virtual_display {
        self::virtual_display(&mut command, profile_dir).await?
    } else {
        None
    };
    let spawned = spawn_with_pipe(command)?;
    let mut browser = Supervised::new("Chromium", spawned.child);
    let cdp = match Cdp::connect(spawned.transport).await {
        Ok(cdp) => cdp,
        Err(error) => return Err(explain(error, &mut browser, display.as_mut(), true).await),
    };
    if let Err(error) = wait_ready(&cdp).await {
        let settle = cdp.is_closed();
        return Err(explain(error, &mut browser, display.as_mut(), settle).await);
    }
    Ok(Process {
        cdp,
        browser,
        display,
        #[cfg(windows)]
        _job: spawned.job,
    })
}

/// `error` with what the failed process left behind appended.
async fn explain(
    error: BrowserError,
    browser: &mut Supervised,
    display: Option<&mut Display>,
    settle: bool,
) -> BrowserError {
    let report = account(browser, display, settle).await;
    BrowserError {
        message: format!("{} ({report})", error.message),
        ..error
    }
}

/// The browser's state and stderr tail, and the virtual display's if that
/// has exited.
async fn account(browser: &mut Supervised, display: Option<&mut Display>, settle: bool) -> String {
    let (_, mut report) = browser.report(settle).await;
    if let Some(display) = display {
        let (exited, display_report) = display.xvfb.report(false).await;
        if exited {
            report.push_str("; ");
            report.push_str(&display_report);
        }
    }
    report
}

async fn wait_ready(cdp: &Cdp) -> Result<(), BrowserError> {
    tokio::time::timeout(
        START_TIMEOUT,
        cdp.call("Browser.getVersion", serde_json::json!({}), None),
    )
    .await
    .map_err(|_| BrowserError::failed("Chromium did not start in time"))?
    .map(|_| ())
}

/// What [`spawn_with_pipe`] started.
struct Spawned {
    transport: super::cdp::Transport,
    child: Child,
    /// Kills Chromium and everything it started when dropped.
    #[cfg(windows)]
    job: job::JobObject,
}

/// A close-on-exec pipe, so no other child the daemon starts inherits the
/// browser's DevTools channel (or Xvfb's display-number pipe).
#[cfg(unix)]
fn cloexec_pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds = [0; 2];
    // Linux creates the pipe close-on-exec atomically.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: fds has room for the two descriptors pipe2() writes.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // macOS has no pipe2: between pipe() and fcntl() a fork+exec on
    // another thread (any provider or tool the daemon spawns) can
    // inherit both ends. That child would hold the DevTools channel
    // open (Chromium sees no EOF when we close ours) but cannot use it
    // unless it already speaks CDP on that fd. The daemon has no global
    // spawn lock to close the window; it is a few instructions wide.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: fds has room for the two descriptors pipe() writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            // SAFETY: fd was just returned by pipe().
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                let error = std::io::Error::last_os_error();
                // SAFETY: both were returned by pipe() and are unused.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(error);
            }
        }
    }
    // SAFETY: both descriptors are open and owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// fds 3 (commands in) and 4 (messages out), as `--remote-debugging-pipe`
/// expects.
#[cfg(unix)]
fn spawn_with_pipe(mut command: Command) -> Result<Spawned, BrowserError> {
    use std::os::fd::AsRawFd;

    use super::cdp::Transport;

    let failed =
        |error: std::io::Error| BrowserError::failed(format!("cannot start Chromium: {error}"));
    let (child_reads, we_write) = cloexec_pipe().map_err(failed)?;
    let (we_read, child_writes) = cloexec_pipe().map_err(failed)?;
    let (read_fd, write_fd) = (child_reads.as_raw_fd(), child_writes.as_raw_fd());
    command.arg("--remote-debugging-pipe");
    // SAFETY: only async-signal-safe calls (dup, dup2) run in the child.
    // `dup` never sets close-on-exec and picks the lowest free fd (>= 3, as
    // 0-2 are taken), so dup'ing both before dup2 cannot clobber one with
    // the other.
    unsafe {
        command.pre_exec(move || {
            let reads = libc::dup(read_fd);
            let writes = libc::dup(write_fd);
            if reads < 0 || writes < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(reads, 3) < 0 || libc::dup2(writes, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(failed)?;
    drop(child_reads);
    drop(child_writes);
    let to_browser = tokio::net::unix::pipe::Sender::from_owned_fd(we_write).map_err(failed)?;
    let from_browser = tokio::net::unix::pipe::Receiver::from_owned_fd(we_read).map_err(failed)?;
    Ok(Spawned {
        transport: Transport::Pipe {
            to_browser,
            from_browser,
        },
        child,
    })
}

/// The DevTools channel on two anonymous pipes whose browser-side ends are
/// handed to Chromium by handle value
/// (`--remote-debugging-pipe --remote-debugging-io-pipes=<read>,<write>`:
/// the first handle is what the browser reads commands from, the second
/// what it writes messages to; `DevToolsAgentHost::StartRemoteDebuggingPipeHandler`
/// adopts them with `_open_osfhandle`, and the values are the same in the
/// child because the handles are inherited). The process starts suspended
/// and is put in a kill-on-close job before it runs, so it cannot outlive
/// the daemon.
#[cfg(windows)]
fn spawn_with_pipe(mut command: Command) -> Result<Spawned, BrowserError> {
    use std::os::windows::io::AsRawHandle;

    use super::cdp::Transport;

    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    let failed =
        |error: std::io::Error| BrowserError::failed(format!("cannot start Chromium: {error}"));
    // Anonymous pipes are not inheritable by default.
    let (child_reads, we_write) = std::io::pipe().map_err(failed)?;
    let (we_read, child_writes) = std::io::pipe().map_err(failed)?;
    let (read_handle, write_handle) = (child_reads.as_raw_handle(), child_writes.as_raw_handle());
    // Only these two, only until the spawn below returns. `CreateProcess`
    // (as std calls it) hands every inheritable handle to the child, so a
    // spawn on another thread in this window would inherit them too: that
    // child would keep Chromium's channel open but cannot speak CDP on it.
    job::set_inheritable(read_handle, true).map_err(failed)?;
    job::set_inheritable(write_handle, true).map_err(failed)?;
    command
        .arg("--remote-debugging-pipe")
        .arg(format!(
            "--remote-debugging-io-pipes={},{}",
            read_handle as usize as u32, write_handle as usize as u32
        ))
        .creation_flags(CREATE_SUSPENDED);
    let child = command.spawn().map_err(failed)?;
    // The browser has its own copies; ours must go so it sees EOF when the
    // daemon lets go.
    drop(child_reads);
    drop(child_writes);
    let started = (|| {
        let pid = child
            .id()
            .ok_or_else(|| std::io::Error::other("the process exited immediately"))?;
        let process = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("the process has no handle"))?;
        let job = job::JobObject::kill_on_close()?;
        job.assign(process)?;
        job::resume_main_thread(pid)?;
        Ok::<_, std::io::Error>(job)
    })();
    let job = match started {
        Ok(job) => job,
        Err(error) => {
            // `kill_on_drop` ends the suspended process with `child`.
            return Err(failed(error));
        }
    };
    Ok(Spawned {
        transport: Transport::Pipe {
            to_browser: we_write,
            from_browser: we_read,
        },
        child,
        job,
    })
}

#[cfg(windows)]
mod job {
    use std::{ffi::c_void, io, mem::size_of, os::windows::io::RawHandle};

    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    fn io_error(error: windows::core::Error) -> io::Error {
        io::Error::other(error)
    }

    /// Closes the handle on drop.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle is open and owned here.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// A job object whose processes are all killed when it closes.
    pub(super) struct JobObject(Owned);

    // SAFETY: a job handle is a process-wide kernel handle usable from any
    // thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        pub(super) fn kill_on_close() -> io::Result<Self> {
            // SAFETY: no security attributes or name are passed.
            let job = Owned(unsafe { CreateJobObjectW(None, None) }.map_err(io_error)?);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `limits` is the structure the information class
            // expects, with its exact size.
            unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast::<c_void>(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            }
            .map_err(io_error)?;
            Ok(Self(job))
        }

        pub(super) fn assign(&self, process: RawHandle) -> io::Result<()> {
            // SAFETY: both handles are open; the process handle is the
            // spawned child's, valid while `Child` lives.
            unsafe { AssignProcessToJobObject((self.0).0, HANDLE(process)) }.map_err(io_error)
        }
    }

    /// Sets or clears whether child processes inherit `handle`.
    pub(super) fn set_inheritable(handle: RawHandle, inheritable: bool) -> io::Result<()> {
        let flags = if inheritable {
            HANDLE_FLAG_INHERIT
        } else {
            HANDLE_FLAGS(0)
        };
        // SAFETY: the handle is an open pipe end owned by the caller.
        unsafe {
            windows::Win32::Foundation::SetHandleInformation(
                HANDLE(handle),
                HANDLE_FLAG_INHERIT.0,
                flags,
            )
        }
        .map_err(io_error)
    }

    /// Resumes the initial thread of a process created `CREATE_SUSPENDED`.
    pub(super) fn resume_main_thread(pid: u32) -> io::Result<()> {
        // SAFETY: a snapshot of thread ids; closed by `Owned`.
        let snapshot =
            Owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(io_error)?);
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: `entry.dwSize` is set as the API requires.
        let mut more = unsafe { Thread32First(snapshot.0, &mut entry) }.is_ok();
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: opens the thread with only the right it needs.
                let thread = Owned(
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                        .map_err(io_error)?,
                );
                // SAFETY: the thread handle is open.
                if unsafe { ResumeThread(thread.0) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snapshot.0, &mut entry) }.is_ok();
        }
        Err(io::Error::other("the new process has no thread to resume"))
    }
}

/// On Linux without a graphical session, runs the browser in an Xvfb
/// display (headed, just not visible on a screen). The server picks its own
/// display number (`-displayfd`) and only accepts clients holding a random
/// cookie, kept in owner-only files that go away with the display.
#[cfg(unix)]
async fn virtual_display(
    command: &mut Command,
    profile_dir: &Path,
) -> Result<Option<Display>, BrowserError> {
    if !cfg!(target_os = "linux")
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
    {
        return Ok(None);
    }
    let Some(xvfb) = find_in_path("Xvfb") else {
        return Err(BrowserError::new(
            "UNAVAILABLE",
            "this computer has no graphical session; install Xvfb (e.g. `apt install xvfb`) so the agent browser can run",
        ));
    };
    // `<data>/agent-browser/profiles/<id>` -> `<data>/agent-browser/x11`.
    let x11_root = profile_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| BrowserError::failed("the browser profile has no data directory"))?
        .join("x11");
    let auth = tokio::task::spawn_blocking(move || x11::AuthFiles::create(&x11_root))
        .await
        .map_err(|error| BrowserError::failed(format!("cannot prepare the display: {error}")))?
        .map_err(|error| BrowserError::failed(format!("cannot prepare the display: {error}")))?;
    let (xvfb, number) = start_xvfb(&xvfb, &auth).await?;
    command
        .env("DISPLAY", format!(":{number}"))
        .env("XAUTHORITY", &auth.client);
    Ok(Some(Display { xvfb, _auth: auth }))
}

#[cfg(not(unix))]
async fn virtual_display(
    _command: &mut Command,
    _profile_dir: &Path,
) -> Result<Option<Display>, BrowserError> {
    Ok(None)
}

/// Starts `xvfb` on a display of its choosing and returns it with that
/// display's number.
#[cfg(unix)]
async fn start_xvfb(xvfb: &Path, auth: &x11::AuthFiles) -> Result<(Supervised, u32), BrowserError> {
    use std::os::fd::AsRawFd;

    use tokio::io::AsyncReadExt;

    /// The descriptor Xvfb is told to write its display number to.
    const DISPLAY_FD: libc::c_int = 3;
    let failed =
        |error: std::io::Error| BrowserError::failed(format!("cannot start Xvfb: {error}"));
    let (number_reader, number_writer) = cloexec_pipe().map_err(failed)?;
    let write_fd = number_writer.as_raw_fd();
    let mut command = Command::new(xvfb);
    command
        .args(["-displayfd", &DISPLAY_FD.to_string()])
        .args(["-screen", "0", "1920x1080x24", "-nolisten", "tcp", "-auth"])
        .arg(&auth.server)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe calls (dup, dup2, close) run in the
    // child. `dup` clears close-on-exec, which dup2 onto the same number
    // would not.
    unsafe {
        command.pre_exec(move || {
            let duplicate = libc::dup(write_fd);
            if duplicate < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if duplicate != DISPLAY_FD {
                if libc::dup2(duplicate, DISPLAY_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(duplicate);
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(failed)?;
    let mut xvfb = Supervised::new("Xvfb", child);
    // Ours must close, or the read below never sees Xvfb exit.
    drop(number_writer);
    let mut reader =
        tokio::net::unix::pipe::Receiver::from_owned_fd(number_reader).map_err(failed)?;
    let read = async {
        let mut received = Vec::new();
        let mut chunk = [0_u8; 32];
        while !received.contains(&b'\n') && received.len() < 64 {
            let count = reader.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            received.extend_from_slice(&chunk[..count]);
        }
        std::io::Result::Ok(received)
    };
    let received = tokio::time::timeout(DISPLAY_TIMEOUT, read).await;
    let number = match &received {
        Ok(Ok(bytes)) => x11::parse_displayfd(bytes),
        _ => None,
    };
    match number {
        Some(number) => Ok((xvfb, number)),
        None => {
            let (_, report) = xvfb.report(true).await;
            Err(BrowserError::failed(format!(
                "Xvfb did not report its display ({report})"
            )))
        }
    }
}

#[cfg(unix)]
fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The X11 authority side of the virtual display.
#[cfg(unix)]
mod x11 {
    use std::{
        io,
        path::{Path, PathBuf},
    };

    use rand_core::{OsRng, RngCore};
    use uuid::Uuid;

    /// `FamilyWild`: the entry applies whatever the address.
    const FAMILY_WILD: u16 = 0xffff;
    const AUTH_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
    pub(super) const COOKIE_BYTES: usize = 16;

    /// One `.Xauthority` entry for every host and display: big-endian
    /// family, then address, display number, name and data, each preceded
    /// by its big-endian 16-bit length. The address and number are empty,
    /// which libXau/xcb match against any.
    pub(super) fn encode_xauthority(cookie: &[u8; COOKIE_BYTES]) -> Vec<u8> {
        fn counted(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&FAMILY_WILD.to_be_bytes());
        counted(&mut out, b"");
        counted(&mut out, b"");
        counted(&mut out, AUTH_NAME);
        counted(&mut out, cookie);
        out
    }

    /// The display number `Xvfb -displayfd` writes (digits and a newline).
    pub(super) fn parse_displayfd(bytes: &[u8]) -> Option<u32> {
        let line = bytes.split(|byte| *byte == b'\n').next()?;
        // No newline yet: the number may still be arriving.
        if line.len() == bytes.len() {
            return None;
        }
        let text = std::str::from_utf8(line).ok()?.trim();
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    }

    /// The server's and the clients' copy of one random cookie, in a
    /// 0700 directory removed on drop.
    pub(super) struct AuthFiles {
        dir: PathBuf,
        pub(super) server: PathBuf,
        pub(super) client: PathBuf,
    }

    impl AuthFiles {
        pub(super) fn create(root: &Path) -> io::Result<Self> {
            let mut cookie = [0_u8; COOKIE_BYTES];
            OsRng
                .try_fill_bytes(&mut cookie)
                .map_err(|error| io::Error::other(format!("no random cookie: {error}")))?;
            let contents = encode_xauthority(&cookie);
            crate::secure_fs::ensure_owner_only_dir(root)?;
            let dir = root.join(Uuid::new_v4().simple().to_string());
            crate::secure_fs::ensure_owner_only_dir(&dir)?;
            let files = Self {
                server: dir.join("server.auth"),
                client: dir.join("client.auth"),
                dir,
            };
            // On failure `files` drops and removes what was written.
            crate::secure_fs::create_owner_only(&files.server, &contents)?;
            crate::secure_fs::create_owner_only(&files.client, &contents)?;
            Ok(files)
        }
    }

    impl Drop for AuthFiles {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.dir) {
                if error.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%error, dir = %self.dir.display(), "cannot remove the X authority files");
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn xauthority_entries_use_big_endian_lengths() {
            let cookie: [u8; COOKIE_BYTES] = std::array::from_fn(|index| index as u8 + 1);
            let encoded = encode_xauthority(&cookie);
            let mut expected = vec![0xff, 0xff, 0, 0, 0, 0, 0, 18];
            expected.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
            expected.extend_from_slice(&[0, 16]);
            expected.extend_from_slice(&cookie);
            assert_eq!(encoded, expected);
        }

        #[test]
        fn displayfd_output_is_a_number_and_a_newline() {
            assert_eq!(parse_displayfd(b"99\n"), Some(99));
            assert_eq!(parse_displayfd(b"0\nextra"), Some(0));
            assert_eq!(parse_displayfd(b" 7 \n"), Some(7));
            // Incomplete, empty or not a number.
            assert_eq!(parse_displayfd(b"99"), None);
            assert_eq!(parse_displayfd(b"\n"), None);
            assert_eq!(parse_displayfd(b"-1\n"), None);
            assert_eq!(parse_displayfd(b"x11\n"), None);
            assert_eq!(parse_displayfd(b"99999999999\n"), None);
            assert_eq!(parse_displayfd(b""), None);
        }

        #[test]
        fn auth_files_are_private_random_and_removed_on_drop() {
            let root = std::env::temp_dir().join(format!("todex-x11-{}", Uuid::new_v4()));
            let first = AuthFiles::create(&root).unwrap();
            let second = AuthFiles::create(&root).unwrap();
            let read = |path: &Path| std::fs::read(path).unwrap();
            assert_eq!(read(&first.server), read(&first.client));
            assert_ne!(read(&first.server), read(&second.server));
            assert_eq!(read(&first.server).len(), 2 + 2 + 2 + 2 + 18 + 2 + 16);
            {
                use std::os::unix::fs::PermissionsExt;
                let mode =
                    |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode(&first.dir), 0o700);
                assert_eq!(mode(&first.server), 0o600);
            }
            let dir = first.dir.clone();
            drop(first);
            assert!(!dir.exists());
            assert!(second.dir.exists());
            drop(second);
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("todex-launch-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An executable "browser" script that ignores every Chromium flag.
    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_browser_that_exits_reports_its_status_and_stderr() {
        let dir = temp_dir("exit");
        let browser = script(&dir, "browser", "echo boom >&2\nexit 3");
        let error = match launch_in(&browser, &dir.join("profile"), false).await {
            Ok(_) => panic!("a browser that exits cannot be ready"),
            Err(error) => error,
        };
        assert!(error.message.contains("boom"), "{}", error.message);
        assert!(
            error.message.contains("exit status: 3"),
            "{}",
            error.message
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_tails_are_bounded_and_secrets_redacted() {
        let dir = temp_dir("tail");
        // More than the tail keeps, ending in a credential-looking token.
        let browser = script(
            &dir,
            "browser",
            "head -c 100000 /dev/zero | tr '\\0' 'x' >&2\necho >&2\necho 'key=sk-abcdefghijklmnopqrstuvwxyz0123' >&2\nexit 1",
        );
        let error = match launch_in(&browser, &dir.join("profile"), false).await {
            Ok(_) => panic!("a browser that exits cannot be ready"),
            Err(error) => error,
        };
        assert!(
            !error.message.contains("sk-abcdefghijklmnop"),
            "{}",
            error.message
        );
        assert!(error.message.len() < 4096, "{}", error.message.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn xvfb_reports_its_display_through_displayfd() {
        let dir = temp_dir("xvfb");
        // Writes the number the way Xvfb does, then keeps running.
        let xvfb = script(&dir, "Xvfb", "echo 42 >&3\nexec sleep 30");
        let auth = x11::AuthFiles::create(&dir.join("x11")).unwrap();
        let (mut display, number) = start_xvfb(&xvfb, &auth).await.unwrap();
        assert_eq!(number, 42);
        let (exited, report) = display.report(false).await;
        assert!(!exited, "{report}");
        drop(display);
        drop(auth);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_xvfb_that_dies_before_reporting_is_explained() {
        let dir = temp_dir("xvfb-dies");
        let xvfb = script(&dir, "Xvfb", "echo 'no screens' >&2\nexit 1");
        let auth = x11::AuthFiles::create(&dir.join("x11")).unwrap();
        let error = match start_xvfb(&xvfb, &auth).await {
            Ok(_) => panic!("no display was reported"),
            Err(error) => error,
        };
        assert!(error.message.contains("no screens"), "{}", error.message);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Not run by the author (no Windows host): Chromium's
    /// `--remote-debugging-io-pipes` handles, the suspended start and the
    /// job object are only exercised on `windows-latest` CI.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_browser_that_exits_reports_its_status_and_stderr_on_windows() {
        let dir = temp_dir("exit-win");
        let browser = dir.join("browser.bat");
        std::fs::write(&browser, "@echo boom 1>&2\r\n@exit /b 3\r\n").unwrap();
        let error = match launch_in(&browser, &dir.join("profile"), false).await {
            Ok(_) => panic!("a browser that exits cannot be ready"),
            Err(error) => error,
        };
        assert!(error.message.contains("boom"), "{}", error.message);
        assert!(error.message.contains('3'), "{}", error.message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
