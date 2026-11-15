//! The blocking half of Computer Use: reads accessibility trees and
//! screenshots through xa11y, resolves what an action touches, applies
//! [`policy`], and performs it. Runs on blocking threads behind a mutex;
//! refs and the screenshot mapping belong to the latest observation.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use xa11y::{
    input::{InputSim, MouseButton, ScrollDelta},
    ElementData, Point, Provider, Rect, Screenshot, ScreenshotProvider,
};

use super::{
    agent_desktop_engine::{self as external, Cli, ExternalElement, RefAction},
    keys, platform,
    platform::{Display, Typed},
    policy::{self, ShotMapping, Target},
    tree, ComputerError,
};

const SCREENSHOT_MAX_WIDTH: u32 = 1280;
const SCREENSHOT_QUALITY: u8 = 70;
const MAX_WINDOWS: usize = 40;
pub(super) const MAX_NODES: usize = 4000;
const MAX_DEPTH: usize = 60;
const MAX_WAIT_MS: u64 = 10_000;
/// How long an app just brought forward may take to own the keyboard.
const FRONT_SETTLE: Duration = Duration::from_millis(500);
/// Typed text goes out in pieces of this many characters.
const TYPE_CHUNK_CHARS: usize = 32;

/// A window listed by the latest observation; agents refer to it by `id`.
struct WindowEntry {
    /// Its window-server number where the OS has one (stable for the
    /// window's life, macOS), else its place in the list.
    id: u64,
    /// Window-server number, when known.
    number: Option<u32>,
    app: Target,
    title: String,
    /// Bounds in global coordinates.
    element: ElementData,
    /// What moves its subtree's AT-SPI bounds into global coordinates
    /// (KDE Wayland; zero elsewhere).
    offset: (i32, i32),
    /// Shown on the current desktop: not in another Space (a full-screen
    /// app there included) or minimized.
    on_screen: bool,
}

/// What a screenshot shows.
enum Shot {
    /// A global rectangle; with `only_pid`, that process's windows alone.
    Region { rect: Rect, only_pid: Option<u32> },
    /// One window on its own, wherever it is; `bounds` is its frame.
    Window { number: u32, bounds: Rect },
}

#[derive(Default)]
pub(super) struct Engine {
    provider: Option<Arc<dyn Provider>>,
    /// The same provider as `provider`, for what only macOS has (windows in
    /// other Spaces by number).
    #[cfg(target_os = "macos")]
    mac: Option<Arc<xa11y_macos::MacOSProvider>>,
    screenshots: Option<Arc<dyn ScreenshotProvider>>,
    input: Option<Arc<InputSim>>,
    elements: Vec<ElementData>,
    refs: HashMap<String, usize>,
    windows: Vec<WindowEntry>,
    shot: Option<ShotMapping>,
    /// The app the latest observation looked at: what a ref without a
    /// process id belongs to.
    observed: Option<Target>,
    /// The screen lease the observation above belongs to.
    lease: u64,
    /// Elements of the latest observation when agent-desktop read its tree
    /// (`refs` index these instead of `elements` then).
    external: Vec<ExternalElement>,
    external_observed: bool,
    /// The latest observation named an app or window (not a display): its
    /// refs act on that window alone, never through the pointer.
    window_mode: bool,
    /// The window observed is not on the current screen, so pointer input
    /// cannot reach it.
    off_screen: bool,
    /// Index into `windows` of the window observed.
    observed_window: Option<usize>,
    /// Window elements seen on the current desktop, by window-server
    /// number. macOS lists a window's accessibility element only while the
    /// window is in the current Space; one already held keeps working when
    /// the window is elsewhere (TodeX full screen in its own Space), so
    /// these reach such windows. Not per observation: kept until the window
    /// closes.
    known_windows: HashMap<u32, ElementData>,
}

/// What `act` may touch, decided by the daemon (never by the agent).
pub(super) struct Grants<'a> {
    pub allowed_apps: &'a [String],
    pub confirmed: bool,
    /// Set when the caller stopped waiting (timeout, cancellation): input
    /// not sent yet is dropped.
    pub cancelled: &'a AtomicBool,
}

impl Engine {
    /// Forgets the latest observation (a new conversation took the screen).
    pub(super) fn reset(&mut self) {
        self.elements.clear();
        self.refs.clear();
        self.windows.clear();
        self.shot = None;
        self.observed = None;
        self.external.clear();
        self.external_observed = false;
        self.window_mode = false;
        self.off_screen = false;
        self.observed_window = None;
    }

    /// A call of screen lease `lease` starts: another lease's refs and
    /// screenshot mapping are not its own.
    pub(super) fn enter(&mut self, lease: u64) {
        if self.lease != lease {
            self.reset();
            self.lease = lease;
        }
    }

    fn provider(&mut self) -> Result<Arc<dyn Provider>, ComputerError> {
        require_permissions(false)?;
        if let Some(provider) = &self.provider {
            return Ok(provider.clone());
        }
        // Built here rather than through `xa11y::provider()`, which would
        // cache a failure (missing permission) for the process lifetime.
        #[cfg(target_os = "macos")]
        let provider: Arc<dyn Provider> = {
            let mac = Arc::new(xa11y_macos::MacOSProvider::new().map_err(ComputerError::platform)?);
            self.mac = Some(mac.clone());
            mac
        };
        #[cfg(not(target_os = "macos"))]
        let provider = native_provider().map_err(ComputerError::platform)?;
        self.provider = Some(provider.clone());
        Ok(provider)
    }

    /// `pid`'s windows numbered `numbers` in another Space, which the
    /// accessibility API does not list (macOS).
    #[cfg(target_os = "macos")]
    fn windows_by_number(&self, pid: u32, numbers: &[u32]) -> Vec<ElementData> {
        self.mac
            .as_ref()
            .map(|mac| mac.windows_by_number(pid, numbers))
            .unwrap_or_default()
    }

    #[cfg(not(target_os = "macos"))]
    fn windows_by_number(&self, _pid: u32, _numbers: &[u32]) -> Vec<ElementData> {
        Vec::new()
    }

    fn screenshots(&mut self) -> Result<Arc<dyn ScreenshotProvider>, ComputerError> {
        require_permissions(true)?;
        if let Some(screenshots) = &self.screenshots {
            return Ok(screenshots.clone());
        }
        let screenshots = native_screenshots().map_err(ComputerError::platform)?;
        self.screenshots = Some(screenshots.clone());
        Ok(screenshots)
    }

    fn input(&mut self) -> Result<Arc<InputSim>, ComputerError> {
        require_permissions(false)?;
        if let Some(input) = &self.input {
            return Ok(input.clone());
        }
        let input = Arc::new(native_input().map_err(ComputerError::platform)?);
        self.input = Some(input.clone());
        Ok(input)
    }

    // ---- Observe --------------------------------------------------------

