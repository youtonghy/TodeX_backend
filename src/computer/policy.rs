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
    // The confirmation dialog tools (see `host_ui`): only the person at
    // the host may answer them.
    "kdialog",
    "org.kde.kdialog",
    "zenity",
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
    if is_protected(target, own_pid) {
        return Some(blocked(label));
    }
    if allowed_apps.iter().any(|app| app == &target.id) {
        return None;
    }
    if target.id.is_empty() {
        // Nothing identifiable there (the desktop, a menu bar extra, a
        // window whose owner cannot be read): the user decides each time.
        // An empty entry in `allowed_apps` is that one-off approval.
        return Some(PolicyFailure {
            code: "APP_CONFIRM",
            message: "the target of this action cannot be identified".to_owned(),
            detail: Some(json!({ "bundleId": "", "name": label, "unidentified": true })),
        });
    }
    Some(PolicyFailure {
        code: "APP_CONFIRM",
        message: format!("first action in {label} during this conversation"),
        detail: Some(json!({ "bundleId": target.id, "name": label })),
    })
}

/// Whether two identities are one app: the same app id (helper processes
/// and multi-process apps share it across pids) or the same process.
fn same_app(a: &Target, b: &Target) -> bool {
    (!a.id.is_empty() && a.id.eq_ignore_ascii_case(&b.id)) || (a.pid != 0 && a.pid == b.pid)
}

/// The failure to report for pointer input about to land on `landing` (the
/// app the hit test found at the point; `None` when the platform cannot
/// tell) after `target` was checked. Protected apps are always refused.
/// Where the agent chose the point (`agent_point`), the app under it must
/// still be `target`, else the screen changed since it looked
/// (`TARGET_CHANGED`). Elsewhere (the centre of a ref, a scroll's default
/// point, a drag's destination) another app is allowed only if the user
/// approved it, else `APP_CONFIRM` names it.
pub(crate) fn check_landing(
    landing: Option<Target>,
    target: &Target,
    allowed_apps: &[String],
    agent_point: bool,
    own_pid: u32,
) -> Option<PolicyFailure> {
    // No hit test here: the target check stands, as before.
    let landing = landing?;
    if is_protected(&landing, own_pid) {
        let label = if landing.name.is_empty() {
            &landing.id
        } else {
            &landing.name
        };
        return Some(blocked(label));
    }
    if same_app(&landing, target) {
        return None;
    }
    let target_known = !target.id.is_empty() || target.pid != 0;
    if agent_point && target_known {
        return Some(PolicyFailure {
            code: "TARGET_CHANGED",
            message: "a different app came to the front before the input was sent; observe again"
                .to_owned(),
            detail: None,
        });
    }
    check_target(&landing, allowed_apps, own_pid)
}

/// TodeX itself (`own_pid`) or an app no agent may touch or see.
pub(crate) fn is_protected(target: &Target, own_pid: u32) -> bool {
    (target.pid != 0 && target.pid == own_pid) || is_blocked(&target.id)
}

pub(crate) fn blocked(label: &str) -> PolicyFailure {
    PolicyFailure {
        code: "TARGET_BLOCKED",
        message: format!("{label} can never be controlled by an agent."),
        // The app's name is screen text; the MCP layer fences it.
        detail: Some(json!({ "label": label })),
    }
}

/// A window on screen, for hiding protected apps in screenshots.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StackWindow {
    pub pid: u32,
    /// Global coordinates, as capture rectangles use.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Opaque and in the normal layer: what it covers is not visible.
    pub covers: bool,
}

/// A rectangle (x, y, width, height) in global coordinates.
pub(crate) type Area = (f64, f64, f64, f64);

/// The visible parts of protected windows. `stack` runs front to back;
/// a protected window's area minus every covering window in front of it.
pub(crate) fn protected_areas(
    stack: &[StackWindow],
    mut protected: impl FnMut(u32) -> bool,
) -> Vec<Area> {
    let mut areas = Vec::new();
    for (index, window) in stack.iter().enumerate() {
        if window.width <= 0.0 || window.height <= 0.0 || !protected(window.pid) {
            continue;
        }
        let mut visible = vec![(window.x, window.y, window.width, window.height)];
        for front in stack[..index].iter().filter(|front| front.covers) {
            let cover = (front.x, front.y, front.width, front.height);
            visible = visible
                .into_iter()
                .flat_map(|area| subtract(area, cover))
                .collect();
        }
        areas.extend(visible);
    }
    areas
}

