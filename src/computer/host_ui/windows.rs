//! Windows host UI: a dedicated thread runs a Win32 message loop and owns
//! a topmost status window (with a Stop button), the pointer marker and
//! the Ctrl+Alt+Shift+Esc hotkey (Ctrl+Shift+Esc is Task Manager). Both
//! windows never take focus and are excluded from screen capture.
//! Confirmations are message boxes on their own threads.

use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use ::windows::{
    core::{w, BOOL, HSTRING, PCWSTR},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, WPARAM},
        Globalization::GetUserDefaultLocaleName,
        Graphics::Gdi::{
            CreateFontW, CreateSolidBrush, DeleteObject, GetMonitorInfoW, MonitorFromPoint,
            SetBkColor, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
            DEFAULT_PITCH, FW_NORMAL, HDC, HFONT, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
            MONITOR_DEFAULTTOPRIMARY, OUT_DEFAULT_PRECIS,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
                VK_ESCAPE,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
                EnumThreadWindows, GetMessageW, KillTimer, LoadCursorW, MessageBoxW, PeekMessageW,
                PostMessageW, PostThreadMessageW, RegisterClassW, SendMessageW,
                SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowPos,
                SetWindowTextW, ShowWindow, TranslateMessage, BN_CLICKED, BS_PUSHBUTTON, HMENU,
                HWND_TOPMOST, IDC_ARROW, IDNO, IDYES, LWA_ALPHA, MA_NOACTIVATE, MB_DEFBUTTON2,
                MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MSG, PM_NOREMOVE,
                SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE,
                WDA_EXCLUDEFROMCAPTURE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
                WM_CTLCOLORSTATIC, WM_HOTKEY, WM_MOUSEACTIVATE, WM_SETFONT, WM_TIMER, WNDCLASSW,
                WS_CHILD, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
                WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
            },
        },
    },
};

use super::Strings;

pub(super) const STOP_SHORTCUT: Option<&str> = Some("Ctrl+Alt+Shift+Esc");

/// Wakes the UI thread to drain [`COMMANDS`].
const WM_COMMANDS: u32 = WM_APP + 1;
const STOP_HOTKEY_ID: i32 = 1;
const STOP_BUTTON_ID: usize = 1;
const MARKER_TIMER_ID: usize = 1;
const MARKER_VISIBLE_MS: u32 = 1500;
const PILL_CLASS: PCWSTR = w!("TodexComputerStatus");
const MARKER_CLASS: PCWSTR = w!("TodexComputerMarker");
/// Sizes at 100% scaling.
const PILL_WIDTH: f64 = 460.0;
const PILL_HEIGHT: f64 = 44.0;
const STOP_WIDTH: f64 = 190.0;
const MARKER_SIZE: f64 = 18.0;
const PILL_COLOR: COLORREF = rgb(26, 26, 26);
const MARKER_COLOR: COLORREF = rgb(245, 158, 11);
// Static control styles (Win32_System_SystemServices would be a large
// feature for three constants).
const SS_CENTERIMAGE: u32 = 0x200;
const SS_ENDELLIPSIS: u32 = 0x4000;
const LOCALE_NAME_MAX_LENGTH: usize = 85;

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF(red as u32 | (green as u32) << 8 | (blue as u32) << 16)
}

enum Command {
    Show { text: String, stop: String },
    Hide,
    Mark { x: i32, y: i32 },
}

static UI_THREAD: OnceLock<u32> = OnceLock::new();
/// The status window's background brush (an HBRUSH), for its label.
static PILL_BRUSH: OnceLock<usize> = OnceLock::new();
static COMMANDS: Mutex<VecDeque<Command>> = Mutex::new(VecDeque::new());
/// One confirmation dialog at a time.
static CONFIRMING: Mutex<()> = Mutex::new(());

thread_local! {
    /// UI-thread-only state.
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
}

#[derive(Default)]
struct Ui {
    pill: Option<Pill>,
    marker: Option<HWND>,
    hotkey: bool,
}

struct Pill {
    window: HWND,
    label: HWND,
    button: HWND,
    font: HFONT,
}

