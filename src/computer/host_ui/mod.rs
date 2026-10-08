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
        atomic::{AtomicBool, AtomicUsize, Ordering},
        OnceLock,
    },
    time::Duration,
};

use tokio::sync::broadcast;

/// The global shortcut that stops a session on this OS, as shown to
/// people; `None` where the only stop control is the pill's or the status
/// notification's Stop (Linux outside KDE Plasma Wayland, or when the
/// system refused the shortcut).
pub(crate) fn stop_shortcut() -> Option<&'static str> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        native::stop_shortcut()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
/// `timeout` passes (`Some(false)`). `None` when nobody can be asked: no
/// host UI, or no dialog mechanism on this desktop.
pub(crate) fn confirm(title: &str, message: &str, timeout: Duration) -> Option<bool> {
    if !available() {
        return None;
    }
    CONFIRMING.fetch_add(1, Ordering::SeqCst);
    let _open = Confirming;
    native::confirm(&strings(), title, message, timeout)
}

/// Confirmations on screen. While one is, agents may not act at all: on
/// Linux it is another process (a notification or `kdialog`), so the
/// hit test cannot tell it is ours, and nobody but the person at the host
/// may answer it.
static CONFIRMING: AtomicUsize = AtomicUsize::new(0);

struct Confirming;

impl Drop for Confirming {
    fn drop(&mut self) {
        CONFIRMING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A host confirmation is on screen.
pub(crate) fn confirming() -> bool {
    CONFIRMING.load(Ordering::SeqCst) > 0
}

/// The window number of the pointer marker, which lets clicks through;
/// hit tests skip it.
#[cfg(target_os = "macos")]
pub(crate) fn marker_window() -> Option<u32> {
    native::marker_window()
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
