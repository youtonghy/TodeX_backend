//! KDE Plasma (6.6 or later) Wayland sessions. xa11y still reads the
//! AT-SPI tree; this module supplies what Wayland keeps from ordinary
//! clients:
//!
//! - **Input** through the RemoteDesktop portal's `Notify*` methods. The
//!   session also selects ScreenCast monitors, whose streams carry each
//!   output's position so absolute pointer motion can be addressed. The
//!   user consents once; the portal's restore token (stored owner-only in
//!   `$XDG_STATE_HOME/todex/remote-desktop.token`) skips the dialog later.
//! - **Screenshots** through KWin's `org.kde.KWin.ScreenShot2`, which KWin
//!   only serves to executables named by a `.desktop` entry with
//!   `X-KDE-DBUS-Restricted-Interfaces`; [`ensure_identity`] installs one.
//!   Without it the Screenshot portal captures the whole desktop instead.
//! - **Windows and outputs** through short-lived KWin scripts that report
//!   back over D-Bus ([`run_script`]), since Wayland has no global window
//!   list for clients.
//! - **Idle time** through `ext-idle-notify-v1`.
//!
//! Every D-Bus call has a timeout and reports its failure; nothing here
//! writes KDE's permission store.

use std::{
    collections::HashMap,
    io::Read as _,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll, Wake, Waker},
    time::{Duration, Instant},
};

use futures_util::StreamExt as _;
use serde_json::{json, Value as Json};
use xa11y::{
    input::{InputProvider, Key, MouseButton, Point, ScrollDelta},
    screenshot::CaptureMapping,
    Rect, Screenshot, ScreenshotProvider,
};
use zbus::{
    blocking::Connection as Bus,
    message::Type as MessageType,
    zvariant::{self, ObjectPath, OwnedObjectPath, OwnedValue, Value},
    MatchRule, MessageStream,
};

use super::{
    kde_desktop::{
        self, button_code, char_keysym, crop_rect, desktop_bounds, evdev_code, identity_entry,
        needs_paste, parse_script_reply, parse_version, raw_to_rgba, stream_point, IdleEvent,
        KwinSnapshot, Stream, Version, APP_ID, SCROLL_PIXELS_PER_TICK,
    },
    Display,
};
use crate::secure_fs::{ensure_owner_only_dir, write_owner_only_atomic};

/// Ordinary D-Bus calls.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Portal requests answered without asking the user.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// The RemoteDesktop consent dialog.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
/// A KWin script's answer.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a window snapshot answers repeated lookups.
const SNAPSHOT_TTL: Duration = Duration::from_millis(300);
/// How long screenshots skip KWin after it refused one.
const KWIN_RETRY: Duration = Duration::from_secs(30);
/// How long Klipper keeps pasted text before the previous clipboard returns.
const PASTE_RESTORE_DELAY: Duration = Duration::from_millis(400);

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const KWIN: &str = "org.kde.KWin";
const SCRIPT_INTERFACE: &str = "com.unbaked0692.todex.KWinScript";

type Results = HashMap<String, OwnedValue>;

// ---- D-Bus plumbing --------------------------------------------------------

/// This module's own session-bus connection: calls time out after
/// [`CALL_TIMEOUT`], and the portals know it as [`APP_ID`].
fn bus() -> Result<Bus, String> {
    static BUS: Mutex<Option<Bus>> = Mutex::new(None);
    let mut bus = BUS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(bus) = bus.as_ref() {
        return Ok(bus.clone());
    }
    let connection = zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.method_timeout(CALL_TIMEOUT).build())
        .map_err(|error| format!("cannot connect to the session bus: {error}"))?;
    register_app_id(&connection);
    *bus = Some(connection.clone());
    Ok(connection)
}

/// Tells the portals which app this connection is, before any portal call
/// (they would otherwise see an unidentified host process, which cannot
/// keep RemoteDesktop permissions). Portals older than 1.19 lack the
/// registry; they work without it.
fn register_app_id(bus: &Bus) {
    let options: HashMap<&str, Value<'_>> = HashMap::new();
    if let Err(error) = call::<_, ()>(
        bus,
        PORTAL,
        PORTAL_PATH,
        "org.freedesktop.host.portal.Registry",
        "Register",
        &(APP_ID, options),
    ) {
        eprintln!("todex-agentd: the portal registry did not take TodeX's app id: {error}");
    }
}

