//! Linux (KDE Plasma is the tested desktop): AT-SPI over the session bus
//! and `.desktop` files for installed apps everywhere; on X11, EWMH window
//! lookups and activation through x11rb; on KDE Plasma 6.6+ Wayland, KWin
//! and the portals through [`super::kde_wayland`]. Other Wayland desktops
//! are reported as unsupported.
//!
//! Coordinates follow xa11y-linux: on X11, X pixels divided by its integer
//! `Xft.dpi` scale (1 unless the session uses integer HiDPI scaling); on
//! Wayland, logical pixels, with each window's AT-SPI subtree moved from
//! surface-local to global coordinates by [`window_offsets`].
//!
//! An app's identity is its executable's file name (`dolphin`,
//! `keepassxc`), read from `/proc`, never from inherited launch hints:
//! an app started from a terminal must not pass for the terminal.
//!
//! Limits: keystrokes go to the focused window, so `type` without an
//! element and `key` activate the app first (no background delivery), and
//! a password field focused that way is not detected before typing (only
//! fields an agent targets by ref are, through their AT-SPI role).

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use x11rb::{
    connection::Connection as _,
    protocol::{
        randr::ConnectionExt as _,
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        screensaver::ConnectionExt as _,
        xproto::{
            Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask,
            MapState, Window, CLIENT_MESSAGE_EVENT,
        },
    },
    rust_connection::RustConnection,
};
use xa11y::{ElementData, InputProvider, ScreenshotProvider};
use zbus::{
    blocking::{Connection as Bus, Proxy},
    proxy::CacheProperties,
};

use super::{
    kde_desktop::{match_window, offset_rect, version_problem, window_at, window_offset},
    kde_wayland,
    linux_desktop::{
        desktop_id, exe_name, names_app, parse_desktop_entry, parse_xft_dpi, scale_from_dpi,
        session_kind, session_problem, DesktopEntry, Session,
    },
    Display, Permissions, Typed,
};
use crate::computer::{keys::Chord, policy::Target};

type XResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// How long the installed-apps index stays fresh.
const DESKTOP_INDEX_TTL: Duration = Duration::from_secs(60);
/// EWMH source indication for pagers and taskbars, which window managers
/// (KWin included) obey without focus-stealing prevention.
const SOURCE_PAGER: u32 = 2;

pub(crate) fn adopt_own_permission_identity() {}

fn session() -> Session {
    session_kind(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var_os("DISPLAY").is_some(),
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
    )
}

/// Whether this is a KDE Plasma Wayland session (any version).
fn kde_wayland_session() -> bool {
    session() == Session::KdeWayland
}

/// Why Computer Use cannot run here: Wayland desktops other than KDE
/// Plasma 6.6 or later, and processes without a display.
pub(crate) fn unsupported_reason() -> Option<String> {
    let session = session();
    if let Some(problem) = session_problem(session) {
        return Some(problem);
    }
    if session != Session::KdeWayland {
        return None;
    }
    match kde_wayland::plasma_version() {
        Ok(version) => version_problem(version),
        Err(error) => Some(format!(
            "Computer Use could not read the KDE Plasma version from KWin ({error}). It needs \
             KDE Plasma 6.6 or later; start the TodeX backend inside the Plasma session."
        )),
    }
}

/// Accessibility means the AT-SPI bus is up and enabled: Qt, Chromium and
/// Electron apps only publish their trees while `org.a11y.Status.IsEnabled`
/// is true. X11 needs no screen-capture grant; KDE Wayland needs KWin's
/// screenshot authorization or the Screenshot portal.
pub(crate) fn permissions() -> Permissions {
    if kde_wayland_session() {
        // Idle notifications need two seconds before the first answer.
        kde_wayland::start_idle_watch();
        return Permissions {
            screen: kde_wayland::screen_capture_available(),
            accessibility: accessibility_enabled(),
        };
    }
    Permissions {
        screen: true,
        accessibility: accessibility_enabled(),
    }
}

/// What the settings screen should tell the user is missing.
pub(crate) fn missing_permissions_reason(permissions: Permissions) -> String {
    let mut missing = Vec::new();
    if !permissions.accessibility {
        missing.push(
            "accessibility (AT-SPI) is off for this session; Request access in TodeX Settings → \
             Computer Use turns it on (Chromium and Electron apps may need a restart)",
        );
    }
    if !permissions.screen {
        missing.push(
            "screen capture is unavailable: neither KWin's screenshot authorization for the \
             TodeX backend nor the Screenshot portal (xdg-desktop-portal-kde) is present; \
             Request access installs the authorization",
        );
    }
    format!("Computer Use needs: {}.", missing.join("; "))
}

