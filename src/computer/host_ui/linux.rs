//! Linux host UI through freedesktop notifications on the session bus
//! (KDE Plasma shows them with action buttons): a persistent notification
//! with a Stop action while an agent is in control, and a notification with
//! Allow/Deny for confirmations, falling back to `kdialog` or `zenity`
//! when no notification server offers actions. There is no overlay and no
//! pointer marker. On KDE Plasma Wayland, Ctrl+Alt+Shift+Esc registered
//! with KGlobalAccel also stops a session; elsewhere there is no global
//! stop shortcut.

use std::{
    collections::HashMap,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Mutex,
    },
    time::{Duration, Instant},
};

use zbus::{
    blocking::{Connection as Bus, MessageIterator, Proxy},
    message::Type as MessageType,
    zvariant::Value,
    MatchRule,
};

use super::Strings;
use crate::computer::platform::{plain_proxy, session_bus};

/// Whether KGlobalAccel took the stop shortcut.
static SHORTCUT_REGISTERED: AtomicBool = AtomicBool::new(false);

pub(super) fn stop_shortcut() -> Option<&'static str> {
    SHORTCUT_REGISTERED
        .load(Ordering::SeqCst)
        .then_some(kglobalaccel::SHORTCUT_LABEL)
}

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP_NAME: &str = "TodeX";
/// Freedesktop urgency "critical": stays until closed.
const CRITICAL: u8 = 2;

/// A notification event for a waiting [`confirm`].
enum Event {
    Action(String),
    Closed,
}

/// The status notification's id, 0 when none is shown.
static STATUS: Mutex<u32> = Mutex::new(0);
/// Confirmations waiting for their notification's answer, by id.
static WAITERS: Mutex<Option<HashMap<u32, mpsc::Sender<Event>>>> = Mutex::new(None);
/// One confirmation at a time.
static CONFIRMING: Mutex<()> = Mutex::new(());

pub(super) fn run_with_main_loop(body: impl FnOnce()) -> ! {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland || std::env::var_os("DISPLAY").is_some() {
        match session_bus() {
            Some(bus) => match listen(bus) {
                Ok(()) => {
                    super::mark_available();
                    if wayland && kglobalaccel::kde_desktop() {
                        match kglobalaccel::register() {
                            Ok(()) => SHORTCUT_REGISTERED.store(true, Ordering::SeqCst),
                            Err(error) => eprintln!(
                                "todex-agentd: no Computer Use stop shortcut (KGlobalAccel): {error}"
                            ),
                        }
                    }
                }
                Err(error) => {
                    eprintln!("todex-agentd: Computer Use host UI unavailable: {error}");
                }
            },
            None => {
                eprintln!("todex-agentd: Computer Use host UI unavailable: no D-Bus session bus")
            }
        }
    }
    body();
    unreachable!("the body exits the process");
}

/// Dispatches notification signals on a background thread.
fn listen(bus: Bus) -> Result<(), String> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(SERVICE)
        .and_then(|rule| rule.path(PATH))
        .map_err(|error| error.to_string())?
        .build();
    let messages =
        MessageIterator::for_match_rule(rule, &bus, None).map_err(|error| error.to_string())?;
    std::thread::Builder::new()
        .name("todex-notifications".to_owned())
        .spawn(move || {
            for message in messages.flatten() {
                let header = message.header();
                let Some(member) = header.member().map(|member| member.as_str().to_owned()) else {
                    continue;
                };
                let (id, event) = match member.as_str() {
                    "ActionInvoked" => match message.body().deserialize::<(u32, String)>() {
                        Ok((id, action)) => (id, Event::Action(action)),
                        Err(_) => continue,
                    },
                    "NotificationClosed" => match message.body().deserialize::<(u32, u32)>() {
                        Ok((id, _reason)) => (id, Event::Closed),
                        Err(_) => continue,
                    },
                    _ => continue,
                };
                dispatch(id, event);
            }
            eprintln!("todex-agentd: Computer Use notifications stopped");
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn dispatch(id: u32, event: Event) {
    {
        let mut status = STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *status == id && id != 0 {
            match &event {
                Event::Action(action) if action == "stop" => super::request_stop(),
                Event::Closed => *status = 0,
                Event::Action(_) => {}
            }
            return;
        }
    }
    let waiters = WAITERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(waiter) = waiters.as_ref().and_then(|waiters| waiters.get(&id)) {
        let _ = waiter.send(event);
    }
}

fn notifications() -> Option<Proxy<'static>> {
    plain_proxy(&session_bus()?, SERVICE, PATH, SERVICE)
}

/// Posts (or replaces) a notification and returns its id.
fn notify(
    proxy: &Proxy<'_>,
    replaces: u32,
    summary: &str,
    body: &str,
    actions: &[&str],
    resident: bool,
    timeout_ms: i32,
) -> zbus::Result<u32> {
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("urgency", Value::U8(CRITICAL));
    if resident {
        hints.insert("resident", Value::Bool(true));
    }
    proxy.call(
        "Notify",
        &(
            APP_NAME,
            replaces,
            "dialog-warning",
            summary,
            escape_markup(body),
            actions.to_vec(),
            hints,
            timeout_ms,
        ),
    )
}

/// Notification bodies may be interpreted as markup.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn supports_actions(proxy: &Proxy<'_>) -> bool {
    proxy
        .call::<_, _, Vec<String>>("GetCapabilities", &())
        .is_ok_and(|capabilities| capabilities.iter().any(|cap| cap == "actions"))
}

