//! Windows: Win32 process and window lookups, UI Automation for password
//! fields. Windows has no permission grants like macOS TCC; what it needs
//! is an interactive desktop session.
//!
//! Coordinates: the process is Per-Monitor-V2 DPI aware (set at startup,
//! and by xa11y-windows on first use), so UIA bounds, GDI screenshots,
//! `SendInput`, [`displays`] and [`app_at`] all use physical desktop
//! pixels. A display's `scale` is its effective DPI / 96, for information
//! only: screenshots are one pixel per coordinate unit.
//!
//! Limits: keystrokes and chords go to the foreground window, so `type`
//! without an element and `key` bring the target app to the front first
//! ([`activate`]), which Windows may refuse (foreground lock); and an
//! app's identity is its executable name, so every app hosted by one
//! executable (e.g. `javaw.exe`) shares an approval.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{Mutex, Once},
    time::Duration,
};

use ::windows::{
    core::{w, BOOL, HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HWND, LPARAM, POINT, RECT},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
            Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
        },
        Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ},
            RemoteDesktop::ProcessIdToSessionId,
            StationsAndDesktops::{
                CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
            },
            SystemInformation::GetTickCount,
            Threading::{
                AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
                QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::{
            Accessibility::{
                CUIAutomation, IUIAutomation, IUIAutomationElement, UIA_EditControlTypeId,
            },
            HiDpi::{
                GetDpiForMonitor, SetProcessDpiAwarenessContext,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
            },
            Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
            Shell::ShellExecuteW,
            WindowsAndMessaging::{
                EnumChildWindows, EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow,
                GetWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
                IsWindowVisible, PeekMessageW, SetForegroundWindow, ShowWindow, WindowFromPoint,
                GA_ROOT, GWL_EXSTYLE, GW_OWNER, MONITORINFOF_PRIMARY, MSG, PM_NOREMOVE, SW_RESTORE,
                SW_SHOWNORMAL, WS_EX_TOOLWINDOW,
            },
        },
    },
};
use xa11y::ElementData;

use super::{
    windows_names::{
        exe_file_name, exe_id, exe_stem, idle_from_ticks, looks_like_password, names_app,
        session_problem, unquote,
    },
    Display, Permissions, Typed,
};
use crate::computer::{keys::Chord, policy::Target};

/// Hosts the windows of UWP apps; the app itself is another process.
const FRAME_HOST_CLASS: &str = "ApplicationFrameWindow";
const UWP_CONTENT_CLASS: &str = "Windows.UI.Core.CoreWindow";
const MAX_PATH_CHARS: usize = 32_768;

static DPI_AWARENESS: Once = Once::new();

/// Makes the process Per-Monitor-V2 DPI aware before any window exists,
/// so every coordinate Computer Use handles is a physical desktop pixel.
/// Windows has no per-process permission identity to adopt.
pub(crate) fn adopt_own_permission_identity() {
    ensure_dpi_aware();
}

fn ensure_dpi_aware() {
    DPI_AWARENESS.call_once(|| {
        // Fails when a manifest or an earlier call fixed the awareness;
        // xa11y-windows makes the same call and copes the same way.
        // SAFETY: a process-wide setting without pointers.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

/// Why Computer Use cannot run here: a service session has no desktop, and
/// a locked workstation's input desktop is not ours to drive.
pub(crate) fn unsupported_reason() -> Option<String> {
    let mut session = u32::MAX;
    // SAFETY: plain queries; the desktop handle is closed right away.
    let (session_zero, input_desktop) = unsafe {
        let session_zero =
            ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_ok() && session == 0;
        let input_desktop =
            match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) {
                Ok(desktop) => {
                    let _ = CloseDesktop(desktop);
                    true
                }
                Err(_) => false,
            };
        (session_zero, input_desktop)
    };
    session_problem(session_zero, input_desktop)
}

/// Windows has no Screen Recording or Accessibility grants.
pub(crate) fn permissions() -> Permissions {
    Permissions {
        screen: true,
        accessibility: true,
    }
}

pub(crate) fn request_permissions() -> Permissions {
    permissions()
}

/// Seconds since the last keyboard or mouse input in this session,
/// excluding input Computer Use injected itself (`SendInput` counts here).
pub(crate) fn idle_seconds() -> f64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: info is a correctly sized LASTINPUTINFO.
    let raw = unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            idle_from_ticks(GetTickCount(), info.dwTime)
        } else {
            f64::INFINITY
        }
    };
    super::injected::user_idle(raw)
}

