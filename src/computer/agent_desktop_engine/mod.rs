//! The optional [agent-desktop](https://github.com/vercel-labs/agent-desktop)
//! engine: when selected, observations read the window's accessibility
//! tree through its CLI and ref actions go to its refs.
//!
//! Everything that keeps the person at the host in control stays with the
//! native engine ([`super::engine`]): window listing, which app a call
//! targets and the [`super::policy`] checks, screenshots and their
//! painting-over of protected windows, typing (so text never reaches a
//! command line), coordinates, keys and app switching. The CLI only reads a
//! window that already passed the policy and acts on refs of that read.

mod install;

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        RwLock,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
pub(crate) use install::AGENT_DESKTOP_VERSION;
pub(crate) use install::{InstallState, Installer};

use super::{policy::Target, tree, ComputerError};

/// Snapshot namespace of TodeX's calls, apart from the user's own use.
const SESSION: &str = "todex";
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Output beyond this is not a snapshot TodeX can use.
const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DEPTH: u32 = 40;
/// How often a running command is checked for cancellation.
const POLL: Duration = Duration::from_millis(20);
/// The single automatic retry waits at most this long.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(1);

/// Which engine Computer Use runs on.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum EngineKind {
    #[default]
    Native,
    AgentDesktop,
}

/// What the settings screens show about the agent-desktop engine.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentDesktopEngineStatus {
    #[serde(flatten)]
    pub install: InstallState,
    /// Installed and runnable on this host.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The selected engine and the agent-desktop install.
pub(crate) struct EngineChoice {
    kind: RwLock<EngineKind>,
    installer: Installer,
}

impl EngineChoice {
    /// `state_dir`: the daemon's private `agent-desktop` state directory.
    pub(crate) fn new(state_dir: &Path, kind: EngineKind) -> Self {
        Self {
            kind: RwLock::new(kind),
            installer: Installer::new(state_dir.join("engine")),
        }
    }

    pub(crate) fn kind(&self) -> EngineKind {
        *self
            .kind
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn set_kind(&self, kind: EngineKind) {
        *self
            .kind
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = kind;
        if kind == EngineKind::AgentDesktop {
            self.installer.start_install();
        }
    }

    pub(crate) fn status(&self) -> AgentDesktopEngineStatus {
        let install = self.installer.state();
        let reason = if install.installed {
            None
        } else if install.downloading {
            Some("agent-desktop is downloading.".to_owned())
        } else if let Some(error) = &install.error {
            Some(format!("agent-desktop could not be installed: {error}"))
        } else {
            Some("agent-desktop is not installed yet.".to_owned())
        };
        AgentDesktopEngineStatus {
            available: reason.is_none(),
            reason,
            install,
        }
    }

    /// The CLI calls go to: `None` on the native engine, an error when
    /// agent-desktop is selected but cannot run. Never falls back to the
    /// native engine on its own, so agents and users see which one ran.
    pub(crate) fn cli(&self) -> Result<Option<Cli>, ComputerError> {
        if self.kind() != EngineKind::AgentDesktop {
            return Ok(None);
        }
        match self.installer.executable() {
            Some(path) => Ok(Some(Cli {
                path,
                tmp_dir: cli_tmp_dir(),
            })),
            None => {
                self.installer.start_install();
                Err(ComputerError::new(
                    "ENGINE_UNAVAILABLE",
                    self.status()
                        .reason
                        .unwrap_or_else(|| "agent-desktop is not available".to_owned())
                        + " Computer Use is set to the agent-desktop engine in Settings; retry once it is installed or switch back to the built-in engine.",
                ))
            }
        }
    }
}

/// Longest `TMPDIR` the CLI is given: its command host listens on a Unix
/// socket inside it, and socket paths are limited to ~104 bytes.
const MAX_TMPDIR_LEN: usize = 48;

/// The CLI's `TMPDIR` when the inherited one will not do (`None`: keep
/// it). It must be this user's alone; a long one (a daemon started without
/// the per-user one) is replaced by `/tmp/todex-ad-<uid>`.
fn cli_tmp_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let inherited =
            std::env::var_os("TMPDIR").filter(|dir| !dir.is_empty() && dir.len() <= MAX_TMPDIR_LEN);
        if inherited.is_some() {
            return None;
        }
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        Some(PathBuf::from(format!("/tmp/todex-ad-{uid}")))
    }
    #[cfg(not(unix))]
    None
}