    /// Protected apps (TodeX itself, [`policy::is_blocked`]) stay unseen:
    /// naming one is refused, its tree is never read, its window titles
    /// are withheld and its windows are painted over in screenshots.
    /// `cli`: agent-desktop reads the tree instead (its engine is selected).
    pub(super) fn observe(
        &mut self,
        args: &Value,
        own_pid: u32,
        cli: Option<&Cli>,
        cancelled: &AtomicBool,
    ) -> Result<Value, ComputerError> {
        let provider = self.provider()?;
        self.reset();
        let display = args["display"].as_u64();
        // The process a window id or app name points to: its windows in
        // other Spaces are looked up too (the accessibility API lists only
        // the current desktop's).
        let named_pid = match (args["window"].as_u64(), args["app"].as_str()) {
            (Some(id), _) => platform::window_states().and_then(|states| {
                states
                    .into_iter()
                    .find(|state| u64::from(state.number) == id)
                    .map(|state| state.pid)
            }),
            (None, Some(identifier)) => platform::running_app(identifier).map(|app| app.pid),
            (None, None) => None,
        };
        self.list_windows(&provider, named_pid);
        let states = platform::window_states();
        let target_window = args["window"]
            .as_u64()
            .and_then(|id| self.windows.iter().position(|window| window.id == id));
        // A window in another Space whose elements this daemon never held
        // (see `known_windows`): only the window server knows it.
        let mut unseen = match (args["window"].as_u64(), target_window) {
            (Some(id), None) => Some(
                states
                    .iter()
                    .flatten()
                    .find(|state| u64::from(state.number) == id)
                    .cloned()
                    .ok_or_else(|| {
                        ComputerError::invalid(format!("window {id} is not open; observe again"))
                    })?,
            ),
            _ => None,
        };
        let app = match (target_window, args["app"].as_str()) {
            (Some(index), _) => self.windows[index].app.clone(),
            (None, _) if unseen.is_some() => {
                platform::app_identity(unseen.as_ref().map_or(0, |state| state.pid))
            }
            (None, Some(identifier)) => platform::running_app(identifier).ok_or_else(|| {
                ComputerError::invalid(format!("{identifier} is not running; use open_app"))
            })?,
            (None, None) => frontmost(&provider).or_else(|error| {
                // The accessibility API names no front app while apps
                // switch; the window server still knows it.
                platform::frontmost_pid()
                    .map(platform::app_identity)
                    .filter(|app| app.pid != 0)
                    .ok_or(error)
            })?,
        };
        let protected = policy::is_protected(&app, own_pid);
        if protected && (target_window.is_some() || unseen.is_some() || args["app"].is_string()) {
            return Err(ComputerError::from(policy::blocked(label(&app))));
        }
        self.observed = Some(app.clone());
        // An app or window named (not a display): work on that window alone.
        let window_mode = display.is_none()
            && (target_window.is_some() || unseen.is_some() || args["app"].is_string());
        // The front app is protected: show the display without its tree.
        // Its window on the current screen, else its first one.
        let window_index = target_window.or_else(|| {
            let mine = |window: &&WindowEntry| !protected && window.app.pid == app.pid;
            self.windows
                .iter()
                .position(|window| mine(&window) && window.on_screen)
                .or_else(|| self.windows.iter().position(|window| mine(&window)))
        });
        if window_mode && window_index.is_none() && unseen.is_none() && !protected {
            unseen = states
                .iter()
                .flatten()
                .find(|state| state.pid == app.pid && state.is_main())
                .cloned();
        }
        // A window in another Space (a full-screen app there) or minimized,
        // or a named app with no window at all: pointer input cannot reach it.
        let off_screen = match window_index {
            Some(index) => !self.windows[index].on_screen,
            None => (args["app"].is_string() || unseen.is_some()) && !protected,
        };
        self.window_mode = window_mode;
        self.off_screen = off_screen;
        self.observed_window = window_index;
        // Said to the agent above the tree: why it is not agent-desktop's,
        // or what the screenshot and the pointer can reach.
        let mut notes: Vec<String> = Vec::new();
        let (tree_text, truncated) = if protected {
            (
                format!(
                    "({} is protected: its contents are hidden. Name another app or window to observe it.)",
                    label(&app)
                ),
                false,
            )
        } else if let Some(state) = &unseen {
            (
                format!(
                    "({}'s window is in another desktop Space and has not been on this screen since TodeX started, so macOS does not let its contents be read. The screenshot shows the window. Bring it forward once with computer_act focus_window (window {}); after that it can be read and used in the background even when it is back in its Space.)",
                    label(&app),
                    state.number
                ),
                false,
            )
        } else {
            // agent-desktop lists only windows on the current desktop.
            let external = match cli.filter(|_| !off_screen) {
                Some(cli) => {
                    let title = window_index.map(|index| self.windows[index].title.clone());
                    match cli.snapshot(&app, title.as_deref(), cancelled) {
                        Ok(snapshot) => Some(snapshot),
                        Err(error) if error.code == "CANCELLED" => return Err(error),
                        // A window agent-desktop cannot read yet (a
                        // Chromium app builds its tree on first contact)
                        // or does not list: the built-in reader may.
                        Err(error) => {
                            notes.push(format!(
                                "agent-desktop could not read this window ({}: {}); this tree comes from the built-in engine.",
                                error.code, error.message
                            ));
                            None
                        }
                    }
                }
                None => None,
            };
            match external {
                Some(snapshot) => {
                    let formatted = tree::format(&snapshot.root);
                    self.refs = formatted.refs;
                    self.external = snapshot.elements;
                    self.external_observed = true;
                    (formatted.text, formatted.truncated || snapshot.truncated)
                }
                None => {
                    let root = match window_index {
                        Some(index) => Ok(self.windows[index].element.clone()),
                        None => provider.app_by_pid(app.pid),
                    };
                    match root {
                        Ok(root) => {
                            let offset =
                                window_index.map_or((0, 0), |index| self.windows[index].offset);
                            let mut budget = MAX_NODES;
                            let node = self.read_tree(&provider, root, 0, offset, &mut budget);
                            let formatted = tree::format(&node);
                            self.refs = formatted.refs;
                            (formatted.text, formatted.truncated || budget == 0)
                        }
                        // The screenshot still shows it: act by coordinates.
                        Err(error) => (
                            format!(
                                "({}'s accessibility tree could not be read: {error}. Use the screenshot and x/y coordinates.)",
                                label(&app)
                            ),
                            false,
                        ),
                    }
                }
            }
        };
        if off_screen && unseen.is_none() {
            notes.push(match window_index {
                Some(index) if self.windows[index].element.states.minimized == Some(true) => format!(
                    "{}'s window is minimized: it cannot be shown or reached until it is restored. Bring it back with computer_act focus_window, then observe again.",
                    label(&app)
                ),
                Some(_) => format!(
                    "{}'s window is not on the current screen (it is in another desktop Space or a full-screen app there). The screenshot shows the window itself and refs act on it in the background; pointer input (x/y, hover, drag, scroll, double_click) cannot reach it until computer_act focus_window brings it forward.",
                    label(&app)
                ),
                None => format!(
                    "{} has no window open, so there is nothing to show or reach; open one (open_app) or observe another app.",
                    label(&app)
                ),
            });
        }
        let tree_text = if notes.is_empty() {
            tree_text
        } else {
            format!("{}\n\n{tree_text}", notes.join("\n"))
        };

        let displays = platform::displays();
        // In window mode other apps' windows are not the agent's business:
        // only their names are listed.
        let listed = |window: &&WindowEntry| !window_mode || window.app.pid == app.pid;
        let mut windows: Vec<Value> = self.windows.iter().filter(listed).take(30).map(|window| json!({
            "id": window.id,
            "app": window.app.name,
            "bundleId": window.app.id,
            "title": if policy::is_protected(&window.app, own_pid) { "" } else { &window.title },
            "onScreen": window.on_screen,
        })).collect();
        let unseen_json = unseen.as_ref().map(|state| {
            json!({
                "id": state.number,
                "app": app.name,
                "bundleId": app.id,
                "title": state.title,
                "onScreen": false,
                "x": state.x.round(),
                "y": state.y.round(),
                "width": state.width.round(),
                "height": state.height.round(),
            })
        });
        if let Some(window) = &unseen_json {
            windows.insert(0, window.clone());
        }
        let mut result = json!({
            "app": app_json(&app),
            "windows": windows,
            "displays": displays,
            "tree": tree_text,
            "truncated": truncated,
        });
        if window_mode {
            let mut others: Vec<&str> = Vec::new();
            for window in &self.windows {
                if window.app.pid != app.pid
                    && !policy::is_protected(&window.app, own_pid)
                    && !window.app.name.is_empty()
                    && !others.contains(&window.app.name.as_str())
                {
                    others.push(&window.app.name);
                }
            }
            result["otherApps"] = json!(others);
        }
        if let Some(index) = window_index {
            let window = &self.windows[index];
            let bounds = window.element.bounds;
            result["window"] = json!({
                "id": window.id,
                "app": window.app.name,
                "bundleId": window.app.id,
                "title": window.title,
                "onScreen": window.on_screen,
                "x": bounds.map(|b| b.x).unwrap_or(0),
                "y": bounds.map(|b| b.y).unwrap_or(0),
                "width": bounds.map(|b| b.width).unwrap_or(0),
                "height": bounds.map(|b| b.height).unwrap_or(0),
            });
        }
        if let Some(window) = unseen_json {
            result["window"] = window;
        }
        if args["screenshot"].as_bool() != Some(false) {
            let shot = match &unseen {
                Some(state) => Some(Shot::Window {
                    number: state.number,
                    bounds: Rect {
                        x: state.x.round() as i32,
                        y: state.y.round() as i32,
                        width: state.width.round().max(1.0) as u32,
                        height: state.height.round().max(1.0) as u32,
                    },
                }),
                None => self.shot_for(display, window_mode, window_index, &app, &displays)?,
            };
            if let Some(shot) = shot {
                let (screenshot, mapping) = self.capture(shot, own_pid)?;
                self.shot = Some(mapping);
                result["screenshot"] = screenshot;
            }
        }
        Ok(result)
    }

    /// What an observation's screenshot shows: the display asked for; in
    /// window mode the app's own windows over the window's frame (its menus
    /// and sheets included, other apps left out), or the window alone when
    /// it is in another Space; otherwise the screen over the window. `None`
    /// when there is nothing to show (no window, minimized).
    fn shot_for(
        &self,
        display: Option<u64>,
        window_mode: bool,
        window_index: Option<usize>,
        app: &Target,
        displays: &[Display],
    ) -> Result<Option<Shot>, ComputerError> {
        if let Some(index) = display {
            let display = displays
                .get(index as usize)
                .ok_or_else(|| ComputerError::invalid(format!("no display {index}")))?;
            return Ok(Some(Shot::Region {
                rect: display_rect(display),
                only_pid: None,
            }));
        }
        let window = window_index.map(|index| &self.windows[index]);
        let bounds = window
            .and_then(|window| window.element.bounds)
            .filter(|bounds| bounds.width > 0 && bounds.height > 0);
        if window_mode {
            let (Some(window), Some(bounds)) = (window, bounds) else {
                return Ok(None);
            };
            if window.element.states.minimized == Some(true) {
                return Ok(None);
            }
            return Ok(Some(match (window.on_screen, window.number) {
                (true, _) => Shot::Region {
                    rect: clip_to_display(bounds, displays),
                    only_pid: Some(app.pid),
                },
                (false, Some(number)) => Shot::Window { number, bounds },
                (false, None) => return Ok(None),
            }));
        }
        if window.is_some_and(|window| !window.on_screen) {
            return Ok(None);
        }
        let rect = bounds
            .map(|bounds| clip_to_display(bounds, displays))
            .or_else(|| displays.first().map(display_rect))
            .ok_or_else(|| ComputerError::platform("no display to capture"))?;
        Ok(Some(Shot::Region {
            rect,
            only_pid: None,
        }))
    }

    /// Running apps as `@app:` mentions name them, with the front app's
    /// pid; protected apps (TodeX, [`policy::is_blocked`]) left out, as
    /// observations hide them. Leaves the latest observation alone.
    pub(super) fn running_apps(
        &mut self,
        own_pid: u32,
    ) -> Result<(Vec<Target>, Option<u32>), ComputerError> {
        let provider = self.provider()?;
        let front = provider.focused_app().ok().and_then(|app| app.pid);
        let apps = provider.list_apps().map_err(ComputerError::platform)?;
        let running = apps
            .into_iter()
            .filter_map(|app| {
                let identity = platform::app_identity(app.pid?);
                if policy::is_protected(&identity, own_pid) {
                    return None;
                }
                Some(Target {
                    id: platform::listed_id(&identity),
                    name: if identity.name.is_empty() {
                        app.name.unwrap_or_default()
                    } else {
                        identity.name
                    },
                    pid: identity.pid,
                })
            })
            .collect();
        Ok((running, front))
    }