/// Turns AT-SPI on for the session (as a screen reader would). Running Qt
/// apps follow the switch; Chromium and Electron apps may need a restart.
/// On KDE Wayland also installs the screenshot authorization and asks for
/// remote control of the pointer and keyboard (KDE's consent dialog), so
/// later sessions start without prompts.
pub(crate) fn request_permissions() -> Permissions {
    if let Some(proxy) = a11y_status_proxy() {
        if let Err(error) = proxy.set_property("IsEnabled", true) {
            eprintln!("todex-agentd: could not enable AT-SPI: {error}");
        }
    }
    if kde_wayland_session() {
        if let Err(error) = kde_wayland::ensure_identity() {
            eprintln!("todex-agentd: could not install the KDE screenshot authorization: {error}");
        }
        kde_wayland::begin_remote_session();
    }
    permissions()
}

fn accessibility_enabled() -> bool {
    let Some(proxy) = a11y_status_proxy() else {
        return false;
    };
    match proxy.get_property::<bool>("IsEnabled") {
        Ok(enabled) => enabled,
        // An AT-SPI without the Status interface publishes unconditionally;
        // the bus answering GetAddress is all there is to check.
        Err(_) => session_bus().is_some_and(|bus| {
            plain_proxy(&bus, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
                .and_then(|proxy| proxy.call::<_, _, String>("GetAddress", &()).ok())
                .is_some()
        }),
    }
}

fn a11y_status_proxy() -> Option<Proxy<'static>> {
    let bus = session_bus()?;
    plain_proxy(&bus, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status")
}

/// The session bus, connected once and reconnected after a failure.
pub(crate) fn session_bus() -> Option<Bus> {
    static BUS: Mutex<Option<Bus>> = Mutex::new(None);
    let mut bus = BUS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if bus.is_none() {
        *bus = Bus::session().ok();
    }
    bus.clone()
}

/// A proxy that does not prefetch properties (some AT-SPI and notification
/// services reject `GetAll`).
pub(crate) fn plain_proxy(
    bus: &Bus,
    destination: &'static str,
    path: &'static str,
    interface: &'static str,
) -> Option<Proxy<'static>> {
    zbus::blocking::proxy::Builder::<Proxy>::new(bus)
        .destination(destination)
        .ok()?
        .path(path)
        .ok()?
        .interface(interface)
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .ok()
}

/// Seconds since the last X input (the screensaver extension's counter;
/// on KDE Wayland, `ext-idle-notify-v1`), excluding input Computer Use
/// injected itself (XTest and the RemoteDesktop portal count there).
pub(crate) fn idle_seconds() -> f64 {
    if kde_wayland_session() {
        return super::injected::user_idle(kde_wayland::idle_seconds());
    }
    let raw = with_x11(|conn, root| {
        Ok(f64::from(
            conn.screensaver_query_info(root)?
                .reply()?
                .ms_since_user_input,
        ) / 1000.0)
    })
    .unwrap_or(f64::INFINITY);
    super::injected::user_idle(raw)
}

pub(crate) fn app_identity(pid: u32) -> Target {
    let Some(id) = process_exe(pid) else {
        return Target {
            pid,
            ..Target::default()
        };
    };
    let name = desktop_entry_for_exe(&id)
        .map(|entry| entry.name)
        .unwrap_or_else(|| id.clone());
    Target { id, name, pid }
}

/// A running app with a window, by executable, desktop id or name.
pub(crate) fn running_app(identifier: &str) -> Option<Target> {
    let mut seen = Vec::new();
    for pid in window_pids_top_down() {
        if seen.contains(&pid) {
            continue;
        }
        seen.push(pid);
        let app = app_identity(pid);
        let entry = desktop_entry_for_exe(&app.id);
        let desktop = entry.as_ref().map(|entry| entry.id.as_str()).unwrap_or("");
        if names_app(identifier, &app.id, desktop, &app.name) {
            return Some(app);
        }
    }
    None
}

/// The app `open_app` would launch, for the policy check before launching.
pub(crate) fn installed_app(identifier: &str) -> Option<Target> {
    if let Some(app) = running_app(identifier) {
        return Some(app);
    }
    match resolve_launch(identifier)? {
        Launch::Entry(entry) => Some(Target {
            id: entry.exe()?,
            name: entry.name,
            pid: 0,
        }),
        Launch::Binary(path) => {
            let id = exe_name(&path.to_string_lossy());
            Some(Target {
                name: id.clone(),
                id,
                pid: 0,
            })
        }
    }
}

