//! Pure parts of the KDE Plasma Wayland layer (`kde_wayland.rs`): version
//! checks, the app-identity `.desktop` entry, KWin script results, window
//! matching and coordinate offsets, portal stream mapping, key codes,
//! screenshot pixel formats and idle time. Kept apart so they are
//! unit-tested on every host.

use serde::Deserialize;
use xa11y::{input::Key, MouseButton, Rect};

use super::Display;

/// The app id TodeX registers with the portals and KWin.
pub(super) const APP_ID: &str = "com.unbaked0692.todex.agentd";

/// The oldest Plasma whose portals and KWin APIs Computer Use relies on.
pub(super) const MIN_PLASMA: Version = Version(6, 6, 0);

/// Plasma releases before this one need the clipboard to type text that
/// keysyms cannot deliver (input methods, Chromium).
pub(super) const KEYSYM_TEXT_PLASMA: Version = Version(6, 8, 0);

/// Seconds without input before KWin reports the user idle; matches
/// `policy::USER_ACTIVE_SECONDS`.
pub(super) const IDLE_NOTIFY_SECONDS: f64 = 2.0;

/// Smooth-scroll distance of one wheel notch (KWin and libinput use 15).
pub(super) const SCROLL_PIXELS_PER_TICK: f64 = 15.0;

/// A Plasma / KWin release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version in KWin's `supportInformation` ("KWin version: 6.6.1") or
/// in `plasmashell --version` ("plasmashell 6.6.1").
pub(super) fn parse_version(text: &str) -> Option<Version> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let rest = line
            .strip_prefix("KWin version:")
            .or_else(|| line.strip_prefix("plasmashell"))?;
        let mut parts = rest
            .trim()
            .split(|c: char| !c.is_ascii_digit())
            .take(3)
            .map(|part| part.parse::<u32>().ok());
        let major = parts.next()??;
        let minor = parts.next().flatten().unwrap_or(0);
        let patch = parts.next().flatten().unwrap_or(0);
        Some(Version(major, minor, patch))
    })
}

/// Why this Plasma cannot run Computer Use.
pub(super) fn version_problem(version: Version) -> Option<String> {
    (version < MIN_PLASMA).then(|| {
        format!(
            "Computer Use on Wayland needs KDE Plasma {}.{} or later (this session runs KWin \
             {version}). Update Plasma, or log into the Plasma (X11) session.",
            MIN_PLASMA.0, MIN_PLASMA.1
        )
    })
}

/// The hidden `.desktop` entry that names this executable for KWin (whose
/// `ScreenShot2` interface only serves programs whose entry lists it in
/// `X-KDE-DBUS-Restricted-Interfaces`) and for the portals' app id.
pub(super) fn identity_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=TodeX Agent Daemon\n\
         Comment=Lets TodeX agents use this computer (Computer Use)\n\
         Exec={}\n\
         NoDisplay=true\n\
         Terminal=false\n\
         X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n",
        exec_quote(exe)
    )
}

/// Quotes a path for an `Exec` key: reserved characters inside double
/// quotes are backslash-escaped, `%` is doubled.
fn exec_quote(path: &str) -> String {
    let escaped: String = path
        .chars()
        .flat_map(|c| match c {
            '"' | '`' | '$' | '\\' => vec!['\\', c],
            '%' => vec!['%', '%'],
            c => vec![c],
        })
        .collect();
    if escaped
        .chars()
        .any(|c| c.is_whitespace() || "\"'`$\\<>~|&;*?#()".contains(c))
    {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

// ---- KWin scripts --------------------------------------------------------

/// A rectangle as KWin scripts report it (logical pixels).
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
pub(super) struct KwinRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl KwinRect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// A KWin window, as the snapshot script reports it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub(super) struct KwinWindow {
    pub internal_id: String,
    pub pid: u32,
    pub caption: String,
    pub resource_class: String,
    pub desktop_file_name: String,
    /// Frame (decorations included).
    pub frame_geometry: KwinRect,
    /// The client's own surface, without server-side decorations.
    pub client_geometry: KwinRect,
    /// An XWayland window: its AT-SPI coordinates are already global.
    pub x11_client: bool,
    pub popup_window: bool,
    pub minimized: bool,
    /// Shown on the current virtual desktop and not hidden.
    pub visible: bool,
}

/// A KWin output.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub(super) struct KwinScreen {
    pub name: String,
    pub geometry: KwinRect,
    pub device_pixel_ratio: f64,
}