pub(super) fn run_with_main_loop(body: impl FnOnce() + Send + 'static) -> ! {
    // Nobody can see a service session's windows (see the platform layer).
    if crate::computer::platform::unsupported_reason().is_none() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("todex-host-ui".to_owned())
            .spawn(move || ui_thread(ready_tx));
        match spawned.map(|_| ready_rx.recv()) {
            Ok(Ok(Ok(thread))) => {
                let _ = UI_THREAD.set(thread);
                super::mark_available();
            }
            Ok(Ok(Err(error))) => {
                eprintln!("todex-agentd: Computer Use host UI unavailable: {error}");
            }
            Ok(Err(_)) => eprintln!("todex-agentd: Computer Use host UI thread stopped"),
            Err(error) => {
                eprintln!("todex-agentd: Computer Use host UI unavailable: {error}");
            }
        }
    }
    body();
    unreachable!("the body exits the process");
}

fn ui_thread(ready: mpsc::Sender<Result<u32, String>>) {
    // SAFETY: Win32 calls on this thread's own queue and window classes.
    unsafe {
        // Create the message queue before anyone posts to it.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        if let Err(error) = register_classes() {
            let _ = ready.send(Err(error));
            return;
        }
        let _ = ready.send(Ok(GetCurrentThreadId()));
        loop {
            let got = GetMessageW(&mut msg, None, 0, 0);
            if got.0 <= 0 {
                break;
            }
            if msg.hwnd.is_invalid() {
                match msg.message {
                    WM_COMMANDS => drain_commands(),
                    WM_HOTKEY if msg.wParam.0 == STOP_HOTKEY_ID as usize => super::request_stop(),
                    _ => {}
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe fn register_classes() -> Result<(), String> {
    // SAFETY: the caller is the UI thread; the classes' strings are static.
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|error| error.to_string())?;
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let pill_brush = CreateSolidBrush(PILL_COLOR);
        let _ = PILL_BRUSH.set(pill_brush.0 as usize);
        for (class, procedure, brush) in [
            (PILL_CLASS, pill_procedure as _, pill_brush),
            (
                MARKER_CLASS,
                marker_procedure as _,
                CreateSolidBrush(MARKER_COLOR),
            ),
        ] {
            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                hCursor: cursor,
                hbrBackground: brush,
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(format!(
                    "RegisterClassW failed: {}",
                    ::windows::core::Error::from_thread()
                ));
            }
        }
        Ok(())
    }
}

fn send(command: Command) {
    let Some(thread) = UI_THREAD.get() else {
        return;
    };
    COMMANDS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(command);
    // SAFETY: posting a plain message to the UI thread's queue.
    if let Err(error) = unsafe { PostThreadMessageW(*thread, WM_COMMANDS, WPARAM(0), LPARAM(0)) } {
        eprintln!("todex-agentd: Computer Use host UI did not respond: {error}");
    }
}

fn drain_commands() {
    loop {
        let command = COMMANDS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        let Some(command) = command else {
            return;
        };
        UI.with(|ui| {
            let mut ui = ui.borrow_mut();
            // SAFETY: on the UI thread, which owns every window touched.
            unsafe {
                match command {
                    Command::Show { text, stop } => show(&mut ui, &text, &stop),
                    Command::Hide => hide(&mut ui),
                    Command::Mark { x, y } => mark(&mut ui, x, y),
                }
            }
        });
    }
}

/// Scale factor (effective DPI / 96) of a monitor.
fn monitor_scale(monitor: HMONITOR) -> f64 {
    let (mut x, mut y) = (0u32, 0u32);
    // SAFETY: plain query into two u32s.
    match unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x > 0 => f64::from(x) / 96.0,
        _ => 1.0,
    }
}

/// Makes a window topmost-only overlay material: never in screenshots.
unsafe fn exclude_from_capture(window: HWND) {
    // SAFETY: the caller owns `window`.
    // Needs Windows 10 2004; on older systems the overlay can show up in
    // screenshots, which is cosmetic.
    let _ = unsafe { SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE) };
}