pub(crate) fn open_app(identifier: &str) -> Result<(), String> {
    if let Some(app) = running_app(identifier) {
        return activate(app.pid, None);
    }
    // The same resolution `installed_app` checked against the policy.
    match resolve_launch(identifier).ok_or_else(|| format!("no app named {identifier}"))? {
        Launch::Entry(entry) => launch_entry(&entry),
        Launch::Binary(path) => spawn_detached(Command::new(&path))
            .map_err(|error| format!("could not start {}: {error}", path.display())),
    }
}

/// Processes owning windows, top-most window first.
fn window_pids_top_down() -> Vec<u32> {
    if kde_wayland_session() {
        return match kde_wayland::snapshot() {
            Ok(snapshot) => snapshot
                .windows
                .iter()
                .rev()
                .map(|window| window.pid)
                .filter(|pid| *pid != 0)
                .collect(),
            Err(error) => {
                eprintln!("todex-agentd: cannot list KWin windows: {error}");
                Vec::new()
            }
        };
    }
    client_windows_top_down()
        .unwrap_or_default()
        .into_iter()
        .filter_map(window_pid)
        .collect()
}

/// Raises and focuses `pid`'s window titled `title` (else its top-most)
/// through the window manager.
pub(crate) fn activate(pid: u32, title: Option<&str>) -> Result<(), String> {
    if kde_wayland_session() {
        return kde_wayland::activate(pid, title);
    }
    let windows: Vec<Window> = client_windows_top_down()
        .map_err(|error| format!("cannot list windows: {error}"))?
        .into_iter()
        .filter(|window| window_pid(*window) == Some(pid))
        .collect();
    let window = windows
        .iter()
        .copied()
        .find(|window| title.is_some() && window_title(*window).as_deref() == title)
        .or_else(|| windows.first().copied())
        .ok_or_else(|| format!("process {pid} has no window"))?;
    with_x11(|conn, root| {
        let active = atom(conn, b"_NET_ACTIVE_WINDOW")?;
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window,
            type_: active,
            data: ClientMessageData::from([SOURCE_PAGER, x11rb::CURRENT_TIME, 0, 0, 0]),
        };
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;
        Ok(())
    })
    .map_err(|error| format!("cannot activate process {pid}: {error}"))?;
    // Let the window manager act before input follows.
    std::thread::sleep(Duration::from_millis(80));
    Ok(())
}

/// The process owning the top-most visible window (frame included) at a
/// point.
pub(crate) fn app_at(x: f64, y: f64) -> Option<u32> {
    if kde_wayland_session() {
        let snapshot = kde_wayland::snapshot()
            .map_err(|error| eprintln!("todex-agentd: cannot list KWin windows: {error}"))
            .ok()?;
        return window_at(&snapshot, x, y)
            .map(|window| window.pid)
            .filter(|pid| *pid != 0);
    }
    let scale = coordinate_scale();
    let (px, py) = ((x * scale).round() as i32, (y * scale).round() as i32);
    for window in client_windows_top_down().ok()? {
        let Ok(Some(frame)) = window_frame(window) else {
            continue;
        };
        if px >= frame.0 && py >= frame.1 && px < frame.0 + frame.2 && py < frame.1 + frame.3 {
            // The top-most window here decides, identified or not.
            return window_pid(window);
        }
    }
    None
}

/// RandR monitors (else the whole root window), the primary first; KWin's
/// outputs on Wayland.
pub(crate) fn displays() -> Vec<Display> {
    if kde_wayland_session() {
        return kde_wayland::displays();
    }
    let scale = coordinate_scale();
    let monitors = with_x11(|conn, root| {
        let mut monitors: Vec<(bool, i32, i32, u32, u32)> = conn
            .randr_get_monitors(root, true)?
            .reply()?
            .monitors
            .into_iter()
            .map(|monitor| {
                (
                    monitor.primary,
                    i32::from(monitor.x),
                    i32::from(monitor.y),
                    u32::from(monitor.width),
                    u32::from(monitor.height),
                )
            })
            .collect();
        if monitors.is_empty() {
            let geometry = conn.get_geometry(root)?.reply()?;
            monitors.push((
                true,
                0,
                0,
                u32::from(geometry.width),
                u32::from(geometry.height),
            ));
        }
        Ok(monitors)
    })
    .unwrap_or_default();
    let mut monitors = monitors;
    monitors.sort_by_key(|(primary, x, y, _, _)| (!primary, *x, *y));
    monitors
        .into_iter()
        .enumerate()
        .map(|(index, (_, x, y, width, height))| Display {
            index,
            x: f64::from(x) / scale,
            y: f64::from(y) / scale,
            width: f64::from(width) / scale,
            height: f64::from(height) / scale,
            scale,
        })
        .collect()
}

