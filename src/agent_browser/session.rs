//! One running browser profile: its Chromium, the conversations' tabs (each
//! in its own background window), the navigation guard, and the actions.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use super::{
    ax,
    cdp::Cdp,
    launch::Process,
    policy::{self, allowed_top_level, Navigation, Popup},
    BrowserError, Frames,
};

const VIEWPORT_WIDTH: u32 = 1280;
const VIEWPORT_HEIGHT: u32 = 800;
const SCREENSHOT_MAX_WIDTH: f64 = 1280.0;
const SCREENSHOT_QUALITY: u8 = 70;
const SETTLE_LIMIT: Duration = Duration::from_secs(10);
const MAX_WAIT_MS: u64 = 10_000;
/// Live frames arrive at most this often: each frame is acked only once
/// this long has passed since the previous one, and Chromium encodes the
/// next frame only after the ack.
const FRAME_INTERVAL: Duration = Duration::from_millis(66);
const SCREENCAST_QUALITY: u8 = 60;

/// Permissions agent pages never get (no prompt reaches anyone).
const DENIED_PERMISSIONS: &[&str] = &[
    "geolocation",
    "notifications",
    "audioCapture",
    "videoCapture",
    "midi",
    "midiSysex",
    "clipboardReadWrite",
    "displayCapture",
    "durableStorage",
    "idleDetection",
    "localFonts",
    "sensors",
    "storageAccess",
    "windowManagement",
    "nfc",
];

pub(crate) struct Tab {
    pub target_id: String,
    pub session_id: String,
    pub refs: HashMap<String, i64>,
    /// The last agent tool call on this tab.
    pub last_used: Instant,
    /// `Page.startScreencast` is in effect.
    screencasting: bool,
}

/// What the event loop needs to know about a tab without awaiting.
#[derive(Clone)]
struct SessionInfo {
    conversation_id: String,
    target_id: String,
    main_frame_id: String,
}

pub(crate) struct Running {
    pub cdp: Cdp,
    process: Mutex<Option<Process>>,
    pub tabs: tokio::sync::Mutex<HashMap<String, Tab>>,
    sessions: Mutex<HashMap<String, SessionInfo>>,
    pub last_used: Mutex<Instant>,
    frames: Weak<Frames>,
}

