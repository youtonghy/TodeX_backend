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
    BrowserError, CloseReason, Shared,
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
/// A popup whose address is not known when it opens is let run with its
/// navigations intercepted; one that loads nothing within this long is
/// closed.
const POPUP_GRACE: Duration = Duration::from_secs(5);

/// One CDP command: method, params, session.
type Command = (&'static str, Value, Option<String>);

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

/// A popup of an agent tab, running only until its first navigation.
#[derive(Clone)]
struct PendingPopup {
    target_id: String,
    opener_session: String,
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
    /// Popup session → its pending popup.
    popups: Mutex<HashMap<String, PendingPopup>>,
    pub last_used: Mutex<Instant>,
    /// Slots, frames and tab-closure events of the browser service.
    shared: Weak<Shared>,
    /// The browser profile this Chromium runs.
    profile: String,
}

/// Whether a `Page.javascriptDialogOpening` is accepted: leaving the page
/// (`beforeunload`) goes ahead, so a tab can navigate and close; alerts,
/// confirms and prompts are dismissed.
fn dialog_accept(params: &Value) -> bool {
    params["type"].as_str() == Some("beforeunload")
}

/// A target created for a tab that is not in [`Running::tabs`] yet. Dropped
/// before [`Self::commit`] (an error, or the opening call cancelled), it
/// closes the target again so no window is left behind.
struct PendingTarget<'a> {
    running: &'a Running,
    target_id: String,
    session_id: Option<String>,
    armed: bool,
}

impl PendingTarget<'_> {
    fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingTarget<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(session_id) = &self.session_id {
            self.running
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(session_id);
        }
        self.running.cdp.notify(
            "Target.closeTarget",
            json!({ "targetId": self.target_id }),
            None,
        );
    }
}

impl Running {
    pub(crate) async fn start(
        process: Process,
        shared: Weak<Shared>,
        profile: String,
    ) -> Result<Arc<Self>, BrowserError> {
        let cdp = process.cdp.clone();
        Self::connect(cdp, Some(process), shared, profile).await
    }

    /// A browser session over a scripted connection (no process).
    #[cfg(test)]
    pub(crate) async fn start_scripted(
        cdp: Cdp,
        shared: Weak<Shared>,
        profile: String,
    ) -> Result<Arc<Self>, BrowserError> {
        Self::connect(cdp, None, shared, profile).await
    }