/// AT-SPI reports password fields by role (the engine checks the same).
pub(crate) fn is_secure(element: &ElementData) -> bool {
    element.raw.get("atspi_role").and_then(|role| role.as_str()) == Some("password text")
}

/// No background insertion on Linux; the engine activates the app and
/// types with XTest.
pub(crate) fn type_into_focused(_pid: u32, _text: &str, _confirmed: bool) -> Typed {
    Typed::Unsupported
}

/// X11 cannot reliably deliver a chord to a background window; the engine
/// activates the app and uses XTest.
pub(crate) fn post_chord(_pid: u32, _chord: &Chord) -> Result<bool, String> {
    Ok(false)
}

// ---- Wayland hooks -----------------------------------------------------------

/// Offsets that move each AT-SPI top-level window's subtree into global
/// coordinates (see [`window_offset`]); zeros on X11, where AT-SPI already
/// reports global coordinates.
pub(crate) fn window_offsets(windows: &[ElementData]) -> Vec<(i32, i32)> {
    if !kde_wayland_session() || windows.is_empty() {
        return vec![(0, 0); windows.len()];
    }
    let snapshot = match kde_wayland::snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("todex-agentd: cannot place windows on screen: {error}");
            return vec![(0, 0); windows.len()];
        }
    };
    windows
        .iter()
        .map(|window| {
            let matched = window.pid.and_then(|pid| {
                match_window(
                    &snapshot.windows,
                    pid,
                    window.name.as_deref(),
                    window.bounds,
                )
            });
            window_offset(matched, window.bounds)
        })
        .collect()
}

/// Moves an element's bounds by a window offset.
pub(crate) fn globalize(element: &mut ElementData, offset: (i32, i32)) {
    if offset != (0, 0) {
        element.bounds = element.bounds.map(|bounds| offset_rect(bounds, offset));
    }
}

/// KDE Wayland input; `None` on X11, where xa11y's XTest input serves.
pub(crate) fn wayland_input() -> Option<std::sync::Arc<dyn InputProvider>> {
    kde_wayland_session().then(|| std::sync::Arc::new(kde_wayland::KdeInput) as _)
}

/// KDE Wayland screenshots; `None` on X11.
pub(crate) fn wayland_screenshots() -> Option<std::sync::Arc<dyn ScreenshotProvider>> {
    kde_wayland_session().then(|| std::sync::Arc::new(kde_wayland::KdeScreenshot) as _)
}

/// A Computer Use session started: on KDE Wayland, open the remote
/// control session now, so KDE's consent dialog (first time only) appears
/// while the person who granted access is still at the computer.
pub(crate) fn begin_session() {
    if kde_wayland_session() {
        kde_wayland::begin_remote_session();
    }
}

/// A Computer Use session ended: give the pointer and keyboard back.
pub(crate) fn end_session() {
    if kde_wayland_session() {
        kde_wayland::end_remote_session();
    }
}

/// Pastes text that keystrokes cannot type into `pid`'s focused field (KDE
/// Wayland before Plasma 6.8; see [`kde_wayland::paste_text`]). The caller
/// guarantees the field is not a password field.
pub(crate) fn paste_text(pid: u32, text: &str) -> Result<bool, String> {
    if !kde_wayland_session() {
        return Ok(false);
    }
    let exe = process_exe(pid).unwrap_or_default();
    kde_wayland::paste_text(pid, &exe, text)
}

// ---- X11 ----------------------------------------------------------------

/// Runs `work` on a shared X connection, reconnecting after any failure.
fn with_x11<T>(work: impl FnOnce(&RustConnection, Window) -> XResult<T>) -> XResult<T> {
    static X11: Mutex<Option<(RustConnection, Window)>> = Mutex::new(None);
    let mut x11 = X11.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if x11.is_none() {
        let (conn, screen) = RustConnection::connect(None)?;
        let root = conn
            .setup()
            .roots
            .get(screen)
            .ok_or("the X display has no such screen")?
            .root;
        *x11 = Some((conn, root));
    }
    let (conn, root) = x11.as_ref().expect("connected above");
    let result = work(conn, *root);
    if result.is_err() {
        *x11 = None;
    }
    result
}