    /// Windows of every app (`named_pid`'s first, then the front app's),
    /// with those of `named_pid` in other Spaces, which the accessibility
    /// API leaves out. Ids are window-server numbers where the OS has them,
    /// else places in the list from 1.
    fn list_windows(&mut self, provider: &Arc<dyn Provider>, named_pid: Option<u32>) {
        let front = provider.focused_app().ok().and_then(|app| app.pid);
        let mut apps = provider.list_apps().unwrap_or_default();
        apps.sort_by_key(|app| {
            (
                app.pid != named_pid || named_pid.is_none(),
                app.pid != front,
            )
        });
        let states = platform::window_states();
        let on_screen = |number: Option<u32>| match (&states, number) {
            (Some(states), Some(number)) => states
                .iter()
                .find(|state| state.number == number)
                .is_none_or(|state| state.on_screen),
            _ => true,
        };
        // Closed windows are forgotten.
        if let Some(states) = &states {
            self.known_windows
                .retain(|number, _| states.iter().any(|state| state.number == *number));
        }
        let mut windows = Vec::new();
        for app in apps {
            let Some(pid) = app.pid else { continue };
            let identity = platform::app_identity(pid);
            let identity = Target {
                name: if identity.name.is_empty() {
                    app.name.clone().unwrap_or_default()
                } else {
                    identity.name
                },
                ..identity
            };
            let mut elements = provider.app_windows(&app).unwrap_or_default();
            for element in &elements {
                if let Some(number) = window_number(element) {
                    self.known_windows.insert(number, element.clone());
                }
            }
            if Some(pid) == named_pid {
                elements.extend(self.other_space_windows(pid, &elements, states.as_deref()));
            }
            for window in elements {
                if windows.len() >= MAX_WINDOWS {
                    break;
                }
                let number = window_number(&window);
                windows.push(WindowEntry {
                    id: number.map_or(windows.len() as u64 + 1, u64::from),
                    number,
                    app: identity.clone(),
                    title: window.name.clone().unwrap_or_default(),
                    on_screen: on_screen(number),
                    element: window,
                    offset: (0, 0),
                });
            }
        }
        let elements: Vec<ElementData> = windows
            .iter()
            .map(|window| window.element.clone())
            .collect();
        for (window, offset) in windows.iter_mut().zip(platform::window_offsets(&elements)) {
            window.offset = offset;
            platform::globalize(&mut window.element, offset);
        }
        self.windows = windows;
    }

    /// `pid`'s windows the window server knows but `listed` (its
    /// accessibility windows) lacks: those in another desktop Space, as
    /// seen on this desktop before, else found by number.
    fn other_space_windows(
        &self,
        pid: u32,
        listed: &[ElementData],
        states: Option<&[platform::WindowState]>,
    ) -> Vec<ElementData> {
        let Some(states) = states else {
            return Vec::new();
        };
        let known: Vec<u32> = listed.iter().filter_map(window_number).collect();
        let missing: Vec<u32> = states
            .iter()
            .filter(|state| {
                state.pid == pid
                    && !state.on_screen
                    && state.is_main()
                    && !known.contains(&state.number)
            })
            .map(|state| state.number)
            .collect();
        let mut found: Vec<ElementData> = missing
            .iter()
            .filter_map(|number| self.known_windows.get(number))
            .filter(|element| element.pid == Some(pid))
            .cloned()
            .collect();
        let unknown: Vec<u32> = missing
            .into_iter()
            .filter(|number| {
                !found
                    .iter()
                    .any(|element| window_number(element) == Some(*number))
            })
            .collect();
        if !unknown.is_empty() {
            found.extend(self.windows_by_number(pid, &unknown));
        }
        found
    }

    /// Reads a subtree; `offset` moves descendants' bounds into global
    /// coordinates (the root's are already).
    fn read_tree(
        &mut self,
        provider: &Arc<dyn Provider>,
        mut element: ElementData,
        depth: usize,
        offset: (i32, i32),
        budget: &mut usize,
    ) -> tree::Node {
        if depth > 0 {
            platform::globalize(&mut element, offset);
        }
        *budget = budget.saturating_sub(1);
        let handle = self.elements.len();
        let mut node = tree::Node {
            role: element.role.to_snake_case().to_owned(),
            name: element.name.clone(),
            value: element.value.clone(),
            enabled: element.states.enabled,
            focused: element.states.focused,
            selected: element.states.selected,
            secure: platform::is_secure(&element) || is_secure_role(&element),
            actionable: !element.actions.is_empty(),
            bounds: element
                .bounds
                .map(|b| (b.x, b.y, b.width as i32, b.height as i32)),
            handle,
            children: Vec::new(),
        };
        self.elements.push(element.clone());
        if depth < MAX_DEPTH && *budget > 0 {
            for child in provider.get_children(Some(&element)).unwrap_or_default() {
                if *budget == 0 {
                    break;
                }
                let child = self.read_tree(provider, child, depth + 1, offset, budget);
                node.children.push(child);
            }
        }
        node
    }