pub(super) fn preferred_language() -> Option<String> {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| {
            value
                .split(':')
                .next()
                .filter(|language| !language.is_empty() && *language != "C")
                .map(str::to_owned)
        })
}

pub(super) fn show_status(strings: &Strings, summary: &str) {
    let Some(proxy) = notifications() else {
        return;
    };
    let mut status = STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match notify(
        &proxy,
        *status,
        strings.controlling,
        summary,
        &["stop", strings.stop],
        true,
        0,
    ) {
        Ok(id) => *status = id,
        Err(error) => eprintln!("todex-agentd: could not show the Computer Use status: {error}"),
    }
}

pub(super) fn hide_status() {
    let id = std::mem::take(
        &mut *STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    if id == 0 {
        return;
    }
    if let Some(proxy) = notifications() {
        let _ = proxy.call::<_, _, ()>("CloseNotification", &(id,));
    }
}

pub(super) fn mark_point(_x: f64, _y: f64) {}

pub(super) fn confirm(strings: &Strings, title: &str, message: &str, timeout: Duration) -> bool {
    let _one_at_a_time = CONFIRMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(proxy) = notifications().filter(supports_actions) {
        if let Some(answer) = confirm_by_notification(&proxy, strings, title, message, timeout) {
            return answer;
        }
    }
    confirm_by_dialog(strings, title, message, timeout)
}

/// `None` when the notification could not be posted.
fn confirm_by_notification(
    proxy: &Proxy<'_>,
    strings: &Strings,
    title: &str,
    message: &str,
    timeout: Duration,
) -> Option<bool> {
    let (sender, answers) = mpsc::channel();
    let id = {
        // Held across Notify so an instant answer cannot beat the waiter.
        let mut waiters = WAITERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let id = match notify(
            proxy,
            0,
            title,
            message,
            &["allow", strings.allow, "deny", strings.deny],
            false,
            timeout_ms,
        ) {
            Ok(id) => id,
            Err(error) => {
                eprintln!("todex-agentd: could not post the Computer Use confirmation: {error}");
                return None;
            }
        };
        waiters.get_or_insert_with(HashMap::new).insert(id, sender);
        id
    };
    let allowed =
        matches!(answers.recv_timeout(timeout), Ok(Event::Action(action)) if action == "allow");
    if let Some(waiters) = WAITERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        waiters.remove(&id);
    }
    if !allowed {
        let _ = proxy.call::<_, _, ()>("CloseNotification", &(id,));
    }
    Some(allowed)
}

/// `kdialog`, else `zenity`; false when neither exists or on timeout.
fn confirm_by_dialog(strings: &Strings, title: &str, message: &str, timeout: Duration) -> bool {
    let seconds = timeout.as_secs().max(1).to_string();
    let attempts: [(&str, Vec<&str>); 2] = [
        (
            "kdialog",
            vec![
                "--title",
                title,
                "--yes-label",
                strings.allow,
                "--no-label",
                strings.deny,
                "--warningyesno",
                message,
            ],
        ),
        (
            "zenity",
            vec![
                "--question",
                "--title",
                title,
                "--text",
                message,
                "--ok-label",
                strings.allow,
                "--cancel-label",
                strings.deny,
                "--timeout",
                &seconds,
            ],
        ),
    ];
    for (program, args) in attempts {
        let mut child = match Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                eprintln!("todex-agentd: could not run {program}: {error}");
                continue;
            }
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }
    eprintln!(
        "todex-agentd: no notification server with actions, kdialog or zenity to confirm Computer Use"
    );
    false
}