fn atom(conn: &RustConnection, name: &[u8]) -> XResult<Atom> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn cardinals(conn: &RustConnection, window: Window, property: &[u8]) -> XResult<Vec<u32>> {
    let property = atom(conn, property)?;
    let reply = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, 1 << 16)?
        .reply()?;
    Ok(reply
        .value32()
        .map(|values| values.collect())
        .unwrap_or_default())
}

/// `_NET_WM_NAME`, else the legacy `WM_NAME`.
fn window_title(window: Window) -> Option<String> {
    with_x11(|conn, _| {
        for property in [&b"_NET_WM_NAME"[..], b"WM_NAME"] {
            let property = atom(conn, property)?;
            let reply = conn
                .get_property(false, window, property, AtomEnum::ANY, 0, 1 << 12)?
                .reply()?;
            if !reply.value.is_empty() {
                return Ok(Some(String::from_utf8_lossy(&reply.value).into_owned()));
            }
        }
        Ok(None)
    })
    .ok()
    .flatten()
}

/// Managed windows, top-most first.
fn client_windows_top_down() -> XResult<Vec<Window>> {
    with_x11(|conn, root| {
        let mut windows = cardinals(conn, root, b"_NET_CLIENT_LIST_STACKING")?;
        if windows.is_empty() {
            windows = cardinals(conn, root, b"_NET_CLIENT_LIST")?;
        }
        windows.reverse();
        Ok(windows)
    })
}

/// `_NET_WM_PID`, else the owning client's pid from the X-Resource
/// extension.
fn window_pid(window: Window) -> Option<u32> {
    with_x11(|conn, _| {
        if let Some(pid) = cardinals(conn, window, b"_NET_WM_PID")?.first() {
            return Ok(Some(*pid));
        }
        let spec = ClientIdSpec {
            client: window,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        };
        let reply = conn.res_query_client_ids(&[spec])?.reply()?;
        Ok(reply
            .ids
            .iter()
            .find(|id| u32::from(id.spec.mask) & u32::from(ClientIdMask::LOCAL_CLIENT_PID) != 0)
            .and_then(|id| id.value.first().copied()))
    })
    .ok()
    .flatten()
    .filter(|pid| *pid != 0)
}

/// A viewable window's frame on the root (x, y, width, height), its
/// decorations (`_NET_FRAME_EXTENTS`) included; `None` when not viewable
/// (minimized, another virtual desktop).
fn window_frame(window: Window) -> XResult<Option<(i32, i32, i32, i32)>> {
    with_x11(|conn, root| {
        if conn.get_window_attributes(window)?.reply()?.map_state != MapState::VIEWABLE {
            return Ok(None);
        }
        let geometry = conn.get_geometry(window)?.reply()?;
        let origin = conn.translate_coordinates(window, root, 0, 0)?.reply()?;
        let extents = cardinals(conn, window, b"_NET_FRAME_EXTENTS")?;
        let extent = |index: usize| extents.get(index).map_or(0, |value| *value as i32);
        let (left, right, top, bottom) = (extent(0), extent(1), extent(2), extent(3));
        Ok(Some((
            i32::from(origin.dst_x) - left,
            i32::from(origin.dst_y) - top,
            i32::from(geometry.width) + left + right,
            i32::from(geometry.height) + top + bottom,
        )))
    })
}

/// xa11y-linux's coordinate scale on X11 (integer `Xft.dpi` / 96).
fn coordinate_scale() -> f64 {
    static SCALE: OnceLock<f64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        with_x11(|conn, root| {
            let reply = conn
                .get_property(
                    false,
                    root,
                    AtomEnum::RESOURCE_MANAGER,
                    AtomEnum::STRING,
                    0,
                    1 << 18,
                )?
                .reply()?;
            Ok(parse_xft_dpi(&String::from_utf8_lossy(&reply.value)))
        })
        .ok()
        .flatten()
        .map_or(1.0, scale_from_dpi)
    })
}

// ---- Processes and installed apps ------------------------------------------

