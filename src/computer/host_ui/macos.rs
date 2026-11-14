//! macOS host UI: an accessory `NSApplication` on the main thread (no Dock
//! icon), panels excluded from screen capture, ⌘⇧⎋ through
//! `global-hotkey`, and a non-modal `NSAlert` window for confirmations
//! (a modal loop would run inside a main-queue block and starve every other
//! main-queue block, including its own timeout).

use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Mutex,
    },
    time::{Duration, Instant},
};

use dispatch2::run_on_main;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel, AnyThread, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath,
    NSButton, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSFont, NSImage, NSImageScaling, NSImageView, NSPanel,
    NSPasteboardItem, NSScreen, NSTextField, NSView, NSWindowCollectionBehavior,
    NSWindowSharingType, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSLocale, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};

use super::Strings;
use crate::computer::platform::{self, Permission};

const SHORTCUT_LABEL: &str = "⌘⇧⎋";

/// Whether the stop shortcut can be used: true until the system refuses to
/// register it (another app owns ⌘⇧⎋).
static STOP_KEY_OK: AtomicBool = AtomicBool::new(true);

/// The stop shortcut as shown to people; `None` when registering it failed.
pub(super) fn stop_shortcut() -> Option<&'static str> {
    STOP_KEY_OK.load(Ordering::SeqCst).then_some(SHORTCUT_LABEL)
}

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
    guide_target: Retained<GuideTarget>,
    /// The permission guide on screen, if any.
    guide: Option<Guide>,
}

/// The panel that offers `todex-agentd` to drag into System Settings.
struct Guide {
    panel: Retained<NSPanel>,
    purpose: Retained<NSTextField>,
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
                    guide_target: GuideTarget::new(mtm),
                    guide: None,
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
    let stop_title = strings.stop;
    with_ui(move |ui, mtm| {
        if !ui.stop_key_registered {
            match ui.hotkeys.register(ui.stop_key) {
                Ok(()) => {
                    ui.stop_key_registered = true;
                    STOP_KEY_OK.store(true, Ordering::SeqCst);
                }
                Err(error) => {
                    // Only the pill's Stop button remains.
                    STOP_KEY_OK.store(false, Ordering::SeqCst);
                    eprintln!(
                        "todex-agentd: could not register the Computer Use stop shortcut {SHORTCUT_LABEL}: {error}"
                    );
                }
            }
        }
        let stop = match stop_shortcut() {
            Some(shortcut) => format!("{stop_title} {shortcut}"),
            None => stop_title.to_owned(),
        };
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

pub(super) fn confirm(
    strings: &Strings,
    title: &str,
    message: &str,
    timeout: Duration,
) -> Option<bool> {
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
    Some(answer.unwrap_or(false))
}

const GUIDE_WIDTH: f64 = 440.0;
const GUIDE_HEIGHT: f64 = 120.0;
const GUIDE_INSET: f64 = 14.0;
/// Height of the draggable row.
const GUIDE_ROW: f64 = 44.0;
/// How often the guide follows System Settings and rereads the grants.
const GUIDE_TICK: Duration = Duration::from_millis(300);
/// A guide nobody acted on goes away.
const GUIDE_TIMEOUT: Duration = Duration::from_secs(300);
/// The guide on screen; a newer guide or closing it moves this on, which
/// ends the older guide's thread.
static GUIDE_GENERATION: AtomicU64 = AtomicU64::new(0);
/// The guide generation whose icon was last dropped somewhere that took
/// it; 0 when none was since the guide last read it.
static GUIDE_DROPPED: AtomicU64 = AtomicU64::new(0);
/// How long a drop may go without the grant showing before the guide
/// explains what is still missing.
const GUIDE_DROP_GRACE: Duration = Duration::from_secs(3);

define_class!(
    // SAFETY: NSObject has no subclassing requirements and GuideTarget
    // does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "TodexComputerGuideTarget"]
    struct GuideTarget;

    impl GuideTarget {
        #[unsafe(method(closeGuide:))]
        fn close_guide(&self, _sender: Option<&AnyObject>) {
            GUIDE_GENERATION.fetch_add(1, Ordering::SeqCst);
            super::set_permission_guide(None);
            UI.with(|ui| {
                if let Some(guide) = ui.borrow_mut().as_mut().and_then(|ui| ui.guide.take()) {
                    guide.panel.orderOut(None);
                }
            });
        }
    }
);

impl GuideTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: plain NSObject init.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

define_class!(
    // SAFETY: NSView allows subclassing, the overrides keep its contracts
    // (a drag instead of the default mouse handling), and DragRow does not
    // implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "TodexComputerDragRow"]
    struct DragRow;

    impl DragRow {
        // The panel never becomes key: the first click must start the drag.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // The whole row is the handle: its icon and name never take the
        // mouse.
        #[unsafe(method_id(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> Option<Retained<NSView>> {
            let frame = self.frame();
            let inside = point.x >= frame.origin.x
                && point.y >= frame.origin.y
                && point.x < frame.origin.x + frame.size.width
                && point.y < frame.origin.y + frame.size.height;
            inside.then(|| Retained::into_super(self.retain()))
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.drag_executable(event);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let shape = NSRect::new(
                NSPoint::new(bounds.origin.x + 0.5, bounds.origin.y + 0.5),
                NSSize::new(bounds.size.width - 1.0, bounds.size.height - 1.0),
            );
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(shape, 8.0, 8.0);
            // Opaque, so the drag image reads on a light System Settings too.
            NSColor::colorWithWhite_alpha(0.2, 1.0).setFill();
            path.fill();
            NSColor::colorWithWhite_alpha(0.38, 1.0).setStroke();
            path.setLineWidth(1.0);
            path.stroke();
        }
    }

    unsafe impl NSObjectProtocol for DragRow {}

    unsafe impl NSDraggingSource for DragRow {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(ignoreModifierKeysForDraggingSession:))]
        fn ignore_modifier_keys(&self, _session: &NSDraggingSession) -> bool {
            true
        }

        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn began(&self, _session: &NSDraggingSession, _point: NSPoint) {
            // The row travels with the pointer; a faint copy stays behind.
            self.setAlphaValue(0.35);
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            self.setAlphaValue(1.0);
            if operation != NSDragOperation::None {
                GUIDE_DROPPED.store(GUIDE_GENERATION.load(Ordering::SeqCst), Ordering::SeqCst);
            }
        }
    }
);

