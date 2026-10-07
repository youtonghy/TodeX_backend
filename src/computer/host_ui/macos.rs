//! macOS host UI: an accessory `NSApplication` on the main thread (no Dock
//! icon), panels excluded from screen capture, ⌘⇧⎋ through
//! `global-hotkey`, and a non-modal `NSAlert` window for confirmations
//! (a modal loop would run inside a main-queue block and starve every other
//! main-queue block, including its own timeout).

use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex,
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
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSColor,
    NSPanel, NSScreen, NSTextField, NSWindowCollectionBehavior, NSWindowSharingType,
    NSWindowStyleMask,
};
use objc2_foundation::{NSLocale, NSObject, NSPoint, NSRect, NSSize, NSString};

use super::Strings;

pub(super) const STOP_SHORTCUT: Option<&str> = Some("⌘⇧⎋");

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
    answer_target: Retained<AnswerTarget>,
    /// The confirmation on screen, if any.
    confirming: Option<Retained<NSAlert>>,
}

struct Pill {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
}

static MARKER_GENERATION: AtomicU64 = AtomicU64::new(0);
/// `windowNumber` of the marker panel; 0 before it exists.
static MARKER_WINDOW: AtomicU64 = AtomicU64::new(0);
/// One confirmation dialog at a time.
static CONFIRMING: Mutex<()> = Mutex::new(());
/// Where the on-screen confirmation sends its answer.
static ANSWER: Mutex<Option<mpsc::Sender<bool>>> = Mutex::new(None);
/// Button tags of the confirmation.
const ALLOW_TAG: isize = 1;
const DENY_TAG: isize = 2;

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

define_class!(
    // SAFETY: NSObject has no subclassing requirements and AnswerTarget
    // does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TodexComputerAnswerTarget"]
    struct AnswerTarget;

    impl AnswerTarget {
        #[unsafe(method(answer:))]
        fn answer(&self, sender: Option<&NSButton>) {
            let allowed = sender.is_some_and(|button| button.tag() == ALLOW_TAG);
            close_confirmation();
            if let Some(answer) = ANSWER
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = answer.send(allowed);
            }
        }
    }
);

impl AnswerTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: plain NSObject init.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/// Main thread: takes the confirmation off screen.
fn close_confirmation() {
    UI.with(|ui| {
        if let Some(alert) = ui.borrow_mut().as_mut().and_then(|ui| ui.confirming.take()) {
            alert.window().orderOut(None);
        }
    });
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
                    answer_target: AnswerTarget::new(mtm),
                    confirming: None,
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
        // Known once on screen (a window gets its number when ordered in).
        if let Ok(number) = u64::try_from(marker.windowNumber()) {
            MARKER_WINDOW.store(number, Ordering::SeqCst);
        }
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

pub(super) fn marker_window() -> Option<u32> {
    u32::try_from(MARKER_WINDOW.load(Ordering::SeqCst))
        .ok()
        .filter(|number| *number != 0)
}

pub(super) fn confirm(strings: &Strings, title: &str, message: &str, timeout: Duration) -> bool {
    let _one_at_a_time = CONFIRMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (sender, receiver) = mpsc::channel();
    *ANSWER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sender);
    let (title, message) = (title.to_owned(), message.to_owned());
    let (allow, deny) = (strings.allow, strings.deny);
    // Shown without a modal loop: this block returns at once and the
    // buttons answer through `ANSWER`.
    with_ui(move |ui, mtm| {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&title));
        alert.setInformativeText(&NSString::from_str(&message));
        for (label, tag) in [(allow, ALLOW_TAG), (deny, DENY_TAG)] {
            let button = alert.addButtonWithTitle(&NSString::from_str(label));
            button.setTag(tag);
            // SAFETY: the target outlives the button (kept in `Ui`).
            unsafe {
                button.setTarget(Some(&ui.answer_target));
                button.setAction(Some(sel!(answer:)));
            }
        }
        alert.layout();
        let window = alert.window();
        window.setLevel(STATUS_LEVEL);
        window.center();
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        window.makeKeyAndOrderFront(None);
        // An accessory app may not become active; show it in front anyway.
        window.orderFrontRegardless();
        ui.confirming = Some(alert);
    });
    let answer = receiver.recv_timeout(timeout);
    if answer.is_err() {
        // Nobody answered: withdraw the question.
        ANSWER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        run_on_main(|_| close_confirmation());
    }
    answer.unwrap_or(false)
}