fn call<B, R>(
    bus: &Bus,
    destination: &str,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<R, String>
where
    B: serde::Serialize + zvariant::DynamicType,
    R: serde::de::DeserializeOwned + zvariant::Type,
{
    let reply = bus
        .call_method(Some(destination), path, Some(interface), method, body)
        .map_err(|error| format!("{interface}.{method}: {error}"))?;
    reply
        .body()
        .deserialize::<R>()
        .map_err(|error| format!("{interface}.{method} replied unexpectedly: {error}"))
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future on this thread until it completes or `timeout` passes.
/// zbus drives its connections on its own executor thread, so its streams
/// only need polling here.
fn block_on_timeout<F: std::future::Future>(future: F, timeout: Duration) -> Option<F::Output> {
    let deadline = Instant::now() + timeout;
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Some(output);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::park_timeout(deadline - now);
    }
}

fn subscribe(bus: &Bus, rule: MatchRule<'_>) -> Result<MessageStream, String> {
    block_on_timeout(
        MessageStream::for_match_rule(rule, bus.inner(), Some(8)),
        CALL_TIMEOUT,
    )
    .ok_or("timed out subscribing to D-Bus messages")?
    .map_err(|error| format!("cannot subscribe to D-Bus messages: {error}"))
}

/// A fresh token for portal requests and script names (public on the bus,
/// so unpredictable).
fn token() -> String {
    format!("todex_{}", uuid::Uuid::new_v4().simple())
}

/// Calls a portal method that answers through a `Request.Response` signal
/// and returns the response's results.
fn portal_request<B>(
    bus: &Bus,
    interface: &str,
    method: &str,
    handle_token: &str,
    body: &B,
    timeout: Duration,
) -> Result<Results, String>
where
    B: serde::Serialize + zvariant::DynamicType,
{
    let sender = bus
        .unique_name()
        .ok_or("the session bus gave no unique name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let expected = format!("{PORTAL_PATH}/request/{sender}/{handle_token}");
    let rule = |path: &str| -> Result<MatchRule<'static>, String> {
        Ok(MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface("org.freedesktop.portal.Request")
            .and_then(|rule| rule.member("Response"))
            .and_then(|rule| rule.path(path.to_owned()))
            .map_err(|error| error.to_string())?
            .build())
    };
    // Subscribed before calling: the answer may beat the reply.
    let mut responses = subscribe(bus, rule(&expected)?)?;
    let handle: OwnedObjectPath = call(bus, PORTAL, PORTAL_PATH, interface, method, body)?;
    if handle.as_str() != expected {
        // A portal that ignores handle_token.
        responses = subscribe(bus, rule(handle.as_str())?)?;
    }
    let message = match block_on_timeout(responses.next(), timeout) {
        Some(Some(Ok(message))) => message,
        Some(Some(Err(error))) => return Err(format!("{method}: {error}")),
        Some(None) => return Err(format!("{method}: the session bus closed")),
        None => {
            let _ = call::<_, ()>(
                bus,
                PORTAL,
                handle.as_str(),
                "org.freedesktop.portal.Request",
                "Close",
                &(),
            );
            return Err(format!(
                "{method}: the portal did not answer within {} s",
                timeout.as_secs()
            ));
        }
    };
    let (response, results): (u32, Results) = message
        .body()
        .deserialize()
        .map_err(|error| format!("{method}: unreadable portal response: {error}"))?;
    match response {
        0 => Ok(results),
        1 => Err(format!(
            "{method}: the request was declined on this computer"
        )),
        other => Err(format!("{method}: the portal failed (response {other})")),
    }
}

/// Strips variant wrappers.
fn inner<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(boxed) => inner(boxed),
        other => other,
    }
}

fn string_result(results: &Results, key: &str) -> Option<String> {
    match inner(results.get(key)?) {
        Value::Str(text) => Some(text.as_str().to_owned()),
        Value::ObjectPath(path) => Some(path.as_str().to_owned()),
        _ => None,
    }
}

fn u32_result(results: &Results, key: &str) -> Option<u32> {
    match inner(results.get(key)?) {
        Value::U32(number) => Some(*number),
        _ => None,
    }
}

// ---- Plasma version and app identity ----------------------------------------

/// KWin's version, from `supportInformation`, else `plasmashell --version`.
/// Successes are kept for the process lifetime, failures for 10 seconds.
pub(super) fn plasma_version() -> Result<Version, String> {
    static VERSION: Mutex<Option<(Instant, Result<Version, String>)>> = Mutex::new(None);
    let mut cached = VERSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, result)) = cached.as_ref() {
        if result.is_ok() || at.elapsed() < Duration::from_secs(10) {
            return result.clone();
        }
    }
    let from_kwin = bus()
        .and_then(|bus| call::<_, String>(&bus, KWIN, "/KWin", KWIN, "supportInformation", &()));
    let result = match from_kwin {
        Ok(text) => {
            parse_version(&text).ok_or_else(|| "KWin did not report its version".to_owned())
        }
        Err(kwin_error) => {
            run_with_timeout(Command::new("plasmashell").arg("--version"), CALL_TIMEOUT)
                .and_then(|output| {
                    parse_version(&output)
                        .ok_or_else(|| "plasmashell reported no version".to_owned())
                })
                .map_err(|shell_error| format!("{kwin_error}; {shell_error}"))
        }
    };
    *cached = Some((Instant::now(), result.clone()));
    result
}

/// Runs a program and returns its standard output, killing it after
/// `timeout`.
fn run_with_timeout(command: &mut Command, timeout: Duration) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_string(&mut output);
                }
                return if status.success() {
                    Ok(output)
                } else {
                    Err(format!("{program} failed ({status})"))
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} did not finish within {} s",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(format!("{program}: {error}")),
        }
    }
}

fn identity_entry_path() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(data.join("applications").join(format!("{APP_ID}.desktop")))
}

fn current_identity_entry() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate the TodeX backend executable: {error}"))?;
    Ok(identity_entry(&exe.to_string_lossy()))
}

/// Whether the `.desktop` entry naming this executable is in place.
pub(super) fn identity_installed() -> bool {
    match (identity_entry_path(), current_identity_entry()) {
        (Some(path), Ok(entry)) => std::fs::read_to_string(path).is_ok_and(|text| text == entry),
        _ => false,
    }
}