    /// Takes `shot`. Pictures that may show any app have protected apps
    /// painted over; one app's windows alone, or one window, need not.
    fn take_shot(
        &mut self,
        shot: &Shot,
        own_pid: u32,
    ) -> Result<(Rect, Screenshot), ComputerError> {
        require_permissions(true)?;
        Ok(match *shot {
            Shot::Region { rect, only_pid } => {
                let area = (
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.width),
                    f64::from(rect.height),
                );
                let (mut picture, filtered) = match platform::capture_region(area, only_pid) {
                    Some(result) => (result.map_err(capture_failed)?, only_pid.is_some()),
                    // Elsewhere: the whole screen content, never filtered.
                    None => (
                        self.screenshots()?
                            .capture_region(rect)
                            .map_err(ComputerError::capture)?,
                        false,
                    ),
                };
                if !filtered {
                    self.hide_protected(&mut picture, rect, own_pid)?;
                }
                (rect, picture)
            }
            Shot::Window { number, bounds } => {
                let picture = platform::capture_window(number)
                    .unwrap_or_else(|| Err("this computer cannot capture one window".to_owned()))
                    .map_err(capture_failed)?;
                (bounds, picture)
            }
        })
    }

    fn capture(&mut self, shot: Shot, own_pid: u32) -> Result<(Value, ShotMapping), ComputerError> {
        let (rect, shot) = self.take_shot(&shot, own_pid)?;
        let shot = if shot.width > SCREENSHOT_MAX_WIDTH {
            let height = (u64::from(shot.height) * u64::from(SCREENSHOT_MAX_WIDTH)
                / u64::from(shot.width))
            .max(1) as u32;
            shot.resize(SCREENSHOT_MAX_WIDTH, height)
                .map_err(ComputerError::platform)?
        } else {
            shot
        };
        let jpeg = encode_jpeg(&shot.pixels, shot.width, shot.height)?;
        let mapping = ShotMapping {
            origin_x: f64::from(rect.x),
            origin_y: f64::from(rect.y),
            points_per_pixel: f64::from(rect.width) / f64::from(shot.width.max(1)),
            width: shot.width,
            height: shot.height,
        };
        Ok((
            json!({
                "mimeType": "image/jpeg",
                "data": BASE64.encode(jpeg),
                "width": shot.width,
                "height": shot.height,
                "originX": mapping.origin_x,
                "originY": mapping.origin_y,
                "pointsPerPixel": mapping.points_per_pixel,
            }),
            mapping,
        ))
    }

    /// Paints over protected apps' visible windows. Where the platform
    /// cannot list windows, a capture that may show one is refused.
    fn hide_protected(
        &self,
        shot: &mut Screenshot,
        rect: Rect,
        own_pid: u32,
    ) -> Result<(), ComputerError> {
        let mut known: HashMap<u32, bool> = HashMap::new();
        let mut protected = |pid: u32| {
            *known.entry(pid).or_insert_with(|| {
                pid != 0 && policy::is_protected(&platform::app_identity(pid), own_pid)
            })
        };
        let capture = (
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.width),
            f64::from(rect.height),
        );
        let Some(stack) = platform::window_stack() else {
            let shown = self.windows.iter().find(|window| {
                protected(window.app.pid)
                    && window.element.bounds.is_some_and(|bounds| {
                        overlaps(
                            capture,
                            (
                                f64::from(bounds.x),
                                f64::from(bounds.y),
                                f64::from(bounds.width),
                                f64::from(bounds.height),
                            ),
                        )
                    })
            });
            return match shown {
                Some(window) => Err(ComputerError {
                    code: "TARGET_BLOCKED".to_owned(),
                    message: format!(
                        "{} is on screen and this computer cannot hide it from a screenshot; \
                         observe with screenshot: false or ask the user to move it away.",
                        label(&window.app)
                    ),
                    // The app's name is screen text; the MCP layer fences it.
                    detail: Some(json!({ "label": label(&window.app) })),
                }),
                None => Ok(()),
            };
        };
        let areas = policy::protected_areas(&stack, &mut protected);
        let (width, height) = (shot.width, shot.height);
        policy::paint_areas(&mut shot.pixels, width, height, capture, &areas);
        Ok(())
    }

    /// A JPEG of the display a session works on, for live viewers. Protected
    /// apps are painted over as in `observe`: viewers never see more than
    /// the agent does.
    pub(super) fn frame(
        &mut self,
        max_width: u32,
        quality: u8,
        own_pid: u32,
    ) -> Result<Vec<u8>, ComputerError> {
        let displays = platform::displays();
        let display = self
            .shot
            .and_then(|shot| {
                displays
                    .iter()
                    .find(|display| display.contains(shot.origin_x, shot.origin_y))
            })
            .or_else(|| displays.first())
            .ok_or_else(|| ComputerError::platform("no display to capture"))?;
        let rect = display_rect(display);
        let (_, shot) = self.take_shot(
            &Shot::Region {
                rect,
                only_pid: None,
            },
            own_pid,
        )?;
        let shot = if shot.width > max_width {
            let height = (u64::from(shot.height) * u64::from(max_width) / u64::from(shot.width))
                .max(1) as u32;
            shot.resize(max_width, height)
                .map_err(ComputerError::platform)?
        } else {
            shot
        };
        encode_jpeg_quality(&shot.pixels, shot.width, shot.height, quality)
    }

    // ---- Act --------------------------------------------------------------

    /// `cli`: the agent-desktop engine is selected; refs of an observation
    /// it read go to it.
    pub(super) fn act(
        &mut self,
        args: &Value,
        grants: Grants<'_>,
        own_pid: u32,
        cli: Option<&Cli>,
    ) -> Result<Value, ComputerError> {
        let action = args["action"].as_str().unwrap_or_default().to_owned();
        let cancelled = grants.cancelled;
        if action != "wait" {
            live(cancelled)?;
        }
        let chord = if action == "key" {
            let chord = keys::parse(args["keys"].as_str().unwrap_or_default())
                .map_err(ComputerError::invalid)?;
            if let Some(what) = keys::system_shortcut(&chord) {
                return Err(ComputerError::new(
                    "TARGET_BLOCKED",
                    format!(
                        "{} is a system shortcut ({what}) that agents may not send.",
                        args["keys"].as_str().unwrap_or_default()
                    ),
                ));
            }
            Some(chord)
        } else {
            None
        };
        let provider = if action == "wait" || action == "open_app" {
            None
        } else {
            Some(self.provider()?)
        };
        let named = RefAction::from_action(&action);
        let reference = args["ref"]
            .as_str()
            .filter(|reference| !reference.is_empty());
        let unknown = |reference: &str| {
            ComputerError::invalid(format!(
                "unknown ref {reference}; refs come from the latest computer_observe"
            ))
        };
        let (element, external) = match reference {
            Some(reference) if self.external_observed => (
                None,
                Some(
                    self.refs
                        .get(reference)
                        .and_then(|handle| self.external.get(*handle))
                        .cloned()
                        .ok_or_else(|| unknown(reference))?,
                ),
            ),
            Some(reference) => (
                Some(
                    self.refs
                        .get(reference)
                        .and_then(|handle| self.elements.get(*handle))
                        .cloned()
                        .ok_or_else(|| unknown(reference))?,
                ),
                None,
            ),
            None => (None, None),
        };
        // Its ref, when agent-desktop gave the element one.
        let external_ref = external
            .as_ref()
            .map(|element| element.reference.clone())
            .filter(|reference| !reference.is_empty());
        let external_cli = cli.filter(|_| external_ref.is_some());
        if named.is_some() && reference.is_none() {
            return Err(ComputerError::invalid(format!(
                "{action} needs a ref from the latest computer_observe"
            )));
        }
        let point = self.map_point(args["x"].as_f64(), args["y"].as_f64())?;
        let to = self.map_point(args["toX"].as_f64(), args["toY"].as_f64())?;
        // What the latest observation allows the pointer: nothing when its
        // window is not on this screen; in window mode, only coordinates
        // the agent gave (a ref never falls back to the pointer there).
        let (window_mode, off_screen) = (self.window_mode, self.off_screen);
        let observed_label = self
            .observed
            .as_ref()
            .map(|app| label(app).to_owned())
            .unwrap_or_default();
        let pointer = |explicit: bool| -> Result<(), ComputerError> {
            if off_screen {
                return Err(needs_pointer(&observed_label, true));
            }
            if window_mode && !explicit {
                return Err(needs_pointer(&observed_label, false));
            }
            Ok(())
        };

        let target = if action == "wait" {
            Target::default()
        } else {
            // A ref agent-desktop read belongs to the app observed.
            let target = match (&external, &self.observed) {
                (Some(_), Some(observed)) => observed.clone(),
                _ => self.target(&action, args, element.as_ref(), point, provider.as_ref())?,
            };
            if let Some(failure) = policy::check_target(&target, grants.allowed_apps, own_pid) {
                return Err(ComputerError::from(failure));
            }
            target
        };
        // Pointer input lands wherever the pointer is: right before it is
        // sent, the app under it must pass the policy too (see
        // `policy::check_landing`).
        let landing = |at: (f64, f64), agent_point: bool| {
            check_landing(
                at,
                &target,
                grants.allowed_apps,
                agent_point,
                own_pid,
                cancelled,
            )
        };
        if policy::uses_pointer(&action, point.is_some()) {
            if let Some(failure) =
                policy::check_user_active(&action, point.is_some(), platform::idle_seconds())
            {
                return Err(ComputerError::from(failure));
            }
        }
        let center = element
            .as_ref()
            .and_then(|element| element.bounds)
            .map(|bounds| {
                (
                    f64::from(bounds.x) + f64::from(bounds.width) / 2.0,
                    f64::from(bounds.y) + f64::from(bounds.height) / 2.0,
                )
            })
            .or_else(|| external.as_ref().and_then(ExternalElement::center));
        // agent-desktop performs a ref action in the background; when it
        // could not, `Ok(false)` lets the built-in engine try.
        let by_cli = |command: RefAction, text: Option<&str>| -> Result<bool, ComputerError> {
            let (Some(cli), Some(reference)) = (external_cli, external_ref.as_deref()) else {
                return Ok(false);
            };
            if point.is_some() {
                return Ok(false);
            }
            live(cancelled)?;
            match cli.act(command, reference, text, cancelled) {
                Ok(()) => Ok(true),
                Err(error) if external::may_fall_back(&error) => {
                    tracing::debug!(%error, "agent-desktop could not perform the action; trying the built-in engine");
                    Ok(false)
                }
                Err(error) => Err(error),
            }
        };
        // The built-in element a ref stands for: its own, or the one at the
        // same place as the element agent-desktop read.
        let native = || -> Option<ElementData> {
            element.clone().or_else(|| {
                let external = external.as_ref()?;
                self.native_at(provider.as_ref()?, external)
            })
        };
        let pointer_at = point.or(center);
        // Said to the agent with the result.
        let mut note: Option<&'static str> = None;

        let _injecting = (!matches!(action.as_str(), "wait" | "open_app" | "focus_window"))
            .then(platform::injecting);
        let path = match action.as_str() {
            "click" | "right_click" if point.is_none() && reference.is_some() => {
                let command = if action == "click" {
                    RefAction::Click
                } else {
                    RefAction::RightClick
                };
                let done = by_cli(command, None)?
                    || match (native(), provider.as_ref()) {
                        (Some(native), Some(provider)) => {
                            live(cancelled)?;
                            native_action(provider, &native, &action, None)?
                        }
                        _ => false,
                    };
                if done {
                    mark_background(center, off_screen);
                    "background"
                } else {
                    // No way without the pointer.
                    pointer(false)?;
                    let at = require_point(pointer_at)?;
                    landing(at, false)?;
                    super::host_ui::mark_point(at.0, at.1);
                    let button = if action == "click" {
                        MouseButton::Left
                    } else {
                        MouseButton::Right
                    };
                    self.input()?
                        .backend()
                        .pointer_click(to_point(at), button, 1)
                        .map_err(ComputerError::platform)?;
                    "pointer"
                }
            }
            _ if named.is_some() => {
                let named = named.expect("named action");
                let text = args["text"].as_str();
                let secure = external.as_ref().is_some_and(|element| element.secure)
                    || element.as_ref().is_some_and(|element| {
                        platform::is_secure(element) || is_secure_role(element)
                    });
                if named == RefAction::SetValue && secure {
                    return Err(ComputerError::invalid(
                        "set_value cannot fill password fields; use type",
                    ));
                }
                if named.takes_text() && text.is_none() && named != RefAction::Select {
                    return Err(ComputerError::invalid(format!("{action} needs text")));
                }
                // agent-desktop first; a toggle whose state did not change
                // reached the control all the same (a "switch" that is a
                // menu button opens its menu).
                let mut done = match (external_cli, external_ref.as_deref()) {
                    (Some(cli), Some(reference)) => {
                        live(cancelled)?;
                        match cli.act(named, reference, text, cancelled) {
                            Ok(()) => true,
                            Err(error)
                                if named == RefAction::Toggle
                                    && external::state_unchanged(&error) =>
                            {
                                note = Some("the control did not change state: it may have opened a menu or popover instead; observe to see what it shows, then select or click an item");
                                true
                            }
                            Err(error) if external::may_fall_back(&error) => false,
                            Err(error) => return Err(error),
                        }
                    }
                    _ => false,
                };
                if !done {
                    if let (Some(native), Some(provider)) = (native(), provider.as_ref()) {
                        live(cancelled)?;
                        done = native_action(provider, &native, &action, text)?;
                    }
                }
                if !done {
                    return Err(ComputerError::new(
                        "ACTION_NOT_SUPPORTED",
                        format!("{action} is not available for this element; observe again and try click, or another element"),
                    ));
                }
                mark_background(center, off_screen);
                "background"
            }
            "type" if external.is_some() => {
                let element = external.as_ref().expect("external ref");
                if element.secure && !grants.confirmed {
                    return Err(sensitive());
                }
                // Focus through agent-desktop, then type natively: the text
                // never reaches a command line.
                let (Some(cli), Some(reference)) = (external_cli, external_ref.as_deref()) else {
                    return Err(ComputerError::invalid(
                        "that element cannot take text; type into a text field's ref",
                    ));
                };
                live(cancelled)?;
                cli.act(RefAction::Focus, reference, None, cancelled)?;
                self.type_text(
                    args["text"].as_str().unwrap_or_default(),
                    &target,
                    None,
                    provider.as_ref(),
                    &grants,
                    own_pid,
                )?
            }
            "click" | "right_click" => {
                pointer(point.is_some())?;
                let at = require_point(pointer_at)?;
                landing(at, point.is_some())?;
                super::host_ui::mark_point(at.0, at.1);
                let button = if action == "click" {
                    MouseButton::Left
                } else {
                    MouseButton::Right
                };
                self.input()?
                    .backend()
                    .pointer_click(to_point(at), button, 1)
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            // Inherently pointer actions: allowed on refs in window mode too.
            "double_click" => {
                pointer(true)?;
                let at = require_point(pointer_at)?;
                landing(at, point.is_some())?;
                super::host_ui::mark_point(at.0, at.1);
                self.input()?
                    .backend()
                    .pointer_click(to_point(at), MouseButton::Left, 2)
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "hover" => {
                pointer(true)?;
                let at = require_point(pointer_at)?;
                landing(at, point.is_some())?;
                self.input()?
                    .mouse()
                    .move_to(to_point(at))
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "drag" => {
                pointer(true)?;
                let from = require_point(pointer_at)?;
                let to = to.ok_or_else(|| ComputerError::invalid("drag needs toX and toY"))?;
                landing(from, point.is_some())?;
                landing(to, false)?;
                super::host_ui::mark_point(from.0, from.1);
                self.input()?
                    .mouse()
                    .drag(to_point(from), to_point(to))
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "scroll" => {
                pointer(true)?;
                let delta = ScrollDelta::new(
                    args["deltaX"].as_f64().unwrap_or(0.0).round() as i32,
                    args["deltaY"].as_f64().unwrap_or(0.0).round() as i32,
                );
                let at = match pointer_at {
                    Some(at) => at,
                    None => self.default_point()?,
                };
                landing(at, point.is_some())?;
                self.input()?
                    .mouse()
                    .scroll(to_point(at), delta)
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "type" => self.type_text(
                args["text"].as_str().unwrap_or_default(),
                &target,
                element.as_ref(),
                provider.as_ref(),
                &grants,
                own_pid,
            )?,
            "key" => {
                let chord = chord.ok_or_else(|| ComputerError::invalid("key needs keys"))?;
                // The app the policy checked, not whatever is in front now.
                let pid = Some(target.pid).filter(|pid| *pid != 0).ok_or_else(|| {
                    ComputerError::new("TARGET_CHANGED", "no app to send the keys to")
                })?;
                live(cancelled)?;
                if platform::post_chord(pid, &chord).map_err(ComputerError::platform)? {
                    "background"
                } else {
                    platform::activate(pid, None).map_err(ComputerError::platform)?;
                    check_front(provider.as_ref(), pid, own_pid, cancelled)?;
                    self.input()?
                        .keyboard()
                        .chord(chord.key.clone(), &chord.held)
                        .map_err(ComputerError::platform)?;
                    "keyboard"
                }
            }
            "wait" => {
                let ms = args["ms"].as_u64().unwrap_or(1000).min(MAX_WAIT_MS);
                std::thread::sleep(Duration::from_millis(ms));
                "none"
            }
            "open_app" => {
                live(cancelled)?;
                platform::open_app(args["app"].as_str().unwrap_or_default())
                    .map_err(ComputerError::invalid)?;
                "background"
            }
            "focus_window" => {
                live(cancelled)?;
                if let Some(id) = args["window"].as_u64() {
                    match self.windows.iter().find(|window| window.id == id) {
                        Some(window) => {
                            // Raising it switches to its Space and un-minimizes it.
                            let _ = provider
                                .as_ref()
                                .map(|provider| provider.activate(&window.element));
                            platform::activate(window.app.pid, Some(&window.title))
                                .map_err(ComputerError::platform)?;
                        }
                        // In another Space and never held: its app comes
                        // forward, macOS switches to the Space it is in.
                        None => {
                            let pid = window_owner(id).ok_or_else(|| {
                                ComputerError::invalid(format!(
                                    "window {id} is not open; observe again"
                                ))
                            })?;
                            platform::activate(pid, None).map_err(ComputerError::platform)?;
                        }
                    }
                } else {
                    let identifier = args["app"].as_str().unwrap_or_default();
                    let app = platform::running_app(identifier).ok_or_else(|| {
                        ComputerError::invalid(format!("{identifier} is not running"))
                    })?;
                    platform::activate(app.pid, None).map_err(ComputerError::platform)?;
                }
                "background"
            }
            other => return Err(ComputerError::invalid(format!("unknown action {other}"))),
        };
        let app = element
            .as_ref()
            .and_then(|element| element.pid)
            .map(platform::app_identity)
            .or_else(|| external.as_ref().and(self.observed.clone()))
            .or_else(|| args["app"].as_str().and_then(platform::running_app))
            .or_else(|| {
                point
                    .and_then(|(x, y)| platform::app_at(x, y))
                    .map(platform::app_identity)
            })
            .or_else(|| {
                provider
                    .as_ref()
                    .and_then(|provider| provider.focused_app().ok())
                    .and_then(|app| app.pid)
                    .map(platform::app_identity)
            });
        let mut result =
            json!({ "app": app.as_ref().map(app_json).unwrap_or_else(|| json!({})), "path": path });
        if let Some(note) = note {
            result["note"] = json!(note);
        }
        Ok(result)
    }

    /// The built-in element at the same place, and of the same kind, as
    /// `external` (read by agent-desktop) in the window observed, so the
    /// built-in engine can act where agent-desktop could not.
    fn native_at(
        &self,
        provider: &Arc<dyn Provider>,
        external: &ExternalElement,
    ) -> Option<ElementData> {
        let (x, y, width, height) = external.bounds?;
        let root = match self.observed_window {
            Some(index) => self.windows.get(index)?.element.clone(),
            None => provider.app_by_pid(self.observed.as_ref()?.pid).ok()?,
        };
        let near = |a: i32, b: f64| (f64::from(a) - b).abs() <= 1.5;
        let same_place = |element: &ElementData| {
            element.bounds.is_some_and(|bounds| {
                near(bounds.x, x)
                    && near(bounds.y, y)
                    && near(bounds.width as i32, width)
                    && near(bounds.height as i32, height)
            })
        };
        let mut fallback = None;
        let mut queue = std::collections::VecDeque::from([root]);
        let mut budget = MAX_NODES;
        while let Some(element) = queue.pop_front() {
            if budget == 0 {
                break;
            }
            budget -= 1;
            if same_place(&element) {
                if element.role.to_snake_case() == external.role {
                    return Some(element);
                }
                fallback.get_or_insert(element.clone());
            }
            queue.extend(provider.get_children(Some(&element)).unwrap_or_default());
        }
        fallback
    }

    fn map_point(
        &self,
        x: Option<f64>,
        y: Option<f64>,
    ) -> Result<Option<(f64, f64)>, ComputerError> {
        let (Some(x), Some(y)) = (x, y) else {
            return Ok(None);
        };
        let mapping = self.shot.as_ref().ok_or_else(|| {
            ComputerError::invalid("x/y refer to a screenshot; call computer_observe first")
        })?;
        policy::to_screen_point(mapping, x, y)
            .map(Some)
            .ok_or_else(|| {
                ComputerError::invalid(format!("({x}, {y}) is outside the latest screenshot"))
            })
    }

    /// Centre of the latest screenshot, else of the primary display.
    fn default_point(&self) -> Result<(f64, f64), ComputerError> {
        if let Some(shot) = &self.shot {
            return Ok((
                shot.origin_x + f64::from(shot.width) * shot.points_per_pixel / 2.0,
                shot.origin_y + f64::from(shot.height) * shot.points_per_pixel / 2.0,
            ));
        }
        platform::displays()
            .first()
            .map(|display| {
                (
                    display.x + display.width / 2.0,
                    display.y + display.height / 2.0,
                )
            })
            .ok_or_else(|| ComputerError::invalid("scroll needs a ref or x and y"))
    }

    /// The app an action would touch, for the policy check.
    fn target(
        &self,
        action: &str,
        args: &Value,
        element: Option<&ElementData>,
        point: Option<(f64, f64)>,
        provider: Option<&Arc<dyn Provider>>,
    ) -> Result<Target, ComputerError> {
        if action == "open_app" {
            let identifier = args["app"].as_str().unwrap_or_default();
            return platform::installed_app(identifier)
                .ok_or_else(|| ComputerError::invalid(format!("no app named {identifier}")));
        }
        if let Some(element) = element {
            if let Some(app) = ref_app(element.pid, self.observed.as_ref(), platform::app_identity)
            {
                return Ok(app);
            }
        }
        if let Some((x, y)) = point {
            return Ok(platform::app_at(x, y)
                .map(platform::app_identity)
                .unwrap_or_default());
        }
        if action == "focus_window" {
            if let Some(id) = args["window"].as_u64() {
                return self
                    .windows
                    .iter()
                    .find(|window| window.id == id)
                    .map(|window| window.app.clone())
                    .or_else(|| window_owner(id).map(platform::app_identity))
                    .ok_or_else(|| ComputerError::invalid(format!("window {id} is not open")));
            }
        }
        if let Some(identifier) = args["app"].as_str() {
            return platform::running_app(identifier)
                .ok_or_else(|| ComputerError::invalid(format!("{identifier} is not running")));
        }
        match provider {
            Some(provider) => frontmost(provider),
            None => Ok(Target::default()),
        }
    }

    /// Inserts text without keystrokes where possible, so input methods
    /// cannot rewrite it and the app stays in the background.
    fn type_text(
        &mut self,
        text: &str,
        target: &Target,
        element: Option<&ElementData>,
        provider: Option<&Arc<dyn Provider>>,
        grants: &Grants<'_>,
        own_pid: u32,
    ) -> Result<&'static str, ComputerError> {
        let (confirmed, cancelled) = (grants.confirmed, grants.cancelled);
        live(cancelled)?;
        let provider =
            provider.ok_or_else(|| ComputerError::platform("no accessibility provider"))?;
        // Set once the app accepted inserted text but did not show it, as
        // web editors (Electron, browsers) do: inserting again is pointless.
        let mut ignored = false;
        if let Some(element) = element {
            if (platform::is_secure(element) || is_secure_role(element)) && !confirmed {
                return Err(sensitive());
            }
            let focused = provider.focus(element).is_ok();
            // Only a focused ref is the element whose text can be read back.
            let read = || {
                element
                    .pid
                    .filter(|_| focused)
                    .and_then(platform::focused_text)
            };
            let before = read();
            if provider.type_text(element, text).is_ok() {
                if platform::insertion_settled(
                    before.as_deref(),
                    read,
                    text,
                    platform::INSERT_SETTLE,
                ) {
                    return Ok("background");
                }
                tracing::debug!("the app ignored the inserted text; typing it instead");
                ignored = true;
            }
            // A focused field known not to be a password field: the
            // platform may paste text its keystrokes cannot type.
            let secure = platform::is_secure(element) || is_secure_role(element);
            if let (true, false, Some(pid)) = (focused, secure, element.pid) {
                live(cancelled)?;
                if platform::paste_text(pid, text).map_err(ComputerError::platform)? {
                    return Ok("keyboard");
                }
            }
        }
        // The app the policy checked, not whatever is in front now.
        let pid = typing_pid(element.and_then(|element| element.pid), target);
        // Never typed blind: keystrokes go to whatever has the keyboard.
        let pid = pid.ok_or_else(|| {
            ComputerError::new(
                "TARGET_CHANGED",
                "cannot tell which app would receive the text; observe again",
            )
        })?;
        if !ignored {
            match platform::type_into_focused(pid, text, confirmed) {
                Typed::Inserted => return Ok("background"),
                Typed::Secure => return Err(sensitive()),
                Typed::Unsupported => {}
            }
        }
        platform::activate(pid, None).map_err(ComputerError::platform)?;
        check_front(Some(provider), pid, own_pid, cancelled)?;
        // Enter can submit and clear a field, Tab move focus elsewhere:
        // then an unchanged value says nothing about the typing.
        let before = if text.contains(['\n', '\r', '\t']) {
            None
        } else {
            platform::focused_text(pid)
        };
        let keyboard = self.input()?;
        // In chunks, so a stop (or a confirmation opening) ends a long text
        // within a moment instead of after the last character.
        for chunk in text_chunks(text, TYPE_CHUNK_CHARS) {
            live(cancelled)?;
            keyboard
                .keyboard()
                .type_text(chunk)
                .map_err(ComputerError::platform)?;
        }
        if !platform::insertion_settled(
            before.as_deref(),
            || platform::focused_text(pid),
            text,
            platform::KEYSTROKE_SETTLE,
        ) {
            return Err(ComputerError::new(
                "TYPE_NOT_APPLIED",
                "the focused field's text did not change after typing; observe to check before typing again",
            ));
        }
        Ok("keyboard")
    }
}

/// Whether input may still be sent: the caller has not given up, and no
/// host confirmation is open (on Linux it is another process, so the hit
/// test cannot tell it is ours, and only the person at the host may
/// answer it).
fn live(cancelled: &AtomicBool) -> Result<(), ComputerError> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(ComputerError::new(
            "CANCELLED",
            "the call timed out or was cancelled before its input was sent",
        ));
    }
    if super::host_ui::confirming() {
        return Err(ComputerError::new(
            "TARGET_BLOCKED",
            "a TodeX confirmation is open on this computer; only the person there may answer it. Retry after it closes.",
        ));
    }
    Ok(())
}

/// Why the pointer may not be used: the window observed is not on this
/// screen, or (window mode) a ref has no way to be acted on in the
/// background, which never silently becomes a click of the user's pointer.
fn needs_pointer(app: &str, off_screen: bool) -> ComputerError {
    let label = if app.is_empty() { "The app" } else { app };
    ComputerError {
        code: "NEEDS_POINTER".to_owned(),
        message: if off_screen {
            format!("{label}'s window is not on the current screen, so pointer input cannot reach it. Use refs (they act in the background), or bring the window forward with computer_act focus_window and observe again.")
        } else {
            format!("this element of {label} cannot be acted on in the background. Pass x and y from the screenshot to use the pointer there, or observe a display (display: 0) to work on the whole screen.")
        },
        // The app's name is screen text; the MCP layer fences it.
        detail: Some(json!({ "label": label })),
    }
}

/// Shows where a background action landed, as pointer clicks do, when the
/// element is on this screen.
fn mark_background(center: Option<(f64, f64)>, off_screen: bool) {
    if let (Some((x, y)), false) = (center, off_screen) {
        super::host_ui::mark_point(x, y);
    }
}

/// Rows and cells: clicked by selecting them (outline and table rows
/// publish no press action).
fn is_selectable(element: &ElementData) -> bool {
    matches!(
        element.role.to_snake_case(),
        "tree_item" | "table_row" | "list_item" | "table_cell"
    )
}

/// Performs a ref action on a built-in element without the pointer;
/// `Ok(false)` when the element has no way to.
fn native_action(
    provider: &Arc<dyn Provider>,
    element: &ElementData,
    action: &str,
    text: Option<&str>,
) -> Result<bool, ComputerError> {
    let attempt = |result: xa11y::Result<()>| -> Result<bool, ComputerError> {
        match result {
            Ok(()) => Ok(true),
            Err(xa11y::Error::ActionNotSupported { .. }) => Ok(false),
            Err(error) => Err(ComputerError::platform(error)),
        }
    };
    // Selecting is tried on the element and its row: a failure there only
    // means "not this way".
    let selected = |element: &ElementData| match provider.select(element) {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, "selecting the element did not work");
            false
        }
    };
    let has = |verb: &str| element.actions.iter().any(|name| name == verb);
    match action {
        "click" => {
            if has("press") {
                return attempt(provider.press(element));
            }
            if is_selectable(element) && selected(element) {
                return Ok(true);
            }
            Ok(match provider.get_parent(element) {
                Ok(Some(parent)) if is_selectable(&parent) => selected(&parent),
                _ => false,
            })
        }
        "right_click" if has("show_menu") => attempt(provider.show_menu(element)),
        "right_click" => Ok(false),
        "toggle" => attempt(provider.toggle(element)),
        "expand" | "collapse" => {
            let expand = action == "expand";
            let result = if expand {
                provider.expand(element)
            } else {
                provider.collapse(element)
            };
            if attempt(result)? {
                return Ok(true);
            }
            // Section headers disclose through a button inside them.
            match disclosure_button(provider, element) {
                Some((_, open)) if open == expand => Ok(true),
                Some((button, _)) => attempt(provider.press(&button)),
                None => Ok(false),
            }
        }
        "set_value" => attempt(provider.set_value(element, text.unwrap_or_default())),
        "select" => match text {
            Some(text) => {
                let option = find_descendant(provider, element, |candidate| {
                    candidate.name.as_deref() == Some(text)
                        || candidate.value.as_deref() == Some(text)
                });
                match option {
                    Some(option) if option.actions.iter().any(|name| name == "press") => {
                        attempt(provider.press(&option))
                    }
                    Some(option) => Ok(selected(&option)),
                    None => Ok(false),
                }
            }
            None => Ok(selected(element)),
        },
        "scroll_to" => attempt(provider.scroll_into_view(element)),
        _ => Ok(false),
    }
}