/// One element of an agent-desktop snapshot, as an action needs it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExternalElement {
    /// Qualified ref (`@<snapshot>:eN`).
    pub reference: String,
    pub secure: bool,
    /// Global screen coordinates (x, y, width, height).
    pub bounds: Option<(f64, f64, f64, f64)>,
}

impl ExternalElement {
    pub(crate) fn center(&self) -> Option<(f64, f64)> {
        self.bounds
            .filter(|(_, _, width, height)| *width > 0.0 && *height > 0.0)
            .map(|(x, y, width, height)| (x + width / 2.0, y + height / 2.0))
    }
}

/// A window's tree read by agent-desktop: nodes whose handles index
/// `elements`.
pub(crate) struct Snapshot {
    pub root: tree::Node,
    pub elements: Vec<ExternalElement>,
    pub truncated: bool,
}

/// A ref action of the CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefAction {
    Click,
    DoubleClick,
    RightClick,
    Focus,
    Toggle,
    Expand,
    Collapse,
    ScrollTo,
    SetValue,
    Select,
}

impl RefAction {
    /// The `computer_act` action, for those agents name directly.
    pub(crate) fn from_action(action: &str) -> Option<Self> {
        Some(match action {
            "set_value" => Self::SetValue,
            "select" => Self::Select,
            "toggle" => Self::Toggle,
            "expand" => Self::Expand,
            "collapse" => Self::Collapse,
            "scroll_to" => Self::ScrollTo,
            _ => return None,
        })
    }

    fn command(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::DoubleClick => "double-click",
            Self::RightClick => "right-click",
            Self::Focus => "focus",
            Self::Toggle => "toggle",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::ScrollTo => "scroll-to",
            Self::SetValue => "set-value",
            Self::Select => "select",
        }
    }

    /// Actions that take a value argument.
    pub(crate) fn takes_text(self) -> bool {
        matches!(self, Self::SetValue | Self::Select)
    }
}

/// The agent-desktop CLI, one process per command.
#[derive(Clone, Debug)]
pub(crate) struct Cli {
    path: PathBuf,
    /// See [`cli_tmp_dir`].
    tmp_dir: Option<PathBuf>,
}

impl Cli {
    /// Reads `app`'s window titled `title` (the first of its windows when
    /// absent or not found), or the app when it has no window.
    pub(crate) fn snapshot(
        &self,
        app: &Target,
        title: Option<&str>,
        cancelled: &AtomicBool,
    ) -> Result<Snapshot, ComputerError> {
        // A listing that fails (agent-desktop names the app differently)
        // leaves the snapshot to find the app by name, and to say why not.
        let window = match self.run(
            &["list-windows", "--app", &app.name],
            SNAPSHOT_TIMEOUT,
            cancelled,
        ) {
            Ok(windows) => pick_window(&windows, app.pid, title),
            Err(error) if error.code == "CANCELLED" => return Err(error),
            Err(error) => {
                tracing::debug!(%error, "agent-desktop could not list the app's windows");
                None
            }
        };
        let depth = MAX_DEPTH.to_string();
        let mut args = vec!["snapshot", "--include-bounds", "--max-depth", &depth];
        let id;
        match &window {
            Some(window) => {
                id = window.clone();
                args.extend(["--window-id", id.as_str()]);
            }
            None => args.extend(["--app", app.name.as_str()]),
        }
        let data = self.run(&args, SNAPSHOT_TIMEOUT, cancelled)?;
        convert_snapshot(&data)
    }

