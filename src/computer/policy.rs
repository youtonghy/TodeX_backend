//! What an agent may touch, checked before any Computer Use action runs.
//! The daemon decides who may use the screen ([`crate::agent_desktop`]);
//! this decides which apps on it may be controlled.

use serde_json::{json, Value};

/// The user's own pointer/keyboard activity this recent pauses pointer
/// actions.
pub(crate) const USER_ACTIVE_SECONDS: f64 = 2.0;

/// Apps an agent may never control, whatever the user approved: TodeX
/// itself (its permission prompts must stay out of reach), authentication
/// and credential stores, and system settings. Matched case-insensitively
/// against the app id (bundle id on macOS, executable name on Windows,
/// executable or desktop id on Linux).
#[cfg(target_os = "macos")]
const BLOCKED: &[&str] = &[
    "com.unbaked0692.todexdesktop",
    "com.github.electron",
    "com.apple.securityagent",
    "com.apple.localauthentication.uiagent",
    "com.apple.coreautha",
    "com.apple.keychainaccess",
    "com.apple.passwords",
    "com.apple.systempreferences",
    "com.apple.settings",
    "com.apple.loginwindow",
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.lastpass.lastpass",
    "com.dashlane.dashlanephonefinal",
    "in.sinew.enpass-desktop",
    "com.keepassxc.keepassxc",
];

#[cfg(target_os = "windows")]
const BLOCKED: &[&str] = &[
    "todex.exe",
    "electron.exe",
    "consent.exe",
    "credentialuibroker.exe",
    "logonui.exe",
    "lockapp.exe",
    "systemsettings.exe",
    "regedit.exe",
    "1password.exe",
    "bitwarden.exe",
    "keepass.exe",
    "keepassxc.exe",
    "lastpass.exe",
    "dashlane.exe",
    "enpass.exe",
];

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const BLOCKED: &[&str] = &[
    "todex",
    "todex-desktop",
    "electron",
    "kwalletmanager5",
    "kwalletmanager",
    "org.kde.kwalletmanager5",
    "ksecretd",
    "polkit-kde-authentication-agent-1",
    "systemsettings",
    "org.kde.systemsettings",
    "seahorse",
    "gnome-control-center",
    "1password",
    "bitwarden",
    "keepassxc",
    "enpass",
];

/// The app an action would touch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Target {
    /// Bundle id, executable name or desktop id; empty when nothing
    /// identifiable is there (the desktop, a menu bar extra).
    pub id: String,
    pub name: String,
    pub pid: u32,
}

/// A refused action, reported to the agent as `CODE: message`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PolicyFailure {
    pub code: &'static str,
    pub message: String,
    pub detail: Option<Value>,
}

/// Actions that always move the user's pointer.
const POINTER_ONLY: &[&str] = &["double_click", "hover", "drag"];

pub(crate) fn is_blocked(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    BLOCKED.iter().any(|blocked| *blocked == id)
}

/// Whether an action moves the pointer (coordinates, or pointer-only
/// actions).
pub(crate) fn uses_pointer(action: &str, has_point: bool) -> bool {
    has_point || POINTER_ONLY.contains(&action)
}

/// The failure to report for acting on `target`, or `None` when allowed.
/// `own_pid` blocks the daemon itself even if it ever owned a window.
pub(crate) fn check_target(
    target: &Target,
    allowed_apps: &[String],
    own_pid: u32,
) -> Option<PolicyFailure> {
    let label = if target.name.is_empty() {
        &target.id
    } else {
        &target.name
    };
    if (target.pid != 0 && target.pid == own_pid) || is_blocked(&target.id) {
        return Some(PolicyFailure {
            code: "TARGET_BLOCKED",
            message: format!("{label} can never be controlled by an agent."),
            detail: None,
        });
    }
    if target.id.is_empty() || allowed_apps.iter().any(|app| app == &target.id) {
        return None;
    }
    Some(PolicyFailure {
        code: "APP_CONFIRM",
        message: format!("first action in {label} during this conversation"),
        detail: Some(json!({ "bundleId": target.id, "name": label })),
    })
}