/// Installs (or updates) the hidden `.desktop` entry that names this
/// executable, then refreshes KDE's service cache so KWin sees it. Does
/// nothing when it is already current.
pub(super) fn ensure_identity() -> Result<(), String> {
    let path = identity_entry_path().ok_or("neither XDG_DATA_HOME nor HOME is set")?;
    let entry = current_identity_entry()?;
    if std::fs::read_to_string(&path).is_ok_and(|text| text == entry) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    }
    write_owner_only_atomic(&path, entry.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    match run_with_timeout(&mut Command::new("kbuildsycoca6"), Duration::from_secs(30)) {
        Ok(_) => Ok(()),
        Err(error) => {
            // KDE rebuilds the cache on its own when the file changes; only
            // the first capture may fall back to the portal.
            eprintln!("todex-agentd: kbuildsycoca6 did not refresh KDE's service cache: {error}");
            Ok(())
        }
    }
}

/// Whether some screen capture path exists: KWin's (with the identity
/// entry installed) or the Screenshot portal.
pub(super) fn screen_capture_available() -> bool {
    static PORTAL_SCREENSHOT: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    if identity_installed() {
        return true;
    }
    let mut cached = PORTAL_SCREENSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, available)) = *cached {
        if at.elapsed() < Duration::from_secs(30) {
            return available;
        }
    }
    let available = bus()
        .and_then(|bus| {
            call::<_, OwnedValue>(
                &bus,
                PORTAL,
                PORTAL_PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                &("org.freedesktop.portal.Screenshot", "version"),
            )
        })
        .is_ok();
    *cached = Some((Instant::now(), available));
    available
}

// ---- KWin scripts --------------------------------------------------------

/// Lists windows (bottom to top) and outputs.
const SNAPSHOT_SCRIPT: &str = r#"
const rect = (g) => ({ x: g.x, y: g.y, width: g.width, height: g.height });
const current = workspace.currentDesktop;
const windows = [];
for (const w of workspace.stackingOrder) {
    try {
        if (!w || w.deleted) continue;
        const onDesktop = w.onAllDesktops || (w.desktops || []).some((d) => current && d.id === current.id);
        windows.push({
            internalId: String(w.internalId),
            pid: w.pid || 0,
            caption: w.caption || "",
            resourceClass: String(w.resourceClass || ""),
            desktopFileName: w.desktopFileName || "",
            frameGeometry: rect(w.frameGeometry),
            clientGeometry: rect(w.clientGeometry),
            x11Client: typeof w.x11Client === "boolean" ? w.x11Client
                : (typeof w.windowId === "number" && w.windowId > 0),
            popupWindow: !!w.popupWindow,
            minimized: !!w.minimized,
            visible: !w.minimized && !w.hidden && onDesktop,
        });
    } catch (e) {}
}
const screens = workspace.screens.map((s) => ({
    name: s.name, geometry: rect(s.geometry), devicePixelRatio: s.devicePixelRatio,
}));
return { windows: windows, screens: screens };
"#;

/// Raises and focuses the top-most window of `input.pid`.
const ACTIVATE_SCRIPT: &str = r#"
const mine = workspace.stackingOrder.filter((w) => w && !w.deleted && w.pid === input.pid
    && (w.normalWindow || w.dialog));
if (mine.length === 0) return false;
const w = mine[mine.length - 1];
if (w.minimized) w.minimized = false;
workspace.activeWindow = w;
workspace.raiseWindow(w);
return true;
"#;

/// A private directory for script files.
fn script_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    // SAFETY: getuid has no preconditions.
    let dir = base.join(format!("todex-kwin-{}", unsafe { libc::getuid() }));
    ensure_owner_only_dir(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    Ok(dir)
}

/// Runs `body` (a trusted function body that sees `input` and returns a
/// JSON-able value) as a temporary KWin script and returns its value.
/// `input` reaches the script as a JSON literal, never spliced as code.
/// Only answers sent by KWin itself are accepted.
fn run_script(body: &str, input: &Json) -> Result<Json, String> {
    let bus = bus()?;
    let name = token();
    let reply_path = format!("/com/unbaked0692/todex/kwin/{name}");
    let own_name = bus
        .unique_name()
        .ok_or("the session bus gave no unique name")?
        .to_string();
    let source = format!(
        "(function () {{\n\
         const input = {input};\n\
         let reply;\n\
         try {{ reply = {{ ok: true, value: (function () {{ {body} }})() }}; }}\n\
         catch (error) {{ reply = {{ ok: false, error: String(error) }}; }}\n\
         callDBus({service}, {path}, {interface}, \"Done\", JSON.stringify(reply));\n\
         }})();\n",
        input = serde_json::to_string(input).map_err(|error| error.to_string())?,
        service = Json::from(own_name),
        path = Json::from(reply_path.as_str()),
        interface = Json::from(SCRIPT_INTERFACE),
    );
    let file = script_dir()?.join(format!("{name}.js"));
    write_owner_only_atomic(&file, source.as_bytes())
        .map_err(|error| format!("cannot write a KWin script: {error}"))?;
    let result = run_script_file(&bus, &file, &name, &reply_path);
    let _ = std::fs::remove_file(&file);
    result
}