impl DragRow {
    /// Starts dragging this executable's file, dressed like a drag out of
    /// Finder: System Settings' privacy lists refuse a drop that only
    /// carries a file URL object and take one that carries every type
    /// Finder writes (as their "+" button would).
    fn drag_executable(&self, event: &NSEvent) {
        let Some(path) = executable_path() else {
            return;
        };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
        let Some(address) = url.absoluteString() else {
            return;
        };
        let item = NSPasteboardItem::new();
        for kind in [
            "public.file-url",
            "public.url",
            "com.apple.pasteboard.promised-file-url",
        ] {
            item.setString_forType(&address, &NSString::from_str(kind));
        }
        let path_string = NSString::from_str(&path);
        item.setString_forType(&path_string, &NSString::from_str("public.utf8-plain-text"));
        let paths = NSArray::from_slice(&[&*path_string]);
        // SAFETY: an array of strings is the property list this type holds.
        unsafe {
            item.setPropertyList_forType(&paths, &NSString::from_str("NSFilenamesPboardType"));
        }
        let dragging = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*item),
        );
        let bounds = self.bounds();
        let snapshot = self
            .bitmapImageRepForCachingDisplayInRect(bounds)
            .map(|rep| {
                self.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
                let image = NSImage::initWithSize(NSImage::alloc(), bounds.size);
                image.addRepresentation(&rep);
                image
            });
        // SAFETY: an NSImage is a valid dragging-frame content.
        unsafe {
            dragging.setDraggingFrame_contents(
                bounds,
                snapshot
                    .as_deref()
                    .map(|image| -> &AnyObject { image.as_ref() }),
            );
        }
        let items = NSArray::from_retained_slice(&[dragging]);
        let session = self.beginDraggingSessionWithItems_event_source(
            &items,
            event,
            ProtocolObject::from_ref(self),
        );
        session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    }
}

/// The file TCC knows this daemon by.
fn executable_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.to_str().map(str::to_owned)
}

fn guide_purpose(permission: Permission) -> &'static str {
    match (permission, super::chinese()) {
        // macOS applies a new Screen Recording grant to a running process
        // only once it restarts.
        (Permission::Screen, true) => "授予“屏幕录制”后，需重启 TodeX 后端才会生效",
        (Permission::Accessibility, true) => "授予“辅助功能”，Agent 才能点击和输入",
        (Permission::Screen, false) => "Screen Recording applies once the TodeX backend restarts",
        (Permission::Accessibility, false) => "Accessibility lets agents click and type",
    }
}