pub(crate) fn app_identity(pid: u32) -> Target {
    let Some(path) = image_path(pid) else {
        // Protected processes refuse even limited queries; the process
        // list still names their executable.
        let id = snapshot_exe(pid)
            .map(|exe| exe_id(&exe))
            .unwrap_or_default();
        return Target {
            name: exe_stem(&id).to_owned(),
            id,
            pid,
        };
    };
    let id = exe_id(&path);
    let name = file_description(&path).unwrap_or_else(|| exe_stem(&id).to_owned());
    Target { id, name, pid }
}

/// A running app with a visible window, by executable name (with or
/// without `.exe`), display name, or failing those a window title.
pub(crate) fn running_app(identifier: &str) -> Option<Target> {
    let windows = top_level_windows();
    let mut seen = Vec::new();
    for window in &windows {
        if seen.contains(&window.pid) {
            continue;
        }
        seen.push(window.pid);
        let app = app_identity(window.pid);
        if names_app(identifier, &app.id, &app.name) {
            return Some(app);
        }
    }
    let wanted = identifier.trim().to_lowercase();
    windows
        .iter()
        .find(|window| !wanted.is_empty() && window.title.to_lowercase() == wanted)
        .map(|window| app_identity(window.pid))
}

/// The app `open_app` would launch, for the policy check before launching.
pub(crate) fn installed_app(identifier: &str) -> Option<Target> {
    if let Some(app) = running_app(identifier) {
        return Some(app);
    }
    let path = resolve_executable(identifier)?;
    let path = path.to_string_lossy().into_owned();
    let id = exe_id(&path);
    Some(Target {
        name: file_description(&path).unwrap_or_else(|| exe_stem(&id).to_owned()),
        id,
        pid: 0,
    })
}

pub(crate) fn open_app(identifier: &str) -> Result<(), String> {
    if let Some(app) = running_app(identifier) {
        return activate(app.pid);
    }
    // The same resolution `installed_app` checked against the policy.
    let path =
        resolve_executable(identifier).ok_or_else(|| format!("no app named {identifier}"))?;
    let file = HSTRING::from(path.as_os_str());
    // SAFETY: all strings outlive the call.
    let instance = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &file,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success as a value above 32.
    if instance.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!(
            "could not open {} (ShellExecute error {})",
            path.display(),
            instance.0 as usize
        ))
    }
}

/// Brings `pid`'s main window to the front. Windows only lets the
/// foreground process move the foreground; when the plain call is refused
/// this attaches to the foreground thread's input once (the documented
/// workaround). It can still be refused, e.g. while a full-screen app or
/// an elevated window has the foreground; the error says so.
pub(crate) fn activate(pid: u32) -> Result<(), String> {
    let window = top_level_windows()
        .into_iter()
        .find(|window| window.pid == pid)
        .ok_or_else(|| format!("process {pid} has no visible window"))?;
    let hwnd = window.hwnd;
    // SAFETY: plain window calls on a handle that may have died meanwhile,
    // which they tolerate. The thread attachment is undone before return.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() && is_foreground(hwnd) {
            return Ok(());
        }
        // AttachThreadInput needs a message queue on this thread.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let ours = GetCurrentThreadId();
        let theirs = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached =
            theirs != 0 && theirs != ours && AttachThreadInput(ours, theirs, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(ours, theirs, false);
        }
    }
    for _ in 0..5 {
        if is_foreground(hwnd) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(format!(
        "Windows did not bring {} to the front (another app holds the foreground); \
         ask the user to switch to it",
        if window.title.is_empty() {
            format!("process {pid}")
        } else {
            window.title
        }
    ))
}

fn is_foreground(hwnd: HWND) -> bool {
    // SAFETY: plain window queries.
    unsafe { GetAncestor(GetForegroundWindow(), GA_ROOT) == hwnd }
}