fn run_script_file(
    bus: &Bus,
    file: &std::path::Path,
    name: &str,
    reply_path: &str,
) -> Result<Json, String> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::MethodCall)
        .interface(SCRIPT_INTERFACE)
        .and_then(|rule| rule.member("Done"))
        .and_then(|rule| rule.path(reply_path.to_owned()))
        .map_err(|error| error.to_string())?
        .build();
    let mut replies = subscribe(bus, rule)?;
    let kwin_owner: String = call(
        bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetNameOwner",
        &(KWIN,),
    )?;
    let id: i32 = call(
        bus,
        KWIN,
        "/Scripting",
        "org.kde.kwin.Scripting",
        "loadScript",
        &(file.to_string_lossy().as_ref(), name),
    )?;
    let result = (|| {
        if id < 0 {
            return Err("KWin refused to load a TodeX script".to_owned());
        }
        call::<_, ()>(
            bus,
            KWIN,
            &format!("/Scripting/Script{id}"),
            "org.kde.kwin.Script",
            "run",
            &(),
        )?;
        let deadline = Instant::now() + SCRIPT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = match block_on_timeout(replies.next(), remaining) {
                Some(Some(Ok(message))) => message,
                Some(Some(Err(error))) => return Err(format!("KWin script reply: {error}")),
                Some(None) => return Err("the session bus closed".to_owned()),
                None => return Err("KWin did not answer a TodeX script in time".to_owned()),
            };
            let header = message.header();
            if header.sender().map(|sender| sender.as_str()) != Some(kwin_owner.as_str()) {
                // Someone other than KWin called our reply path.
                continue;
            }
            if let Err(error) = bus.reply(&header, &()) {
                eprintln!("todex-agentd: could not acknowledge a KWin script: {error}");
            }
            let text: String = message
                .body()
                .deserialize()
                .map_err(|error| format!("unreadable KWin script reply: {error}"))?;
            return parse_script_reply(&text);
        }
    })();
    if let Err(error) = call::<_, bool>(
        bus,
        KWIN,
        "/Scripting",
        "org.kde.kwin.Scripting",
        "unloadScript",
        &(name,),
    ) {
        eprintln!("todex-agentd: could not unload a KWin script: {error}");
    }
    result
}

/// Windows and outputs, reused for [`SNAPSHOT_TTL`].
pub(super) fn snapshot() -> Result<Arc<KwinSnapshot>, String> {
    static SNAPSHOT: Mutex<Option<(Instant, Arc<KwinSnapshot>)>> = Mutex::new(None);
    let mut cached = SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, snapshot)) = cached.as_ref() {
        if at.elapsed() < SNAPSHOT_TTL {
            return Ok(snapshot.clone());
        }
    }
    let value = run_script(SNAPSHOT_SCRIPT, &Json::Null)?;
    let snapshot: Arc<KwinSnapshot> = Arc::new(
        serde_json::from_value(value)
            .map_err(|error| format!("unreadable KWin windows: {error}"))?,
    );
    *cached = Some((Instant::now(), snapshot.clone()));
    Ok(snapshot)
}

pub(super) fn displays() -> Vec<Display> {
    match snapshot() {
        Ok(snapshot) => kde_desktop::displays(&snapshot),
        Err(error) => {
            eprintln!("todex-agentd: cannot list KWin outputs: {error}");
            Vec::new()
        }
    }
}

/// Raises and focuses `pid`'s top-most window.
pub(super) fn activate(pid: u32) -> Result<(), String> {
    let activated = run_script(ACTIVATE_SCRIPT, &json!({ "pid": pid }))?;
    if activated != Json::Bool(true) {
        return Err(format!("process {pid} has no window"));
    }
    // Let KWin act before input follows.
    std::thread::sleep(Duration::from_millis(80));
    Ok(())
}

// ---- Screenshots -----------------------------------------------------------

/// Captures through KWin's ScreenShot2, else the Screenshot portal.
pub(crate) struct KdeScreenshot;

impl ScreenshotProvider for KdeScreenshot {
    fn capture_full(&self) -> xa11y::Result<(Screenshot, Point)> {
        let snapshot = snapshot().map_err(platform_error)?;
        let bounds =
            desktop_bounds(&snapshot).ok_or_else(|| platform_error("KWin reports no outputs"))?;
        let rect = Rect {
            x: bounds.x.round() as i32,
            y: bounds.y.round() as i32,
            width: bounds.width.round().max(1.0) as u32,
            height: bounds.height.round().max(1.0) as u32,
        };
        Ok((self.capture_region(rect)?, Point::new(rect.x, rect.y)))
    }

    fn capture_region(&self, rect: Rect) -> xa11y::Result<Screenshot> {
        if rect.width == 0 || rect.height == 0 {
            return Err(platform_error("zero-sized capture"));
        }
        // After KWin refuses (no authorization yet), go straight to the
        // portal for a while instead of asking KWin on every frame.
        static KWIN_REFUSED: Mutex<Option<Instant>> = Mutex::new(None);
        let refused_recently = KWIN_REFUSED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|at| at.elapsed() < KWIN_RETRY);
        let kwin = if refused_recently {
            Err("KWin refused the last screenshot".to_owned())
        } else {
            capture_kwin(rect)
        };
        let (width, height, pixels) = match kwin {
            Ok(image) => image,
            Err(kwin_error) => {
                if kwin_error.contains("NoAuthorized") {
                    *KWIN_REFUSED
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
                }
                capture_portal(rect).map_err(|portal_error| {
                    platform_error(format!("{kwin_error}; {portal_error}"))
                })?
            }
        };
        let scale = width as f32 / rect.width as f32;
        Ok(
            Screenshot::new(width, height, pixels, scale).with_mapping(CaptureMapping::single(
                Point::new(rect.x, rect.y),
                width,
                height,
                scale,
            )),
        )
    }
}

fn platform_error(message: impl std::fmt::Display) -> xa11y::Error {
    xa11y::Error::Platform {
        code: -1,
        message: message.to_string(),
    }
}