impl Running {
    pub(crate) async fn start(
        process: Process,
        frames: Weak<Frames>,
    ) -> Result<Arc<Self>, BrowserError> {
        let cdp = process.cdp.clone();
        let running = Arc::new(Self {
            cdp: cdp.clone(),
            process: Mutex::new(Some(process)),
            tabs: tokio::sync::Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            last_used: Mutex::new(Instant::now()),
            frames,
        });
        cdp.call(
            "Browser.setDownloadBehavior",
            json!({ "behavior": "deny" }),
            None,
        )
        .await?;
        for permission in DENIED_PERMISSIONS {
            // Names unknown to this Chromium are rejected; the rest apply.
            let _ = cdp
                .call(
                    "Browser.setPermission",
                    json!({ "permission": { "name": permission }, "setting": "denied" }),
                    None,
                )
                .await;
        }
        cdp.call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )
        .await?;
        Self::spawn_events(&running);
        Self::spawn_frames(&running);
        Ok(running)
    }

    pub(crate) fn alive(&self) -> bool {
        !self.cdp.is_closed()
    }

    pub(crate) fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    pub(crate) fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .elapsed()
    }

    /// Ends the Chromium process.
    pub(crate) fn shut_down(&self) {
        self.process
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    fn session_info(&self, session_id: &str) -> Option<SessionInfo> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned()
    }

    /// Navigation guard, dialogs, popups and closed windows. These events
    /// arrive on a lossless queue: a dropped `Fetch.requestPaused` would
    /// leave that navigation hanging.
    fn spawn_events(running: &Arc<Self>) {
        let weak = Arc::downgrade(running);
        let Some(mut events) = running.cdp.take_events() else {
            return;
        };
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(running) = weak.upgrade() else { break };
                let session = event
                    .session_id
                    .as_deref()
                    .and_then(|session| running.session_info(session));
                match (event.method.as_str(), session) {
                    ("Fetch.requestPaused", info) => {
                        let params = &event.params;
                        let request_id =
                            params["requestId"].as_str().unwrap_or_default().to_owned();
                        let url = params["request"]["url"].as_str().unwrap_or_default();
                        // A tab not (or no longer) registered is guarded as
                        // if every Document were top-level.
                        let main_frame = info.as_ref().is_none_or(|info| {
                            params["frameId"].as_str() == Some(info.main_frame_id.as_str())
                        });
                        let resource_type = params["resourceType"].as_str().unwrap_or_default();
                        // `Aborted` cancels the navigation and keeps the current page
                        // (other reasons commit an error page).
                        let (method, body) = match policy::guard_request(
                            url,
                            resource_type,
                            main_frame,
                        ) {
                            Navigation::Block => {
                                tracing::info!(url, "agent browser blocked a top-level navigation");
                                (
                                    "Fetch.failRequest",
                                    json!({ "requestId": request_id, "errorReason": "Aborted" }),
                                )
                            }
                            Navigation::Continue => {
                                ("Fetch.continueRequest", json!({ "requestId": request_id }))
                            }
                        };
                        running
                            .cdp
                            .notify(method, body, event.session_id.as_deref());
                    }
                    ("Page.javascriptDialogOpening", Some(_)) => {
                        // Nobody is there to answer; a pending dialog would
                        // freeze the page for every later action.
                        running.cdp.notify(
                            "Page.handleJavaScriptDialog",
                            json!({ "accept": false }),
                            event.session_id.as_deref(),
                        );
                    }
                    ("Target.targetCreated", _) => running.fold_popup(&event.params),
                    ("Target.detachedFromTarget", _) => {
                        // The window was closed (by the person at the host,
                        // or a crash): the conversation has no tab now.
                        if let Some(session_id) = event.params["sessionId"].as_str() {
                            let removed = running
                                .sessions
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(session_id);
                            if let Some(info) = removed {
                                running.tabs.lock().await.remove(&info.conversation_id);
                                if let Some(frames) = running.frames.upgrade() {
                                    frames.tab_closed(&info.conversation_id);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// Popups stay in the agent's tab: local ones load there, the rest
    /// are refused.
    fn fold_popup(&self, params: &Value) {
        let info = &params["targetInfo"];
        let opener = info["openerId"].as_str().unwrap_or_default();
        if opener.is_empty() || info["type"] != "page" {
            return;
        }
        let Some(opener_session) = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(_, session)| session.target_id == opener)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let target_id = info["targetId"].as_str().unwrap_or_default();
        let url = info["url"].as_str().unwrap_or_default();
        self.cdp
            .notify("Target.closeTarget", json!({ "targetId": target_id }), None);
        if policy::popup(url) == Popup::LoadInTab {
            self.cdp.notify(
                "Page.navigate",
                json!({ "url": url }),
                Some(&opener_session),
            );
        }
    }

    /// Publishes live frames and paces Chromium: each frame is acked
    /// [`FRAME_INTERVAL`] after the previous one, so the browser encodes no
    /// more frames than viewers get.
    fn spawn_frames(running: &Arc<Self>) {
        let weak = Arc::downgrade(running);
        let cdp = running.cdp.clone();
        tokio::spawn(async move {
            // Session → when its previous frame was acked.
            let mut last_ack: HashMap<String, Instant> = HashMap::new();
            loop {
                cdp.frame_ready().await;
                if cdp.is_closed() {
                    break;
                }
                let Some(running) = weak.upgrade() else { break };
                for (session_id, mut params) in cdp.take_frames() {
                    let Some(info) = running.session_info(&session_id) else {
                        // The tab is gone; its screencast went with it.
                        continue;
                    };
                    if let (Some(frames), Value::String(data)) =
                        (running.frames.upgrade(), params["data"].take())
                    {
                        frames.publish(&info.conversation_id, data, &params["metadata"]);
                    }
                    let now = Instant::now();
                    let due = last_ack
                        .get(&session_id)
                        .map_or(now, |last| (*last + FRAME_INTERVAL).max(now));
                    last_ack.insert(session_id.clone(), due);
                    let ack = json!({ "sessionId": params["sessionId"].take() });
                    let cdp = cdp.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(due.into()).await;
                        cdp.notify("Page.screencastFrameAck", ack, Some(&session_id));
                    });
                }
                // Forget sessions whose tab closed.
                let sessions = running
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                last_ack.retain(|session_id, _| sessions.contains_key(session_id));
            }
        });
    }

    /// Opens the conversation's tab in a new background window.
    pub(crate) async fn open_tab(&self, conversation_id: &str) -> Result<(), BrowserError> {
        let created = self
            .cdp
            .call(
                "Target.createTarget",
                json!({ "url": "about:blank", "newWindow": true, "background": true }),
                None,
            )
            .await?;
        let target_id = created["targetId"]
            .as_str()
            .ok_or_else(|| BrowserError::failed("Chromium opened no tab"))?
            .to_owned();
        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let session_id = attached["sessionId"]
            .as_str()
            .ok_or_else(|| BrowserError::failed("cannot attach to the tab"))?
            .to_owned();
        let session = Some(session_id.as_str());
        self.cdp.call("Page.enable", json!({}), session).await?;
        self.cdp.call("Runtime.enable", json!({}), session).await?;
        let tree = self
            .cdp
            .call("Page.getFrameTree", json!({}), session)
            .await?;
        let main_frame_id = tree["frameTree"]["frame"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                session_id.clone(),
                SessionInfo {
                    conversation_id: conversation_id.to_owned(),
                    target_id: target_id.clone(),
                    main_frame_id,
                },
            );
        self.cdp
            .call(
                "Fetch.enable",
                json!({ "patterns": [{ "resourceType": "Document", "requestStage": "Request" }] }),
                session,
            )
            .await?;
        self.place_window(&target_id).await;
        self.tabs.lock().await.insert(
            conversation_id.to_owned(),
            Tab {
                target_id,
                session_id,
                refs: HashMap::new(),
                last_used: Instant::now(),
                screencasting: false,
            },
        );
        Ok(())
    }

    /// Lower right of the primary display, without activating anything.
    async fn place_window(&self, target_id: &str) {
        let Ok(window) = self
            .cdp
            .call(
                "Browser.getWindowForTarget",
                json!({ "targetId": target_id }),
                None,
            )
            .await
        else {
            return;
        };
        let mut bounds = json!({ "width": VIEWPORT_WIDTH, "height": VIEWPORT_HEIGHT });
        if let Some(display) = crate::computer::platform::displays().first() {
            bounds["left"] = json!(
                (display.x + display.width - f64::from(VIEWPORT_WIDTH) - 24.0).max(display.x)
                    as i64
            );
            bounds["top"] = json!(
                (display.y + display.height - f64::from(VIEWPORT_HEIGHT) - 48.0).max(display.y)
                    as i64
            );
        }
        let _ = self
            .cdp
            .call(
                "Browser.setWindowBounds",
                json!({ "windowId": window["windowId"], "bounds": bounds }),
                None,
            )
            .await;
    }

    /// A minimized window renders nothing; restore it (without focus).
    pub(crate) async fn unminimize(&self, target_id: &str) {
        let Ok(window) = self
            .cdp
            .call(
                "Browser.getWindowForTarget",
                json!({ "targetId": target_id }),
                None,
            )
            .await
        else {
            return;
        };
        if window["bounds"]["windowState"] == "minimized" {
            let _ = self
                .cdp
                .call(
                    "Browser.setWindowBounds",
                    json!({ "windowId": window["windowId"], "bounds": { "windowState": "normal" } }),
                    None,
                )
                .await;
        }
    }

    pub(crate) async fn close_tab(&self, conversation_id: &str) -> bool {
        let Some(tab) = self.tabs.lock().await.remove(conversation_id) else {
            return false;
        };
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&tab.session_id);
        let _ = self
            .cdp
            .call(
                "Target.closeTarget",
                json!({ "targetId": tab.target_id }),
                None,
            )
            .await;
        true
    }

    pub(crate) async fn tab(
        &self,
        conversation_id: &str,
    ) -> Result<(String, String), BrowserError> {
        self.tabs
            .lock()
            .await
            .get(conversation_id)
            .map(|tab| (tab.target_id.clone(), tab.session_id.clone()))
            .ok_or_else(|| {
                BrowserError::new(
                    "NO_TAB",
                    "This conversation has no browser tab; call browser_open first.",
                )
            })
    }

    /// Starts or stops the tab's screencast; a no-op when it already is
    /// in that state. Callers serialize per conversation.
    pub(crate) async fn set_screencast(&self, conversation_id: &str, on: bool) {
        let session_id = {
            let tabs = self.tabs.lock().await;
            match tabs.get(conversation_id) {
                Some(tab) if tab.screencasting != on => tab.session_id.clone(),
                _ => return,
            }
        };
        let result = if on {
            if let Ok((target_id, _)) = self.tab(conversation_id).await {
                self.unminimize(&target_id).await;
            }
            self.cdp
                .call(
                    "Page.startScreencast",
                    json!({ "format": "jpeg", "quality": SCREENCAST_QUALITY, "maxWidth": 1280, "maxHeight": 1280, "everyNthFrame": 1 }),
                    Some(&session_id),
                )
                .await
        } else {
            self.cdp
                .call("Page.stopScreencast", json!({}), Some(&session_id))
                .await
        };
        match result {
            Ok(_) => {
                if let Some(tab) = self.tabs.lock().await.get_mut(conversation_id) {
                    if tab.session_id == session_id {
                        tab.screencasting = on;
                    }
                }
            }
            Err(error) => tracing::debug!(%error, on, "agent browser screencast toggle failed"),
        }
    }

    /// Records an agent tool call on the conversation's tab.
    pub(crate) async fn touch_tab(&self, conversation_id: &str) {
        self.touch();
        if let Some(tab) = self.tabs.lock().await.get_mut(conversation_id) {
            tab.last_used = Instant::now();
        }
    }

    /// Conversations whose tab saw no tool call for `idle`.
    pub(crate) async fn idle_tabs(&self, idle: Duration) -> Vec<String> {
        self.tabs
            .lock()
            .await
            .iter()
            .filter(|(_, tab)| tab.last_used.elapsed() >= idle)
            .map(|(conversation_id, _)| conversation_id.clone())
            .collect()
    }

    // ---- Tools ----------------------------------------------------------

    pub(crate) async fn page(&self, target_id: &str) -> Result<Value, BrowserError> {
        let info = self
            .cdp
            .call(
                "Target.getTargetInfo",
                json!({ "targetId": target_id }),
                None,
            )
            .await?;
        Ok(json!({
            "url": info["targetInfo"]["url"],
            "title": info["targetInfo"]["title"],
        }))
    }

    pub(crate) async fn load(
        &self,
        conversation_id: &str,
        url: &str,
    ) -> Result<Value, BrowserError> {
        if !allowed_top_level(url) {
            return Err(BrowserError::new(
                "NAVIGATION_BLOCKED",
                format!("{url} is not a local page."),
            ));
        }
        let (target_id, session_id) = self.tab(conversation_id).await?;
        self.unminimize(&target_id).await;
        // Error pages (connection refused) still leave a page to inspect.
        let navigated = self
            .cdp
            .call("Page.navigate", json!({ "url": url }), Some(&session_id))
            .await?;
        if let Some(error) = navigated["errorText"]
            .as_str()
            .filter(|error| !error.is_empty())
        {
            tracing::debug!(error, "agent browser load failed");
        }
        self.settle(&session_id).await;
        self.page(&target_id).await
    }

    pub(crate) async fn navigate(
        &self,
        conversation_id: &str,
        args: &Value,
    ) -> Result<Value, BrowserError> {
        if let Some(url) = args["url"].as_str() {
            return self.load(conversation_id, url).await;
        }
        let (target_id, session_id) = self.tab(conversation_id).await?;
        let session = Some(session_id.as_str());
        match args["action"].as_str() {
            Some("reload") => {
                self.cdp.call("Page.reload", json!({}), session).await?;
            }
            Some(direction @ ("back" | "forward")) => {
                let history = self
                    .cdp
                    .call("Page.getNavigationHistory", json!({}), session)
                    .await?;
                let current = history["currentIndex"].as_i64().unwrap_or(0);
                let index = if direction == "back" {
                    current - 1
                } else {
                    current + 1
                };
                if let Some(entry) = history["entries"].as_array().and_then(|entries| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| entries.get(index))
                }) {
                    if allowed_top_level(entry["url"].as_str().unwrap_or_default()) {
                        self.cdp
                            .call(
                                "Page.navigateToHistoryEntry",
                                json!({ "entryId": entry["id"] }),
                                session,
                            )
                            .await?;
                    }
                }
            }
            other => {
                return Err(BrowserError::invalid(format!(
                    "unknown navigation {}",
                    other.unwrap_or_default()
                )))
            }
        }
        self.settle(&session_id).await;
        self.page(&target_id).await
    }

    pub(crate) async fn snapshot(
        &self,
        conversation_id: &str,
        screenshot: bool,
    ) -> Result<Value, BrowserError> {
        let (target_id, session_id) = self.tab(conversation_id).await?;
        let session = Some(session_id.as_str());
        let tree = self
            .cdp
            .call("Accessibility.getFullAXTree", json!({}), session)
            .await?;
        let formatted = ax::format(
            tree["nodes"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default(),
        );
        if let Some(tab) = self.tabs.lock().await.get_mut(conversation_id) {
            tab.refs = formatted.refs;
        }
        let mut result = self.page(&target_id).await?;
        result["tree"] = Value::from(formatted.tree);
        result["truncated"] = Value::from(formatted.truncated);
        if screenshot {
            result["screenshot"] = self.screenshot(&session_id).await?;
        }
        Ok(result)
    }

    async fn screenshot(&self, session_id: &str) -> Result<Value, BrowserError> {
        let session = Some(session_id);
        let metrics = self
            .cdp
            .call("Page.getLayoutMetrics", json!({}), session)
            .await?;
        let viewport = &metrics["cssVisualViewport"];
        let width = viewport["clientWidth"]
            .as_f64()
            .unwrap_or(f64::from(VIEWPORT_WIDTH))
            .max(1.0);
        let height = viewport["clientHeight"]
            .as_f64()
            .unwrap_or(f64::from(VIEWPORT_HEIGHT))
            .max(1.0);
        let scale = (SCREENSHOT_MAX_WIDTH / width).min(1.0);
        let shot = self
            .cdp
            .call(
                "Page.captureScreenshot",
                json!({
                    "format": "jpeg",
                    "quality": SCREENSHOT_QUALITY,
                    "clip": {
                        "x": viewport["pageX"].as_f64().unwrap_or(0.0),
                        "y": viewport["pageY"].as_f64().unwrap_or(0.0),
                        "width": width, "height": height, "scale": scale
                    },
                }),
                session,
            )
            .await?;
        let data = shot["data"]
            .as_str()
            .ok_or_else(|| BrowserError::failed("the page could not be captured"))?;
        Ok(json!({
            "mimeType": "image/jpeg",
            "data": data,
            "width": (width * scale).round() as u32,
            "height": (height * scale).round() as u32,
        }))
    }

    /// A JPEG of the tab now (the live frame's fallback).
    pub(crate) async fn frame(&self, conversation_id: &str) -> Result<Vec<u8>, BrowserError> {
        let (_, session_id) = self.tab(conversation_id).await?;
        let shot = self.screenshot(&session_id).await?;
        BASE64
            .decode(shot["data"].as_str().unwrap_or_default())
            .map_err(|error| BrowserError::failed(error.to_string()))
    }

    async fn node_for(
        &self,
        conversation_id: &str,
        reference: Option<&str>,
    ) -> Result<i64, BrowserError> {
        let tabs = self.tabs.lock().await;
        reference
            .and_then(|reference| tabs.get(conversation_id)?.refs.get(reference).copied())
            .ok_or_else(|| {
                BrowserError::new(
                    "REF_NOT_FOUND",
                    format!(
                        "{} is not in the latest snapshot; call browser_snapshot again.",
                        reference.unwrap_or("ref")
                    ),
                )
            })
    }

    async fn center(&self, session_id: &str, node: i64) -> Result<(f64, f64), BrowserError> {
        let session = Some(session_id);
        let _ = self
            .cdp
            .call(
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": node }),
                session,
            )
            .await;
        let model = self
            .cdp
            .call("DOM.getBoxModel", json!({ "backendNodeId": node }), session)
            .await?;
        let quad: Vec<f64> = model["model"]["content"]
            .as_array()
            .map(|points| points.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        if quad.len() < 6 {
            return Err(BrowserError::failed("the element has no box"));
        }
        Ok(((quad[0] + quad[4]) / 2.0, (quad[1] + quad[5]) / 2.0))
    }

    async fn call_on(
        &self,
        session_id: &str,
        node: i64,
        function: &str,
        arguments: Vec<Value>,
    ) -> Result<Value, BrowserError> {
        let session = Some(session_id);
        let resolved = self
            .cdp
            .call("DOM.resolveNode", json!({ "backendNodeId": node }), session)
            .await?;
        let result = self
            .cdp
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": resolved["object"]["objectId"],
                    "functionDeclaration": function,
                    "arguments": arguments.into_iter().map(|value| json!({ "value": value })).collect::<Vec<_>>(),
                    "returnByValue": true,
                }),
                session,
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err(BrowserError::failed("the page rejected the action"));
        }
        Ok(result["result"]["value"].clone())
    }

    async fn mouse(
        &self,
        session_id: &str,
        kind: &str,
        (x, y): (f64, f64),
        extra: Value,
    ) -> Result<(), BrowserError> {
        let mut event = json!({ "type": kind, "x": x, "y": y });
        if let (Value::Object(event), Value::Object(extra)) = (&mut event, extra) {
            event.extend(extra);
        }
        self.cdp
            .call("Input.dispatchMouseEvent", event, Some(session_id))
            .await
            .map(|_| ())
    }

    pub(crate) async fn act(
        &self,
        conversation_id: &str,
        args: &Value,
    ) -> Result<Value, BrowserError> {
        let (target_id, session_id) = self.tab(conversation_id).await?;
        self.unminimize(&target_id).await;
        let reference = args["ref"].as_str();
        match args["action"].as_str().unwrap_or_default() {
            action @ ("click" | "hover") => {
                let node = self.node_for(conversation_id, reference).await?;
                let point = self.center(&session_id, node).await?;
                self.mouse(&session_id, "mouseMoved", point, json!({}))
                    .await?;
                if action == "click" {
                    let press = json!({ "button": "left", "clickCount": 1 });
                    self.mouse(&session_id, "mousePressed", point, press.clone())
                        .await?;
                    self.mouse(&session_id, "mouseReleased", point, press)
                        .await?;
                }
            }
            "type" => {
                let node = self.node_for(conversation_id, reference).await?;
                let password = self
                    .call_on(
                        &session_id,
                        node,
                        "function () { return this instanceof HTMLInputElement && this.type === 'password'; }",
                        Vec::new(),
                    )
                    .await?;
                if password == Value::Bool(true) && args["confirmed"] != Value::Bool(true) {
                    return Err(BrowserError::new(
                        "SENSITIVE_ACTION",
                        "typing into a password field",
                    ));
                }
                self.call_on(
                    &session_id,
                    node,
                    "function () { this.focus(); if (typeof this.select === 'function') this.select(); }",
                    Vec::new(),
                )
                .await?;
                self.cdp
                    .call(
                        "Input.insertText",
                        json!({ "text": args["text"].as_str().unwrap_or_default() }),
                        Some(&session_id),
                    )
                    .await?;
            }
            "select" => {
                let node = self.node_for(conversation_id, reference).await?;
                let wanted = args["text"].as_str().unwrap_or_default();
                let matched = self
                    .call_on(
                        &session_id,
                        node,
                        "function (wanted) {
                          if (!(this instanceof HTMLSelectElement)) return false;
                          const option = [...this.options].find(o => o.value === wanted || o.label === wanted || o.text.trim() === wanted);
                          if (!option) return false;
                          this.value = option.value;
                          this.dispatchEvent(new Event('input', { bubbles: true }));
                          this.dispatchEvent(new Event('change', { bubbles: true }));
                          return true;
                        }",
                        vec![Value::from(wanted)],
                    )
                    .await?;
                if matched != Value::Bool(true) {
                    return Err(BrowserError::invalid(format!(
                        "no option \"{wanted}\" in {}",
                        reference.unwrap_or_default()
                    )));
                }
            }
            "press" => {
                let key = args["key"].as_str().unwrap_or_default();
                match named_key(key) {
                    Some((name, code, key_code, text)) => {
                        let mut down = json!({
                            "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
                            "key": name, "code": code,
                            "windowsVirtualKeyCode": key_code, "nativeVirtualKeyCode": key_code,
                        });
                        if let Some(text) = text {
                            down["text"] = Value::from(text);
                        }
                        self.cdp
                            .call("Input.dispatchKeyEvent", down, Some(&session_id))
                            .await?;
                        self.cdp
                            .call(
                                "Input.dispatchKeyEvent",
                                json!({ "type": "keyUp", "key": name, "code": code, "windowsVirtualKeyCode": key_code, "nativeVirtualKeyCode": key_code }),
                                Some(&session_id),
                            )
                            .await?;
                    }
                    None if key.chars().count() == 1 => {
                        self.cdp
                            .call(
                                "Input.insertText",
                                json!({ "text": key }),
                                Some(&session_id),
                            )
                            .await?;
                    }
                    None => return Err(BrowserError::invalid(format!("unsupported key {key}"))),
                }
            }
            "scroll" => {
                let center = (
                    f64::from(VIEWPORT_WIDTH) / 2.0,
                    f64::from(VIEWPORT_HEIGHT) / 2.0,
                );
                let delta = args["deltaY"].as_f64().unwrap_or(600.0);
                self.mouse(
                    &session_id,
                    "mouseWheel",
                    center,
                    json!({ "deltaX": 0, "deltaY": delta }),
                )
                .await?;
            }
            "wait" => {
                let ms = args["ms"].as_u64().unwrap_or(1000).min(MAX_WAIT_MS);
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            other => return Err(BrowserError::invalid(format!("unknown action {other}"))),
        }
        // Let a click-triggered navigation start before reporting the page.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.settle(&session_id).await;
        self.page(&target_id).await
    }

    /// Waits (bounded) until the document finished loading.
    async fn settle(&self, session_id: &str) {
        let deadline = Instant::now() + SETTLE_LIMIT;
        while Instant::now() < deadline {
            let state = self
                .cdp
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "document.readyState", "returnByValue": true }),
                    Some(session_id),
                )
                .await;
            match state {
                Ok(state) if state["result"]["value"] == "complete" => return,
                Err(_) if self.cdp.is_closed() => return,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
}

/// CDP key data for named keys: (key, code, keyCode, text).
fn named_key(name: &str) -> Option<(&'static str, &'static str, u32, Option<&'static str>)> {
    Some(match name {
        "Enter" => ("Enter", "Enter", 13, Some("\r")),
        "Tab" => ("Tab", "Tab", 9, None),
        "Escape" => ("Escape", "Escape", 27, None),
        "Backspace" => ("Backspace", "Backspace", 8, None),
        "Delete" => ("Delete", "Delete", 46, None),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, None),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, None),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, None),
        "Home" => ("Home", "Home", 36, None),
        "End" => ("End", "End", 35, None),
        "PageUp" => ("PageUp", "PageUp", 33, None),
        "PageDown" => ("PageDown", "PageDown", 34, None),
        "Space" => (" ", "Space", 32, Some(" ")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_cdp_key_data() {
        assert!(named_key("Enter").is_some());
        assert!(named_key("F13").is_none());
    }
}
