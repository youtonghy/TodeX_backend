//! macOS: TCC permissions, CoreGraphics and AppKit lookups, and key chords
//! posted to one app's process without activating it.

use std::{
    ffi::{c_void, CString},
    process::Command,
    time::Duration,
};

use core_foundation::{
    array::CFArray,
    base::{CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};
use xa11y::{input::Key, ElementData};

use super::{Display, Permission, Permissions, Typed};
use crate::computer::{
    keys::Chord,
    policy::{StackWindow, Target},
};

type AXUIElementRef = *const c_void;
type CGEventRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relative_to: u32,
    ) -> core_foundation::array::CFArrayRef;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> CGEventRef;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventPostToPid(pid: i32, event: CGEventRef);
}

unsafe extern "C" {
    fn responsibility_spawnattrs_setdisclaim(
        attrs: *mut libc::posix_spawnattr_t,
        disclaim: libc::c_int,
    ) -> libc::c_int;
    static environ: *const *mut libc::c_char;
}

const AX_SUCCESS: i32 = 0;
const HID_SYSTEM_STATE: i32 = 1;
const ANY_INPUT_EVENT: u32 = !0;
const ON_SCREEN_ONLY: u32 = 1 << 0;
const EXCLUDE_DESKTOP: u32 = 1 << 4;
const POSIX_SPAWN_SETEXEC: libc::c_short = 0x0040;
/// Set once the daemon runs as its own TCC-responsible process.
const OWN_IDENTITY_ENV: &str = "TODEX_COMPUTER_OWN_TCC";
/// macOS 14 Sonoma.
const MIN_MAJOR: u32 = 14;

/// Re-executes the daemon in place (same pid) as its own TCC-responsible
/// process, so Screen Recording and Accessibility are asked for and granted
/// to `todex-agentd` itself rather than the terminal that started it. Call
/// first thing in `main`, before threads exist. Failure only means the
/// grants follow the launching app; it is logged, never fatal.
pub(crate) fn adopt_own_permission_identity() {
    if std::env::var_os(OWN_IDENTITY_ENV).is_some() {
        // Already re-executed. Children (an update restart, providers)
        // must not inherit the marker: each daemon takes its own identity.
        // SAFETY: no other thread exists yet (see above).
        unsafe { std::env::remove_var(OWN_IDENTITY_ENV) };
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe) = exe.to_str().and_then(|path| CString::new(path).ok()) else {
        return;
    };
    let args: Vec<CString> = std::env::args_os()
        .filter_map(|arg| arg.into_string().ok())
        .filter_map(|arg| CString::new(arg).ok())
        .collect();
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
    argv.push(std::ptr::null_mut());
    // SAFETY: setenv happens before any thread is spawned (see above).
    unsafe { std::env::set_var(OWN_IDENTITY_ENV, "1") };
    // SAFETY: attrs is initialised before use and destroyed on failure;
    // argv and the environment are NUL-terminated arrays that outlive the
    // call; on success SETEXEC replaces this process image.
    let rc = unsafe {
        let mut attrs: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawnattr_init(&mut attrs);
        libc::posix_spawnattr_setflags(&mut attrs, POSIX_SPAWN_SETEXEC);
        responsibility_spawnattrs_setdisclaim(&mut attrs, 1);
        let rc = libc::posix_spawn(
            std::ptr::null_mut(),
            exe.as_ptr(),
            std::ptr::null(),
            &attrs,
            argv.as_ptr(),
            environ,
        );
        libc::posix_spawnattr_destroy(&mut attrs);
        rc
    };
    eprintln!("todex-agentd: could not take its own permission identity (posix_spawn {rc}); Screen Recording and Accessibility follow the launching app");
}

/// Why Computer Use cannot run here, if it cannot.
pub(crate) fn unsupported_reason() -> Option<String> {
    let version = os_version();
    let major = version
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .unwrap_or(0);
    (major < MIN_MAJOR).then(|| {
        format!("Computer Use needs macOS {MIN_MAJOR} or later (this host runs {version}).")
    })
}

fn os_version() -> String {
    let name = CString::new("kern.osproductversion").expect("static name");
    let mut buffer = [0u8; 64];
    let mut size = buffer.len();
    // SAFETY: buffer/size describe a writable region of `size` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buffer[..size.saturating_sub(1)]).into_owned()
}