    /// Performs `action` on `reference`; `text` is the value of
    /// `set_value` / `select`.
    pub(crate) fn act(
        &self,
        action: RefAction,
        reference: &str,
        text: Option<&str>,
        cancelled: &AtomicBool,
    ) -> Result<(), ComputerError> {
        // `--`: a value starting with `-` is not an option.
        let mut args = vec![action.command(), "--", reference];
        if action.takes_text() {
            args.push(text.unwrap_or_default());
        }
        self.run(&args, ACTION_TIMEOUT, cancelled).map(|_| ())
    }

    /// Runs one command and returns its `data`. Retries once only when the
    /// CLI says nothing was delivered and a plain retry is safe.
    fn run(
        &self,
        args: &[&str],
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Value, ComputerError> {
        match self.run_once(args, timeout, cancelled) {
            Err(Failure::Retry(after, _)) => {
                std::thread::sleep(after.min(MAX_RETRY_AFTER));
                self.run_once(args, timeout, cancelled)
                    .map_err(Failure::into_error)
            }
            other => other.map_err(Failure::into_error),
        }
    }

    fn run_once(
        &self,
        args: &[&str],
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Value, Failure> {
        if cancelled.load(Ordering::SeqCst) {
            return Err(Failure::Final(cancelled_error()));
        }
        let mut command = Command::new(&self.path);
        if let Some(tmp_dir) = &self.tmp_dir {
            // Fails when another user owns it: never share it.
            if let Err(error) = crate::secure_fs::ensure_owner_only_dir(tmp_dir) {
                return Err(Failure::Final(ComputerError::platform(format!(
                    "cannot prepare {} for agent-desktop: {error}",
                    tmp_dir.display()
                ))));
            }
            command.env("TMPDIR", tmp_dir);
        }
        let mut child = command
            .args(["--session", SESSION])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                Failure::Final(ComputerError::new(
                    "ENGINE_UNAVAILABLE",
                    format!("cannot run agent-desktop: {error}"),
                ))
            })?;
        // Read while it runs: a large snapshot would fill the pipe and
        // stall the process before it exits.
        let mut stdout = child.stdout.take().expect("piped stdout");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = (&mut stdout)
                .take(MAX_OUTPUT_BYTES as u64 + 1)
                .read_to_end(&mut bytes);
            result.map(|_| bytes)
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    return Err(Failure::Final(ComputerError::platform(error)));
                }
            }
            if cancelled.load(Ordering::SeqCst) || started.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Final(if cancelled.load(Ordering::SeqCst) {
                    cancelled_error()
                } else {
                    ComputerError::new(
                        "TIMEOUT",
                        format!(
                            "agent-desktop {} took longer than {} s; observe again before retrying",
                            args[0],
                            timeout.as_secs()
                        ),
                    )
                }));
            }
            std::thread::sleep(POLL);
        };
        let bytes = reader
            .join()
            .map_err(|_| {
                Failure::Final(ComputerError::platform(
                    "agent-desktop output reader failed",
                ))
            })?
            .map_err(|error| Failure::Final(ComputerError::platform(error)))?;
        if bytes.len() > MAX_OUTPUT_BYTES {
            return Err(Failure::Final(ComputerError::platform(
                "agent-desktop printed more than TodeX reads",
            )));
        }
        parse_envelope(&bytes).map_err(|failure| match failure {
            // A process that died without an envelope says why only by
            // its status.
            Failure::Final(error) if error.code == "EXECUTOR_FAILED" && !status.success() => {
                Failure::Final(ComputerError::platform(format!(
                    "agent-desktop {} failed ({status})",
                    args[0]
                )))
            }
            other => other,
        })
    }
}

fn cancelled_error() -> ComputerError {
    ComputerError::new(
        "CANCELLED",
        "the call timed out or was cancelled before its input was sent",
    )
}

#[derive(Debug)]
enum Failure {
    /// Safe to retry after the delay.
    Retry(Duration, ComputerError),
    Final(ComputerError),
}

