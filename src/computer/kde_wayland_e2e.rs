//! End-to-end Computer Use on a real KDE Plasma Wayland session, driving
//! `ci/kde-wayland/test_app.py` through the engine. It moves the real
//! pointer and keyboard, so it is ignored by default and also needs
//! `TODEX_KDE_WAYLAND_E2E=1`; `ci/kde-wayland/run.sh` provides both inside
//! a private, headless KWin (the `kde-wayland` GitHub workflow).
//! KWin captures screens (screenshots and the portal streams that pointer
//! input targets) only with OpenGL, i.e. on a GPU render node; without one
//! (`TODEX_KDE_WAYLAND_E2E_NO_GPU=1`, set by run.sh on hosted runners) the
//! test covers observation and window focus only.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Engine, Grants};
use crate::computer::ComputerError;

const MAIN_TITLE: &str = "TodeX KDE Test";
const SECOND_TITLE: &str = "TodeX KDE Second";

struct Harness {
    engine: Engine,
    gpu: bool,
    allowed: Vec<String>,
    windows: Value,
    tree: String,
}

impl Harness {
    /// Observes the test app's main window (or `window_title`).
    fn observe(&mut self, window_title: &str) -> Value {
        let listing = self
            .engine
            .observe(&json!({ "screenshot": false }), std::process::id())
            .unwrap();
        self.windows = listing["windows"].clone();
        let id = self
            .window_id(window_title)
            .unwrap_or_else(|| panic!("no window {window_title:?} in {}", listing["windows"]));
        let observed = self
            .engine
            .observe(
                &json!({ "window": id, "screenshot": self.gpu }),
                std::process::id(),
            )
            .unwrap();
        self.tree = observed["tree"].as_str().unwrap_or_default().to_owned();
        if self.allowed.is_empty() {
            self.allowed
                .push(observed["app"]["bundleId"].as_str().unwrap().to_owned());
        }
        observed
    }

    fn window_id(&self, title: &str) -> Option<u64> {
        self.windows
            .as_array()?
            .iter()
            .find(|window| window["title"] == title)
            .and_then(|window| window["id"].as_u64())
    }

    /// The ref of the first tree line containing `needle`.
    fn reference(&self, needle: &str) -> String {
        let line = self
            .tree
            .lines()
            .find(|line| line.contains(needle) && line.contains("[ref="))
            .unwrap_or_else(|| panic!("no ref for {needle:?} in\n{}", self.tree));
        let start = line.find("[ref=").unwrap() + 5;
        line[start..].split(']').next().unwrap().to_owned()
    }

    /// Acts, retrying while the (2-second granular) idle check still sees
    /// the previous action as user activity.
    fn act(&mut self, args: Value) -> Value {
        for _ in 0..4 {
            match self.engine.act(
                &args,
                Grants {
                    allowed_apps: &self.allowed,
                    confirmed: false,
                },
                std::process::id(),
            ) {
                Ok(result) => return result,
                Err(ComputerError { code, .. }) if code == "USER_ACTIVE" => {
                    std::thread::sleep(Duration::from_millis(2500));
                }
                Err(error) => panic!("{args} failed: {error}"),
            }
        }
        panic!("{args}: the user never became idle");
    }

    /// Waits until the main window's tree shows `text`.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.observe(MAIN_TITLE);
            if self.tree.contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{text:?} never appeared in\n{}",
                self.tree
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// The centre of a ref's element in the latest screenshot's pixels.
    fn screenshot_point(&self, reference: &str) -> (f64, f64) {
        let bounds = self.engine.refs[reference];
        let bounds = self.engine.elements[bounds].bounds.expect("element bounds");
        let shot = self.engine.shot.expect("a screenshot");
        (
            (f64::from(bounds.x) + f64::from(bounds.width) / 2.0 - shot.origin_x)
                / shot.points_per_pixel,
            (f64::from(bounds.y) + f64::from(bounds.height) / 2.0 - shot.origin_y)
                / shot.points_per_pixel,
        )
    }
}