pub(crate) fn permissions() -> Permissions {
    // SAFETY: plain preflight queries without side effects.
    unsafe {
        Permissions {
            screen: CGPreflightScreenCaptureAccess(),
            accessibility: AXIsProcessTrusted(),
        }
    }
}

/// Shows the system prompt for each requested permission that is missing.
/// macOS asks once per identity: after an answer, or a dismissed prompt,
/// the request calls do nothing, so a permission still missing afterwards
/// also opens its pane in System Settings.
pub(crate) fn request_permissions(which: Option<Permission>) -> Permissions {
    let current = permissions();
    // SAFETY: the options dictionary lives across the call.
    unsafe {
        if Permission::Accessibility.requested(which) && !current.accessibility {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
        }
        if Permission::Screen.requested(which) && !current.screen {
            CGRequestScreenCaptureAccess();
        }
    }
    let after = permissions();
    if Permission::Accessibility.requested(which) && !after.accessibility {
        open_privacy_pane("Privacy_Accessibility");
    }
    if Permission::Screen.requested(which) && !after.screen {
        open_privacy_pane("Privacy_ScreenCapture");
    }
    after
}

fn open_privacy_pane(anchor: &str) {
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{anchor}");
    match Command::new("open").arg(url).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("todex-agentd: open System Settings ({anchor}) failed: {status}"),
        Err(error) => eprintln!("todex-agentd: could not open System Settings ({anchor}): {error}"),
    }
}

/// Seconds since the user last touched the mouse or keyboard. Events this
/// process posts are not hardware input and do not count.
pub(crate) fn idle_seconds() -> f64 {
    // SAFETY: a pure query.
    unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT) }
}

pub(crate) fn app_identity(pid: u32) -> Target {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return Target::default();
    };
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid_i32);
    Target {
        id: app
            .as_ref()
            .and_then(|app| app.bundleIdentifier())
            .map(|id| id.to_string())
            .unwrap_or_default(),
        name: app
            .as_ref()
            .and_then(|app| app.localizedName())
            .map(|name| name.to_string())
            .unwrap_or_default(),
        pid,
    }
}

/// A running app by bundle id or name (case-insensitive, `.app` optional).
pub(crate) fn running_app(identifier: &str) -> Option<Target> {
    let wanted = identifier.trim_end_matches(".app").to_lowercase();
    let apps = NSWorkspace::sharedWorkspace().runningApplications();
    apps.iter().find_map(|app| {
        let id = app
            .bundleIdentifier()
            .map(|id| id.to_string())
            .unwrap_or_default();
        let name = app
            .localizedName()
            .map(|name| name.to_string())
            .unwrap_or_default();
        (id.to_lowercase() == wanted || name.to_lowercase() == wanted).then(|| Target {
            id,
            name,
            pid: u32::try_from(app.processIdentifier()).unwrap_or(0),
        })
    })
}

/// The app `open_app` would launch, for the policy check before launching.
pub(crate) fn installed_app(identifier: &str) -> Option<Target> {
    if let Some(app) = running_app(identifier) {
        return Some(app);
    }
    let output = Command::new("mdfind")
        .arg(format!(
            "kMDItemContentType == 'com.apple.application-bundle' && (kMDItemCFBundleIdentifier == '{0}'c || kMDItemFSName == '{0}.app'c || kMDItemDisplayName == '{0}'c)",
            identifier.trim_end_matches(".app").replace(['\'', '\\'], "")
        ))
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .to_owned();
    let plist = Command::new("defaults")
        .args([
            "read",
            &format!("{path}/Contents/Info"),
            "CFBundleIdentifier",
        ])
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&plist.stdout).trim().to_owned();
    let name = std::path::Path::new(&path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    (!id.is_empty()).then_some(Target { id, name, pid: 0 })
}