/// Windows (bottom to top, as KWin stacks them) and outputs.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct KwinSnapshot {
    pub windows: Vec<KwinWindow>,
    pub screens: Vec<KwinScreen>,
}

/// The envelope every TodeX KWin script sends back through `callDBus`.
#[derive(Deserialize)]
struct ScriptReply {
    ok: bool,
    #[serde(default)]
    value: serde_json::Value,
    #[serde(default)]
    error: String,
}

/// A script's `value`, or its error.
pub(super) fn parse_script_reply(json: &str) -> Result<serde_json::Value, String> {
    let reply: ScriptReply = serde_json::from_str(json)
        .map_err(|error| format!("unreadable KWin script reply: {error}"))?;
    if reply.ok {
        Ok(reply.value)
    } else {
        Err(format!("KWin script failed: {}", reply.error))
    }
}

/// The top-most visible window whose frame contains a logical point.
pub(super) fn window_at(snapshot: &KwinSnapshot, x: f64, y: f64) -> Option<&KwinWindow> {
    snapshot
        .windows
        .iter()
        .rev()
        .find(|window| window.visible && !window.minimized && window.frame_geometry.contains(x, y))
}

/// KWin outputs as displays, in KWin's order (the first is primary).
pub(super) fn displays(snapshot: &KwinSnapshot) -> Vec<Display> {
    snapshot
        .screens
        .iter()
        .enumerate()
        .map(|(index, screen)| Display {
            index,
            x: screen.geometry.x,
            y: screen.geometry.y,
            width: screen.geometry.width,
            height: screen.geometry.height,
            scale: if screen.device_pixel_ratio > 0.0 {
                screen.device_pixel_ratio
            } else {
                1.0
            },
        })
        .collect()
}

/// The bounding box of all outputs (logical).
pub(super) fn desktop_bounds(snapshot: &KwinSnapshot) -> Option<KwinRect> {
    let mut screens = snapshot.screens.iter().map(|screen| screen.geometry);
    let first = screens.next()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (first.x + first.width, first.y + first.height);
    for rect in screens {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x + rect.width);
        bottom = bottom.max(rect.y + rect.height);
    }
    Some(KwinRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

/// The KWin window an AT-SPI top-level window belongs to: by process,
/// then title, then the closest client size.
pub(super) fn match_window<'a>(
    windows: &'a [KwinWindow],
    pid: u32,
    title: Option<&str>,
    bounds: Option<Rect>,
) -> Option<&'a KwinWindow> {
    let candidates: Vec<&KwinWindow> = windows
        .iter()
        .filter(|window| window.pid == pid && pid != 0)
        .collect();
    if candidates.len() <= 1 {
        return candidates.first().copied();
    }
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        let titled: Vec<&KwinWindow> = candidates
            .iter()
            .copied()
            .filter(|window| window.caption == title)
            .collect();
        if titled.len() == 1 {
            return titled.first().copied();
        }
    }
    let Some(bounds) = bounds else {
        // Without a size, the top-most one.
        return candidates.last().copied();
    };
    candidates.into_iter().rev().min_by(|a, b| {
        let distance = |window: &KwinWindow| {
            (window.client_geometry.width - f64::from(bounds.width)).abs()
                + (window.client_geometry.height - f64::from(bounds.height)).abs()
        };
        distance(a).total_cmp(&distance(b))
    })
}

