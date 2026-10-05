//! OS services Computer Use needs beyond what xa11y covers: permissions,
//! idle time, app identity, the window under a point, displays, launching
//! and activating apps. One module per desktop OS; every function has the
//! same signature on each.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;

#[cfg(not(target_os = "macos"))]
mod other;
#[cfg(not(target_os = "macos"))]
pub(crate) use other::*;

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
    Inserted,
    /// The focused element is a password field and the user has not
    /// confirmed.
    Secure,
    /// No insertable focused element; the caller falls back to keystrokes.
    Unsupported,
}