    async fn connect(
        cdp: Cdp,
        process: Option<Process>,
        shared: Weak<Shared>,
        profile: String,
    ) -> Result<Arc<Self>, BrowserError> {
        let running = Arc::new(Self {
            cdp: cdp.clone(),
            process: Mutex::new(process),
            tabs: tokio::sync::Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            popups: Mutex::new(HashMap::new()),
            last_used: Mutex::new(Instant::now()),
            shared,
            profile,
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
        // Every new page is attached and paused before it loads anything,
        // so a popup is judged (and closed) before it can make a request;
        // other pages are resumed right away.
        cdp.call(
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                "filter": [{ "type": "page" }]
            }),
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
                    ("Fetch.requestPaused", _)
                        if running
                            .fold_pending_popup(&event.params, event.session_id.as_deref()) => {}
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
                            json!({ "accept": dialog_accept(&event.params) }),
                            event.session_id.as_deref(),
                        );
                    }
                    ("Target.attachedToTarget", _) => running.fold_popup(&event.params),
                    ("Target.detachedFromTarget", _) => {
                        // The window was closed (by the person at the host,
                        // or a crash): the conversation has no tab now.
                        if let Some(session_id) = event.params["sessionId"].as_str() {
                            running
                                .popups
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(session_id);
                            let removed = running
                                .sessions
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(session_id);
                            if let Some(info) = removed {
                                // A late detach of an earlier tab must not
                                // take the conversation's current one.
                                let mut tabs = running.tabs.lock().await;
                                if tabs
                                    .get(&info.conversation_id)
                                    .is_some_and(|tab| tab.session_id == session_id)
                                {
                                    tabs.remove(&info.conversation_id);
                                    drop(tabs);
                                    running.gone(
                                        &info.conversation_id,
                                        &info.target_id,
                                        CloseReason::User,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // The browser connection ended: whatever tabs were left went
            // with it.
            if let Some(running) = weak.upgrade() {
                if running.cdp.is_closed() {
                    running.release_all(CloseReason::Crash).await;
                }
            }
        });
    }

    /// The conversation's tab is gone: its slot, live view and clients are
    /// told (see [`Shared::tab_gone`]).
    pub(crate) fn gone(&self, conversation_id: &str, target_id: &str, reason: CloseReason) {
        if let Some(shared) = self.shared.upgrade() {
            shared.tab_gone(conversation_id, &self.profile, target_id, reason);
        }
    }

    /// Forgets every tab of this Chromium (it exited, or is being replaced)
    /// and reports each as closed. Idempotent.
    pub(crate) async fn release_all(&self, reason: CloseReason) {
        let tabs: Vec<(String, Tab)> = self.tabs.lock().await.drain().collect();
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (_, tab) in &tabs {
                sessions.remove(&tab.session_id);
            }
        }
        self.popups
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        for (conversation_id, tab) in tabs {
            self.gone(&conversation_id, &tab.target_id, reason);
        }
    }

    /// A page the browser-wide auto-attach paused (`Target.attachedToTarget`
    /// on the browser session). Popups of an agent tab stay in that tab:
    /// local ones load there, the rest are refused, and the popup closes
    /// without having loaded anything. Any other page is resumed.
    fn fold_popup(self: &Arc<Self>, params: &Value) {
        let opener = params["targetInfo"]["openerId"]
            .as_str()
            .unwrap_or_default();
        let opener_session = (!opener.is_empty())
            .then(|| {
                self.sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .find(|(_, session)| session.target_id == opener)
                    .map(|(id, _)| id.clone())
            })
            .flatten();
        let (commands, pending) = attached_target(params, opener_session.as_deref());
        if let Some(pending) = pending {
            let session_id = params["sessionId"].as_str().unwrap_or_default().to_owned();
            self.popups
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session_id.clone(), pending);
            // Loads nothing: it goes anyway.
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(POPUP_GRACE).await;
                let Some(running) = weak.upgrade() else {
                    return;
                };
                let stale = running
                    .popups
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&session_id);
                if let Some(popup) = stale {
                    running.cdp.notify(
                        "Target.closeTarget",
                        json!({ "targetId": popup.target_id }),
                        None,
                    );
                }
            });
        }
        self.send(commands);
    }

    /// The first navigation of a pending popup: refused there, and folded
    /// into the opener's tab. False when `session_id` is no pending popup.
    fn fold_pending_popup(&self, params: &Value, session_id: Option<&str>) -> bool {
        let Some(popup) = session_id.and_then(|session_id| {
            self.popups
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(session_id)
        }) else {
            return false;
        };
        let mut commands = vec![(
            "Fetch.failRequest",
            json!({ "requestId": params["requestId"], "errorReason": "Aborted" }),
            session_id.map(str::to_owned),
        )];
        commands.extend(fold_commands(
            &popup.target_id,
            params["request"]["url"].as_str().unwrap_or_default(),
            &popup.opener_session,
        ));
        self.send(commands);
        true
    }

    fn send(&self, commands: Vec<Command>) {
        for (method, body, session) in commands {
            self.cdp.notify(method, body, session.as_deref());
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
                    if let (Some(shared), Value::String(data)) =
                        (running.shared.upgrade(), params["data"].take())
                    {
                        shared
                            .frames
                            .publish(&info.conversation_id, data, &params["metadata"]);
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

    /// Opens the conversation's tab in a new background window. A call that
    /// fails or is dropped part-way closes the window again.
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
        let mut pending = PendingTarget {
            running: self,
            target_id: target_id.clone(),
            session_id: None,
            armed: true,
        };
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
        pending.session_id = Some(session_id.clone());
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
        let mut tabs = self.tabs.lock().await;
        // The window may have been closed meanwhile: its detach removed the
        // session.
        if !self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&session_id)
        {
            return Err(BrowserError::failed("the tab was closed while opening"));
        }
        tabs.insert(
            conversation_id.to_owned(),
            Tab {
                target_id,
                session_id,
                refs: HashMap::new(),
                last_used: Instant::now(),
                screencasting: false,
            },
        );
        pending.commit();
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

    /// Forgets the conversation's tab and asks Chromium to close its window
    /// (without waiting, so a cancelled caller cannot leave it half done).
    /// Returns the tab's target id; `None` when it had no tab.
    pub(crate) async fn close_tab(&self, conversation_id: &str) -> Option<String> {
        let tab = self.tabs.lock().await.remove(conversation_id)?;
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&tab.session_id);
        self.cdp.notify(
            "Target.closeTarget",
            json!({ "targetId": tab.target_id }),
            None,
        );
        Some(tab.target_id)
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

    /// The conversation has a tab that saw no tool call for `idle`.
    pub(crate) async fn is_idle(&self, conversation_id: &str, idle: Duration) -> bool {
        self.tabs
            .lock()
            .await
            .get(conversation_id)
            .is_some_and(|tab| tab.last_used.elapsed() >= idle)
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

/// The CDP commands for a target auto-attach reported (`opener_session`
/// is the agent tab that opened it, if one did), and the pending popup to
/// track when its address is not known yet.
fn attached_target(
    params: &Value,
    opener_session: Option<&str>,
) -> (Vec<Command>, Option<PendingPopup>) {
    let info = &params["targetInfo"];
    let session_id = params["sessionId"].as_str().unwrap_or_default().to_owned();
    let waiting = params["waitingForDebugger"] == true && !session_id.is_empty();
    let resume = || {
        (
            "Runtime.runIfWaitingForDebugger",
            json!({}),
            Some(session_id.clone()),
        )
    };
    let Some(opener_session) = opener_session.filter(|_| info["type"] == "page") else {
        // Not an agent tab's popup (the agent's own tabs, pages the person
        // opened): let it run.
        return (waiting.then(resume).into_iter().collect(), None);
    };
    let target_id = info["targetId"].as_str().unwrap_or_default();
    let url = info["url"].as_str().unwrap_or_default();
    if !waiting || !(url.is_empty() || url == "about:blank") {
        return (fold_commands(target_id, url, opener_session), None);
    }
    // `window.open(url)` reports no address yet: let it run with its
    // navigations intercepted, and fold the first one.
    (
        vec![
            (
                "Fetch.enable",
                json!({ "patterns": [{ "resourceType": "Document", "requestStage": "Request" }] }),
                Some(session_id.clone()),
            ),
            resume(),
        ],
        Some(PendingPopup {
            target_id: target_id.to_owned(),
            opener_session: opener_session.to_owned(),
        }),
    )
}

/// Closes popup `target_id` and loads `url` in the opener's tab when it is
/// a local page.
fn fold_commands(target_id: &str, url: &str, opener_session: &str) -> Vec<Command> {
    let mut commands = vec![("Target.closeTarget", json!({ "targetId": target_id }), None)];
    if policy::popup(url) == Popup::LoadInTab {
        commands.push((
            "Page.navigate",
            json!({ "url": url }),
            Some(opener_session.to_owned()),
        ));
    }
    commands
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
    fn paused_targets_are_judged_before_they_run() {
        let attached = |opener: &str, url: &str, waiting: bool| {
            json!({
                "sessionId": "S-new",
                "waitingForDebugger": waiting,
                "targetInfo": { "targetId": "T-new", "type": "page", "url": url, "openerId": opener }
            })
        };
        let close = ("Target.closeTarget", json!({ "targetId": "T-new" }), None);
        let resume = (
            "Runtime.runIfWaitingForDebugger",
            json!({}),
            Some("S-new".to_owned()),
        );
        // A popup with a local address: closed unrun, loaded in the tab.
        let (commands, pending) = attached_target(
            &attached("T-agent", "http://localhost:5173/x", true),
            Some("S-agent"),
        );
        assert!(pending.is_none());
        assert_eq!(
            commands,
            vec![
                close.clone(),
                (
                    "Page.navigate",
                    json!({ "url": "http://localhost:5173/x" }),
                    Some("S-agent".to_owned())
                ),
            ]
        );
        // A remote address: closed, never resumed or loaded.
        let (commands, pending) = attached_target(
            &attached("T-agent", "https://example.com/", true),
            Some("S-agent"),
        );
        assert_eq!((commands, pending.is_none()), (vec![close.clone()], true));
        // No address yet (`window.open(url)`): runs with its navigations
        // intercepted, tracked as pending.
        for url in ["", "about:blank"] {
            let (commands, pending) =
                attached_target(&attached("T-agent", url, true), Some("S-agent"));
            assert_eq!(commands[0].0, "Fetch.enable");
            assert_eq!(commands[0].2.as_deref(), Some("S-new"));
            assert_eq!(commands[1], resume);
            let pending = pending.unwrap();
            assert_eq!(
                (pending.target_id.as_str(), pending.opener_session.as_str()),
                ("T-new", "S-agent")
            );
        }
        // Its first navigation is then folded like a known address.
        assert_eq!(
            fold_commands("T-new", "https://example.com/", "S-agent"),
            vec![close.clone()]
        );
        assert_eq!(
            fold_commands("T-new", "http://127.0.0.1:3000/", "S-agent").len(),
            2
        );
        // Not an agent tab's popup: resumed, nothing else.
        for opener in ["", "T-person"] {
            let (commands, pending) =
                attached_target(&attached(opener, "https://example.com/", true), None);
            assert_eq!((commands, pending.is_none()), (vec![resume.clone()], true));
        }
        // Already running (an explicit attach): left alone.
        let (commands, pending) = attached_target(&attached("", "about:blank", false), None);
        assert!(commands.is_empty() && pending.is_none());
    }

    #[test]
    fn only_leaving_the_page_is_accepted() {
        for (kind, accepted) in [
            ("beforeunload", true),
            ("alert", false),
            ("confirm", false),
            ("prompt", false),
            ("", false),
        ] {
            assert_eq!(dialog_accept(&json!({ "type": kind })), accepted, "{kind}");
        }
        assert!(!dialog_accept(&json!({})));
    }

    #[tokio::test]
    async fn dialogs_are_answered_by_kind() {
        let root = std::env::temp_dir().join(format!("todex-browser-{}", uuid::Uuid::new_v4()));
        let browser = crate::agent_browser::AgentBrowser::load(&root).unwrap();
        let chromium = browser.script_chromium();
        browser
            .invoke(
                "c",
                &json!({ "id": "w", "path": "/w" }),
                "browser_open",
                &json!({ "url": "http://localhost:5173/" }),
                true,
            )
            .await
            .unwrap();
        for kind in ["alert", "beforeunload"] {
            chromium.emit(
                0,
                &json!({ "method": "Page.javascriptDialogOpening", "sessionId": "S-T1", "params": { "type": kind } }),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            chromium.calls("Page.handleJavaScriptDialog"),
            vec![json!({ "accept": false }), json!({ "accept": true })]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_late_detach_of_an_earlier_session_keeps_the_live_tab() {
        let root = std::env::temp_dir().join(format!("todex-browser-{}", uuid::Uuid::new_v4()));
        let browser = crate::agent_browser::AgentBrowser::load(&root).unwrap();
        let chromium = browser.script_chromium();
        let mut closures = browser.tab_closures();
        browser
            .invoke(
                "c",
                &json!({ "id": "w", "path": "/w" }),
                "browser_open",
                &json!({ "url": "http://localhost:5173/" }),
                true,
            )
            .await
            .unwrap();
        let running = browser.running_for("c").await.unwrap();
        // An earlier session of the conversation still on record.
        running.sessions.lock().unwrap().insert(
            "S-old".into(),
            SessionInfo {
                conversation_id: "c".into(),
                target_id: "T0".into(),
                main_frame_id: "F".into(),
            },
        );
        let detached = |session: &str| json!({ "method": "Target.detachedFromTarget", "params": { "sessionId": session } });
        chromium.emit(0, &detached("S-old"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(running.tab("c").await.unwrap().1, "S-T1");
        assert!(browser.inner.shared.slot_of("c").is_some());
        assert!(closures.try_recv().is_err());
        assert!(running.sessions.lock().unwrap().get("S-old").is_none());
        // The live tab's own window closing does end it.
        chromium.emit(0, &detached("S-T1"));
        let closed = tokio::time::timeout(Duration::from_secs(2), closures.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (closed.conversation_id.as_str(), closed.reason),
            ("c", CloseReason::User)
        );
        assert!(running.tab("c").await.is_err());
        assert!(browser.inner.shared.slot_of("c").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn named_keys_map_to_cdp_key_data() {
        assert!(named_key("Enter").is_some());
        assert!(named_key("F13").is_none());
    }
}
