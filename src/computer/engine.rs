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
    keys, platform,
    platform::{Display, Typed},
    policy::{self, ShotMapping, Target},
    tree, ComputerError,
};

const SCREENSHOT_MAX_WIDTH: u32 = 1280;
const SCREENSHOT_QUALITY: u8 = 70;
const MAX_WINDOWS: usize = 40;
const MAX_NODES: usize = 4000;
const MAX_DEPTH: usize = 60;
const MAX_WAIT_MS: u64 = 10_000;
/// How long an app just brought forward may take to own the keyboard.
const FRONT_SETTLE: Duration = Duration::from_millis(500);
/// Typed text goes out in pieces of this many characters.
const TYPE_CHUNK_CHARS: usize = 32;

/// A window listed by the latest observation; agents refer to it by `id`.
struct WindowEntry {
    id: u64,
    app: Target,
    title: String,
    /// Bounds in global coordinates.
    element: ElementData,
    /// What moves its subtree's AT-SPI bounds into global coordinates
    /// (KDE Wayland; zero elsewhere).
    offset: (i32, i32),
}

#[derive(Default)]
pub(super) struct Engine {
    provider: Option<Arc<dyn Provider>>,
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
        let provider = native_provider().map_err(ComputerError::platform)?;
        self.provider = Some(provider.clone());
        Ok(provider)
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
    pub(super) fn observe(&mut self, args: &Value, own_pid: u32) -> Result<Value, ComputerError> {
        let provider = self.provider()?;
        self.reset();
        self.list_windows(&provider);
        let target_window = match args["window"].as_u64() {
            Some(id) => Some(
                self.windows
                    .iter()
                    .position(|window| window.id == id)
                    .ok_or_else(|| {
                        ComputerError::invalid(format!(
                            "window {id} is not on screen; observe again"
                        ))
                    })?,
            ),
            None => None,
        };
        let app = match (target_window, args["app"].as_str()) {
            (Some(index), _) => self.windows[index].app.clone(),
            (None, Some(identifier)) => platform::running_app(identifier).ok_or_else(|| {
                ComputerError::invalid(format!("{identifier} is not running; use open_app"))
            })?,
            (None, None) => frontmost(&provider)?,
        };
        let protected = policy::is_protected(&app, own_pid);
        if protected && (target_window.is_some() || args["app"].is_string()) {
            return Err(ComputerError::from(policy::blocked(label(&app))));
        }
        self.observed = Some(app.clone());
        // The front app is protected: show the display without its tree.
        let window_index = target_window.or_else(|| {
            self.windows
                .iter()
                .position(|window| !protected && window.app.pid == app.pid)
        });
        let (tree_text, truncated) = if protected {
            (
                format!(
                    "({} is protected: its contents are hidden. Name another app or window to observe it.)",
                    label(&app)
                ),
                false,
            )
        } else {
            let root = match window_index {
                Some(index) => self.windows[index].element.clone(),
                None => provider
                    .app_by_pid(app.pid)
                    .map_err(ComputerError::platform)?,
            };
            let offset = window_index.map_or((0, 0), |index| self.windows[index].offset);
            let mut budget = MAX_NODES;
            let node = self.read_tree(&provider, root, 0, offset, &mut budget);
            let formatted = tree::format(&node);
            self.refs = formatted.refs;
            (formatted.text, formatted.truncated || budget == 0)
        };

        let displays = platform::displays();
        let mut result = json!({
            "app": app_json(&app),
            "windows": self.windows.iter().take(30).map(|window| json!({
                "id": window.id,
                "app": window.app.name,
                "bundleId": window.app.id,
                "title": if policy::is_protected(&window.app, own_pid) { "" } else { &window.title },
            })).collect::<Vec<_>>(),
            "displays": displays,
            "tree": tree_text,
            "truncated": truncated,
        });
        if let Some(index) = window_index {
            let window = &self.windows[index];
            let bounds = window.element.bounds;
            result["window"] = json!({
                "id": window.id,
                "app": window.app.name,
                "bundleId": window.app.id,
                "title": window.title,
                "x": bounds.map(|b| b.x).unwrap_or(0),
                "y": bounds.map(|b| b.y).unwrap_or(0),
                "width": bounds.map(|b| b.width).unwrap_or(0),
                "height": bounds.map(|b| b.height).unwrap_or(0),
            });
        }
        if args["screenshot"].as_bool() != Some(false) {
            let rect = match args["display"].as_u64() {
                Some(index) => display_rect(
                    displays
                        .get(index as usize)
                        .ok_or_else(|| ComputerError::invalid(format!("no display {index}")))?,
                ),
                None => window_index
                    .and_then(|index| self.windows[index].element.bounds)
                    .filter(|bounds| bounds.width > 0 && bounds.height > 0)
                    .map(|bounds| clip_to_display(bounds, &displays))
                    .or_else(|| displays.first().map(display_rect))
                    .ok_or_else(|| ComputerError::platform("no display to capture"))?,
            };
            let (screenshot, mapping) = self.capture(rect, own_pid)?;
            self.shot = Some(mapping);
            result["screenshot"] = screenshot;
        }
        Ok(result)
    }