#[test]
#[ignore = "drives a live KDE Plasma Wayland session; run through ci/kde-wayland/run.sh"]
fn kde_wayland_end_to_end() {
    if std::env::var_os("TODEX_KDE_WAYLAND_E2E").is_none() {
        eprintln!("set TODEX_KDE_WAYLAND_E2E=1 inside a disposable KDE session to run this");
        return;
    }
    assert_eq!(crate::computer::platform::unsupported_reason(), None);
    let mut harness = Harness {
        engine: Engine::default(),
        gpu: std::env::var_os("TODEX_KDE_WAYLAND_E2E_NO_GPU").is_none(),
        allowed: Vec::new(),
        windows: Value::Null,
        tree: String::new(),
    };

    // The app starts, AT-SPI publishes it, and its bounds become global.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(listing) = harness.engine.observe(&json!({ "screenshot": false })) {
            harness.windows = listing["windows"].clone();
            if harness.window_id(MAIN_TITLE).is_some() && harness.window_id(SECOND_TITLE).is_some()
            {
                break;
            }
        }
        assert!(Instant::now() < deadline, "the test app never appeared");
        std::thread::sleep(Duration::from_millis(500));
    }
    let observed = harness.observe(MAIN_TITLE);
    assert!(observed["window"]["width"].as_u64().unwrap() > 0);
    assert!(harness.tree.contains("\"Press me\""), "{}", harness.tree);
    if harness.gpu {
        assert!(observed["screenshot"]["width"].as_u64().unwrap() > 0);
        input(&mut harness);
    }

    // Focusing windows.
    let second = harness.window_id(SECOND_TITLE).unwrap();
    harness.act(json!({ "action": "focus_window", "window": second }));
    harness.expect("active: second");
    let main = harness.window_id(MAIN_TITLE).unwrap();
    harness.act(json!({ "action": "focus_window", "window": main }));
    harness.expect("active: main");

    crate::computer::platform::end_session();
}

/// Pointer, keyboard and scrolling through the RemoteDesktop portal.
fn input(harness: &mut Harness) {
    // Let the idle notification settle before the first pointer action.
    std::thread::sleep(Duration::from_secs(3));

    // Pointer by ref (double click is always a pointer action).
    let button = harness.reference("\"Press me\"");
    let result = harness.act(json!({ "action": "double_click", "ref": button }));
    assert_eq!(result["path"], "pointer");
    harness.expect("clicks: 2");

    // Pointer by screenshot coordinates.
    let button = harness.reference("\"Press me\"");
    let (x, y) = harness.screenshot_point(&button);
    harness.act(json!({ "action": "click", "x": x, "y": y }));
    harness.expect("clicks: 3");

    // Unicode into a field by ref (AT-SPI insertion or paste).
    let entry = harness.reference("Entry");
    harness.act(json!({ "action": "type", "ref": entry, "text": "héllo 世界" }));
    harness.expect("typed: héllo 世界");

    // Keysyms into the focused field: click it, then type without a ref.
    let entry = harness.reference("Entry");
    let (x, y) = harness.screenshot_point(&entry);
    harness.act(json!({ "action": "click", "x": x, "y": y }));
    harness.act(json!({ "action": "key", "keys": "end" }));
    harness.act(json!({ "action": "type", "text": " ok ü" }));
    harness.expect("typed: héllo 世界 ok ü");

    // A chord.
    harness.act(json!({ "action": "key", "keys": "ctrl+shift+k" }));
    harness.expect("chord: ok");

    // Scrolling over the list.
    let item = harness.reference("\"Item 3\"");
    harness.act(json!({ "action": "scroll", "ref": item, "deltaY": 5 }));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        harness.observe(MAIN_TITLE);
        if harness.tree.contains("scrolled: ") && !harness.tree.contains("scrolled: 0\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the list never scrolled:\n{}",
            harness.tree
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}
