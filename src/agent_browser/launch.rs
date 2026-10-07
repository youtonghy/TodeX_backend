//! Starting a headed Chromium for one browser profile: no startup window
//! (tabs open in their own background windows, so nothing takes focus),
//! no throttling of covered windows (the live view keeps its frame rate),
//! and the DevTools connection on a pipe where the OS allows it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::process::{Child, Command};

use super::{
    cdp::{Cdp, Transport},
    BrowserError,
};

const START_TIMEOUT: Duration = Duration::from_secs(20);

/// A running Chromium (and its virtual display on headless Linux).
pub(crate) struct Process {
    pub cdp: Cdp,
    _child: Child,
    _display: Option<Child>,
}

pub(crate) async fn launch(executable: &Path, profile_dir: &Path) -> Result<Process, BrowserError> {
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
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // Keep agent profiles out of the user's keychain / keyring.
    if cfg!(target_os = "macos") {
        command.arg("--use-mock-keychain");
    }
    if cfg!(target_os = "linux") {
        command.arg("--password-store=basic");
    }
    let display = virtual_display(&mut command).await?;
    #[cfg(unix)]
    {
        let (transport, child) = spawn_with_pipe(command)?;
        let cdp = Cdp::connect(transport).await?;
        wait_ready(&cdp).await?;
        Ok(Process {
            cdp,
            _child: child,
            _display: display,
        })
    }
    #[cfg(not(unix))]
    {
        let port_file = profile_dir.join("DevToolsActivePort");
        let _ = tokio::fs::remove_file(&port_file).await;
        command.arg("--remote-debugging-port=0");
        let child = command
            .spawn()
            .map_err(|error| BrowserError::failed(format!("cannot start Chromium: {error}")))?;
        let url = devtools_url(&port_file).await?;
        let cdp = Cdp::connect(Transport::WebSocket(url)).await?;
        wait_ready(&cdp).await?;
        Ok(Process {
            cdp,
            _child: child,
            _display: display,
        })
    }
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

/// fds 3 (commands in) and 4 (messages out), as `--remote-debugging-pipe`
/// expects.
#[cfg(unix)]
fn spawn_with_pipe(mut command: Command) -> Result<(Transport, Child), BrowserError> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A close-on-exec pipe, so no other child the daemon starts inherits
    /// the browser's DevTools channel.
    fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
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
    let failed =
        |error: std::io::Error| BrowserError::failed(format!("cannot start Chromium: {error}"));
    let (child_reads, we_write) = pipe().map_err(failed)?;
    let (we_read, child_writes) = pipe().map_err(failed)?;
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
    Ok((
        Transport::Pipe {
            to_browser,
            from_browser,
        },
        child,
    ))
}

#[cfg(not(unix))]
async fn devtools_url(port_file: &Path) -> Result<String, BrowserError> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(contents) = tokio::fs::read_to_string(port_file).await {
            let mut lines = contents.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                return Ok(format!("ws://127.0.0.1:{}{}", port.trim(), path.trim()));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BrowserError::failed("Chromium did not start in time"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// On Linux without a graphical session, runs the browser in an Xvfb
/// display (headed, just not visible on a screen).
async fn virtual_display(command: &mut Command) -> Result<Option<Child>, BrowserError> {
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
    let number = (99..200)
        .find(|number| !Path::new(&format!("/tmp/.X{number}-lock")).exists())
        .ok_or_else(|| BrowserError::failed("no free X display number for Xvfb"))?;
    let child = Command::new(xvfb)
        .args([
            format!(":{number}"),
            "-screen".into(),
            "0".into(),
            "1920x1080x24".into(),
            "-nolisten".into(),
            "tcp".into(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| BrowserError::failed(format!("cannot start Xvfb: {error}")))?;
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
    for _ in 0..50 {
        if socket.exists() {
            command.env("DISPLAY", format!(":{number}"));
            return Ok(Some(child));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(BrowserError::failed("Xvfb did not start"))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}