impl Failure {
    fn into_error(self) -> ComputerError {
        match self {
            Self::Retry(_, error) | Self::Final(error) => error,
        }
    }
}

/// The `data` of a successful envelope, or its error as TodeX reports it.
fn parse_envelope(bytes: &[u8]) -> Result<Value, Failure> {
    let envelope: Value = serde_json::from_slice(bytes).map_err(|error| {
        Failure::Final(ComputerError::platform(format!(
            "agent-desktop printed no JSON envelope: {error}"
        )))
    })?;
    if envelope["ok"].as_bool() == Some(true) {
        return Ok(envelope["data"].clone());
    }
    let error = &envelope["error"];
    let code = error["code"].as_str().unwrap_or("EXECUTOR_FAILED");
    let mut message = error["message"]
        .as_str()
        .unwrap_or("agent-desktop failed")
        .to_owned();
    let retry = error["disposition"]["retry"].as_str().unwrap_or("unknown");
    let (code, hint) = match code {
        // Refs come from TodeX's observation, not the agent's own.
        "STALE_REF" | "AMBIGUOUS_TARGET" | "ELEMENT_NOT_FOUND" => (
            "STALE_REF",
            Some("the screen changed; call computer_observe and use its refs"),
        ),
        "PERM_DENIED" | "PERMISSION_DENIED" => (
            "PERMISSION_MISSING",
            Some("grant the TodeX backend the missing permission in Settings > Computer Use"),
        ),
        other => (other, error["suggestion"].as_str()),
    };
    if let Some(hint) = hint {
        message = format!("{message} ({hint})");
    }
    let detail = serde_json::json!({
        "engine": "agentDesktop",
        "delivery": error["disposition"]["delivery"],
        "retry": retry,
    });
    let error = ComputerError {
        code: code.to_owned(),
        message,
        detail: Some(detail),
    };
    let retryable =
        retry == "safe" && matches!(error.code.as_str(), "TIMEOUT" | "APP_UNRESPONSIVE");
    Err(if retryable {
        let after = envelope["error"]["recovery"]["retry_after_ms"]
            .as_u64()
            .unwrap_or(250);
        Failure::Retry(Duration::from_millis(after), error)
    } else {
        Failure::Final(error)
    })
}

/// Whether nothing reached the app, so another way may be tried.
pub(crate) fn not_delivered(error: &ComputerError) -> bool {
    error
        .detail
        .as_ref()
        .is_some_and(|detail| detail["delivery"] == "not_delivered")
}

/// The window id of `pid`'s window titled `title` (its first window when
/// none matches) from `list-windows` data.
fn pick_window(windows: &Value, pid: u32, title: Option<&str>) -> Option<String> {
    let windows: Vec<&Value> = windows
        .as_array()?
        .iter()
        .filter(|window| window["pid"].as_u64() == Some(u64::from(pid)))
        .collect();
    windows
        .iter()
        .find(|window| title.is_some_and(|title| window["title"].as_str() == Some(title)))
        .or_else(|| windows.first())
        .and_then(|window| window["id"].as_str().map(str::to_owned))
}

fn convert_snapshot(data: &Value) -> Result<Snapshot, ComputerError> {
    let tree = data
        .get("tree")
        .filter(|tree| tree.is_object())
        .ok_or_else(|| ComputerError::platform("agent-desktop returned no tree"))?;
    let mut elements = Vec::new();
    let mut budget = super::engine::MAX_NODES;
    let root = convert_node(tree, &mut elements, &mut budget, 0);
    Ok(Snapshot {
        root,
        elements,
        truncated: data["complete"].as_bool() == Some(false) || budget == 0,
    })
}

