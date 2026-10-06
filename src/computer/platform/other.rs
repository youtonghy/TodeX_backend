//! Platforms without a native Computer Use implementation yet.

use xa11y::ElementData;

use super::{Display, Permissions, Typed};
use crate::computer::{keys::Chord, policy::Target};

pub(crate) fn adopt_own_permission_identity() {}

pub(crate) fn unsupported_reason() -> Option<String> {
    Some(format!(
        "Computer Use is not available on {} yet.",
        std::env::consts::OS
    ))
}

pub(crate) fn permissions() -> Permissions {
    Permissions::default()
}

pub(crate) fn request_permissions() -> Permissions {
    permissions()
}

pub(crate) fn idle_seconds() -> f64 {
    f64::INFINITY
}

pub(crate) fn app_identity(pid: u32) -> Target {
    Target {
        pid,
        ..Target::default()
    }
}

pub(crate) fn running_app(_identifier: &str) -> Option<Target> {
    None
}

pub(crate) fn installed_app(_identifier: &str) -> Option<Target> {
    None
}

pub(crate) fn open_app(identifier: &str) -> Result<(), String> {
    Err(format!("cannot open {identifier} on this platform"))
}

pub(crate) fn activate(pid: u32, _title: Option<&str>) -> Result<(), String> {
    Err(format!("cannot activate process {pid} on this platform"))
}

pub(crate) fn app_at(_x: f64, _y: f64) -> Option<u32> {
    None
}

pub(crate) fn displays() -> Vec<Display> {
    Vec::new()
}

pub(crate) fn is_secure(_element: &ElementData) -> bool {
    false
}

pub(crate) fn type_into_focused(_pid: u32, _text: &str, _confirmed: bool) -> Typed {
    Typed::Unsupported
}

pub(crate) fn post_chord(_pid: u32, _chord: &Chord) -> Result<bool, String> {
    Ok(false)
}