/// What the guide says when a drop went through but the grant still does
/// not show: macOS applies Screen Recording to a running process only once
/// it restarts, and an Accessibility entry may have landed switched off.
fn guide_after_drop(permission: Permission) -> &'static str {
    match (permission, super::chinese()) {
        (Permission::Screen, true) => "已添加？屏幕录制需重启 TodeX 后端后生效",
        (Permission::Accessibility, true) => "已添加？请确认列表中 todex-agentd 的开关已打开",
        (Permission::Screen, false) => {
            "Added? Restart the TodeX backend for Screen Recording to apply"
        }
        (Permission::Accessibility, false) => {
            "Added? Make sure todex-agentd is switched on in the list"
        }
    }
}

/// Replaces the guide's second line if `generation` is still on screen.
fn set_guide_text(generation: u64, text: &'static str) {
    with_ui(move |ui, _| {
        if GUIDE_GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        if let Some(guide) = &ui.guide {
            guide.purpose.setStringValue(&NSString::from_str(text));
        }
    });
}

fn build_guide(ui: &Ui, mtm: MainThreadMarker) -> Guide {
    let chinese = super::chinese();
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(GUIDE_WIDTH, GUIDE_HEIGHT),
    );
    let panel = overlay_panel(mtm, frame);
    panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.1, 0.94)));
    let text_width = GUIDE_WIDTH - GUIDE_INSET * 2.0;
    let title = NSTextField::labelWithString(
        &NSString::from_str(if chinese {
            "把下面这一条拖到上方的列表中"
        } else {
            "Drag the row below into the list above"
        }),
        mtm,
    );
    title.setTextColor(Some(&NSColor::whiteColor()));
    title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    title.setFrame(NSRect::new(
        NSPoint::new(GUIDE_INSET, GUIDE_HEIGHT - GUIDE_INSET - 18.0),
        NSSize::new(text_width, 18.0),
    ));
    let purpose = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    purpose.setTextColor(Some(&NSColor::colorWithWhite_alpha(1.0, 0.7)));
    purpose.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    purpose.setFrame(NSRect::new(
        NSPoint::new(GUIDE_INSET, GUIDE_HEIGHT - GUIDE_INSET - 38.0),
        NSSize::new(text_width, 16.0),
    ));

    // The draggable row, shaped like an entry of the list it goes into.
    let close_width = 68.0;
    let row_width = GUIDE_WIDTH - GUIDE_INSET * 3.0 - close_width;
    let row: Retained<DragRow> = {
        let frame = NSRect::new(
            NSPoint::new(GUIDE_INSET, GUIDE_INSET),
            NSSize::new(row_width, GUIDE_ROW),
        );
        // SAFETY: NSView's designated initialiser.
        unsafe { msg_send![DragRow::alloc(mtm), initWithFrame: frame] }
    };
    let icon_size = 28.0;
    let icon_inset = (GUIDE_ROW - icon_size) / 2.0;
    if let Some(path) = executable_path() {
        let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&path));
        let icon = NSImageView::imageViewWithImage(&image, mtm);
        icon.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        icon.setFrame(NSRect::new(
            NSPoint::new(icon_inset + 2.0, icon_inset),
            NSSize::new(icon_size, icon_size),
        ));
        row.addSubview(&icon);
    }
    let name = NSTextField::labelWithString(&NSString::from_str("todex-agentd"), mtm);
    name.setTextColor(Some(&NSColor::whiteColor()));
    name.setFont(Some(&NSFont::boldSystemFontOfSize(14.0)));
    let name_x = icon_inset * 2.0 + icon_size + 4.0;
    name.setFrame(NSRect::new(
        NSPoint::new(name_x, (GUIDE_ROW - 18.0) / 2.0),
        NSSize::new(row_width - name_x - 12.0, 18.0),
    ));
    row.addSubview(&name);

    // SAFETY: the target outlives the button (kept in `Ui`).
    let close = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(if chinese { "关闭" } else { "Close" }),
            Some(&ui.guide_target),
            Some(sel!(closeGuide:)),
            mtm,
        )
    };
    close.setFrame(NSRect::new(
        NSPoint::new(
            GUIDE_WIDTH - GUIDE_INSET - close_width,
            GUIDE_INSET + (GUIDE_ROW - 28.0) / 2.0,
        ),
        NSSize::new(close_width, 28.0),
    ));
    if let Some(content) = panel.contentView() {
        content.addSubview(&title);
        content.addSubview(&purpose);
        content.addSubview(&row);
        content.addSubview(&close);
    }
    Guide { panel, purpose }
}