/// What to add to AT-SPI coordinates of a window's subtree to make them
/// global. Wayland clients (Qt, GTK 4, Chromium) report extents relative to
/// their own surface, so their top-level frame reads about `(0, 0, w, h)`;
/// those are moved by the client area's position. XWayland windows, and
/// windows whose frame is already elsewhere, are left alone.
pub(super) fn window_offset(window: Option<&KwinWindow>, atspi_bounds: Option<Rect>) -> (i32, i32) {
    let Some(window) = window else {
        return (0, 0);
    };
    if window.x11_client {
        return (0, 0);
    }
    let surface_local =
        atspi_bounds.is_none_or(|bounds| bounds.x.abs() <= 1 && bounds.y.abs() <= 1);
    if !surface_local {
        return (0, 0);
    }
    (
        window.client_geometry.x.round() as i32,
        window.client_geometry.y.round() as i32,
    )
}

/// Moves a rectangle by an offset.
pub(super) fn offset_rect(rect: Rect, (dx, dy): (i32, i32)) -> Rect {
    Rect {
        x: rect.x.saturating_add(dx),
        y: rect.y.saturating_add(dy),
        ..rect
    }
}

// ---- RemoteDesktop input -------------------------------------------------

/// A ScreenCast stream of the RemoteDesktop session, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stream {
    pub node: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The stream showing a global logical point, and the point within it
/// (what `NotifyPointerMotionAbsolute` takes).
pub(super) fn stream_point(streams: &[Stream], x: i32, y: i32) -> Option<(u32, f64, f64)> {
    streams
        .iter()
        .find(|stream| {
            x >= stream.x
                && y >= stream.y
                && x < stream.x + stream.width
                && y < stream.y + stream.height
        })
        .map(|stream| {
            (
                stream.node,
                f64::from(x - stream.x),
                f64::from(y - stream.y),
            )
        })
}

pub(super) const BTN_LEFT: i32 = 0x110;
pub(super) const BTN_RIGHT: i32 = 0x111;
pub(super) const BTN_MIDDLE: i32 = 0x112;

pub(super) fn button_code(button: MouseButton) -> i32 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
    }
}

/// The evdev key code of a chord key on a US layout (`linux/input-event-codes.h`).
/// Other layouts map letters by position, as a physical keyboard would.
pub(super) fn evdev_code(key: &Key) -> Option<i32> {
    Some(match key {
        Key::Shift => 42,
        Key::Ctrl => 29,
        Key::Alt => 56,
        Key::Meta => 125,
        Key::Enter => 28,
        Key::Escape => 1,
        Key::Backspace => 14,
        Key::Tab => 15,
        Key::Space => 57,
        Key::Delete => 111,
        Key::Insert => 110,
        Key::ArrowUp => 103,
        Key::ArrowDown => 108,
        Key::ArrowLeft => 105,
        Key::ArrowRight => 106,
        Key::Home => 102,
        Key::End => 107,
        Key::PageUp => 104,
        Key::PageDown => 109,
        Key::F(n @ 1..=10) => 58 + i32::from(*n),
        Key::F(11) => 87,
        Key::F(12) => 88,
        Key::F(n @ 13..=24) => 183 + i32::from(*n - 13),
        Key::F(_) => return None,
        Key::Char(c) => return char_code(*c),
    })
}

fn char_code(c: char) -> Option<i32> {
    const ROWS: [(&str, i32); 4] = [
        ("1234567890-=", 2),
        ("qwertyuiop[]", 16),
        ("asdfghjkl;'", 30),
        ("zxcvbnm,./", 44),
    ];
    if c == '`' {
        return Some(41);
    }
    if c == '\\' {
        return Some(43);
    }
    if c == ' ' {
        return Some(57);
    }
    let lower = c.to_ascii_lowercase();
    ROWS.iter().find_map(|(row, first)| {
        row.chars()
            .position(|key| key == lower)
            .map(|index| first + index as i32)
    })
}

