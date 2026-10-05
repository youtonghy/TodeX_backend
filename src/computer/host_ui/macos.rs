//! macOS host UI: an accessory `NSApplication` on the main thread (no Dock
//! icon), panels excluded from screen capture, ⌘⇧⎋ through
//! `global-hotkey`, and `NSAlert` for confirmations.

use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use dispatch2::run_on_main;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use objc2::{
    define_class, msg_send, rc::Retained, runtime::AnyObject, sel, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    NSBackingStoreType, NSButton, NSColor, NSPanel, NSScreen, NSTextField,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSLocale, NSObject, NSPoint, NSRect, NSSize, NSString};

use super::Strings;

/// `kCGStatusWindowLevel`: above normal and floating windows.
const STATUS_LEVEL: isize = 25;
const PILL_WIDTH: f64 = 380.0;
const PILL_HEIGHT: f64 = 40.0;
const MARKER_SIZE: f64 = 18.0;
const MARKER_VISIBLE: Duration = Duration::from_millis(1500);

thread_local! {
    /// Main-thread-only objects.
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

struct Ui {
    hotkeys: GlobalHotKeyManager,
    stop_key: HotKey,
    stop_key_registered: bool,
    pill: Option<Pill>,
    marker: Option<Retained<NSPanel>>,
    stop_target: Retained<StopTarget>,
}

struct Pill {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
}

static MARKER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// One confirmation dialog at a time.
static CONFIRMING: Mutex<()> = Mutex::new(());

define_class!(
    // SAFETY: NSObject has no subclassing requirements and StopTarget does
    // not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TodexComputerStopTarget"]
    struct StopTarget;

    impl StopTarget {
        #[unsafe(method(stop:))]
        fn stop(&self, _sender: Option<&AnyObject>) {
            super::request_stop();
        }
    }
);

impl StopTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: plain NSObject init.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

pub(super) fn run_with_main_loop(body: impl FnOnce() + Send + 'static) -> ! {
    let Some(mtm) = MainThreadMarker::new() else {
        body();
        unreachable!("the body exits the process");
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    match GlobalHotKeyManager::new() {
        Ok(hotkeys) => {
            GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
                if event.state == HotKeyState::Pressed {
                    super::request_stop();
                }
            }));
            UI.with(|ui| {
                *ui.borrow_mut() = Some(Ui {
                    hotkeys,
                    stop_key: HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Escape),
                    stop_key_registered: false,
                    pill: None,
                    marker: None,
                    stop_target: StopTarget::new(mtm),
                });
            });
            super::mark_available();
        }
        Err(error) => {
            eprintln!("todex-agentd: Computer Use host UI unavailable: {error}");
        }
    }
    std::thread::Builder::new()
        .name("todex-main".to_owned())
        .spawn(body)
        .expect("spawn the daemon thread");
    app.run();
    unreachable!("NSApplication::run returned");
}

pub(super) fn preferred_language() -> Option<String> {
    NSLocale::preferredLanguages()
        .firstObject()
        .map(|language| language.to_string())
}

fn with_ui(work: impl FnOnce(&mut Ui, MainThreadMarker) + Send) {
    run_on_main(|mtm| {
        UI.with(|ui| {
            if let Some(ui) = ui.borrow_mut().as_mut() {
                work(ui, mtm);
            }
        });
    });
}

/// Height of the primary screen, to flip global (top-left) coordinates
/// into AppKit's bottom-left ones.
fn primary_height(mtm: MainThreadMarker) -> f64 {
    NSScreen::screens(mtm)
        .firstObject()
        .map(|screen| screen.frame().size.height)
        .unwrap_or(0.0)
}