/// Main thread: puts the guide under System Settings' window (inside its
/// bottom edge when there is no room below), or in the middle of the main
/// screen while that window is not up.
fn place_guide(guide: &Guide, settings: Option<(f64, f64, f64, f64)>, mtm: MainThreadMarker) {
    let origin = match settings {
        Some((x, y, width, height)) => {
            let bottom = primary_height(mtm) - (y + height);
            let centre = NSPoint::new(x + width / 2.0, bottom + height / 2.0);
            let floor = NSScreen::screens(mtm)
                .iter()
                .find(|screen| {
                    let frame = screen.frame();
                    centre.x >= frame.origin.x
                        && centre.x < frame.origin.x + frame.size.width
                        && centre.y >= frame.origin.y
                        && centre.y < frame.origin.y + frame.size.height
                })
                .map_or(0.0, |screen| screen.visibleFrame().origin.y);
            let below = bottom - 8.0 - GUIDE_HEIGHT;
            NSPoint::new(
                x + (width - GUIDE_WIDTH) / 2.0,
                if below >= floor { below } else { bottom + 16.0 },
            )
        }
        None => {
            let Some(screen) = NSScreen::mainScreen(mtm) else {
                return;
            };
            let visible = screen.visibleFrame();
            NSPoint::new(
                visible.origin.x + (visible.size.width - GUIDE_WIDTH) / 2.0,
                visible.origin.y + (visible.size.height - GUIDE_HEIGHT) / 2.0,
            )
        }
    };
    let current = guide.panel.frame().origin;
    if (current.x - origin.x).abs() > 0.5 || (current.y - origin.y).abs() > 0.5 {
        guide.panel.setFrameOrigin(origin);
    }
}

/// Shows the guide for `permission` if `generation` is still the current
/// one.
fn present_guide(generation: u64, permission: Permission) {
    let settings = platform::settings_window().flatten();
    with_ui(move |ui, mtm| {
        if GUIDE_GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        if ui.guide.is_none() {
            ui.guide = Some(build_guide(ui, mtm));
        }
        if let Some(guide) = &ui.guide {
            guide
                .purpose
                .setStringValue(&NSString::from_str(guide_purpose(permission)));
            place_guide(guide, settings, mtm);
            guide.panel.orderFrontRegardless();
        }
        super::set_permission_guide(Some(permission));
    });
}

/// Takes the guide off screen if `generation` is still the current one.
fn dismiss_guide(generation: u64) {
    with_ui(move |ui, _| {
        if GUIDE_GENERATION
            .compare_exchange(
                generation,
                generation + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        super::set_permission_guide(None);
        if let Some(guide) = ui.guide.take() {
            guide.panel.orderOut(None);
        }
    });
}

pub(super) fn show_permission_guide(mut missing: Vec<Permission>) {
    let Some(first) = missing.first().copied() else {
        return;
    };
    let generation = GUIDE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    present_guide(generation, first);
    let spawned = std::thread::Builder::new()
        .name("todex-permission-guide".to_owned())
        .spawn(move || {
            let opened = Instant::now();
            // System Settings takes a moment to start; only its quitting
            // after it was seen ends the guide.
            let mut settings_seen = false;
            // When the icon was dropped for the permission asked for now,
            // and whether the guide already said what is still missing.
            let mut dropped_at: Option<Instant> = None;
            let mut explained = false;
            loop {
                std::thread::sleep(GUIDE_TICK);
                if GUIDE_GENERATION.load(Ordering::SeqCst) != generation {
                    return;
                }
                let granted = platform::permissions();
                let before = missing.len();
                missing.retain(|permission| !permission.granted(granted));
                let settings = platform::settings_window();
                settings_seen |= settings.is_some();
                let Some(current) = missing.first().copied() else {
                    return dismiss_guide(generation);
                };
                if opened.elapsed() > GUIDE_TIMEOUT || (settings_seen && settings.is_none()) {
                    return dismiss_guide(generation);
                }
                if missing.len() != before {
                    // The one being asked for was granted: on to the next.
                    dropped_at = None;
                    explained = false;
                    GUIDE_DROPPED.store(0, Ordering::SeqCst);
                    platform::open_privacy_pane(current);
                    present_guide(generation, current);
                    continue;
                }
                if GUIDE_DROPPED
                    .compare_exchange(generation, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    dropped_at = Some(Instant::now());
                }
                if !explained && dropped_at.is_some_and(|at| at.elapsed() >= GUIDE_DROP_GRACE) {
                    explained = true;
                    set_guide_text(generation, guide_after_drop(current));
                }
                let settings = settings.flatten();
                with_ui(move |ui, mtm| {
                    if GUIDE_GENERATION.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    if let Some(guide) = &ui.guide {
                        place_guide(guide, settings, mtm);
                    }
                });
            }
        });
    if let Err(error) = spawned {
        eprintln!("todex-agentd: could not follow the permission guide: {error}");
        dismiss_guide(generation);
    }
}