fn convert_node(
    node: &Value,
    elements: &mut Vec<ExternalElement>,
    budget: &mut usize,
    depth: u32,
) -> tree::Node {
    *budget = budget.saturating_sub(1);
    let states: Vec<&str> = node["states"]
        .as_array()
        .map(|states| states.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let role = node["role"].as_str().unwrap_or("unknown");
    let secure = states.contains(&"secure") || role.contains("secure") || role.contains("password");
    let reference = node["ref_id"].as_str().map(str::to_owned);
    let bounds = node
        .get("bounds")
        .filter(|bounds| bounds.is_object())
        .map(|bounds| {
            let number = |key: &str| bounds[key].as_f64().unwrap_or(0.0);
            (number("x"), number("y"), number("width"), number("height"))
        });
    let handle = elements.len();
    // Nodes without a ref keep a slot so handles stay indexes; tree
    // formatting only gives refs to actionable nodes, which have one.
    elements.push(ExternalElement {
        reference: reference.clone().unwrap_or_default(),
        secure,
        bounds,
    });
    let text = |key: &str| node[key].as_str().map(str::to_owned);
    let mut out = tree::Node {
        role: tree_role(role).to_owned(),
        name: text("name").or_else(|| text("description")),
        value: if secure { None } else { text("value") },
        enabled: !states.contains(&"disabled"),
        focused: states.contains(&"focused"),
        selected: states.contains(&"selected") || states.contains(&"checked"),
        secure,
        actionable: reference.is_some(),
        handle,
        children: Vec::new(),
    };
    if depth < MAX_DEPTH {
        if let Some(children) = node["children"].as_array() {
            for child in children {
                if *budget == 0 {
                    break;
                }
                out.children
                    .push(convert_node(child, elements, budget, depth + 1));
            }
        }
    }
    out
}

/// agent-desktop's role tokens in the native engine's spelling, which
/// [`tree::format`] knows (text, structure and actionable roles).
fn tree_role(role: &str) -> &str {
    match role {
        "statictext" => "static_text",
        "textfield" => "text_field",
        "checkbox" => "check_box",
        "radiobutton" => "radio_button",
        "combobox" => "combo_box",
        "menuitem" => "menu_item",
        "treeitem" => "tree_item",
        "scrollbar" => "scroll_bar",
        "" => "unknown",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn snapshots_become_trees_with_refs_and_hidden_secrets() {
        let data = json!({
            "app": "Mail",
            "snapshot_id": "s1",
            "complete": true,
            "tree": {
                "role": "window", "name": "Login",
                "children": [
                    { "role": "textfield", "name": "User", "value": "ann", "ref_id": "@s1:e1",
                      "bounds": { "x": 10.0, "y": 20.0, "width": 100.0, "height": 20.0 } },
                    { "role": "textfield", "name": "Password", "value": "hunter2",
                      "states": ["secure", "focused"], "ref_id": "@s1:e2" },
                    { "role": "button", "name": "Sign in", "states": ["disabled"], "ref_id": "@s1:e3" },
                    { "role": "statictext", "value": "Welcome" }
                ]
            }
        });
        let snapshot = convert_snapshot(&data).unwrap();
        let formatted = tree::format(&snapshot.root);
        assert!(!formatted.text.contains("hunter2"));
        assert!(formatted.text.contains("(password)"));
        assert!(formatted.text.contains("[disabled]"));
        assert!(formatted.text.contains("value=\"ann\""));
        let handle = formatted.refs["e1"];
        assert_eq!(snapshot.elements[handle].reference, "@s1:e1");
        assert_eq!(snapshot.elements[handle].center(), Some((60.0, 30.0)));
        let password = &snapshot.elements[formatted.refs["e2"]];
        assert!(password.secure);
        assert_eq!(password.reference, "@s1:e2");
        assert!(!snapshot.truncated);
    }

    #[test]
    fn a_partial_snapshot_is_reported_truncated() {
        let data = json!({ "complete": false, "tree": { "role": "window" } });
        assert!(convert_snapshot(&data).unwrap().truncated);
        assert!(convert_snapshot(&json!({ "complete": true })).is_err());
    }

    #[test]
    fn the_window_is_picked_by_process_then_title() {
        let windows = json!([
            { "id": "w-1", "title": "Other", "app_name": "Notes", "pid": 7 },
            { "id": "w-2", "title": "Inbox", "app_name": "Mail", "pid": 42 },
            { "id": "w-3", "title": "Draft", "app_name": "Mail", "pid": 42 },
        ]);
        assert_eq!(
            pick_window(&windows, 42, Some("Draft")).as_deref(),
            Some("w-3")
        );
        assert_eq!(
            pick_window(&windows, 42, Some("Gone")).as_deref(),
            Some("w-2")
        );
        assert_eq!(pick_window(&windows, 42, None).as_deref(), Some("w-2"));
        assert_eq!(pick_window(&windows, 99, None), None);
    }

    #[test]
    fn envelopes_map_to_data_or_todex_errors() {
        let ok = br#"{"version":"2.5","ok":true,"command":"click","data":{"action":"click"}}"#;
        assert_eq!(parse_envelope(ok).unwrap()["action"], "click");

        let stale = br#"{"ok":false,"error":{"code":"STALE_REF","message":"gone",
            "disposition":{"delivery":"not_delivered","retry":"safe"}}}"#;
        let error = parse_envelope(stale).unwrap_err().into_error();
        assert_eq!(error.code, "STALE_REF");
        assert!(error.message.contains("computer_observe"));
        assert!(not_delivered(&error));

        let timeout = br#"{"ok":false,"error":{"code":"TIMEOUT","message":"slow",
            "disposition":{"delivery":"not_delivered","retry":"safe"},
            "recovery":{"strategy":"retry","retryable":true,"retry_after_ms":5000}}}"#;
        assert!(matches!(parse_envelope(timeout), Err(Failure::Retry(_, _))));

        let landed = br#"{"ok":false,"error":{"code":"TIMEOUT","message":"slow",
            "disposition":{"delivery":"delivery_uncertain","retry":"unsafe"}}}"#;
        let error = match parse_envelope(landed) {
            Err(Failure::Final(error)) => error,
            other => panic!("expected a final error, got {other:?}"),
        };
        assert!(!not_delivered(&error));

        assert!(parse_envelope(b"not json").is_err());
    }

    /// Runs the CLI `TODEX_AGENT_DESKTOP_PATH` names; without permissions it
    /// still has to answer every call with an envelope TodeX understands.
    #[test]
    #[ignore = "needs an agent-desktop binary in TODEX_AGENT_DESKTOP_PATH"]
    fn the_real_cli_answers_with_envelopes() {
        let path = std::env::var_os(install::PATH_OVERRIDE_ENV).expect("binary path");
        let cli = Cli {
            path: PathBuf::from(path),
            tmp_dir: cli_tmp_dir(),
        };
        let never = AtomicBool::new(false);
        let app = Target {
            id: "com.apple.finder".into(),
            name: "Finder".into(),
            pid: 1,
        };
        match cli.snapshot(&app, None, &never) {
            Ok(snapshot) => assert!(!snapshot.elements.is_empty()),
            Err(error) => assert_ne!(error.code, "EXECUTOR_FAILED", "{error}"),
        }
        for action in [RefAction::Focus, RefAction::SetValue] {
            let error = cli
                .act(action, "@sabc:e1", Some("-5"), &never)
                .expect_err("no such ref");
            assert!(
                matches!(error.code.as_str(), "STALE_REF" | "PERMISSION_MISSING"),
                "{error}"
            );
            assert!(not_delivered(&error));
        }
        let cancelled = AtomicBool::new(true);
        let error = cli
            .act(RefAction::Focus, "@sabc:e1", None, &cancelled)
            .expect_err("cancelled");
        assert_eq!(error.code, "CANCELLED");
    }

    #[test]
    fn named_ref_actions_map_to_cli_commands() {
        assert_eq!(
            RefAction::from_action("scroll_to"),
            Some(RefAction::ScrollTo)
        );
        assert_eq!(RefAction::ScrollTo.command(), "scroll-to");
        assert!(RefAction::SetValue.takes_text());
        assert!(!RefAction::Toggle.takes_text());
        assert_eq!(RefAction::from_action("click"), None);
    }
}