/// `CaptureArea` at logical size; the image arrives raw over a pipe.
fn capture_kwin(rect: Rect) -> Result<(u32, u32, Vec<u8>), String> {
    if let Err(error) = ensure_identity() {
        eprintln!("todex-agentd: {error}");
    }
    let bus = bus()?;
    let (read_end, write_end) = pipe()?;
    let reader = std::thread::Builder::new()
        .name("todex-kwin-shot".to_owned())
        .spawn(move || read_to_end(read_end, Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("include-cursor", Value::Bool(false));
    options.insert("native-resolution", Value::Bool(false));
    let results: Result<Results, String> = call(
        &bus,
        KWIN,
        "/org/kde/KWin/ScreenShot2",
        "org.kde.KWin.ScreenShot2",
        "CaptureArea",
        &(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            options,
            zvariant::Fd::from(&write_end),
        ),
    );
    // Our copy of the write end must close for the reader to see the end.
    drop(write_end);
    let data = reader
        .join()
        .map_err(|_| "the screenshot reader panicked".to_owned())?;
    let results = results?;
    let data = data?;
    let field =
        |key: &str| u32_result(&results, key).ok_or_else(|| format!("KWin screenshot lacks {key}"));
    let (width, height) = (field("width")?, field("height")?);
    let pixels = raw_to_rgba(&data, width, height, field("stride")?, field("format")?)?;
    Ok((width, height, pixels))
}

fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: pipe2 succeeded, so both descriptors are open and ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Reads a pipe until its writers close it, giving up after `timeout`.
fn read_to_end(fd: OwnedFd, timeout: Duration) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut file = std::fs::File::from(fd);
    let mut data = Vec::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("KWin did not finish sending the screenshot".to_owned());
        }
        let mut poll = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("reading the screenshot: {error}"));
        }
        if ready == 0 {
            continue;
        }
        match file.read(&mut buffer) {
            Ok(0) => return Ok(data),
            Ok(read) => data.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("reading the screenshot: {error}")),
        }
    }
}

/// The Screenshot portal's non-interactive full-desktop PNG, cropped.
fn capture_portal(rect: Rect) -> Result<(u32, u32, Vec<u8>), String> {
    let bus = bus()?;
    let handle_token = token();
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(handle_token.as_str()));
    options.insert("interactive", Value::Bool(false));
    options.insert("modal", Value::Bool(false));
    let results = portal_request(
        &bus,
        "org.freedesktop.portal.Screenshot",
        "Screenshot",
        &handle_token,
        &("", options),
        CONSENT_TIMEOUT,
    )?;
    let uri = string_result(&results, "uri").ok_or("the Screenshot portal returned no image")?;
    let path = uri
        .strip_prefix("file://")
        .ok_or_else(|| format!("unexpected screenshot location {uri}"))?;
    let path = percent_decode(path);
    let bytes = std::fs::read(&path).map_err(|error| format!("reading {path}: {error}"));
    let _ = std::fs::remove_file(&path);
    let (width, height, rgba) = decode_png(&bytes?)?;
    let snapshot = snapshot()?;
    let desktop = desktop_bounds(&snapshot).ok_or("KWin reports no outputs")?;
    let (x, y, crop_width, crop_height) =
        crop_rect(desktop, width, height, rect).ok_or("the region is off screen")?;
    let mut out = Vec::with_capacity((crop_width * crop_height * 4) as usize);
    for row in y..y + crop_height {
        let start = ((row * width + x) * 4) as usize;
        out.extend_from_slice(&rgba[start..start + (crop_width * 4) as usize]);
    }
    Ok((crop_width, crop_height, out))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("unreadable screenshot PNG: {error}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or("screenshot PNG too large")?;
    let mut buffer = vec![0; size];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("unreadable screenshot PNG: {error}"))?;
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buffer[..frame.buffer_size()].to_vec(),
        png::ColorType::Rgb => buffer[..frame.buffer_size()]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 0xff])
            .collect(),
        other => return Err(format!("unsupported screenshot PNG colours {other:?}")),
    };
    Ok((frame.width, frame.height, rgba))
}

// ---- RemoteDesktop input ---------------------------------------------------

/// A started RemoteDesktop session with pointer and keyboard.
struct RemoteSession {
    handle: OwnedObjectPath,
    streams: Vec<Stream>,
}

static REMOTE: Mutex<Option<RemoteSession>> = Mutex::new(None);

fn token_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("todex").join("remote-desktop.token"))
}