/// The process owning the top-level window at a desktop point.
pub(crate) fn app_at(x: f64, y: f64) -> Option<u32> {
    ensure_dpi_aware();
    let point = POINT {
        x: x.round() as i32,
        y: y.round() as i32,
    };
    // SAFETY: plain window queries.
    let root = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    if root.is_invalid() {
        return None;
    }
    Some(window_pid(root)).filter(|pid| *pid != 0)
}

/// Monitors in physical desktop pixels, the primary first.
pub(crate) fn displays() -> Vec<Display> {
    ensure_dpi_aware();
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback gets our Vec back through lparam while
    // EnumDisplayMonitors runs.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut monitors as *mut Vec<HMONITOR> as isize),
        );
    }
    let mut found: Vec<(bool, Display)> = monitors
        .into_iter()
        .filter_map(|monitor| {
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            // SAFETY: info is a correctly sized MONITORINFO.
            if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
                return None;
            }
            let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
            // SAFETY: plain query into two u32s.
            let scale = match unsafe {
                GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y)
            } {
                Ok(()) if dpi_x > 0 => f64::from(dpi_x) / 96.0,
                _ => 1.0,
            };
            let rect = info.rcMonitor;
            Some((
                info.dwFlags & MONITORINFOF_PRIMARY != 0,
                Display {
                    index: 0,
                    x: f64::from(rect.left),
                    y: f64::from(rect.top),
                    width: f64::from(rect.right - rect.left),
                    height: f64::from(rect.bottom - rect.top),
                    scale,
                },
            ))
        })
        .collect();
    found.sort_by(|(a_primary, a), (b_primary, b)| {
        b_primary
            .cmp(a_primary)
            .then(a.x.total_cmp(&b.x))
            .then(a.y.total_cmp(&b.y))
    });
    found
        .into_iter()
        .enumerate()
        .map(|(index, (_, display))| Display { index, ..display })
        .collect()
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `displays` passes a live Vec<HMONITOR>.
    let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    monitors.push(monitor);
    BOOL(1)
}