pub(crate) fn check_user_active(
    action: &str,
    has_point: bool,
    idle_seconds: f64,
) -> Option<PolicyFailure> {
    if !uses_pointer(action, has_point) || idle_seconds >= USER_ACTIVE_SECONDS {
        return None;
    }
    Some(PolicyFailure {
        code: "USER_ACTIVE",
        message: "The user is using the mouse or keyboard; retry in a few seconds.".to_owned(),
        detail: None,
    })
}

/// How a screenshot's pixels map to global screen points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShotMapping {
    pub origin_x: f64,
    pub origin_y: f64,
    pub points_per_pixel: f64,
    pub width: u32,
    pub height: u32,
}

/// Screenshot pixel → global point; `None` outside the screenshot.
pub(crate) fn to_screen_point(mapping: &ShotMapping, x: f64, y: f64) -> Option<(f64, f64)> {
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x > f64::from(mapping.width)
        || y > f64::from(mapping.height)
    {
        return None;
    }
    Some((
        mapping.origin_x + x * mapping.points_per_pixel,
        mapping.origin_y + y * mapping.points_per_pixel,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, pid: u32) -> Target {
        Target {
            id: id.to_owned(),
            name: id.to_owned(),
            pid,
        }
    }

    #[test]
    fn blocked_apps_and_the_daemon_are_refused_before_anything_else() {
        let blocked = BLOCKED[0];
        assert_eq!(
            check_target(&target(blocked, 9), &[blocked.to_owned()], 1).map(|f| f.code),
            Some("TARGET_BLOCKED")
        );
        assert_eq!(
            check_target(&target(&blocked.to_ascii_uppercase(), 9), &[], 1).map(|f| f.code),
            Some("TARGET_BLOCKED")
        );
        assert_eq!(
            check_target(&target("org.example.renamed", 1), &[], 1).map(|f| f.code),
            Some("TARGET_BLOCKED")
        );
    }

    #[test]
    fn the_first_action_in_an_app_asks_and_approved_apps_pass() {
        let editor = target("org.example.editor", 50);
        let failure = check_target(&editor, &[], 1).unwrap();
        assert_eq!(failure.code, "APP_CONFIRM");
        assert_eq!(
            failure.detail,
            Some(json!({ "bundleId": "org.example.editor", "name": "org.example.editor" }))
        );
        assert_eq!(
            check_target(&editor, &["org.example.editor".to_owned()], 1),
            None
        );
        assert_eq!(check_target(&Target::default(), &[], 1), None);
    }

    #[test]
    fn only_pointer_actions_wait_for_an_idle_user() {
        assert_eq!(
            check_user_active("click", true, 0.5).map(|f| f.code),
            Some("USER_ACTIVE")
        );
        assert_eq!(
            check_user_active("drag", false, 0.5).map(|f| f.code),
            Some("USER_ACTIVE")
        );
        assert_eq!(check_user_active("click", false, 0.5), None);
        assert_eq!(check_user_active("type", false, 0.0), None);
        assert_eq!(check_user_active("click", true, 3.0), None);
    }

    #[test]
    fn screenshot_pixels_map_to_global_points() {
        let mapping = ShotMapping {
            origin_x: 100.0,
            origin_y: 50.0,
            points_per_pixel: 0.5,
            width: 1280,
            height: 800,
        };
        assert_eq!(
            to_screen_point(&mapping, 200.0, 100.0),
            Some((200.0, 100.0))
        );
        assert_eq!(to_screen_point(&mapping, -1.0, 0.0), None);
        assert_eq!(to_screen_point(&mapping, 1281.0, 0.0), None);
        assert_eq!(to_screen_point(&mapping, f64::NAN, 0.0), None);
    }
}