/// A disclosure control inside `element` and whether it shows its content.
fn disclosure_button(
    provider: &Arc<dyn Provider>,
    element: &ElementData,
) -> Option<(ElementData, bool)> {
    let button = find_descendant(provider, element, |candidate| {
        matches!(
            candidate.role.to_snake_case(),
            "button" | "disclosure_triangle"
        ) && (candidate.states.expanded.is_some()
            || matches!(candidate.value.as_deref(), Some("Expanded" | "Collapsed")))
    })?;
    let open = button.states.expanded == Some(true) || button.value.as_deref() == Some("Expanded");
    Some((button, open))
}

/// The first descendant of `root` (breadth first) that `wanted` accepts.
fn find_descendant(
    provider: &Arc<dyn Provider>,
    root: &ElementData,
    wanted: impl Fn(&ElementData) -> bool,
) -> Option<ElementData> {
    const BUDGET: usize = 500;
    let mut queue: std::collections::VecDeque<ElementData> =
        provider.get_children(Some(root)).unwrap_or_default().into();
    let mut seen = 0;
    while let Some(element) = queue.pop_front() {
        seen += 1;
        if seen > BUDGET {
            return None;
        }
        if wanted(&element) {
            return Some(element);
        }
        queue.extend(provider.get_children(Some(&element)).unwrap_or_default());
    }
    None
}