pub(crate) fn open_app(identifier: &str) -> Result<(), String> {
    // The same resolution `installed_app` checked against the policy, then
    // launched by that bundle id (not by name, which `open -a` may resolve
    // to another app).
    let app = installed_app(identifier).ok_or_else(|| format!("no app named {identifier}"))?;
    if app.pid != 0 {
        return activate(app.pid, None);
    }
    let status = Command::new("open")
        .args(["-b", &app.id])
        .status()
        .map_err(|error| format!("could not run open: {error}"))?;
    if !status.success() {
        return Err(format!("no app named {identifier}"));
    }
    Ok(())
}

/// Brings the app forward; the window itself was raised through AX
/// (`AXRaise`), so `_title` is not needed here.
pub(crate) fn activate(pid: u32, _title: Option<&str>) -> Result<(), String> {
    let app = i32::try_from(pid)
        .ok()
        .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        .ok_or_else(|| format!("process {pid} is not a running app"))?;
    #[allow(deprecated)]
    app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    Ok(())
}

/// An on-screen window from the window server, front to back.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CgWindow {
    layer: i64,
    pid: u32,
    alpha: f64,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl CgWindow {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

/// On-screen windows, front to back, without the desktop and the pointer
/// marker; `None` when the window server does not answer.
fn on_screen_windows() -> Option<Vec<CgWindow>> {
    // SAFETY: the returned array follows the create rule.
    let windows: CFArray<CFDictionary<CFString, CFType>> = unsafe {
        let array = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if array.is_null() {
            return None;
        }
        CFArray::wrap_under_create_rule(array)
    };
    let number = |dict: &CFDictionary<CFString, CFType>, key: &'static str| {
        dict.find(CFString::from_static_string(key))
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|number| number.to_f64())
    };
    let marker = crate::computer::host_ui::marker_window();
    let mut found = Vec::new();
    for window in windows.iter() {
        let Some(bounds) = window
            .find(CFString::from_static_string("kCGWindowBounds"))
            .and_then(|value| value.downcast::<CFDictionary>())
        else {
            continue;
        };
        // SAFETY: kCGWindowBounds is a dictionary of CFString → CFNumber.
        let bounds: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef()) };
        let (Some(x), Some(y), Some(width), Some(height)) = (
            number(&bounds, "X"),
            number(&bounds, "Y"),
            number(&bounds, "Width"),
            number(&bounds, "Height"),
        ) else {
            continue;
        };
        let window_number = number(&window, "kCGWindowNumber").map_or(0, |n| n as u32);
        if marker.is_some_and(|marker| marker == window_number) {
            continue;
        }
        found.push(CgWindow {
            layer: number(&window, "kCGWindowLayer").map_or(0, |layer| layer as i64),
            pid: number(&window, "kCGWindowOwnerPID").map_or(0, |pid| pid as u32),
            alpha: number(&window, "kCGWindowAlpha").unwrap_or(1.0),
            x,
            y,
            width,
            height,
        });
    }
    Some(found)
}

/// The process that receives a click at a global point: the front-most
/// normal-layer window, unless a window of a protected process (TodeX's
/// own panels and dialogs sit at the status level, the system's
/// authentication dialogs higher still) lies above it there. Other
/// windows above the normal layer (the menu bar, the Dock, overlays) are
/// skipped as before.
fn hit(windows: &[CgWindow], x: f64, y: f64, protected: impl Fn(u32) -> bool) -> Option<u32> {
    windows
        .iter()
        .filter(|window| window.contains(x, y))
        .find(|window| window.layer == 0 || protected(window.pid))
        .map(|window| window.pid)
}

fn is_protected_pid(pid: u32) -> bool {
    pid != 0
        && (pid == std::process::id() || crate::computer::policy::is_blocked(&app_identity(pid).id))
}

/// The process owning the window an input at a global point lands in.
pub(crate) fn app_at(x: f64, y: f64) -> Option<u32> {
    hit(&on_screen_windows()?, x, y, is_protected_pid).filter(|pid| *pid != 0)
}

/// On-screen windows front to back, for hiding protected apps in
/// screenshots. Only opaque normal-layer windows count as covering.
pub(crate) fn window_stack() -> Option<Vec<StackWindow>> {
    Some(
        on_screen_windows()?
            .into_iter()
            .map(|window| StackWindow {
                pid: window.pid,
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
                covers: window.layer == 0 && window.alpha >= 1.0,
            })
            .collect(),
    )
}

