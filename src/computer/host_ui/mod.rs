//! What the person at the controlled computer sees while an agent uses it:
//! a status pill with a Stop button, a marker where pointer actions land,
//! a global stop shortcut, and the dialog that grants a conversation
//! Computer Use. Only processes that run [`run_with_main_loop`] (the
//! server) have a host UI; everywhere else these calls do nothing and
//! [`confirm`] reports that nobody can be asked.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as native;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use self::windows as native;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as native;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod other;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
use other as native;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::Duration,
};

use tokio::sync::broadcast;

/// The global shortcut that stops a session on this OS, as shown to
/// people; `None` where the only stop control is the status
/// notification's Stop action (Linux outside KDE Plasma Wayland, or when
/// KDE refused the shortcut).
pub(crate) fn stop_shortcut() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        native::stop_shortcut()
    }
    #[cfg(not(target_os = "linux"))]
    {
        native::STOP_SHORTCUT
    }
}

static AVAILABLE: AtomicBool = AtomicBool::new(false);
static STOPS: OnceLock<broadcast::Sender<()>> = OnceLock::new();

fn stops() -> &'static broadcast::Sender<()> {
    STOPS.get_or_init(|| broadcast::channel(4).0)
}

/// Runs `body` while the main thread serves the host UI, then exits the
/// process with `body`'s outcome (as `main` returning it would).
pub(crate) fn run_with_main_loop<F>(body: F) -> !
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    native::run_with_main_loop(move || {
        let code = match body() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("Error: {error:?}");
                1
            }
        };
        std::process::exit(code)
    })
}

pub(crate) fn mark_available() {
    AVAILABLE.store(true, Ordering::SeqCst);
}

/// Whether this process can show anything to the person at the screen.
pub(crate) fn available() -> bool {
    AVAILABLE.load(Ordering::SeqCst)
}

/// The Stop button or the stop shortcut was used.
pub(crate) fn stop_requests() -> broadcast::Receiver<()> {
    stops().subscribe()
}

pub(crate) fn request_stop() {
    let _ = stops().send(());
}

/// Shows (or updates) the pill saying an agent is in control, and arms the
/// stop shortcut.
pub(crate) fn show_status(summary: &str) {
    if available() {
        native::show_status(&strings(), summary);
    }
}

pub(crate) fn hide_status() {
    if available() {
        native::hide_status();
    }
}

/// Briefly marks a global point a pointer action is about to hit.
pub(crate) fn mark_point(x: f64, y: f64) {
    if available() {
        native::mark_point(x, y);
    }
}

/// Asks the person at this computer; blocks until they answer or
/// `timeout` passes (`Some(false)`). `None` when there is no host UI.
pub(crate) fn confirm(title: &str, message: &str, timeout: Duration) -> Option<bool> {
    available().then(|| native::confirm(&strings(), title, message, timeout))
}

/// Host UI strings in the system language.
pub(crate) struct Strings {
    pub controlling: &'static str,
    pub stop: &'static str,
    // Windows message boxes label their buttons in the system language.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub allow: &'static str,
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub deny: &'static str,
}

pub(crate) fn chinese() -> bool {
    native::preferred_language().is_some_and(|language| language.starts_with("zh"))
}

fn strings() -> Strings {
    if chinese() {
        Strings {
            controlling: "Agent 正在控制这台电脑",
            stop: "停止",
            allow: "允许",
            deny: "拒绝",
        }
    } else {
        Strings {
            controlling: "An agent is controlling this computer",
            stop: "Stop",
            allow: "Allow",
            deny: "Deny",
        }
    }
}