/// The X keysym that types a character: Latin-1 directly, control
/// characters as their keys, everything else in the Unicode plane.
pub(super) fn char_keysym(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        c if matches!(c as u32, 0x20..=0x7e | 0xa0..=0xff) => c as u32,
        c => 0x0100_0000 | c as u32,
    }
}

/// Executables of Chromium-based browsers and Electron apps, which ignore
/// synthetic keysyms outside the active layout on older Plasma.
const CHROMIUM_LIKE: &[&str] = &[
    "chrome",
    "chromium",
    "brave",
    "msedge",
    "vivaldi-bin",
    "opera",
    "electron",
    "code",
    "slack",
    "discord",
];

/// Whether `text` should be pasted rather than typed as keysyms: on
/// Plasma before 6.8, any non-ASCII text (input methods swallow it) and
/// anything typed into Chromium-based apps.
pub(super) fn needs_paste(version: Version, text: &str, app_exe: &str) -> bool {
    version < KEYSYM_TEXT_PLASMA
        && (!text
            .chars()
            .all(|c| (c.is_ascii() && !c.is_ascii_control()) || c == '\n' || c == '\t')
            || CHROMIUM_LIKE.contains(&app_exe))
}

// ---- Screenshots ---------------------------------------------------------

/// `QImage::Format` values KWin's `ScreenShot2` sends.
const FORMAT_RGB32: u32 = 4;
const FORMAT_ARGB32: u32 = 5;
const FORMAT_ARGB32_PREMULTIPLIED: u32 = 6;
const FORMAT_RGBX8888: u32 = 16;
const FORMAT_RGBA8888: u32 = 17;
const FORMAT_RGBA8888_PREMULTIPLIED: u32 = 18;

/// Converts a raw `QImage` buffer to opaque RGBA8 (screens have no
/// transparency, so premultiplication does not matter).
pub(super) fn raw_to_rgba(
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
) -> Result<Vec<u8>, String> {
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    if width == 0 || height == 0 || stride < width * 4 {
        return Err(format!(
            "bad screenshot geometry {width}x{height} stride {stride}"
        ));
    }
    let needed = stride * (height - 1) + width * 4;
    if data.len() < needed {
        return Err(format!(
            "short screenshot: {} of {needed} bytes",
            data.len()
        ));
    }
    // 32-bit ARGB formats are stored as B, G, R, A bytes on little-endian.
    let bgra = match format {
        FORMAT_RGB32 | FORMAT_ARGB32 | FORMAT_ARGB32_PREMULTIPLIED => true,
        FORMAT_RGBX8888 | FORMAT_RGBA8888 | FORMAT_RGBA8888_PREMULTIPLIED => false,
        other => return Err(format!("unsupported screenshot format {other}")),
    };
    let mut out = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let line = &data[row * stride..row * stride + width * 4];
        for pixel in line.as_chunks::<4>().0 {
            if bgra {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xff]);
            } else {
                out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
            }
        }
    }
    Ok(out)
}