/// The app a ref belongs to: its own process, else the app the latest
/// observation looked at.
fn ref_app(
    element_pid: Option<u32>,
    observed: Option<&Target>,
    identify: impl FnOnce(u32) -> Target,
) -> Option<Target> {
    element_pid.map(identify).or_else(|| observed.cloned())
}

/// The process keystrokes go to: the ref's, else the checked target's.
fn typing_pid(element_pid: Option<u32>, target: &Target) -> Option<u32> {
    element_pid.or(Some(target.pid).filter(|pid| *pid != 0))
}

/// `text` in pieces of at most `size` characters.
fn text_chunks(text: &str, size: usize) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .char_indices()
            .nth(size)
            .map_or(rest.len(), |(index, _)| index);
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        Some(chunk)
    })
}

fn changed() -> ComputerError {
    ComputerError::new(
        "TARGET_CHANGED",
        "a different app came to the front before the input was sent; observe again",
    )
}

/// See `landing` in [`Engine::act`].
fn check_landing(
    at: (f64, f64),
    target: &Target,
    allowed_apps: &[String],
    agent_point: bool,
    own_pid: u32,
    cancelled: &AtomicBool,
) -> Result<(), ComputerError> {
    live(cancelled)?;
    let landing = platform::app_at(at.0, at.1).map(platform::app_identity);
    if landing.is_none() {
        tracing::debug!(
            x = at.0,
            y = at.1,
            "no app found under the pointer target; relying on the target check"
        );
    }
    match policy::check_landing(landing, target, allowed_apps, agent_point, own_pid) {
        Some(failure) => Err(ComputerError::from(failure)),
        None => Ok(()),
    }
}