/// `area` minus `cover`, as up to four rectangles.
fn subtract(area: Area, cover: Area) -> Vec<Area> {
    let (ax, ay, aw, ah) = area;
    let (ar, ab) = (ax + aw, ay + ah);
    let (cx, cy) = (cover.0.max(ax), cover.1.max(ay));
    let (cr, cb) = ((cover.0 + cover.2).min(ar), (cover.1 + cover.3).min(ab));
    if cr <= cx || cb <= cy {
        return vec![area];
    }
    [
        (ax, ay, aw, cy - ay),
        (ax, cb, aw, ab - cb),
        (ax, cy, cx - ax, cb - cy),
        (cr, cy, ar - cr, cb - cy),
    ]
    .into_iter()
    .filter(|(_, _, w, h)| *w > 0.0 && *h > 0.0)
    .collect()
}

/// Paints `areas` (global coordinates) grey in an RGBA capture of
/// `capture` (global coordinates) that is `width` × `height` pixels.
/// Returns whether anything was painted.
pub(crate) fn paint_areas(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    capture: Area,
    areas: &[Area],
) -> bool {
    let (cx, cy, cw, ch) = capture;
    if cw <= 0.0 || ch <= 0.0 || pixels.len() < width as usize * height as usize * 4 {
        return false;
    }
    let (sx, sy) = (f64::from(width) / cw, f64::from(height) / ch);
    let mut painted = false;
    for (x, y, w, h) in areas {
        // Rounded outwards so no sliver of the window survives.
        let left = (((x - cx) * sx).floor().max(0.0) as u32).min(width);
        let top = (((y - cy) * sy).floor().max(0.0) as u32).min(height);
        let right = (((x + w - cx) * sx).ceil().max(0.0) as u32).min(width);
        let bottom = (((y + h - cy) * sy).ceil().max(0.0) as u32).min(height);
        for row in top..bottom {
            let start = (row as usize * width as usize + left as usize) * 4;
            let end = (row as usize * width as usize + right as usize) * 4;
            for pixel in pixels[start..end].as_chunks_mut::<4>().0 {
                *pixel = [96, 96, 96, 255];
                painted = true;
            }
        }
    }
    painted
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
    }

    #[test]
    fn an_unidentified_target_asks_every_time_but_the_daemon_stays_blocked() {
        let failure =
            check_target(&Target::default(), &["org.example.editor".to_owned()], 1).unwrap();
        assert_eq!(failure.code, "APP_CONFIRM");
        assert_eq!(failure.detail.as_ref().unwrap()["unidentified"], true);
        assert_eq!(failure.detail.as_ref().unwrap()["bundleId"], "");
        // The one-off approval for this action.
        assert_eq!(check_target(&Target::default(), &[String::new()], 1), None);
        // A window of the daemon without a bundle id is still refused.
        let own = Target {
            pid: 1,
            ..Target::default()
        };
        assert_eq!(
            check_target(&own, &[String::new()], 1).map(|f| f.code),
            Some("TARGET_BLOCKED")
        );
    }

    #[test]
    fn pointer_input_may_only_land_where_the_policy_allows() {
        let editor = target("org.example.editor", 50);
        let other = target("org.example.other", 60);
        let approved = vec!["org.example.other".to_owned()];
        let landing = |landing: Option<Target>, allowed: &[String], agent_point| {
            check_landing(landing, &editor, allowed, agent_point, 1).map(|f| f.code)
        };
        // No hit test: today's behaviour (the target check stands).
        assert_eq!(landing(None, &[], true), None);
        // Protected apps are refused whoever chose the point.
        for agent_point in [true, false] {
            assert_eq!(
                landing(Some(target(BLOCKED[0], 9)), &approved, agent_point),
                Some("TARGET_BLOCKED")
            );
            assert_eq!(
                landing(Some(target("org.example.renamed", 1)), &[], agent_point),
                Some("TARGET_BLOCKED")
            );
        }
        // The same app, by id (another process of it) or by process.
        assert_eq!(
            landing(Some(target("org.example.editor", 51)), &[], true),
            None
        );
        assert_eq!(
            landing(Some(target("ORG.EXAMPLE.EDITOR", 50)), &[], true),
            None
        );
        // The agent's own point now shows another app: the screen moved.
        assert_eq!(
            landing(Some(other.clone()), &approved, true),
            Some("TARGET_CHANGED")
        );
        assert_eq!(
            landing(Some(other.clone()), &[], true),
            Some("TARGET_CHANGED")
        );
        // Not the agent's point (a ref's centre, a drag's destination):
        // an approved app passes, another one is asked about.
        assert_eq!(landing(Some(other.clone()), &approved, false), None);
        let ask = check_landing(Some(other.clone()), &editor, &[], false, 1).unwrap();
        assert_eq!(ask.code, "APP_CONFIRM");
        assert_eq!(
            ask.detail,
            Some(json!({ "bundleId": "org.example.other", "name": "org.example.other" }))
        );
        // Nothing identifiable under it is asked about once.
        let nothing = check_landing(Some(Target::default()), &editor, &[], false, 1).unwrap();
        assert_eq!(nothing.detail.unwrap()["unidentified"], true);
        assert_eq!(
            check_landing(Some(Target::default()), &editor, &[String::new()], false, 1),
            None
        );
        // An unidentified target cannot tell the screen changed: the
        // landing app is simply checked like any other.
        assert_eq!(
            check_landing(Some(other.clone()), &Target::default(), &approved, true, 1),
            None
        );
        assert_eq!(
            check_landing(Some(other), &Target::default(), &[], true, 1).map(|f| f.code),
            Some("APP_CONFIRM")
        );
        // Same pid, no ids: the same process.
        let anonymous = Target {
            id: String::new(),
            name: String::new(),
            pid: 77,
        };
        assert_eq!(
            check_landing(Some(anonymous.clone()), &anonymous, &[], true, 1),
            None
        );
    }

    fn window(pid: u32, x: f64, y: f64, width: f64, height: f64, covers: bool) -> StackWindow {
        StackWindow {
            pid,
            x,
            y,
            width,
            height,
            covers,
        }
    }

    #[test]
    fn protected_windows_are_hidden_where_visible() {
        let protected = |pid| pid == 9;
        // An editor in front of the left half of a protected window.
        let stack = [
            window(5, 0.0, 0.0, 50.0, 100.0, true),
            window(9, 0.0, 0.0, 100.0, 100.0, true),
        ];
        assert_eq!(
            protected_areas(&stack, protected),
            vec![(50.0, 0.0, 50.0, 100.0)]
        );
        // Fully covered: nothing to hide.
        let stack = [
            window(5, 0.0, 0.0, 200.0, 200.0, true),
            window(9, 10.0, 10.0, 50.0, 50.0, true),
        ];
        assert!(protected_areas(&stack, protected).is_empty());
        // Translucent overlays do not count as covering; a protected
        // window in front hides its whole area.
        let stack = [
            window(9, 20.0, 20.0, 10.0, 10.0, false),
            window(6, 0.0, 0.0, 200.0, 200.0, false),
            window(9, 0.0, 0.0, 100.0, 100.0, true),
        ];
        let areas = protected_areas(&stack, protected);
        assert_eq!(areas.len(), 2);
        assert!(areas.contains(&(0.0, 0.0, 100.0, 100.0)));
        // A hole in the middle leaves four pieces.
        let stack = [
            window(5, 40.0, 40.0, 20.0, 20.0, true),
            window(9, 0.0, 0.0, 100.0, 100.0, true),
        ];
        let area: f64 = protected_areas(&stack, protected)
            .iter()
            .map(|(_, _, w, h)| w * h)
            .sum();
        assert_eq!(area, 100.0 * 100.0 - 20.0 * 20.0);
    }

    #[test]
    fn areas_are_painted_in_capture_pixels() {
        // A 4x2-pixel capture of the global rectangle (10, 10) 2x1 points.
        let mut pixels = vec![0u8; 4 * 2 * 4];
        assert!(paint_areas(
            &mut pixels,
            4,
            2,
            (10.0, 10.0, 2.0, 1.0),
            &[(11.0, 0.0, 50.0, 50.0)]
        ));
        let painted: Vec<bool> = pixels.chunks(4).map(|pixel| pixel[3] == 255).collect();
        assert_eq!(
            painted,
            [false, false, true, true, false, false, true, true]
        );
        // Outside the capture: nothing.
        assert!(!paint_areas(
            &mut pixels,
            4,
            2,
            (10.0, 10.0, 2.0, 1.0),
            &[(100.0, 100.0, 5.0, 5.0)]
        ));
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