pub(crate) fn displays() -> Vec<Display> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    // SAFETY: ids has room for 16 entries.
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != 0 {
        return Vec::new();
    }
    ids.iter()
        .take(count as usize)
        .enumerate()
        .map(|(index, id)| {
            // SAFETY: id is an active display.
            let (bounds, pixels) = unsafe { (CGDisplayBounds(*id), CGDisplayPixelsWide(*id)) };
            Display {
                index,
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
                scale: if bounds.size.width > 0.0 {
                    pixels as f64 / bounds.size.width
                } else {
                    1.0
                },
            }
        })
        .collect()
}

pub(crate) fn is_secure(element: &ElementData) -> bool {
    element
        .raw
        .get("ax_subrole")
        .and_then(|value| value.as_str())
        == Some("AXSecureTextField")
}

/// Inserts text into `pid`'s focused element without keystrokes, so input
/// methods cannot rewrite it and the app stays in the background.
pub(crate) fn type_into_focused(pid: u32, text: &str, confirmed: bool) -> Typed {
    let Ok(pid) = i32::try_from(pid) else {
        return Typed::Unsupported;
    };
    // SAFETY: every AX object we get under the create rule is wrapped and
    // released by its CFType.
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Typed::Unsupported;
        }
        let app = CFType::wrap_under_create_rule(app);
        let mut focused: CFTypeRef = std::ptr::null();
        let attribute = CFString::from_static_string("AXFocusedUIElement");
        if AXUIElementCopyAttributeValue(
            app.as_CFTypeRef(),
            attribute.as_concrete_TypeRef(),
            &mut focused,
        ) != AX_SUCCESS
            || focused.is_null()
        {
            return Typed::Unsupported;
        }
        let focused = CFType::wrap_under_create_rule(focused);
        let mut subrole: CFTypeRef = std::ptr::null();
        let subrole_attribute = CFString::from_static_string("AXSubrole");
        let secure = AXUIElementCopyAttributeValue(
            focused.as_CFTypeRef(),
            subrole_attribute.as_concrete_TypeRef(),
            &mut subrole,
        ) == AX_SUCCESS
            && !subrole.is_null()
            && CFType::wrap_under_create_rule(subrole)
                .downcast::<CFString>()
                .is_some_and(|subrole| subrole == "AXSecureTextField");
        if secure && !confirmed {
            return Typed::Secure;
        }
        let value = CFString::new(text);
        let selected = CFString::from_static_string("AXSelectedText");
        let before = text_value(&focused);
        if AXUIElementSetAttributeValue(
            focused.as_CFTypeRef(),
            selected.as_concrete_TypeRef(),
            value.as_CFTypeRef(),
        ) != AX_SUCCESS
        {
            return Typed::Unsupported;
        }
        // Some apps accept the setter and ignore it.
        if insertion_took(before.as_deref(), text_value(&focused).as_deref(), text) {
            Typed::Inserted
        } else {
            Typed::Unsupported
        }
    }
}

/// The element's `AXValue` when it is text.
///
/// # Safety
/// `element` must be a valid AX element.
unsafe fn text_value(element: &CFType) -> Option<String> {
    let mut value: CFTypeRef = std::ptr::null();
    let attribute = CFString::from_static_string("AXValue");
    if AXUIElementCopyAttributeValue(
        element.as_CFTypeRef(),
        attribute.as_concrete_TypeRef(),
        &mut value,
    ) != AX_SUCCESS
        || value.is_null()
    {
        return None;
    }
    CFType::wrap_under_create_rule(value)
        .downcast::<CFString>()
        .map(|value| value.to_string())
}

/// Whether setting the selected text took effect, given the element's
/// value before and after (`None`: unreadable, as in secure fields, where
/// the setter's success is all there is): inserting something must change
/// the value.
fn insertion_took(before: Option<&str>, after: Option<&str>, text: &str) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => text.is_empty() || before != after,
        _ => true,
    }
}