fn overlay_panel(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSPanel> {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: the panel is owned by the retained handle we keep.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setLevel(STATUS_LEVEL);
    // Never part of a screenshot or the live view.
    panel.setSharingType(NSWindowSharingType::None);
    panel.setHidesOnDeactivate(false);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    panel.setOpaque(false);
    panel
}

pub(super) fn show_status(strings: &Strings, summary: &str) {
    let text = if summary.is_empty() {
        strings.controlling.to_owned()
    } else {
        format!("{} · {summary}", strings.controlling)
    };
    let stop = format!("{} ⌘⇧⎋", strings.stop);
    with_ui(move |ui, mtm| {
        if !ui.stop_key_registered && ui.hotkeys.register(ui.stop_key).is_ok() {
            ui.stop_key_registered = true;
        }
        if ui.pill.is_none() {
            let Some(screen) = NSScreen::mainScreen(mtm) else {
                return;
            };
            let visible = screen.visibleFrame();
            let frame = NSRect::new(
                NSPoint::new(
                    visible.origin.x + visible.size.width - PILL_WIDTH - 16.0,
                    visible.origin.y + visible.size.height - PILL_HEIGHT - 12.0,
                ),
                NSSize::new(PILL_WIDTH, PILL_HEIGHT),
            );
            let panel = overlay_panel(mtm, frame);
            panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.1, 0.92)));
            let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            label.setTextColor(Some(&NSColor::whiteColor()));
            label.setFrame(NSRect::new(
                NSPoint::new(12.0, 11.0),
                NSSize::new(PILL_WIDTH - 120.0, 18.0),
            ));
            // SAFETY: the target outlives the button (kept in `Ui`).
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(&stop),
                    Some(&ui.stop_target),
                    Some(sel!(stop:)),
                    mtm,
                )
            };
            button.setFrame(NSRect::new(
                NSPoint::new(PILL_WIDTH - 104.0, 6.0),
                NSSize::new(96.0, 28.0),
            ));
            if let Some(content) = panel.contentView() {
                content.addSubview(&label);
                content.addSubview(&button);
            }
            ui.pill = Some(Pill { panel, label });
        }
        if let Some(pill) = &ui.pill {
            pill.label.setStringValue(&NSString::from_str(&text));
            pill.panel.orderFrontRegardless();
        }
    });
}

pub(super) fn hide_status() {
    with_ui(|ui, _| {
        if ui.stop_key_registered {
            let _ = ui.hotkeys.unregister(ui.stop_key);
            ui.stop_key_registered = false;
        }
        if let Some(pill) = ui.pill.take() {
            pill.panel.orderOut(None);
        }
        if let Some(marker) = &ui.marker {
            marker.orderOut(None);
        }
    });
}

pub(super) fn mark_point(x: f64, y: f64) {
    let generation = MARKER_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    with_ui(move |ui, mtm| {
        let frame = NSRect::new(
            NSPoint::new(
                x - MARKER_SIZE / 2.0,
                primary_height(mtm) - y - MARKER_SIZE / 2.0,
            ),
            NSSize::new(MARKER_SIZE, MARKER_SIZE),
        );
        let marker = ui.marker.get_or_insert_with(|| {
            let panel = overlay_panel(mtm, frame);
            panel.setIgnoresMouseEvents(true);
            panel.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0.96, 0.62, 0.04, 0.75,
            )));
            panel
        });
        marker.setFrame_display(frame, true);
        marker.orderFrontRegardless();
    });
    std::thread::spawn(move || {
        std::thread::sleep(MARKER_VISIBLE);
        if MARKER_GENERATION.load(Ordering::SeqCst) == generation {
            with_ui(|ui, _| {
                if let Some(marker) = &ui.marker {
                    marker.orderOut(None);
                }
            });
        }
    });
}

pub(super) fn confirm(strings: &Strings, title: &str, message: &str, timeout: Duration) -> bool {
    let _one_at_a_time = CONFIRMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (title, message) = (title.to_owned(), message.to_owned());
    let (allow, deny) = (strings.allow, strings.deny);
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let finished = finished.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            if !finished.load(Ordering::SeqCst) {
                run_on_main(|mtm| {
                    let app = NSApplication::sharedApplication(mtm);
                    if app.modalWindow().is_some() {
                        app.abortModal();
                    }
                });
            }
        })
    };
    let allowed = run_on_main(move |mtm| {
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&title));
        alert.setInformativeText(&NSString::from_str(&message));
        alert.addButtonWithTitle(&NSString::from_str(allow));
        alert.addButtonWithTitle(&NSString::from_str(deny));
        alert.window().setLevel(STATUS_LEVEL);
        alert.runModal() == NSAlertFirstButtonReturn
    });
    finished.store(true, Ordering::SeqCst);
    drop(watchdog);
    allowed
}