/// Whether an element is a password field.
///
/// UI Automation's `IsPassword` is authoritative, but xa11y-windows does
/// not put it in `raw` (it fills `control_type_id`, `automation_id`,
/// `class_name`, `uia_name`, …), so edits are re-resolved here: through
/// their window handle when they have one (`stable_id` `hwnd:…`, classic
/// Win32 edits), else by hit-testing the centre of their bounds and
/// accepting the element there only if it has the same bounds. An edit
/// that is covered by another window or has no bounds falls back to its
/// class (`PasswordBox`) and accessible name, which can miss unlabeled
/// password fields.
pub(crate) fn is_secure(element: &ElementData) -> bool {
    if element
        .raw
        .get("control_type_id")
        .and_then(|id| id.as_i64())
        != Some(i64::from(UIA_EditControlTypeId.0))
    {
        return false;
    }
    let raw_text = |key: &str| {
        element
            .raw
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    if let Some(secure) = uia_is_password(element) {
        return secure;
    }
    looks_like_password(&raw_text("class_name"), &raw_text("uia_name"))
}

/// `IsPassword` of the UIA element behind `element`, when it can be found.
fn uia_is_password(element: &ElementData) -> Option<bool> {
    with_automation(|automation| {
        // SAFETY: COM calls on interfaces owned for the call's duration.
        unsafe {
            if let Some(hwnd) = element
                .stable_id
                .as_deref()
                .and_then(|id| id.strip_prefix("hwnd:0x"))
                .and_then(|hex| usize::from_str_radix(hex, 16).ok())
            {
                let found = automation
                    .ElementFromHandle(HWND(hwnd as *mut c_void))
                    .ok()?;
                return found.CurrentIsPassword().ok().map(BOOL::as_bool);
            }
            let bounds = element.bounds?;
            let center = POINT {
                x: bounds.x + (bounds.width / 2) as i32,
                y: bounds.y + (bounds.height / 2) as i32,
            };
            let walker = automation.RawViewWalker().ok()?;
            let mut candidate = automation.ElementFromPoint(center).ok();
            // The hit may be a child of the edit (e.g. its text run).
            for _ in 0..3 {
                let current = candidate.take()?;
                if same_bounds(&current, &bounds) {
                    return current.CurrentIsPassword().ok().map(BOOL::as_bool);
                }
                candidate = walker.GetParentElement(&current).ok();
            }
            None
        }
    })
    .flatten()
}

fn same_bounds(element: &IUIAutomationElement, bounds: &xa11y::Rect) -> bool {
    // SAFETY: a property read on a live element.
    unsafe { element.CurrentBoundingRectangle() }.is_ok_and(|rect| {
        rect.left == bounds.x
            && rect.top == bounds.y
            && i64::from(rect.right) - i64::from(rect.left) == i64::from(bounds.width)
            && i64::from(rect.bottom) - i64::from(rect.top) == i64::from(bounds.height)
    })
}

thread_local! {
    /// One UI Automation client per (blocking) thread.
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

fn with_automation<T>(work: impl FnOnce(&IUIAutomation) -> T) -> Option<T> {
    AUTOMATION.with(|cell| {
        let mut cell = cell.borrow_mut();
        if cell.is_none() {
            // SAFETY: COM initialisation for this thread (MTA, like
            // xa11y-windows; an existing apartment is kept) and creation
            // of the in-process UIA client.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                *cell = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
            }
        }
        cell.as_ref().map(work)
    })
}

/// Keystrokes reach the foreground window, so this brings `pid` to the
/// front and reports [`Typed::Secure`] when the element that would
/// receive the text is a password field and the user has not confirmed.
/// Otherwise the engine types with `SendInput` ([`Typed::Unsupported`]).
pub(crate) fn type_into_focused(pid: u32, _text: &str, confirmed: bool) -> Typed {
    if confirmed || activate(pid).is_err() {
        // A failed activation surfaces from the engine's own attempt.
        return Typed::Unsupported;
    }
    let secure = with_automation(|automation| {
        // SAFETY: COM calls on interfaces owned for the call's duration.
        unsafe {
            automation
                .GetFocusedElement()
                .and_then(|focused| focused.CurrentIsPassword())
                .is_ok_and(BOOL::as_bool)
        }
    })
    .unwrap_or(false);
    if secure {
        Typed::Secure
    } else {
        Typed::Unsupported
    }
}

/// Windows cannot reliably deliver a chord to a background window; the
/// engine activates the app and uses `SendInput`.
pub(crate) fn post_chord(_pid: u32, _chord: &Chord) -> Result<bool, String> {
    Ok(false)
}

// ---- Windows and processes --------------------------------------------

struct TopLevel {
    hwnd: HWND,
    pid: u32,
    title: String,
}

/// Visible, uncloaked, unowned app windows, front to back.
fn top_level_windows() -> Vec<TopLevel> {
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: the callback gets our Vec back through lparam while
    // EnumWindows runs.
    unsafe {
        let _ = EnumWindows(
            Some(collect_window),
            LPARAM(&mut handles as *mut Vec<HWND> as isize),
        );
    }
    handles
        .into_iter()
        .filter(|hwnd| is_app_window(*hwnd))
        .filter_map(|hwnd| {
            let pid = window_pid(hwnd);
            (pid != 0).then(|| TopLevel {
                hwnd,
                pid,
                title: window_text(hwnd),
            })
        })
        .collect()
}

unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: the enumerating function passes a live Vec<HWND>.
    let handles = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
    handles.push(hwnd);
    BOOL(1)
}

fn is_app_window(hwnd: HWND) -> bool {
    // SAFETY: plain window queries; `cloaked` is a DWORD-sized out value.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return false;
        }
        if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let cloaked_ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok();
        !(cloaked_ok && cloaked != 0)
    }
}

/// The process behind a top-level window; for UWP frames, the hosted app
/// rather than `ApplicationFrameHost.exe`, so each such app has its own
/// identity.
fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: plain query into a u32.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if class_name(hwnd) != FRAME_HOST_CLASS {
        return pid;
    }
    let mut search = (pid, 0u32);
    // SAFETY: the callback gets `search` back through lparam while
    // EnumChildWindows runs.
    unsafe {
        let _ = EnumChildWindows(
            Some(hwnd),
            Some(find_hosted_app),
            LPARAM(&mut search as *mut (u32, u32) as isize),
        );
    }
    if search.1 != 0 {
        search.1
    } else {
        pid
    }
}