/// The lowercase executable file name of `pid`.
fn process_exe(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        let name = exe_name(&path.to_string_lossy());
        // A replaced binary reads as "name (deleted)".
        return Some(name.trim_end_matches(" (deleted)").to_owned());
    }
    // Another user's process: its command name (truncated to 15 bytes).
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|comm| comm.trim().to_lowercase())
        .filter(|comm| !comm.is_empty())
}

/// The first application `.desktop` entry matching `wanted`. Entries come
/// from the XDG data dirs, earlier dirs winning, re-read at most every
/// [`DESKTOP_INDEX_TTL`].
fn find_desktop_entry(wanted: impl Fn(&DesktopEntry) -> bool) -> Option<DesktopEntry> {
    static INDEX: Mutex<Option<(Instant, Vec<DesktopEntry>)>> = Mutex::new(None);
    let mut index = INDEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((read_at, entries)) = index.as_ref() {
        if read_at.elapsed() < DESKTOP_INDEX_TTL {
            return entries.iter().find(|entry| wanted(entry)).cloned();
        }
    }
    let mut entries: Vec<DesktopEntry> = Vec::new();
    for dir in application_dirs() {
        let mut files = Vec::new();
        collect_desktop_files(&dir, &dir, &mut files, 0);
        for (relative, path) in files {
            let Some(id) = desktop_id(&relative) else {
                continue;
            };
            if entries.iter().any(|entry| entry.id == id) {
                continue;
            }
            if let Some(entry) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|contents| parse_desktop_entry(&id, &contents))
            {
                entries.push(entry);
            }
        }
    }
    let found = entries.iter().find(|entry| wanted(entry)).cloned();
    *index = Some((Instant::now(), entries));
    found
}

fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    home.into_iter()
        .chain(system.split(':').map(PathBuf::from))
        .map(|dir| dir.join("applications"))
        .collect()
}

fn collect_desktop_files(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, depth: usize) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        let path = item.path();
        if path.is_dir() {
            if depth < 3 {
                collect_desktop_files(base, &path, out, depth + 1);
            }
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            if let Ok(relative) = path.strip_prefix(base) {
                out.push((relative.to_string_lossy().into_owned(), path));
            }
        }
    }
}

fn desktop_entry_for_exe(exe: &str) -> Option<DesktopEntry> {
    find_desktop_entry(|entry| entry.exe().as_deref() == Some(exe))
}

enum Launch {
    Entry(DesktopEntry),
    Binary(PathBuf),
}

/// A `.desktop` entry by desktop id, name or executable; else an
/// executable on `PATH` by that name.
fn resolve_launch(identifier: &str) -> Option<Launch> {
    let identifier = identifier.trim();
    if identifier.is_empty() || identifier.contains('/') {
        return None;
    }
    let entry = find_desktop_entry(|entry| {
        names_app(
            identifier,
            entry.exe().as_deref().unwrap_or(""),
            &entry.id,
            &entry.name,
        )
    });
    if let Some(entry) = entry.filter(|entry| !entry.argv.is_empty()) {
        return Some(Launch::Entry(entry));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(identifier))
        .find(|path| path.is_file())
        .map(Launch::Binary)
}

/// Starts an entry as the desktop would (KDE's launcher, then GTK's), so
/// it gets its own scope and startup notification; else runs its `Exec`.
fn launch_entry(entry: &DesktopEntry) -> Result<(), String> {
    // kioclient wants the file; ids from sub-directories (`kde-…`) are
    // only found by gtk-launch or the Exec fallback.
    let desktop_file = application_dirs()
        .into_iter()
        .map(|dir| dir.join(format!("{}.desktop", entry.id)))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned());
    let mut launchers: Vec<(&str, Vec<&str>)> = Vec::new();
    if let Some(file) = desktop_file.as_deref() {
        launchers.push(("kioclient", vec!["exec", file]));
        launchers.push(("kioclient5", vec!["exec", file]));
    }
    launchers.push(("gtk-launch", vec![entry.id.as_str()]));
    for (program, args) in launchers {
        let status = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok_and(|status| status.success()) {
            return Ok(());
        }
    }
    let (program, args) = entry
        .argv
        .split_first()
        .ok_or_else(|| format!("{} has no command", entry.name))?;
    let mut command = Command::new(program);
    command.args(args);
    spawn_detached(command).map_err(|error| format!("could not start {}: {error}", entry.name))
}

/// Starts a program without tying it to the daemon's stdio, and reaps it
/// when it exits.
fn spawn_detached(mut command: Command) -> std::io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::Builder::new()
        .name("todex-app-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}