/// Where a logical rectangle lies in a full-desktop image covering
/// `desktop` (logical) with `image_width` × `image_height` pixels, clipped
/// to the image: `(x, y, width, height)` in pixels.
pub(super) fn crop_rect(
    desktop: KwinRect,
    image_width: u32,
    image_height: u32,
    rect: Rect,
) -> Option<(u32, u32, u32, u32)> {
    if desktop.width <= 0.0 || desktop.height <= 0.0 {
        return None;
    }
    let scale_x = f64::from(image_width) / desktop.width;
    let scale_y = f64::from(image_height) / desktop.height;
    let left = ((f64::from(rect.x) - desktop.x) * scale_x).round().max(0.0);
    let top = ((f64::from(rect.y) - desktop.y) * scale_y).round().max(0.0);
    let right = ((f64::from(rect.x) + f64::from(rect.width) - desktop.x) * scale_x)
        .round()
        .min(f64::from(image_width));
    let bottom = ((f64::from(rect.y) + f64::from(rect.height) - desktop.y) * scale_y)
        .round()
        .min(f64::from(image_height));
    (right > left && bottom > top).then_some((
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

// ---- Idle time -------------------------------------------------------------

/// The latest `ext_idle_notification_v1` event and its age in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum IdleEvent {
    /// Nothing yet since the notification was created.
    None,
    Idled(f64),
    Resumed(f64),
}

/// Seconds since the last input as well as KWin's 2-second idle
/// notification can tell. After `idled` the user has been idle for the
/// timeout plus its age. After `resumed` the last input is at most the
/// age of that event; once the timeout has passed without a new `idled`,
/// input has continued, so 0. With no event yet nothing is known: 0.
pub(super) fn idle_seconds(event: IdleEvent) -> f64 {
    match event {
        IdleEvent::None => 0.0,
        IdleEvent::Idled(age) => IDLE_NOTIFY_SECONDS + age.max(0.0),
        IdleEvent::Resumed(age) if age < IDLE_NOTIFY_SECONDS => age.max(0.0),
        IdleEvent::Resumed(_) => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pid: u32, caption: &str, client: (f64, f64, f64, f64)) -> KwinWindow {
        KwinWindow {
            pid,
            caption: caption.to_owned(),
            frame_geometry: KwinRect {
                x: client.0,
                y: client.1 - 30.0,
                width: client.2,
                height: client.3 + 30.0,
            },
            client_geometry: KwinRect {
                x: client.0,
                y: client.1,
                width: client.2,
                height: client.3,
            },
            visible: true,
            ..KwinWindow::default()
        }
    }

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn versions_parse_from_kwin_and_plasmashell() {
        assert_eq!(
            parse_version("KWin Support Information:\n\nVersion\n=======\nKWin version: 6.6.1\nQt Version: 6.10.0\n"),
            Some(Version(6, 6, 1))
        );
        assert_eq!(parse_version("plasmashell 6.7.90"), Some(Version(6, 7, 90)));
        assert_eq!(parse_version("KWin version: 6.8"), Some(Version(6, 8, 0)));
        assert_eq!(parse_version("nothing here"), None);
        assert!(version_problem(Version(6, 5, 5)).unwrap().contains("6.6"));
        assert_eq!(version_problem(Version(6, 6, 0)), None);
        assert!(Version(6, 10, 0) > Version(6, 8, 0));
    }

    #[test]
    fn identity_entry_quotes_the_executable() {
        let entry = identity_entry("/opt/To deX/todex-agentd");
        assert!(entry.contains("Exec=\"/opt/To deX/todex-agentd\"\n"));
        assert!(entry.contains("X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2"));
        assert!(entry.contains("NoDisplay=true"));
        assert!(identity_entry("/usr/bin/todex-agentd").contains("Exec=/usr/bin/todex-agentd\n"));
        assert_eq!(exec_quote("/a/100%$"), "\"/a/100%%\\$\"");
    }

    #[test]
    fn script_replies_and_snapshots_parse() {
        let value = parse_script_reply(
            r#"{"ok":true,"value":{"windows":[{"internalId":"{1}","pid":7,"caption":"Kate",
            "frameGeometry":{"x":10,"y":20,"width":300,"height":200},
            "clientGeometry":{"x":10,"y":50,"width":300,"height":170},
            "x11Client":false,"visible":true}],
            "screens":[{"name":"DP-1","geometry":{"x":0,"y":0,"width":1920,"height":1080},"devicePixelRatio":1.5}]}}"#,
        )
        .unwrap();
        let snapshot: KwinSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(snapshot.windows[0].pid, 7);
        assert_eq!(snapshot.windows[0].client_geometry.y, 50.0);
        assert_eq!(displays(&snapshot)[0].scale, 1.5);
        assert!(parse_script_reply(r#"{"ok":false,"error":"boom"}"#)
            .unwrap_err()
            .contains("boom"));
        assert!(parse_script_reply("not json").is_err());
    }

    #[test]
    fn the_top_most_visible_window_is_hit() {
        let mut hidden = window(3, "hidden", (0.0, 0.0, 500.0, 500.0));
        hidden.visible = false;
        let snapshot = KwinSnapshot {
            windows: vec![
                window(1, "bottom", (0.0, 30.0, 500.0, 500.0)),
                window(2, "top", (100.0, 130.0, 100.0, 100.0)),
                hidden,
            ],
            screens: Vec::new(),
        };
        assert_eq!(window_at(&snapshot, 150.0, 150.0).unwrap().pid, 2);
        assert_eq!(window_at(&snapshot, 50.0, 50.0).unwrap().pid, 1);
        assert_eq!(window_at(&snapshot, 900.0, 50.0), None);
    }

    #[test]
    fn atspi_windows_match_by_pid_title_then_size() {
        let windows = [
            window(1, "Doc A", (0.0, 30.0, 800.0, 600.0)),
            window(1, "Doc B", (900.0, 30.0, 400.0, 300.0)),
            window(2, "Other", (0.0, 30.0, 800.0, 600.0)),
        ];
        assert_eq!(
            match_window(&windows, 2, None, None).unwrap().caption,
            "Other"
        );
        assert_eq!(
            match_window(&windows, 1, Some("Doc B"), None)
                .unwrap()
                .caption,
            "Doc B"
        );
        assert_eq!(
            match_window(&windows, 1, Some("?"), Some(rect(0, 0, 401, 299)))
                .unwrap()
                .caption,
            "Doc B"
        );
        assert_eq!(match_window(&windows, 9, None, None), None);
        assert_eq!(match_window(&windows, 0, None, None), None);
    }

    #[test]
    fn surface_local_windows_move_by_their_client_area() {
        let native = window(1, "a", (100.0, 80.0, 800.0, 600.0));
        assert_eq!(
            window_offset(Some(&native), Some(rect(0, 0, 800, 600))),
            (100, 80)
        );
        assert_eq!(window_offset(Some(&native), None), (100, 80));
        // Already global.
        assert_eq!(
            window_offset(Some(&native), Some(rect(100, 80, 800, 600))),
            (0, 0)
        );
        let mut xwayland = native.clone();
        xwayland.x11_client = true;
        assert_eq!(
            window_offset(Some(&xwayland), Some(rect(0, 0, 800, 600))),
            (0, 0)
        );
        assert_eq!(window_offset(None, Some(rect(0, 0, 1, 1))), (0, 0));
        assert_eq!(offset_rect(rect(5, 6, 7, 8), (10, 20)), rect(15, 26, 7, 8));
    }

    #[test]
    fn points_map_into_the_stream_that_shows_them() {
        let streams = [
            Stream {
                node: 40,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            Stream {
                node: 41,
                x: 1920,
                y: 0,
                width: 1280,
                height: 720,
            },
        ];
        assert_eq!(stream_point(&streams, 10, 20), Some((40, 10.0, 20.0)));
        assert_eq!(stream_point(&streams, 2000, 700), Some((41, 80.0, 700.0)));
        assert_eq!(stream_point(&streams, 2000, 800), None);
        assert_eq!(stream_point(&streams, -1, 0), None);
    }

    #[test]
    fn keys_map_to_evdev_codes_and_keysyms() {
        assert_eq!(evdev_code(&Key::Char('a')), Some(30));
        assert_eq!(evdev_code(&Key::Char('z')), Some(44));
        assert_eq!(evdev_code(&Key::Char('1')), Some(2));
        assert_eq!(evdev_code(&Key::Char('0')), Some(11));
        assert_eq!(evdev_code(&Key::Char('-')), Some(12));
        assert_eq!(evdev_code(&Key::Char('/')), Some(53));
        assert_eq!(evdev_code(&Key::Char('é')), None);
        assert_eq!(evdev_code(&Key::Ctrl), Some(29));
        assert_eq!(evdev_code(&Key::Escape), Some(1));
        assert_eq!(evdev_code(&Key::F(1)), Some(59));
        assert_eq!(evdev_code(&Key::F(10)), Some(68));
        assert_eq!(evdev_code(&Key::F(12)), Some(88));
        assert_eq!(evdev_code(&Key::F(13)), Some(183));
        assert_eq!(evdev_code(&Key::F(25)), None);
        assert_eq!(button_code(MouseButton::Left), 272);
        assert_eq!(button_code(MouseButton::Right), 273);
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('中'), 0x0100_4e2d);
        assert_eq!(char_keysym('\n'), 0xff0d);
    }

    #[test]
    fn older_plasma_pastes_what_keysyms_cannot_type() {
        let old = Version(6, 6, 2);
        assert!(!needs_paste(old, "hello world\n", "kate"));
        assert!(needs_paste(old, "你好", "kate"));
        assert!(needs_paste(old, "hello", "chromium"));
        assert!(!needs_paste(Version(6, 8, 0), "你好", "chromium"));
    }

    #[test]
    fn raw_images_convert_to_rgba() {
        // 2x1 BGRA with one byte of row padding per row... stride 12.
        let data = [1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9];
        assert_eq!(
            raw_to_rgba(&data, 2, 1, 12, FORMAT_ARGB32_PREMULTIPLIED).unwrap(),
            [3, 2, 1, 255, 6, 5, 4, 255]
        );
        assert_eq!(
            raw_to_rgba(&data, 2, 1, 12, FORMAT_RGBA8888).unwrap(),
            [1, 2, 3, 255, 4, 5, 6, 255]
        );
        assert!(raw_to_rgba(&data[..7], 2, 1, 8, FORMAT_RGB32).is_err());
        assert!(raw_to_rgba(&data, 2, 1, 4, FORMAT_RGB32).is_err());
        assert!(raw_to_rgba(&data, 2, 1, 8, 99).is_err());
    }

    #[test]
    fn regions_crop_from_a_scaled_desktop_image() {
        let desktop = KwinRect {
            x: 0.0,
            y: 0.0,
            width: 3200.0,
            height: 1080.0,
        };
        // A 2x image of a two-output desktop.
        assert_eq!(
            crop_rect(desktop, 6400, 2160, rect(1920, 100, 100, 50)),
            Some((3840, 200, 200, 100))
        );
        assert_eq!(
            crop_rect(desktop, 6400, 2160, rect(3150, 1000, 100, 100)),
            Some((6300, 2000, 100, 160))
        );
        assert_eq!(crop_rect(desktop, 6400, 2160, rect(4000, 0, 10, 10)), None);
        let snapshot = KwinSnapshot {
            windows: Vec::new(),
            screens: vec![
                KwinScreen {
                    geometry: KwinRect {
                        x: 0.0,
                        y: 0.0,
                        width: 1920.0,
                        height: 1080.0,
                    },
                    ..KwinScreen::default()
                },
                KwinScreen {
                    geometry: KwinRect {
                        x: 1920.0,
                        y: -200.0,
                        width: 1280.0,
                        height: 720.0,
                    },
                    ..KwinScreen::default()
                },
            ],
        };
        assert_eq!(
            desktop_bounds(&snapshot),
            Some(KwinRect {
                x: 0.0,
                y: -200.0,
                width: 3200.0,
                height: 1280.0
            })
        );
    }

    #[test]
    fn idle_time_follows_kwin_notifications() {
        assert_eq!(idle_seconds(IdleEvent::None), 0.0);
        assert_eq!(idle_seconds(IdleEvent::Idled(3.0)), 5.0);
        assert_eq!(idle_seconds(IdleEvent::Resumed(0.5)), 0.5);
        assert_eq!(idle_seconds(IdleEvent::Resumed(10.0)), 0.0);
    }
}
