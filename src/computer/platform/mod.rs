//! OS services Computer Use needs beyond what xa11y covers: permissions,
//! idle time, app identity, the window under a point, displays, launching
//! and activating apps. One module per desktop OS; every function has the
//! same signature on each.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use self::windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod other;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) use other::*;

// Pure helpers of the Windows and Linux layers, built on every host for
// their unit tests.
#[cfg(any(target_os = "linux", test))]
mod linux_desktop;
#[cfg(any(target_os = "windows", test))]
mod windows_names;

use serde::Serialize;

/// OS permissions Computer Use needs on this host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Permissions {
    pub screen: bool,
    pub accessibility: bool,
}

impl Permissions {
    pub(crate) fn all(self) -> bool {
        self.screen && self.accessibility
    }
}

/// A display in global screen points.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct Display {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

impl Display {
    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// Result of typing without an element.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Typed {
    /// Inserted into the app's focused element in the background.
    #[cfg_attr(any(target_os = "windows", target_os = "linux"), allow(dead_code))]
    Inserted,
    /// The focused element is a password field and the user has not
    /// confirmed.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Secure,
    /// No insertable focused element; the caller falls back to keystrokes.
    Unsupported,
}

/// Held while an action injects pointer or keyboard input. Windows
/// (`GetLastInputInfo`) and X11 (the screensaver idle counter) count
/// injected input as user activity, unlike macOS; their `idle_seconds`
/// ignores input that arrived while this was held, so an agent's own click
/// does not make its next pointer action wait for an "active user".
pub(crate) struct Injecting(());

pub(crate) fn injecting() -> Injecting {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    injected::begin(idle_seconds());
    Injecting(())
}

impl Drop for Injecting {
    fn drop(&mut self) {
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        injected::end();
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
mod injected {
    use std::{sync::Mutex, time::Instant};

    struct Injection {
        start: Instant,
        end: Option<Instant>,
        idle_before: f64,
    }

    static LAST: Mutex<Option<Injection>> = Mutex::new(None);

    pub(super) fn begin(idle_before: f64) {
        *LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Injection {
            start: Instant::now(),
            end: None,
            idle_before,
        });
    }

    pub(super) fn end() {
        if let Some(injection) = LAST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            injection.end = Some(Instant::now());
        }
    }

    /// The user's idle time given the OS counter `raw` (seconds).
    pub(super) fn user_idle(raw: f64) -> f64 {
        let last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(injection) = last.as_ref() else {
            return raw;
        };
        let now = Instant::now();
        super::user_idle(
            raw,
            now.duration_since(injection.start).as_secs_f64(),
            injection
                .end
                .map_or(0.0, |end| now.duration_since(end).as_secs_f64()),
            injection.idle_before,
        )
    }
}

/// Input events can land this long after the injecting call returns.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
const INJECTION_SLACK_SECONDS: f64 = 0.3;

/// When the last input (`raw` seconds ago) falls inside our latest
/// injection (which started `start_age` and ended `end_age` seconds ago),
/// the user has been idle since before it: `idle_before` plus the time
/// since it started. Otherwise the OS counter is the user's own.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
fn user_idle(raw: f64, start_age: f64, end_age: f64, idle_before: f64) -> f64 {
    let ours = raw.is_finite()
        && raw <= start_age + INJECTION_SLACK_SECONDS
        && raw + INJECTION_SLACK_SECONDS >= end_age;
    if ours {
        idle_before + start_age
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_input_does_not_count_as_user_activity() {
        // Our click ran 1.0–0.9 s ago; the last input was 0.95 s ago.
        assert_eq!(user_idle(0.95, 1.0, 0.9, 30.0), 31.0);
        // Input landing shortly after the call returned is still ours.
        assert_eq!(user_idle(0.7, 1.0, 0.9, 30.0), 31.0);
        // The user moved the mouse after our click.
        assert_eq!(user_idle(0.1, 1.0, 0.9, 30.0), 0.1);
        // The user was active before our click and nothing since.
        assert_eq!(user_idle(5.0, 1.0, 0.9, 0.5), 5.0);
        assert_eq!(user_idle(f64::INFINITY, 1.0, 0.9, 0.5), f64::INFINITY);
    }
}