/// Waits briefly for `expected` to own the keyboard after activation;
/// refuses if a protected app has it, aborts if another app does.
fn check_front(
    provider: Option<&Arc<dyn Provider>>,
    expected: u32,
    own_pid: u32,
    cancelled: &AtomicBool,
) -> Result<(), ComputerError> {
    let deadline = Instant::now() + FRONT_SETTLE;
    loop {
        live(cancelled)?;
        let front = provider
            .and_then(|provider| provider.focused_app().ok())
            .and_then(|app| app.pid);
        if let Some(pid) = front {
            let app = platform::app_identity(pid);
            if policy::is_protected(&app, own_pid) {
                return Err(ComputerError::from(policy::blocked(label(&app))));
            }
            if pid == expected {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(changed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn label(app: &Target) -> &str {
    match (app.name.is_empty(), app.id.is_empty()) {
        (false, _) => &app.name,
        (true, false) => &app.id,
        (true, true) => "This app",
    }
}

fn overlaps(a: policy::Area, b: policy::Area) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn sensitive() -> ComputerError {
    ComputerError::new("SENSITIVE_ACTION", "typing into a password field")
}

/// AT-SPI reports password fields by role.
fn is_secure_role(element: &ElementData) -> bool {
    element.raw.get("atspi_role").and_then(Value::as_str) == Some("password text")
}

/// The process owning window-server window `number`.
fn window_owner(number: u64) -> Option<u32> {
    platform::window_states()?
        .into_iter()
        .find(|state| u64::from(state.number) == number)
        .map(|state| state.pid)
}

/// A window element's window-server number (macOS), when it has one.
fn window_number(window: &ElementData) -> Option<u32> {
    window
        .raw
        .get("ax_window_number")
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}

/// A failed screenshot of this module's own capture (see
/// [`ComputerError::capture`] for xa11y's).
fn capture_failed(error: String) -> ComputerError {
    let denied = error.contains("Screen Recording");
    match platform::signing_hint().filter(|_| denied) {
        Some(hint) => ComputerError::new("EXECUTOR_FAILED", format!("{error} {hint}")),
        None => ComputerError::platform(error),
    }
}

fn frontmost(provider: &Arc<dyn Provider>) -> Result<Target, ComputerError> {
    let app = provider.focused_app().map_err(ComputerError::platform)?;
    let pid = app
        .pid
        .ok_or_else(|| ComputerError::platform("the front app has no process id"))?;
    let identity = platform::app_identity(pid);
    Ok(Target {
        name: if identity.name.is_empty() {
            app.name.unwrap_or_default()
        } else {
            identity.name
        },
        ..identity
    })
}

fn require_permissions(screen: bool) -> Result<(), ComputerError> {
    if let Some(reason) = platform::unsupported_reason() {
        return Err(ComputerError::new("UNSUPPORTED", reason));
    }
    let permissions = platform::permissions();
    if !permissions.accessibility || (screen && !permissions.screen) {
        return Err(ComputerError::new(
            "PERMISSION_REQUIRED",
            "The TodeX backend needs Screen Recording and Accessibility on this computer; \
             ask the user to grant them in TodeX Settings → Computer Use (or the system settings).",
        ));
    }
    Ok(())
}

fn require_point(point: Option<(f64, f64)>) -> Result<(f64, f64), ComputerError> {
    point.ok_or_else(|| ComputerError::invalid("this action needs a ref with a frame, or x and y"))
}

fn to_point((x, y): (f64, f64)) -> Point {
    Point::new(x.round() as i32, y.round() as i32)
}

fn app_json(app: &Target) -> Value {
    json!({ "name": app.name, "bundleId": app.id, "pid": app.pid })
}

fn display_rect(display: &Display) -> Rect {
    Rect {
        x: display.x.round() as i32,
        y: display.y.round() as i32,
        width: display.width.round().max(1.0) as u32,
        height: display.height.round().max(1.0) as u32,
    }
}

/// The part of a window on its display (windows may hang off screen).
fn clip_to_display(bounds: Rect, displays: &[Display]) -> Rect {
    let center = (
        f64::from(bounds.x) + f64::from(bounds.width) / 2.0,
        f64::from(bounds.y) + f64::from(bounds.height) / 2.0,
    );
    let Some(display) = displays
        .iter()
        .find(|display| display.contains(center.0, center.1))
        .or_else(|| displays.first())
    else {
        return bounds;
    };
    let left = f64::from(bounds.x).max(display.x);
    let top = f64::from(bounds.y).max(display.y);
    let right = (f64::from(bounds.x) + f64::from(bounds.width)).min(display.x + display.width);
    let bottom = (f64::from(bounds.y) + f64::from(bounds.height)).min(display.y + display.height);
    if right <= left || bottom <= top {
        return display_rect(display);
    }
    Rect {
        x: left.round() as i32,
        y: top.round() as i32,
        width: (right - left).round().max(1.0) as u32,
        height: (bottom - top).round().max(1.0) as u32,
    }
}

fn encode_jpeg(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ComputerError> {
    encode_jpeg_quality(rgba, width, height, SCREENSHOT_QUALITY)
}

fn encode_jpeg_quality(
    rgba: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, ComputerError> {
    let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) else {
        return Err(ComputerError::platform("screenshot too large to encode"));
    };
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, quality)
        .encode(rgba, width, height, jpeg_encoder::ColorType::Rgba)
        .map_err(|error| ComputerError::platform(format!("JPEG encoding failed: {error}")))?;
    Ok(jpeg)
}

#[cfg(target_os = "macos")]
fn native_screenshots() -> xa11y::Result<Arc<dyn ScreenshotProvider>> {
    Ok(Arc::new(xa11y_macos::MacOSScreenshot::new()?))
}
#[cfg(target_os = "macos")]
fn native_input() -> xa11y::Result<InputSim> {
    Ok(InputSim::new(Arc::new(
        xa11y_macos::MacOSInputProvider::new()?,
    )))
}

#[cfg(target_os = "windows")]
fn native_provider() -> xa11y::Result<Arc<dyn Provider>> {
    Ok(Arc::new(xa11y_windows::WindowsProvider::new()?))
}
#[cfg(target_os = "windows")]
fn native_screenshots() -> xa11y::Result<Arc<dyn ScreenshotProvider>> {
    Ok(Arc::new(xa11y_windows::WindowsScreenshot::new()?))
}
#[cfg(target_os = "windows")]
fn native_input() -> xa11y::Result<InputSim> {
    Ok(InputSim::new(Arc::new(
        xa11y_windows::WindowsInputProvider::new()?,
    )))
}

#[cfg(target_os = "linux")]
fn native_provider() -> xa11y::Result<Arc<dyn Provider>> {
    Ok(Arc::new(xa11y_linux::LinuxProvider::new()?))
}
/// KDE Wayland captures through KWin (xa11y would use the portal for the
/// whole screen); X11 through xa11y.
#[cfg(target_os = "linux")]
fn native_screenshots() -> xa11y::Result<Arc<dyn ScreenshotProvider>> {
    if let Some(screenshots) = platform::wayland_screenshots() {
        return Ok(screenshots);
    }
    Ok(Arc::new(xa11y_linux::LinuxScreenshot::new()?))
}
/// KDE Wayland input goes through the RemoteDesktop portal (xa11y would
/// need `/dev/uinput`); X11 through XTest.
#[cfg(target_os = "linux")]
fn native_input() -> xa11y::Result<InputSim> {
    if let Some(input) = platform::wayland_input() {
        return Ok(InputSim::new(input));
    }
    Ok(InputSim::new(Arc::new(
        xa11y_linux::LinuxInputProvider::new()?,
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn native_provider() -> xa11y::Result<Arc<dyn Provider>> {
    Err(xa11y::Error::Unsupported {
        feature: "Computer Use".to_owned(),
    })
}
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn native_screenshots() -> xa11y::Result<Arc<dyn ScreenshotProvider>> {
    native_provider().map(|_| unreachable!())
}
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn native_input() -> xa11y::Result<InputSim> {
    native_provider().map(|_| unreachable!())
}

// Inside `engine` so it can read refs and element bounds.
#[cfg(all(test, target_os = "linux"))]
#[path = "kde_wayland_e2e.rs"]
mod kde_wayland_e2e;

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a real app on this Mac, in whatever Space it is (run it with
    /// TodeX full screen in another Space to see that case):
    /// `TODEX_CU_E2E_APP=com.apple.dt.Devices TODEX_CU_E2E_ROW="iPad mini"
    /// cargo test window_mode_reaches_a_real_window -- --ignored --nocapture`.
    /// With a row named, clicks it by ref (in the background, so the pointer
    /// must not move) and then the previously selected row back.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "drives the app named by TODEX_CU_E2E_APP on this Mac"]
    fn window_mode_reaches_a_real_window() {
        use objc2_app_kit::NSEvent;
        let app = std::env::var("TODEX_CU_E2E_APP").expect("TODEX_CU_E2E_APP");
        let never = AtomicBool::new(false);
        let own_pid = std::process::id();
        let mut engine = Engine::default();
        let observe = |engine: &mut Engine| {
            engine
                .observe(
                    &json!({ "app": app, "screenshot": true }),
                    own_pid,
                    None,
                    &never,
                )
                .expect("observe")
        };
        let result = observe(&mut engine);
        let pid = result["app"]["pid"].as_u64().unwrap();
        let window = &result["window"];
        println!("window: {window}");
        println!("other apps: {}", result["otherApps"]);
        let windows = result["windows"].as_array().unwrap();
        assert!(!windows.is_empty(), "the app's windows are listed");
        // Window mode: no other app's window titles.
        let own_name = result["app"]["name"].as_str().unwrap();
        assert!(windows.iter().all(|listed| listed["app"] == own_name));
        let shot = &result["screenshot"];
        assert!(
            shot.is_object(),
            "a screenshot of the window, wherever it is"
        );
        let jpeg = BASE64.decode(shot["data"].as_str().unwrap()).unwrap();
        std::fs::write("/tmp/todex-cu-e2e.jpg", jpeg).unwrap();
        println!(
            "screenshot {}x{} → /tmp/todex-cu-e2e.jpg",
            shot["width"], shot["height"]
        );
        let tree = result["tree"].as_str().unwrap();
        println!("{}", tree.lines().take(40).collect::<Vec<_>>().join("\n"));
        let Ok(row) = std::env::var("TODEX_CU_E2E_ROW") else {
            return;
        };
        // The ref of the row (or its cell) showing `needle`: rows carry
        // their text on unreferenced children.
        let ref_of = |tree: &str, needle: &str| {
            let mut last = None;
            for line in tree.lines() {
                if let Some(rest) = line.split("[ref=").nth(1) {
                    last = Some(rest.trim_end_matches(']').to_owned());
                }
                if line.contains(needle) {
                    return last.unwrap_or_else(|| panic!("no ref before {needle}"));
                }
            }
            panic!("no {needle} in the tree")
        };
        // The text of the selected row, to select it again at the end.
        let selected_row = |tree: &str| {
            let mut lines = tree.lines().skip_while(|line| !line.contains("[selected]"));
            lines
                .find(|line| line.contains("static_text"))
                .and_then(|line| line.split('"').nth(1))
                .map(str::to_owned)
        };
        let selected_before = selected_row(tree);
        let grants = || Grants {
            allowed_apps: &[],
            confirmed: false,
            cancelled: &never,
        };
        let allowed = [result["app"]["bundleId"].as_str().unwrap().to_owned()];
        let grants_allowed = || Grants {
            allowed_apps: &allowed,
            ..grants()
        };
        // SAFETY-free AppKit read of the pointer position.
        let pointer = || {
            let at = NSEvent::mouseLocation();
            (at.x.round(), at.y.round())
        };
        let before = pointer();
        let reference = ref_of(tree, &row);
        let acted = engine
            .act(
                &json!({ "action": "click", "ref": reference }),
                grants_allowed(),
                own_pid,
                None,
            )
            .expect("click the row");
        println!("click {row}: {acted}");
        assert_eq!(acted["path"], "background");
        assert_eq!(pointer(), before, "the pointer did not move");
        let after = observe(&mut engine);
        let now_selected = selected_row(after["tree"].as_str().unwrap());
        println!("after: selected {now_selected:?}");
        assert!(
            now_selected
                .as_deref()
                .is_some_and(|text| text.contains(&row)),
            "the row is selected"
        );
        assert_eq!(after["app"]["pid"].as_u64().unwrap(), pid);
        // Put the selection back.
        if let Some(name) = selected_before {
            let reference = ref_of(after["tree"].as_str().unwrap(), &name);
            engine
                .act(
                    &json!({ "action": "click", "ref": reference }),
                    grants_allowed(),
                    own_pid,
                    None,
                )
                .expect("restore the selection");
            println!("restored {name}");
        }
        // A section header collapses and expands in the background.
        if let Ok(group) = std::env::var("TODEX_CU_E2E_GROUP") {
            for action in ["collapse", "expand"] {
                let tree = observe(&mut engine)["tree"].as_str().unwrap().to_owned();
                let header = tree
                    .lines()
                    .position(|line| line.contains(&format!("\"{group}\"")))
                    .expect("the group header");
                // The row holding the header button.
                let row = tree
                    .lines()
                    .take(header + 1)
                    .filter(|line| line.contains("tree_item"))
                    .last()
                    .unwrap();
                let reference = row
                    .split("[ref=")
                    .nth(1)
                    .unwrap()
                    .trim_end_matches(']')
                    .to_owned();
                let before = pointer();
                let acted = engine
                    .act(
                        &json!({ "action": action, "ref": reference }),
                        grants_allowed(),
                        own_pid,
                        None,
                    )
                    .unwrap_or_else(|error| panic!("{action}: {error}"));
                assert_eq!(acted["path"], "background");
                assert_eq!(pointer(), before, "the pointer did not move");
                let value = observe(&mut engine)["tree"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .find(|line| line.contains(&format!("\"{group}\"")))
                    .unwrap()
                    .to_owned();
                println!("{action} {group}: {value}");
                let wanted = if action == "collapse" {
                    "Collapsed"
                } else {
                    "Expanded"
                };
                assert!(value.contains(wanted), "{action}: {value}");
            }
        }
    }

    #[test]
    fn a_ref_never_silently_becomes_a_pointer_click() {
        let off = needs_pointer("Device Hub", true);
        assert_eq!(off.code, "NEEDS_POINTER");
        assert!(off.message.contains("focus_window"));
        // The app's name is screen text: the MCP layer fences it.
        assert_eq!(off.detail.as_ref().unwrap()["label"], "Device Hub");
        let here = needs_pointer("", false);
        assert!(here.message.contains("x and y"));
        assert_eq!(here.detail.as_ref().unwrap()["label"], "The app");
    }

    #[test]
    fn windows_are_clipped_to_their_display() {
        let displays = [Display {
            index: 0,
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 800.0,
            scale: 2.0,
        }];
        let clipped = clip_to_display(
            Rect {
                x: -100,
                y: 700,
                width: 400,
                height: 300,
            },
            &displays,
        );
        assert_eq!(
            (clipped.x, clipped.y, clipped.width, clipped.height),
            (0, 700, 300, 100)
        );
    }

    fn app(id: &str, pid: u32) -> Target {
        Target {
            id: id.to_owned(),
            name: id.to_owned(),
            pid,
        }
    }

    #[test]
    fn a_ref_without_a_process_belongs_to_the_observed_app() {
        let observed = app("org.example.observed", 5);
        let by_pid = |pid| app("org.example.by-pid", pid);
        // Its own process wins.
        assert_eq!(
            ref_app(Some(9), Some(&observed), by_pid),
            Some(app("org.example.by-pid", 9))
        );
        // Else what was observed, never the front app or an `app` argument.
        assert_eq!(ref_app(None, Some(&observed), by_pid), Some(observed));
        assert_eq!(ref_app(None, None, by_pid), None);
    }

    #[test]
    fn typing_goes_to_the_checked_target() {
        let target = app("org.example.target", 8);
        assert_eq!(typing_pid(Some(3), &target), Some(3));
        assert_eq!(typing_pid(None, &target), Some(8));
        assert_eq!(typing_pid(None, &Target::default()), None);
    }

    #[test]
    fn text_is_typed_in_character_chunks() {
        assert_eq!(text_chunks("", 3).count(), 0);
        assert_eq!(
            text_chunks("abcdefgh", 3).collect::<Vec<_>>(),
            ["abc", "def", "gh"]
        );
        assert_eq!(text_chunks("abc", 3).collect::<Vec<_>>(), ["abc"]);
        // Never splits a character.
        assert_eq!(
            text_chunks("日本語テキスト", 3).collect::<Vec<_>>(),
            ["日本語", "テキス", "ト"]
        );
        let long = "x".repeat(100);
        assert_eq!(text_chunks(&long, TYPE_CHUNK_CHARS).count(), 4);
    }

    #[test]
    fn another_lease_does_not_see_the_latest_observation() {
        let mut engine = Engine::default();
        engine.enter(1);
        engine.refs.insert("e1".to_owned(), 0);
        engine.observed = Some(app("org.example.observed", 5));
        engine.windows.clear();
        // The same lease keeps its observation.
        engine.enter(1);
        assert!(engine.refs.contains_key("e1"));
        assert!(engine.observed.is_some());
        // A new lease starts from nothing.
        engine.enter(2);
        assert!(engine.refs.is_empty());
        assert!(engine.observed.is_none());
        engine.refs.insert("e2".to_owned(), 0);
        engine.enter(2);
        assert!(engine.refs.contains_key("e2"));
    }

    #[test]
    fn jpeg_encoding_accepts_rgba() {
        let jpeg = encode_jpeg(&[255u8; 4 * 4 * 4], 4, 4).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }
}