/// Posts a chord to one app without activating it. `Ok(false)` when the
/// key has no fixed virtual key code (the caller falls back to keystrokes
/// in the front app).
pub(crate) fn post_chord(pid: u32, chord: &Chord) -> Result<bool, String> {
    let Some(code) = key_code(&chord.key) else {
        return Ok(false);
    };
    let pid = i32::try_from(pid).map_err(|_| format!("invalid process {pid}"))?;
    let flags = chord.held.iter().fold(0u64, |flags, key| {
        flags
            | match key {
                Key::Meta => 0x0010_0000,
                Key::Shift => 0x0002_0000,
                Key::Alt => 0x0008_0000,
                Key::Ctrl => 0x0004_0000,
                _ => 0,
            }
    });
    for down in [true, false] {
        // SAFETY: the event is created, posted and released here.
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), code, down);
            if event.is_null() {
                return Err("could not create a keyboard event".to_owned());
            }
            CGEventSetFlags(event, flags);
            CGEventPostToPid(pid, event);
            core_foundation::base::CFRelease(event as CFTypeRef);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Ok(true)
}

/// US ANSI virtual key codes (positions, independent of the layout).
fn key_code(key: &Key) -> Option<u16> {
    Some(match key {
        Key::Enter => 0x24,
        Key::Tab => 0x30,
        Key::Space => 0x31,
        Key::Backspace => 0x33,
        Key::Escape => 0x35,
        Key::Delete => 0x75,
        Key::Insert => 0x72,
        Key::Home => 0x73,
        Key::End => 0x77,
        Key::PageUp => 0x74,
        Key::PageDown => 0x79,
        Key::ArrowLeft => 0x7B,
        Key::ArrowRight => 0x7C,
        Key::ArrowDown => 0x7D,
        Key::ArrowUp => 0x7E,
        Key::F(number) => *[
            0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B,
            0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A,
        ]
        .get(usize::from(*number).checked_sub(1)?)?,
        Key::Char(c) => match c {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0B,
            'q' => 0x0C,
            'w' => 0x0D,
            'e' => 0x0E,
            'r' => 0x0F,
            'y' => 0x10,
            't' => 0x11,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '6' => 0x16,
            '5' => 0x17,
            '=' => 0x18,
            '9' => 0x19,
            '7' => 0x1A,
            '-' => 0x1B,
            '8' => 0x1C,
            '0' => 0x1D,
            ']' => 0x1E,
            'o' => 0x1F,
            'u' => 0x20,
            '[' => 0x21,
            'i' => 0x22,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            '\'' => 0x27,
            'k' => 0x28,
            ';' => 0x29,
            '\\' => 0x2A,
            ',' => 0x2B,
            '/' => 0x2C,
            'n' => 0x2D,
            'm' => 0x2E,
            '.' => 0x2F,
            '`' => 0x32,
            _ => return None,
        },
        Key::Shift | Key::Ctrl | Key::Alt | Key::Meta => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ignored_text_insertion_is_noticed() {
        // Readable before and after: the value must change.
        assert!(insertion_took(Some("a"), Some("ab"), "b"));
        assert!(!insertion_took(Some("a"), Some("a"), "b"));
        // Nothing to insert changes nothing.
        assert!(insertion_took(Some("a"), Some("a"), ""));
        // Unreadable (secure fields): the setter's success stands.
        assert!(insertion_took(None, None, "b"));
        assert!(insertion_took(Some("a"), None, "b"));
        assert!(insertion_took(None, Some("a"), "b"));
    }

    fn window(layer: i64, pid: u32) -> CgWindow {
        CgWindow {
            layer,
            pid,
            alpha: 1.0,
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        }
    }

    #[test]
    fn protected_windows_above_the_normal_layer_take_the_hit() {
        let own = 7;
        let protected = |pid| pid == own;
        // A TodeX dialog at the status level over an editor.
        let stack = [window(25, own), window(0, 50)];
        assert_eq!(hit(&stack, 10.0, 10.0, protected), Some(own));
        // The menu bar or another overlay is skipped as before.
        let stack = [window(24, 3), window(0, 50)];
        assert_eq!(hit(&stack, 10.0, 10.0, protected), Some(50));
        // Nothing normal there.
        assert_eq!(hit(&[window(24, 3)], 10.0, 10.0, protected), None);
        assert_eq!(hit(&stack, 500.0, 10.0, protected), None);
    }
}