fn read_restore_token() -> Option<String> {
    let token = std::fs::read_to_string(token_path()?).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn store_restore_token(token: &str) {
    let Some(path) = token_path() else {
        return;
    };
    let result = path
        .parent()
        .map_or(Ok(()), ensure_owner_only_dir)
        .and_then(|()| write_owner_only_atomic(&path, token.as_bytes()));
    if let Err(error) = result {
        eprintln!("todex-agentd: cannot keep the remote desktop permission: {error}");
    }
}

fn options(handle_token: &str) -> HashMap<&'static str, Value<'_>> {
    let mut options = HashMap::new();
    options.insert("handle_token", Value::from(handle_token));
    options
}

/// Creates and starts a session; the first time (or after the user
/// revoked it) KDE asks the person at the computer.
fn open_remote(bus: &Bus) -> Result<RemoteSession, String> {
    let request = token();
    let session_token = token();
    let mut create = options(&request);
    create.insert("session_handle_token", Value::from(session_token.as_str()));
    let created = portal_request(
        bus,
        REMOTE_DESKTOP,
        "CreateSession",
        &request,
        &(create,),
        REQUEST_TIMEOUT,
    )?;
    let handle = string_result(&created, "session_handle")
        .ok_or("the RemoteDesktop portal returned no session")?;
    let handle = OwnedObjectPath::try_from(handle)
        .map_err(|error| format!("bad RemoteDesktop session handle: {error}"))?;
    let session = (|| {
        let request = token();
        let mut select = options(&request);
        // Keyboard (1) and pointer (2); remember the grant until revoked (2).
        select.insert("types", Value::U32(3));
        select.insert("persist_mode", Value::U32(2));
        let restore = read_restore_token();
        if let Some(restore) = restore.as_deref() {
            select.insert("restore_token", Value::from(restore));
        }
        portal_request(
            bus,
            REMOTE_DESKTOP,
            "SelectDevices",
            &request,
            &(handle.as_ref(), select),
            REQUEST_TIMEOUT,
        )?;

        let request = token();
        let mut sources = options(&request);
        sources.insert("types", Value::U32(1));
        sources.insert("multiple", Value::Bool(true));
        portal_request(
            bus,
            SCREEN_CAST,
            "SelectSources",
            &request,
            &(handle.as_ref(), sources),
            REQUEST_TIMEOUT,
        )?;

        let request = token();
        let started = portal_request(
            bus,
            REMOTE_DESKTOP,
            "Start",
            &request,
            &(handle.as_ref(), "", options(&request)),
            CONSENT_TIMEOUT,
        )?;
        if let Some(restore) = string_result(&started, "restore_token") {
            store_restore_token(&restore);
        }
        let devices = u32_result(&started, "devices").unwrap_or(0);
        if devices & 3 != 3 {
            return Err("the remote desktop grant lacks the pointer or keyboard".to_owned());
        }
        let streams = started
            .get("streams")
            .map(|value| parse_streams(value))
            .unwrap_or_default();
        if streams.is_empty() {
            return Err("the remote desktop grant includes no screen".to_owned());
        }
        Ok(streams)
    })();
    match session {
        Ok(streams) => Ok(RemoteSession { handle, streams }),
        Err(error) => {
            close_remote(bus, &handle);
            Err(error)
        }
    }
}

/// `a(ua{sv})`: node id with `position` and `size`. A lone stream without
/// a position covers the desktop from its origin.
fn parse_streams(value: &Value<'_>) -> Vec<Stream> {
    let Value::Array(array) = inner(value) else {
        return Vec::new();
    };
    let single = array.len() == 1;
    array
        .inner()
        .iter()
        .filter_map(|stream| {
            let Value::Structure(stream) = inner(stream) else {
                return None;
            };
            let [node, properties] = stream.fields() else {
                return None;
            };
            let (Value::U32(node), Value::Dict(properties)) = (inner(node), inner(properties))
            else {
                return None;
            };
            let pair = |key: &str| -> Option<(i32, i32)> {
                properties.iter().find_map(|(name, value)| {
                    let Value::Str(name) = inner(name) else {
                        return None;
                    };
                    if name.as_str() != key {
                        return None;
                    }
                    let Value::Structure(pair) = inner(value) else {
                        return None;
                    };
                    match pair.fields() {
                        [Value::I32(a), Value::I32(b)] => Some((*a, *b)),
                        _ => None,
                    }
                })
            };
            let (width, height) = pair("size")?;
            let (x, y) = match pair("position") {
                Some(position) => position,
                None if single => (0, 0),
                None => return None,
            };
            Some(Stream {
                node: *node,
                x,
                y,
                width,
                height,
            })
        })
        .collect()
}

fn close_remote(bus: &Bus, handle: &ObjectPath<'_>) {
    if let Err(error) = call::<_, ()>(
        bus,
        PORTAL,
        handle.as_str(),
        "org.freedesktop.portal.Session",
        "Close",
        &(),
    ) {
        eprintln!("todex-agentd: could not close the remote desktop session: {error}");
    }
}

/// Runs `work` on the open session, opening it first. A failed call drops
/// the session (the user may have ended it) so the next action reopens it;
/// the action itself is not repeated.
fn with_remote<T>(
    work: impl FnOnce(&Bus, &RemoteSession) -> Result<T, String>,
) -> xa11y::Result<T> {
    let bus = bus().map_err(platform_error)?;
    let mut remote = REMOTE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if remote.is_none() {
        *remote = Some(
            open_remote(&bus).map_err(|error| xa11y::Error::PermissionDenied {
                instructions: format!(
                    "KDE did not grant remote control of the pointer and keyboard: {error}"
                ),
            })?,
        );
    }
    let session = remote.as_ref().expect("opened above");
    match work(&bus, session) {
        Ok(value) => Ok(value),
        Err(error) => {
            if let Some(session) = remote.take() {
                close_remote(&bus, &session.handle);
            }
            Err(platform_error(error))
        }
    }
}

/// Opens the session (asking for consent if needed) in the background.
/// Does nothing while a session is open or being opened.
pub(super) fn begin_remote_session() {
    match REMOTE.try_lock() {
        Ok(remote) if remote.is_none() => {}
        Err(std::sync::TryLockError::Poisoned(_)) => {}
        _ => return,
    }
    let spawned = std::thread::Builder::new()
        .name("todex-remote-desktop".to_owned())
        .spawn(|| {
            if let Err(error) = with_remote(|_, _| Ok(())) {
                eprintln!("todex-agentd: {error}");
            }
        });
    if let Err(error) = spawned {
        eprintln!("todex-agentd: cannot open the remote desktop session: {error}");
    }
}

/// Closes the session in the background (a Computer Use session ended).
pub(super) fn end_remote_session() {
    let spawned = std::thread::Builder::new()
        .name("todex-remote-desktop".to_owned())
        .spawn(|| {
            let session = REMOTE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let (Some(session), Ok(bus)) = (session, bus()) {
                close_remote(&bus, &session.handle);
            }
        });
    if let Err(error) = spawned {
        eprintln!("todex-agentd: cannot close the remote desktop session: {error}");
    }
}

fn notify<B>(bus: &Bus, method: &str, body: &B) -> Result<(), String>
where
    B: serde::Serialize + zvariant::DynamicType,
{
    call::<_, ()>(bus, PORTAL, PORTAL_PATH, REMOTE_DESKTOP, method, body)
}

fn no_options() -> HashMap<&'static str, Value<'static>> {
    HashMap::new()
}

impl RemoteSession {
    fn motion(&self, bus: &Bus, to: Point) -> Result<(), String> {
        let (node, x, y) = stream_point(&self.streams, to.x, to.y)
            .ok_or_else(|| format!("({}, {}) is on no shared screen", to.x, to.y))?;
        notify(
            bus,
            "NotifyPointerMotionAbsolute",
            &(self.handle.as_ref(), no_options(), node, x, y),
        )
    }

    fn button(&self, bus: &Bus, button: MouseButton, pressed: bool) -> Result<(), String> {
        notify(
            bus,
            "NotifyPointerButton",
            &(
                self.handle.as_ref(),
                no_options(),
                button_code(button),
                u32::from(pressed),
            ),
        )
    }

    fn key(&self, bus: &Bus, key: &Key, pressed: bool) -> Result<(), String> {
        let state = u32::from(pressed);
        match evdev_code(key) {
            Some(code) => notify(
                bus,
                "NotifyKeyboardKeycode",
                &(self.handle.as_ref(), no_options(), code, state),
            ),
            None => match key {
                Key::Char(c) => self.keysym(bus, char_keysym(*c), pressed),
                other => Err(format!("cannot press {other:?}")),
            },
        }
    }

    fn keysym(&self, bus: &Bus, keysym: u32, pressed: bool) -> Result<(), String> {
        notify(
            bus,
            "NotifyKeyboardKeysym",
            &(
                self.handle.as_ref(),
                no_options(),
                keysym as i32,
                u32::from(pressed),
            ),
        )
    }
}

/// Input through the RemoteDesktop portal; points are global logical
/// pixels, like AT-SPI bounds after [`super::window_offsets`].
pub(crate) struct KdeInput;

const CLICK_GAP: Duration = Duration::from_millis(60);
const KEY_GAP: Duration = Duration::from_millis(8);

impl InputProvider for KdeInput {
    fn pointer_move(&self, to: Point) -> xa11y::Result<()> {
        with_remote(|bus, session| session.motion(bus, to))
    }

    fn pointer_down(&self, button: MouseButton) -> xa11y::Result<()> {
        with_remote(|bus, session| session.button(bus, button, true))
    }

    fn pointer_up(&self, button: MouseButton) -> xa11y::Result<()> {
        with_remote(|bus, session| session.button(bus, button, false))
    }

    fn pointer_click(&self, at: Point, button: MouseButton, count: u32) -> xa11y::Result<()> {
        with_remote(|bus, session| {
            session.motion(bus, at)?;
            for click in 0..count.max(1) {
                if click > 0 {
                    std::thread::sleep(CLICK_GAP);
                }
                session.button(bus, button, true)?;
                std::thread::sleep(KEY_GAP);
                session.button(bus, button, false)?;
            }
            Ok(())
        })
    }

    /// Smooth scrolling, [`SCROLL_PIXELS_PER_TICK`] per notch; positive
    /// `dy` scrolls down as libinput (and so KWin) defines it. The sign is
    /// still to be confirmed on hardware.
    fn pointer_scroll(&self, at: Point, delta: ScrollDelta) -> xa11y::Result<()> {
        with_remote(|bus, session| {
            session.motion(bus, at)?;
            let mut finish = HashMap::new();
            finish.insert("finish", Value::Bool(true));
            notify(
                bus,
                "NotifyPointerAxis",
                &(
                    session.handle.as_ref(),
                    finish,
                    f64::from(delta.dx) * SCROLL_PIXELS_PER_TICK,
                    f64::from(delta.dy) * SCROLL_PIXELS_PER_TICK,
                ),
            )
        })
    }

    fn key_down(&self, key: &Key) -> xa11y::Result<()> {
        with_remote(|bus, session| session.key(bus, key, true))
    }

    fn key_up(&self, key: &Key) -> xa11y::Result<()> {
        with_remote(|bus, session| session.key(bus, key, false))
    }

    /// One keysym per character; KWin finds the key (and level) in the
    /// active layout or a spare keycode.
    fn type_text(&self, text: &str) -> xa11y::Result<()> {
        with_remote(|bus, session| {
            for c in text.chars() {
                let keysym = char_keysym(c);
                session.keysym(bus, keysym, true)?;
                session.keysym(bus, keysym, false)?;
                std::thread::sleep(KEY_GAP);
            }
            Ok(())
        })
    }
}

// ---- Clipboard typing -------------------------------------------------------

const KLIPPER: &str = "org.kde.klipper";
const KLIPPER_INTERFACE: &str = "org.kde.klipper.klipper";

/// Pastes `text` into `pid`'s focused field through Klipper and Ctrl+V,
/// where keysyms cannot type it ([`needs_paste`]); `Ok(false)` when typing
/// should be used instead (newer Plasma, plain text, no Klipper). The
/// previous clipboard text comes back afterwards; other clipboard content
/// (images) does not, and Klipper's history keeps the pasted text. The
/// caller has checked that the field is not a password field.
pub(super) fn paste_text(pid: u32, app_exe: &str, text: &str) -> Result<bool, String> {
    if !needs_paste(plasma_version()?, text, app_exe) {
        return Ok(false);
    }
    let bus = bus()?;
    let previous: String = match call(
        &bus,
        KLIPPER,
        "/klipper",
        KLIPPER_INTERFACE,
        "getClipboardContents",
        &(),
    ) {
        Ok(previous) => previous,
        Err(error) => {
            eprintln!("todex-agentd: Klipper is unavailable, typing instead of pasting: {error}");
            return Ok(false);
        }
    };
    call::<_, ()>(
        &bus,
        KLIPPER,
        "/klipper",
        KLIPPER_INTERFACE,
        "setClipboardContents",
        &(text,),
    )?;
    let pasted = activate(pid).and_then(|()| {
        with_remote(|bus, session| {
            session.key(bus, &Key::Ctrl, true)?;
            let result = session
                .key(bus, &Key::Char('v'), true)
                .and_then(|()| session.key(bus, &Key::Char('v'), false));
            session.key(bus, &Key::Ctrl, false)?;
            result
        })
        .map_err(|error| error.to_string())
    });
    std::thread::sleep(PASTE_RESTORE_DELAY);
    let restored = if previous.is_empty() {
        call::<_, ()>(
            &bus,
            KLIPPER,
            "/klipper",
            KLIPPER_INTERFACE,
            "clearClipboardContents",
            &(),
        )
    } else {
        call::<_, ()>(
            &bus,
            KLIPPER,
            "/klipper",
            KLIPPER_INTERFACE,
            "setClipboardContents",
            &(previous.as_str(),),
        )
    };
    if let Err(error) = restored {
        eprintln!("todex-agentd: could not restore the clipboard: {error}");
    }
    pasted.map(|()| true)
}

// ---- Idle time ---------------------------------------------------------------

#[derive(Clone, Copy)]
enum IdleSource {
    Starting,
    Unavailable,
    Event(Option<(bool, Instant)>),
}

static IDLE: Mutex<IdleSource> = Mutex::new(IdleSource::Starting);

/// Seconds since the last input, from `ext-idle-notify-v1` (2-second
/// granularity; see [`kde_desktop::idle_seconds`]). Infinite when the
/// compositor lacks the protocol, as on X11 without the screensaver
/// extension.
pub(super) fn idle_seconds() -> f64 {
    start_idle_watch();
    match *IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        IdleSource::Starting => 0.0,
        IdleSource::Unavailable => f64::INFINITY,
        IdleSource::Event(None) => kde_desktop::idle_seconds(IdleEvent::None),
        IdleSource::Event(Some((idle, at))) => {
            let age = at.elapsed().as_secs_f64();
            kde_desktop::idle_seconds(if idle {
                IdleEvent::Idled(age)
            } else {
                IdleEvent::Resumed(age)
            })
        }
    }
}