unsafe extern "system" fn find_hosted_app(child: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `window_pid` passes a live (frame pid, found pid) pair.
    let search = unsafe { &mut *(data.0 as *mut (u32, u32)) };
    if class_name(child) != UWP_CONTENT_CLASS {
        return BOOL(1);
    }
    let mut pid = 0u32;
    // SAFETY: plain query into a u32.
    unsafe { GetWindowThreadProcessId(child, Some(&mut pid)) };
    if pid != 0 && pid != search.0 {
        search.1 = pid;
        return BOOL(0);
    }
    BOOL(1)
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer is writable for its length.
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn window_text(hwnd: HWND) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer is writable for its length.
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..len.max(0) as usize])
}

fn image_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: the handle is closed before return; the buffer and size
    // describe a writable region.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; MAX_PATH_CHARS];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        Some(String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

/// The executable file name of `pid` from the process list.
fn snapshot_exe(pid: u32) -> Option<String> {
    // SAFETY: the snapshot handle is closed before return; the entry is
    // correctly sized.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32ProcessID == pid {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                found = Some(String::from_utf16_lossy(&entry.szExeFile[..len]));
                break;
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

/// The `FileDescription` of an executable's version resource ("Visual
/// Studio Code" for `code.exe`), cached per path.
fn file_description(path: &str) -> Option<String> {
    static CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(description) = cache.get(path) {
        return description.clone();
    }
    let description = read_file_description(path);
    if cache.len() > 512 {
        cache.clear();
    }
    cache.insert(path.to_owned(), description.clone());
    description
}

fn read_file_description(path: &str) -> Option<String> {
    let file = HSTRING::from(path);
    // SAFETY: the version block is sized by GetFileVersionInfoSizeW and
    // outlives every pointer VerQueryValueW returns into it.
    unsafe {
        let size = GetFileVersionInfoSizeW(&file, None);
        if size == 0 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(&file, None, size, block.as_mut_ptr().cast()).ok()?;
        let mut pointer: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(
            block.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &mut pointer,
            &mut len,
        )
        .as_bool()
            || len < 4
        {
            return None;
        }
        let translation = std::slice::from_raw_parts(pointer as *const u16, 2);
        let query = HSTRING::from(format!(
            "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
            translation[0], translation[1]
        ));
        if !VerQueryValueW(block.as_ptr().cast(), &query, &mut pointer, &mut len).as_bool()
            || len == 0
        {
            return None;
        }
        let text = std::slice::from_raw_parts(pointer as *const u16, len as usize);
        let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
        let description = String::from_utf16_lossy(&text[..end]).trim().to_owned();
        (!description.is_empty()).then_some(description)
    }
}

/// An executable for `identifier`: an existing absolute path, an App Paths
/// registration (per user, then machine), or a file on `PATH`.
fn resolve_executable(identifier: &str) -> Option<PathBuf> {
    let identifier = unquote(identifier);
    if identifier.is_empty() {
        return None;
    }
    let direct = Path::new(identifier);
    if direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    if identifier.contains(['\\', '/']) {
        return None;
    }
    let file_name = exe_file_name(identifier);
    let subkey = format!("Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{file_name}");
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(path) = registry_default(root, &subkey) {
            let path = PathBuf::from(unquote(&path));
            if path.is_file() {
                return Some(path);
            }
        }
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file_name))
        .find(|path| path.is_file())
}

/// The default value of a registry key, as a string.
fn registry_default(root: HKEY, subkey: &str) -> Option<String> {
    let subkey = HSTRING::from(subkey);
    let mut size = 0u32;
    // SAFETY: the first call only sizes; the second writes at most `size`
    // bytes into the buffer.
    unsafe {
        if RegGetValueW(
            root,
            &subkey,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        ) != ERROR_SUCCESS
            || size == 0
        {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        if RegGetValueW(
            root,
            &subkey,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..end]))
    }
}
