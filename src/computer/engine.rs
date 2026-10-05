//! The blocking half of Computer Use: reads accessibility trees and
//! screenshots through xa11y, resolves what an action touches, applies
//! [`policy`], and performs it. Runs on blocking threads behind a mutex;
//! refs and the screenshot mapping belong to the latest observation.

use std::{collections::HashMap, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use xa11y::{
    input::{InputSim, MouseButton, ScrollDelta},
    ElementData, Point, Provider, Rect, ScreenshotProvider,
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
}

/// What `act` may touch, decided by the daemon (never by the agent).
pub(super) struct Grants<'a> {
    pub allowed_apps: &'a [String],
    pub confirmed: bool,
}

impl Engine {
    /// Forgets the latest observation (a new conversation took the screen).
    pub(super) fn reset(&mut self) {
        self.elements.clear();
        self.refs.clear();
        self.windows.clear();
        self.shot = None;
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

    pub(super) fn observe(&mut self, args: &Value) -> Result<Value, ComputerError> {
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
        let window_index = target_window.or_else(|| {
            self.windows
                .iter()
                .position(|window| window.app.pid == app.pid)
        });
        let root = match window_index {
            Some(index) => self.windows[index].element.clone(),
            None => provider
                .app_by_pid(app.pid)
                .map_err(ComputerError::platform)?,
        };
        let offset = window_index.map_or((0, 0), |index| self.windows[index].offset);
        let mut budget = MAX_NODES;
        let node = self.read_tree(&provider, root.clone(), 0, offset, &mut budget);
        let formatted = tree::format(&node);
        self.refs = formatted.refs;

        let displays = platform::displays();
        let mut result = json!({
            "app": app_json(&app),
            "windows": self.windows.iter().take(30).map(|window| json!({
                "id": window.id,
                "app": window.app.name,
                "bundleId": window.app.id,
                "title": window.title,
            })).collect::<Vec<_>>(),
            "displays": displays,
            "tree": formatted.text,
            "truncated": formatted.truncated || budget == 0,
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
            let (screenshot, mapping) = self.capture(rect)?;
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

    fn capture(&mut self, rect: Rect) -> Result<(Value, ShotMapping), ComputerError> {
        let screenshots = self.screenshots()?;
        let shot = screenshots
            .capture_region(rect)
            .map_err(ComputerError::platform)?;
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

    /// A JPEG of the display a session works on, for live viewers.
    pub(super) fn frame(&mut self, max_width: u32, quality: u8) -> Result<Vec<u8>, ComputerError> {
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
        let shot = self
            .screenshots()?
            .capture_region(display_rect(display))
            .map_err(ComputerError::platform)?;
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

        if action != "wait" {
            let target = self.target(&action, args, element.as_ref(), point, provider.as_ref())?;
            if let Some(failure) = policy::check_target(&target, grants.allowed_apps, own_pid) {
                return Err(ComputerError::from(failure));
            }
        }
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
                            && provider.as_ref().is_some_and(|provider| {
                                provider.perform_action(element, verb).is_ok()
                            })
                    });
                if background {
                    "background"
                } else {
                    let at = require_point(pointer_at)?;
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
                super::host_ui::mark_point(at.0, at.1);
                self.input()?
                    .backend()
                    .pointer_click(to_point(at), MouseButton::Left, 2)
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "hover" => {
                let at = require_point(pointer_at)?;
                self.input()?
                    .mouse()
                    .move_to(to_point(at))
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "drag" => {
                let from = require_point(pointer_at)?;
                let to = to.ok_or_else(|| ComputerError::invalid("drag needs toX and toY"))?;
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
                self.input()?
                    .mouse()
                    .scroll(to_point(at), delta)
                    .map_err(ComputerError::platform)?;
                "pointer"
            }
            "type" => {
                self.type_text(args, element.as_ref(), provider.as_ref(), grants.confirmed)?
            }
            "key" => {
                let chord = keys::parse(args["keys"].as_str().unwrap_or_default())
                    .map_err(ComputerError::invalid)?;
                let pid = match args["app"].as_str() {
                    Some(identifier) => Some(
                        platform::running_app(identifier)
                            .ok_or_else(|| {
                                ComputerError::invalid(format!("{identifier} is not running"))
                            })?
                            .pid,
                    ),
                    None => provider
                        .as_ref()
                        .and_then(|provider| provider.focused_app().ok())
                        .and_then(|app| app.pid),
                };
                match pid {
                    Some(pid)
                        if platform::post_chord(pid, &chord)
                            .map_err(ComputerError::platform)? =>
                    {
                        "background"
                    }
                    _ => {
                        if let Some(pid) = pid {
                            platform::activate(pid).map_err(ComputerError::platform)?;
                        }
                        self.input()?
                            .keyboard()
                            .chord(chord.key.clone(), &chord.held)
                            .map_err(ComputerError::platform)?;
                        "keyboard"
                    }
                }
            }
            "wait" => {
                let ms = args["ms"].as_u64().unwrap_or(1000).min(MAX_WAIT_MS);
                std::thread::sleep(Duration::from_millis(ms));
                "none"
            }
            "open_app" => {
                platform::open_app(args["app"].as_str().unwrap_or_default())
                    .map_err(ComputerError::invalid)?;
                "background"
            }
            "focus_window" => {
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
                    platform::activate(window.app.pid).map_err(ComputerError::platform)?;
                } else {
                    let identifier = args["app"].as_str().unwrap_or_default();
                    let app = platform::running_app(identifier).ok_or_else(|| {
                        ComputerError::invalid(format!("{identifier} is not running"))
                    })?;
                    platform::activate(app.pid).map_err(ComputerError::platform)?;
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
        if let Some(pid) = element.and_then(|element| element.pid) {
            return Ok(platform::app_identity(pid));
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
        args: &Value,
        element: Option<&ElementData>,
        provider: Option<&Arc<dyn Provider>>,
        confirmed: bool,
    ) -> Result<&'static str, ComputerError> {
        let text = args["text"].as_str().unwrap_or_default();
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
                if platform::paste_text(pid, text).map_err(ComputerError::platform)? {
                    return Ok("keyboard");
                }
            }
        }
        let pid = match (
            element.and_then(|element| element.pid),
            args["app"].as_str(),
        ) {
            (Some(pid), _) => Some(pid),
            (None, Some(identifier)) => platform::running_app(identifier).map(|app| app.pid),
            (None, None) => provider.focused_app().ok().and_then(|app| app.pid),
        };
        if let Some(pid) = pid {
            match platform::type_into_focused(pid, text, confirmed) {
                Typed::Inserted => return Ok("background"),
                Typed::Secure => return Err(sensitive()),
                Typed::Unsupported => platform::activate(pid).map_err(ComputerError::platform)?,
            }
        }
        self.input()?
            .keyboard()
            .type_text(text)
            .map_err(ComputerError::platform)?;
        Ok("keyboard")
    }
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

    #[test]
    fn jpeg_encoding_accepts_rgba() {
        let jpeg = encode_jpeg(&[255u8; 4 * 4 * 4], 4, 4).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }
}