/// Starts the Wayland thread that follows idle notifications, once.
pub(super) fn start_idle_watch() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("todex-idle".to_owned())
            .spawn(|| {
                if let Err(error) = idle::watch() {
                    eprintln!("todex-agentd: no Wayland idle time: {error}");
                }
                *IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    IdleSource::Unavailable;
            });
        if spawned.is_err() {
            *IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = IdleSource::Unavailable;
        }
    });
}

mod idle {
    use std::time::Instant;

    use wayland_client::{
        globals::{registry_queue_init, GlobalListContents},
        protocol::{wl_registry, wl_seat},
        Connection, Dispatch, Proxy as _, QueueHandle,
    };
    use wayland_protocols::ext::idle_notify::v1::client::{
        ext_idle_notification_v1::{self, ExtIdleNotificationV1},
        ext_idle_notifier_v1::ExtIdleNotifierV1,
    };

    use super::{IdleSource, IDLE};

    /// KWin's idle timeout for TodeX, in milliseconds.
    const TIMEOUT_MS: u32 = (super::kde_desktop::IDLE_NOTIFY_SECONDS * 1000.0) as u32;

    struct State;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotifierV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ExtIdleNotifierV1,
            _: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotificationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ExtIdleNotificationV1,
            event: ext_idle_notification_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let idle = match event {
                ext_idle_notification_v1::Event::Idled => true,
                ext_idle_notification_v1::Event::Resumed => false,
                _ => return,
            };
            *IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                IdleSource::Event(Some((idle, Instant::now())));
        }
    }

    /// Follows input idleness until the connection ends.
    pub(super) fn watch() -> Result<(), String> {
        let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&connection).map_err(|error| error.to_string())?;
        let handle = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&handle, 1..=1, ())
            .map_err(|error| format!("no seat: {error}"))?;
        let notifier: ExtIdleNotifierV1 = globals
            .bind(&handle, 1..=2, ())
            .map_err(|error| format!("no ext-idle-notify-v1: {error}"))?;
        // Version 2 ignores idle inhibitors (video players): only input counts.
        let _notification = if notifier.version() >= 2 {
            notifier.get_input_idle_notification(TIMEOUT_MS, &seat, &handle, ())
        } else {
            notifier.get_idle_notification(TIMEOUT_MS, &seat, &handle, ())
        };
        *IDLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = IdleSource::Event(None);
        let mut state = State;
        loop {
            queue
                .blocking_dispatch(&mut state)
                .map_err(|error| error.to_string())?;
        }
    }
}