unsafe fn show(ui: &mut Ui, text: &str, stop: &str) {
    // SAFETY: the caller is the UI thread.
    unsafe {
        if !ui.hotkey {
            ui.hotkey = RegisterHotKey(
                None,
                STOP_HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
                u32::from(VK_ESCAPE.0),
            )
            .is_ok();
            if !ui.hotkey {
                eprintln!("todex-agentd: the Computer Use stop shortcut is taken by another app");
            }
        }
        if ui.pill.is_none() {
            ui.pill = create_pill();
        }
        let Some(pill) = &ui.pill else {
            return;
        };
        let _ = SetWindowTextW(pill.label, &HSTRING::from(text));
        let _ = SetWindowTextW(pill.button, &HSTRING::from(stop));
        let _ = ShowWindow(pill.window, SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(
            pill.window,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// The status window at the top right of the primary work area.
unsafe fn create_pill() -> Option<Pill> {
    // SAFETY: the caller is the UI thread; every handle created here is
    // owned by the returned Pill (child windows by their parent).
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let scale = monitor_scale(monitor);
        let px = |value: f64| (value * scale).round() as i32;
        let (width, height) = (px(PILL_WIDTH), px(PILL_HEIGHT));
        let work = info.rcWork;
        let instance = GetModuleHandleW(None).ok()?;
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            PILL_CLASS,
            w!("TodeX"),
            WS_POPUP,
            work.right - width - px(16.0),
            work.top + px(12.0),
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let _ = SetLayeredWindowAttributes(window, COLORREF(0), 235, LWA_ALPHA);
        exclude_from_capture(window);
        let font = CreateFontW(
            -px(15.0),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            u32::from(DEFAULT_PITCH.0),
            w!("Segoe UI"),
        );
        let label = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_CENTERIMAGE | SS_ENDELLIPSIS),
            px(12.0),
            0,
            width - px(STOP_WIDTH) - px(24.0),
            height,
            Some(window),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let button = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            width - px(STOP_WIDTH) - px(8.0),
            px(7.0),
            px(STOP_WIDTH),
            height - px(14.0),
            Some(window),
            Some(HMENU(STOP_BUTTON_ID as *mut _)),
            Some(instance.into()),
            None,
        )
        .ok()?;
        for child in [label, button] {
            SendMessageW(
                child,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
        Some(Pill {
            window,
            label,
            button,
            font,
        })
    }
}

unsafe fn hide(ui: &mut Ui) {
    // SAFETY: the caller is the UI thread, which owns these handles.
    unsafe {
        if ui.hotkey {
            let _ = UnregisterHotKey(None, STOP_HOTKEY_ID);
            ui.hotkey = false;
        }
        if let Some(pill) = ui.pill.take() {
            let _ = DestroyWindow(pill.window);
            let _ = DeleteObject(pill.font.into());
        }
        if let Some(marker) = ui.marker {
            let _ = ShowWindow(marker, SW_HIDE);
        }
    }
}

unsafe fn mark(ui: &mut Ui, x: i32, y: i32) {
    // SAFETY: the caller is the UI thread, which owns the marker.
    unsafe {
        if ui.marker.is_none() {
            let Ok(instance) = GetModuleHandleW(None) else {
                return;
            };
            let Ok(marker) = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
                MARKER_CLASS,
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                None,
            ) else {
                return;
            };
            let _ = SetLayeredWindowAttributes(marker, COLORREF(0), 190, LWA_ALPHA);
            exclude_from_capture(marker);
            ui.marker = Some(marker);
        }
        let Some(marker) = ui.marker else {
            return;
        };
        let scale = monitor_scale(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST));
        let size = (MARKER_SIZE * scale).round() as i32;
        let _ = SetWindowPos(
            marker,
            Some(HWND_TOPMOST),
            x - size / 2,
            y - size / 2,
            size,
            size,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        // Re-arming the same timer id restarts it.
        SetTimer(Some(marker), MARKER_TIMER_ID, MARKER_VISIBLE_MS, None);
    }
}

unsafe extern "system" fn pill_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND
            if wparam.0 & 0xFFFF == STOP_BUTTON_ID && (wparam.0 >> 16) as u32 == BN_CLICKED =>
        {
            super::request_stop();
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let dc = HDC(wparam.0 as *mut _);
            // SAFETY: the DC belongs to the static control being painted.
            unsafe {
                SetTextColor(dc, rgb(255, 255, 255));
                SetBkColor(dc, PILL_COLOR);
            }
            // Read without UI's RefCell: painting can happen while a
            // command holds it.
            LRESULT(PILL_BRUSH.get().copied().unwrap_or_default() as isize)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // Only hiding the session removes the status.
        WM_CLOSE => LRESULT(0),
        // SAFETY: default handling for our own window.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe extern "system" fn marker_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: calls on our own window from its thread.
    unsafe {
        match message {
            WM_TIMER if wparam.0 == MARKER_TIMER_ID => {
                let _ = KillTimer(Some(window), MARKER_TIMER_ID);
                let _ = ShowWindow(window, SW_HIDE);
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

pub(super) fn preferred_language() -> Option<String> {
    let mut buffer = [0u16; LOCALE_NAME_MAX_LENGTH];
    // SAFETY: the buffer is writable for its length.
    let len = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    // The length includes the terminating NUL.
    (len > 1).then(|| String::from_utf16_lossy(&buffer[..len as usize - 1]))
}

pub(super) fn show_status(strings: &Strings, summary: &str) {
    let text = if summary.is_empty() {
        strings.controlling.to_owned()
    } else {
        format!("{} · {summary}", strings.controlling)
    };
    let stop = format!("{} {}", strings.stop, STOP_SHORTCUT.unwrap_or_default());
    send(Command::Show { text, stop });
}

pub(super) fn hide_status() {
    send(Command::Hide);
}

pub(super) fn mark_point(x: f64, y: f64) {
    send(Command::Mark {
        x: x.round() as i32,
        y: y.round() as i32,
    });
}

/// A Yes/No message box (No by default) on its own thread; on timeout the
/// box is answered No and this returns false. Message box buttons follow
/// the system language, so `strings` is not needed.
pub(super) fn confirm(_strings: &Strings, title: &str, message: &str, timeout: Duration) -> bool {
    let _one_at_a_time = CONFIRMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (title, message) = (HSTRING::from(title), HSTRING::from(message));
    let box_thread = Arc::new(AtomicU32::new(0));
    let (answer_tx, answer_rx) = mpsc::channel();
    let spawned = {
        let box_thread = box_thread.clone();
        std::thread::Builder::new()
            .name("todex-confirm".to_owned())
            .spawn(move || {
                // SAFETY: the strings outlive the modal call.
                let answer = unsafe {
                    box_thread.store(GetCurrentThreadId(), Ordering::SeqCst);
                    MessageBoxW(
                        None,
                        &message,
                        &title,
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
                    )
                };
                let _ = answer_tx.send(answer == IDYES);
            })
    };
    if let Err(error) = spawned {
        eprintln!("todex-agentd: could not show the Computer Use confirmation: {error}");
        return false;
    }
    match answer_rx.recv_timeout(timeout) {
        Ok(allowed) => allowed,
        Err(mpsc::RecvTimeoutError::Disconnected) => false,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Answer No for the person; the box may still be appearing.
            let thread = box_thread.load(Ordering::SeqCst);
            for _ in 0..20 {
                if thread != 0 {
                    // SAFETY: the callback only posts messages.
                    unsafe {
                        let _ = EnumThreadWindows(thread, Some(answer_no), LPARAM(0));
                    }
                }
                if answer_rx.recv_timeout(Duration::from_millis(100)).is_ok() {
                    break;
                }
            }
            false
        }
    }
}

unsafe extern "system" fn answer_no(window: HWND, _data: LPARAM) -> BOOL {
    // SAFETY: posting a button command to the message box.
    let _ = unsafe { PostMessageW(Some(window), WM_COMMAND, WPARAM(IDNO.0 as usize), LPARAM(0)) };
    BOOL(1)
}