/// The stop shortcut through KDE's global shortcut service, which Wayland
/// clients cannot grab keys without.
mod kglobalaccel {
    use std::time::Duration;

    use zbus::{
        blocking::{Connection as Bus, MessageIterator},
        message::Type as MessageType,
        MatchRule,
    };

    pub(super) const SHORTCUT_LABEL: &str = "Ctrl+Alt+Shift+Esc";

    const SERVICE: &str = "org.kde.kglobalaccel";
    const INTERFACE: &str = "org.kde.KGlobalAccel";
    const COMPONENT: &str = "todex-agentd";
    const ACTION: &str = "stop-computer-use";
    /// Qt::ControlModifier | Qt::AltModifier | Qt::ShiftModifier | Qt::Key_Escape.
    const STOP_KEYS: i32 = 0x0400_0000 | 0x0800_0000 | 0x0200_0000 | 0x0100_0000;
    /// KGlobalAccel's setter flags.
    const IS_DEFAULT: u32 = 1;
    const SET_PRESENT: u32 = 2;

    pub(super) fn kde_desktop() -> bool {
        std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktops| {
            desktops
                .split(':')
                .any(|desktop| desktop.trim().eq_ignore_ascii_case("kde"))
        })
    }

    /// Registers the action and its default keys (a shortcut the user
    /// re-bound in System Settings is kept), then forwards presses to
    /// [`super::super::request_stop`].
    pub(super) fn register() -> Result<(), String> {
        let bus = zbus::blocking::connection::Builder::session()
            .and_then(|builder| builder.method_timeout(Duration::from_secs(5)).build())
            .map_err(|error| error.to_string())?;
        let id = [COMPONENT, ACTION, "TodeX", "Stop Computer Use"];
        call::<_, ()>(&bus, "doRegister", &(id,))?;
        // QKeySequence travels as `(ai)`: up to four combined key codes.
        let keys = vec![(vec![STOP_KEYS, 0, 0, 0],)];
        let assigned = match call::<_, Vec<(Vec<i32>,)>>(
            &bus,
            "setShortcutKeys",
            &(id, keys.clone(), SET_PRESENT),
        ) {
            Ok(assigned) => {
                call::<_, Vec<(Vec<i32>,)>>(&bus, "setShortcutKeys", &(id, keys, IS_DEFAULT))?;
                assigned
                    .iter()
                    .any(|(sequence,)| sequence.iter().any(|key| *key != 0))
            }
            // KGlobalAccel before KF 5.90 only knows plain key codes.
            Err(_) => {
                let assigned =
                    call::<_, Vec<i32>>(&bus, "setShortcut", &(id, vec![STOP_KEYS], SET_PRESENT))?;
                call::<_, Vec<i32>>(&bus, "setShortcut", &(id, vec![STOP_KEYS], IS_DEFAULT))?;
                assigned.iter().any(|key| *key != 0)
            }
        };
        if !assigned {
            return Err(format!("{SHORTCUT_LABEL} is taken by another action"));
        }
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface("org.kde.kglobalaccel.Component")
            .and_then(|rule| rule.member("globalShortcutPressed"))
            .map_err(|error| error.to_string())?
            .build();
        let presses =
            MessageIterator::for_match_rule(rule, &bus, None).map_err(|error| error.to_string())?;
        std::thread::Builder::new()
            .name("todex-stop-shortcut".to_owned())
            .spawn(move || {
                for message in presses.flatten() {
                    let Ok((component, action, _timestamp)) =
                        message.body().deserialize::<(String, String, i64)>()
                    else {
                        continue;
                    };
                    if component == COMPONENT && action == ACTION {
                        super::super::request_stop();
                    }
                }
                eprintln!("todex-agentd: the Computer Use stop shortcut stopped listening");
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn call<B, R>(bus: &Bus, method: &str, body: &B) -> Result<R, String>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        bus.call_method(
            Some(SERVICE),
            "/kglobalaccel",
            Some(INTERFACE),
            method,
            body,
        )
        .map_err(|error| format!("{method}: {error}"))?
        .body()
        .deserialize::<R>()
        .map_err(|error| format!("{method} replied unexpectedly: {error}"))
    }
}