    /// Windows of every app, front app first, numbered from 1.
    fn list_windows(&mut self, provider: &Arc<dyn Provider>) {
        let front = provider.focused_app().ok().and_then(|app| app.pid);
        let mut apps = provider.list_apps().unwrap_or_default();
        apps.sort_by_key(|app| app.pid != front);
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
            for window in provider.app_windows(&app).unwrap_or_default() {
                if windows.len() >= MAX_WINDOWS {
                    break;
                }
                windows.push(WindowEntry {
                    id: windows.len() as u64 + 1,
                    app: identity.clone(),
                    title: window.name.clone().unwrap_or_default(),
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

    fn capture(&mut self, rect: Rect, own_pid: u32) -> Result<(Value, ShotMapping), ComputerError> {
        let screenshots = self.screenshots()?;
        let mut shot = screenshots
            .capture_region(rect)
            .map_err(ComputerError::platform)?;
        self.hide_protected(&mut shot, rect, own_pid)?;
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
        let mut shot = self
            .screenshots()?
            .capture_region(rect)
            .map_err(ComputerError::platform)?;
        self.hide_protected(&mut shot, rect, own_pid)?;
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

    pub(super) fn act(
        &mut self,
        args: &Value,
        grants: Grants<'_>,
        own_pid: u32,
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
        let element = match args["ref"]
            .as_str()
            .filter(|reference| !reference.is_empty())
        {
            Some(reference) => Some(
                self.refs
                    .get(reference)
                    .and_then(|handle| self.elements.get(*handle))
                    .cloned()
                    .ok_or_else(|| {
                        ComputerError::invalid(format!(
                            "unknown ref {reference}; refs come from the latest computer_observe"
                        ))
                    })?,
            ),
            None => None,
        };
        let point = self.map_point(args["x"].as_f64(), args["y"].as_f64())?;
        let to = self.map_point(args["toX"].as_f64(), args["toY"].as_f64())?;

        let target = if action == "wait" {
            Target::default()
        } else {
            let target = self.target(&action, args, element.as_ref(), point, provider.as_ref())?;
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
            });
        let pointer_at = point.or(center);

        let _injecting = (!matches!(action.as_str(), "wait" | "open_app" | "focus_window"))
            .then(platform::injecting);
        let path = match action.as_str() {
            "click" | "right_click" => {
                let background = point.is_none()
                    && element.as_ref().is_some_and(|element| {
                        let verb = if action == "click" {
                            "press"
                        } else {
                            "show_menu"
                        };
                        element.actions.iter().any(|a| a == verb)
                            && live(cancelled).is_ok()
                            && provider.as_ref().is_some_and(|provider| {
                                provider.perform_action(element, verb).is_ok()
                            })
                    });
                if background {
                    "background"
                } else {
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
            }
            "double_click" => {
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
                let at = require_point(pointer_at)?;
                landing(at, point.is_some())?;
                self.input()?
                    .mouse()
                    .move_to(to_point(at))
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "drag" => {
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
                    let window = self
                        .windows
                        .iter()
                        .find(|window| window.id == id)
                        .ok_or_else(|| {
                            ComputerError::invalid(format!("window {id} is not on screen"))
                        })?;
                    let _ = provider
                        .as_ref()
                        .map(|provider| provider.activate(&window.element));
                    platform::activate(window.app.pid, Some(&window.title))
                        .map_err(ComputerError::platform)?;
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
        Ok(json!({ "app": app.as_ref().map(app_json).unwrap_or_else(|| json!({})), "path": path }))
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
                    .ok_or_else(|| {
                        ComputerError::invalid(format!("window {id} is not on screen"))
                    });
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
        if let Some(element) = element {
            if (platform::is_secure(element) || is_secure_role(element)) && !confirmed {
                return Err(sensitive());
            }
            let focused = provider.focus(element).is_ok();
            if provider.type_text(element, text).is_ok() {
                return Ok("background");
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
        match platform::type_into_focused(pid, text, confirmed) {
            Typed::Inserted => return Ok("background"),
            Typed::Secure => return Err(sensitive()),
            Typed::Unsupported => platform::activate(pid, None).map_err(ComputerError::platform)?,
        }
        check_front(Some(provider), pid, own_pid, cancelled)?;
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
fn native_provider() -> xa11y::Result<Arc<dyn Provider>> {
    Ok(Arc::new(xa11y_macos::MacOSProvider::new()?))
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
