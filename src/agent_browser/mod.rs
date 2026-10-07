//! The agent browser (`browser_*` tools of `todex_desktop`), run by the
//! daemon on its own host: a headed Chrome for Testing it downloads and
//! pins ([`install`]), one process per browser profile ([`profiles`]),
//! one background window per conversation ([`session`]). Clients only
//! watch: live frames over `/v2/ws` (`agentBrowser.watch`) and the journal.
//!
//! Top-level navigation stays on loopback (the host's own `localhost`);
//! subresources may come from anywhere. Downloads, permission prompts,
//! dialogs and popups never reach anyone.

pub(crate) mod ax;
mod cdp;
pub(crate) mod install;
mod launch;
pub(crate) mod policy;
pub(crate) mod profiles;
mod session;

use std::{
    collections::HashMap,
    fmt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use self::{
    install::{InstallState, Installer},
    profiles::{ProfileRecord, Profiles, ProfilesState},
    session::Running,
};
use crate::error::AppError;

/// `todex-agentd browser install`: downloads the pinned Chromium now,
/// printing progress.
pub(crate) async fn install_cli(data_dir: &Path) -> anyhow::Result<()> {
    let installer = Installer::new(data_dir);
    if let Some(path) = installer.executable() {
        println!(
            "Chromium {} is installed: {}",
            install::CHROMIUM_VERSION,
            path.display()
        );
        return Ok(());
    }
    let watcher = installer.clone();
    let progress = tokio::spawn(async move {
        let mut last = -1i64;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Some(progress) = watcher.state().progress {
                let percent = (progress * 100.0) as i64;
                if percent != last {
                    println!(
                        "Downloading Chromium {}: {percent}%",
                        install::CHROMIUM_VERSION
                    );
                    last = percent;
                }
            }
        }
    });
    let result = installer.install().await;
    progress.abort();
    let path = result.map_err(|error| anyhow::anyhow!(error.to_string()))?;
    println!(
        "Chromium {} installed: {}",
        install::CHROMIUM_VERSION,
        path.display()
    );
    Ok(())
}

/// The daemon's own port: never opened in the agent browser.
static DAEMON_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

pub(crate) fn set_daemon_port(port: u16) {
    let _ = DAEMON_PORT.set(port);
}

pub(crate) fn daemon_port() -> Option<u16> {
    DAEMON_PORT.get().copied()
}

/// Agent tabs open at once on this host.
const MAX_TABS: usize = 4;
/// Browser profiles running at once.
const MAX_RUNNING: usize = 2;
/// A tab nobody watches and no tool used for this long is closed; the next
/// tool call reopens it at the same URL.
const TAB_IDLE: Duration = Duration::from_secs(600);
/// A profile without tabs and tool calls this long has its Chromium closed.
const IDLE_SHUTDOWN: Duration = TAB_IDLE;
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// A refused or failed browser call, reported as `CODE: message`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BrowserError {
    pub code: String,
    pub message: String,
}

impl BrowserError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_ARGUMENT", message)
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::new("EXECUTOR_FAILED", message)
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// `browser` in `GET /v2/agent-desktop`.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub host: String,
    pub chromium: InstallState,
}

/// One live frame of a conversation's tab.
#[derive(Debug)]
pub(crate) struct Frame {
    pub seq: u64,
    /// Base64 JPEG, as Chromium sent it.
    pub data: String,
    pub width: u32,
    pub height: u32,
}

struct Channel {
    sender: watch::Sender<Option<Arc<Frame>>>,
    watchers: usize,
    seq: u64,
}

/// Latest frame per conversation, and who is watching.
#[derive(Default)]
pub(crate) struct Frames {
    channels: Mutex<HashMap<String, Channel>>,
}

impl Frames {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Channel>> {
        self.channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(&self, conversation_id: &str, data: String, metadata: &Value) {
        let mut channels = self.lock();
        let Some(channel) = channels.get_mut(conversation_id) else {
            return;
        };
        channel.seq += 1;
        let frame = Frame {
            seq: channel.seq,
            data,
            width: metadata["deviceWidth"].as_f64().unwrap_or(0.0) as u32,
            height: metadata["deviceHeight"].as_f64().unwrap_or(0.0) as u32,
        };
        channel.sender.send_replace(Some(Arc::new(frame)));
    }

    fn tab_closed(&self, conversation_id: &str) {
        if let Some(channel) = self.lock().get(conversation_id) {
            channel.sender.send_replace(None);
        }
    }

    /// Returns the receiver and whether this is the first watcher.
    fn subscribe(&self, conversation_id: &str) -> (watch::Receiver<Option<Arc<Frame>>>, bool) {
        let mut channels = self.lock();
        let channel = channels
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Channel {
                sender: watch::channel(None).0,
                watchers: 0,
                seq: 0,
            });
        channel.watchers += 1;
        (channel.sender.subscribe(), channel.watchers == 1)
    }

    /// Returns whether that was the last watcher.
    fn unsubscribe(&self, conversation_id: &str) -> bool {
        let mut channels = self.lock();
        let Some(channel) = channels.get_mut(conversation_id) else {
            return false;
        };
        channel.watchers = channel.watchers.saturating_sub(1);
        if channel.watchers == 0 {
            channels.remove(conversation_id);
            return true;
        }
        false
    }

    fn watched(&self, conversation_id: &str) -> bool {
        self.lock()
            .get(conversation_id)
            .is_some_and(|channel| channel.watchers > 0)
    }

    fn latest(&self, conversation_id: &str) -> Option<Arc<Frame>> {
        self.lock()
            .get(conversation_id)
            .and_then(|channel| channel.sender.borrow().clone())
    }
}

/// Scripted tool results for tests of the MCP flow.
#[cfg(test)]
type TestResponder = Box<dyn Fn(&str, &Value) -> Result<Value, BrowserError> + Send + Sync>;

struct Inner {
    #[cfg(test)]
    responder: Mutex<Option<TestResponder>>,
    installer: Installer,
    profiles: Profiles,
    running: tokio::sync::Mutex<HashMap<String, Arc<Running>>>,
    /// Conversation → the profile its tab is in.
    tabs: Mutex<HashMap<String, String>>,
    /// Conversation → the URL of its tab, closed for being idle; the next
    /// tool call reopens it there.
    parked: Mutex<HashMap<String, String>>,
    /// Conversation → lock serializing screencast start/stop.
    screencast_locks: crate::agent_desktop::KeyedLocks,
    frames: Arc<Frames>,
}

#[derive(Clone)]
pub(crate) struct AgentBrowser {
    inner: Arc<Inner>,
}

/// A live view subscription; dropping it stops the screencast when nobody
/// else watches.
pub(crate) struct BrowserWatch {
    pub frames: watch::Receiver<Option<Arc<Frame>>>,
    browser: AgentBrowser,
    conversation_id: String,
}

impl Drop for BrowserWatch {
    fn drop(&mut self) {
        if self.browser.inner.frames.unsubscribe(&self.conversation_id) {
            let browser = self.browser.clone();
            let conversation_id = self.conversation_id.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { browser.sync_screencast(&conversation_id).await });
            }
        }
    }
}

impl AgentBrowser {
    pub(crate) fn load(data_dir: &Path) -> Result<Self, AppError> {
        let profiles =
            Profiles::load(data_dir).map_err(|error| AppError::InvalidRequest(error.message))?;
        let browser = Self {
            inner: Arc::new(Inner {
                #[cfg(test)]
                responder: Mutex::new(None),
                installer: Installer::new(data_dir),
                profiles,
                running: tokio::sync::Mutex::new(HashMap::new()),
                tabs: Mutex::new(HashMap::new()),
                parked: Mutex::new(HashMap::new()),
                screencast_locks: crate::agent_desktop::KeyedLocks::default(),
                frames: Arc::new(Frames::default()),
            }),
        };
        browser.spawn_sweeper();
        Ok(browser)
    }

    /// Parks tabs nobody watches or uses, then closes the Chromium of
    /// profiles without tabs for a while; stops once the browser service is
    /// gone.
    fn spawn_sweeper(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                let browser = AgentBrowser { inner };
                browser.park_idle_tabs().await;
                let mut running = browser.inner.running.lock().await;
                let mut idle = Vec::new();
                for (id, profile) in running.iter() {
                    if !profile.alive()
                        || (profile.tabs.lock().await.is_empty()
                            && profile.idle_for() >= IDLE_SHUTDOWN)
                    {
                        idle.push(id.clone());
                    }
                }
                for id in idle {
                    if let Some(profile) = running.remove(&id) {
                        profile.shut_down();
                    }
                }
            }
        });
    }

    /// Closes tabs without viewers and without a tool call for
    /// [`TAB_IDLE`], remembering their URL.
    async fn park_idle_tabs(&self) {
        let profiles: Vec<Arc<Running>> =
            self.inner.running.lock().await.values().cloned().collect();
        for running in profiles {
            for conversation_id in running.idle_tabs(TAB_IDLE).await {
                // Serialized with watch start/stop: a viewer arriving now
                // keeps the tab.
                let lock = self.inner.screencast_locks.get(&conversation_id);
                let _serial = lock.lock().await;
                if self.inner.frames.watched(&conversation_id) {
                    continue;
                }
                let Ok((target_id, _)) = running.tab(&conversation_id).await else {
                    continue;
                };
                let url = running
                    .page(&target_id)
                    .await
                    .ok()
                    .and_then(|page| page["url"].as_str().map(str::to_owned))
                    .filter(|url| policy::allowed_top_level(url) && url != "about:blank");
                if !running.close_tab(&conversation_id).await {
                    continue;
                }
                self.inner
                    .tabs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&conversation_id);
                if let Some(url) = url {
                    self.inner
                        .parked
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(conversation_id.clone(), url);
                }
                tracing::debug!(conversation_id, "agent browser closed an idle tab");
            }
        }
    }

    pub(crate) fn status(&self) -> BrowserStatus {
        let chromium = self.inner.installer.state();
        let reason = if install::CHROMIUM_VERSION.is_empty() {
            Some("no Chromium is pinned for this build".to_owned())
        } else if !chromium.installed {
            Some(if chromium.downloading {
                "Chromium is downloading.".to_owned()
            } else {
                "Chromium is not installed yet.".to_owned()
            })
        } else {
            display_problem()
        };
        BrowserStatus {
            available: reason.is_none(),
            reason,
            host: crate::computer::host_name(),
            chromium,
        }
    }

    /// Starts downloading Chromium in the background (no-op when present,
    /// and in unit tests, which never download it).
    pub(crate) fn start_install(&self) {
        if cfg!(test)
            || self.inner.installer.executable().is_some()
            || self.inner.installer.state().downloading
        {
            return;
        }
        let installer = self.inner.installer.clone();
        tokio::spawn(async move {
            if let Err(error) = installer.install().await {
                tracing::warn!(%error, "agent browser: Chromium install failed");
            }
        });
    }

    async fn running_for(&self, conversation_id: &str) -> Option<Arc<Running>> {
        let profile = self
            .inner
            .tabs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(conversation_id)
            .cloned()?;
        self.inner.running.lock().await.get(&profile).cloned()
    }

    /// The profile's Chromium, started if needed (closing an idle profile
    /// when [`MAX_RUNNING`] are up).
    async fn ensure_running(&self, profile: &str) -> Result<Arc<Running>, BrowserError> {
        let Some(executable) = self.inner.installer.executable() else {
            self.start_install();
            let state = self.inner.installer.state();
            return Err(BrowserError::new(
                "BROWSER_INSTALLING",
                match (state.error, state.progress) {
                    (Some(error), _) => format!("Chromium could not be installed ({error}); it is retrying, try again shortly."),
                    (None, Some(progress)) => format!("Chromium is downloading ({:.0}%); try again shortly.", progress * 100.0),
                    (None, None) => "Chromium is downloading; try again shortly.".to_owned(),
                },
            ));
        };
        if let Some(reason) = display_problem() {
            return Err(BrowserError::new("UNAVAILABLE", reason));
        }
        let mut running = self.inner.running.lock().await;
        if let Some(existing) = running.get(profile) {
            if existing.alive() {
                return Ok(existing.clone());
            }
            running.remove(profile);
        }
        if running.len() >= MAX_RUNNING {
            let mut spare = None;
            for (id, candidate) in running.iter() {
                if candidate.tabs.lock().await.is_empty() {
                    spare = Some(id.clone());
                    break;
                }
            }
            match spare.and_then(|id| running.remove(&id)) {
                Some(stopped) => stopped.shut_down(),
                None => {
                    return Err(BrowserError::new(
                        "TAB_LIMIT",
                        format!("{MAX_RUNNING} browser profiles are in use; close a tab first."),
                    ))
                }
            }
        }
        let process = launch::launch(&executable, &self.inner.profiles.dir_of(profile)).await?;
        let started = Running::start(process, Arc::downgrade(&self.inner.frames)).await?;
        running.insert(profile.to_owned(), started.clone());
        Ok(started)
    }

    /// Runs one `browser_*` tool for a conversation of `workspace`
    /// (`{ id, path }`). Results have the shape the agent tools expect.
    pub(crate) async fn invoke(
        &self,
        conversation_id: &str,
        workspace: &Value,
        tool: &str,
        args: &Value,
    ) -> Result<Value, BrowserError> {
        #[cfg(test)]
        if let Some(responder) = self
            .inner
            .responder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            return responder(tool, args);
        }
        if tool == "browser_open" {
            return self
                .open(
                    conversation_id,
                    workspace,
                    args["url"].as_str().unwrap_or_default(),
                )
                .await;
        }
        let parked = self
            .inner
            .parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(conversation_id);
        if tool == "browser_close" && parked.is_some() {
            return Ok(json!({ "closed": true }));
        }
        let mut running = self.running_for(conversation_id).await;
        if running.is_none() {
            if let Some(url) = parked {
                // Closed for being idle: reopen where it was (and keep the
                // URL for the next try if that fails).
                if let Err(error) = self.open(conversation_id, workspace, &url).await {
                    self.inner
                        .parked
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .entry(conversation_id.to_owned())
                        .or_insert(url);
                    return Err(error);
                }
                running = self.running_for(conversation_id).await;
            }
        }
        let running = running.ok_or_else(|| {
            BrowserError::new(
                "NO_TAB",
                "This conversation has no browser tab; call browser_open first.",
            )
        })?;
        running.touch_tab(conversation_id).await;
        match tool {
            "browser_navigate" => running.navigate(conversation_id, args).await,
            "browser_snapshot" => {
                running
                    .snapshot(
                        conversation_id,
                        args["screenshot"].as_bool().unwrap_or(false),
                    )
                    .await
            }
            "browser_act" => running.act(conversation_id, args).await,
            "browser_close" => {
                self.close_conversation(conversation_id).await;
                Ok(json!({ "closed": true }))
            }
            other => Err(BrowserError::invalid(format!("unknown tool {other}"))),
        }
    }

    async fn open(
        &self,
        conversation_id: &str,
        workspace: &Value,
        url: &str,
    ) -> Result<Value, BrowserError> {
        let key = workspace["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .or_else(|| workspace["path"].as_str())
            .unwrap_or_default()
            .to_owned();
        let path = workspace["path"].as_str().unwrap_or_default();
        let label = path
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or(path);
        let profile = self.inner.profiles.for_workspace(&key, label)?;
        let current = self
            .inner
            .tabs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(conversation_id)
            .cloned();
        // The workspace moved to another profile: reopen there.
        if current.as_deref().is_some_and(|current| current != profile) {
            self.close_conversation(conversation_id).await;
        }
        self.inner
            .parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(conversation_id);
        let running = self.ensure_running(&profile).await?;
        running.touch_tab(conversation_id).await;
        if running.tab(conversation_id).await.is_err() {
            let open = self
                .inner
                .tabs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len();
            if open >= MAX_TABS {
                return Err(BrowserError::new(
                    "TAB_LIMIT",
                    format!("At most {MAX_TABS} agent browser tabs can be open on this computer."),
                ));
            }
            running.open_tab(conversation_id).await?;
            self.inner
                .tabs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(conversation_id.to_owned(), profile);
            self.sync_screencast(conversation_id).await;
        }
        running.load(conversation_id, url).await
    }

    /// The conversation's tab goes away (close, revoke, deletion).
    pub(crate) async fn close_conversation(&self, conversation_id: &str) {
        self.inner
            .parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(conversation_id);
        let running = self.running_for(conversation_id).await;
        self.inner
            .tabs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(conversation_id);
        if let Some(running) = running {
            running.close_tab(conversation_id).await;
        }
        self.inner.frames.tab_closed(conversation_id);
    }

    /// Live frames of the conversation's tab, from now on. The
    /// subscription is registered before anything is awaited, so a caller
    /// aborted mid-way still releases it.
    pub(crate) async fn watch(&self, conversation_id: &str) -> BrowserWatch {
        let (frames, first) = self.inner.frames.subscribe(conversation_id);
        let watch = BrowserWatch {
            frames,
            browser: self.clone(),
            conversation_id: conversation_id.to_owned(),
        };
        if first {
            self.sync_screencast(conversation_id).await;
        }
        watch
    }

    /// Makes the tab's screencast match whether anyone watches. Start and
    /// stop for one conversation run one at a time and each re-reads the
    /// watcher count, so a stop racing a new viewer cannot win.
    async fn sync_screencast(&self, conversation_id: &str) {
        let lock = self.inner.screencast_locks.get(conversation_id);
        let _serial = lock.lock().await;
        if let Some(running) = self.running_for(conversation_id).await {
            running
                .set_screencast(conversation_id, self.inner.frames.watched(conversation_id))
                .await;
        }
    }

    /// A JPEG of the tab now: the latest live frame, else a screenshot.
    pub(crate) async fn frame(&self, conversation_id: &str) -> Result<Vec<u8>, BrowserError> {
        if let Some(frame) = self.inner.frames.latest(conversation_id) {
            return BASE64
                .decode(&frame.data)
                .map_err(|error| BrowserError::failed(error.to_string()));
        }
        let running = self
            .running_for(conversation_id)
            .await
            .ok_or_else(|| BrowserError::new("NO_TAB", "this conversation has no browser tab"))?;
        running.frame(conversation_id).await
    }

    // ---- Profiles ---------------------------------------------------------

    pub(crate) fn profiles(&self) -> ProfilesState {
        self.inner.profiles.state()
    }

    pub(crate) fn create_profile(&self, name: &str) -> Result<ProfileRecord, BrowserError> {
        self.inner.profiles.create(name)
    }

    pub(crate) fn rename_profile(&self, id: &str, name: &str) -> Result<(), BrowserError> {
        self.inner.profiles.rename(id, name)
    }

    /// Points a workspace at a profile; its open tabs close (they reopen in
    /// the new profile on the next `browser_open`).
    pub(crate) async fn assign_profile(
        &self,
        workspace: &str,
        id: &str,
    ) -> Result<(), BrowserError> {
        let previous = self
            .inner
            .profiles
            .state()
            .workspaces
            .get(workspace)
            .cloned();
        self.inner.profiles.assign(workspace, id)?;
        if let Some(previous) = previous.filter(|previous| previous != id) {
            self.close_profile_tabs(&previous).await;
        }
        Ok(())
    }

    /// Deletes the profile and its data (cookies, storage, cache).
    pub(crate) async fn delete_profile(&self, id: &str) -> Result<(), BrowserError> {
        self.inner.profiles.remove(id)?;
        self.close_profile_tabs(id).await;
        if let Some(running) = self.inner.running.lock().await.remove(id) {
            running.shut_down();
        }
        // The process needs a moment to release its files.
        let dir = self.inner.profiles.dir_of(id);
        for attempt in 0..10 {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) if attempt == 9 => {
                    return Err(BrowserError::failed(format!(
                        "cannot delete the profile data at {}: {error}",
                        dir.display()
                    )))
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
            }
        }
        Ok(())
    }

    async fn close_profile_tabs(&self, profile: &str) {
        let conversations: Vec<String> = self
            .inner
            .tabs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, owner)| owner.as_str() == profile)
            .map(|(conversation, _)| conversation.clone())
            .collect();
        for conversation in conversations {
            self.close_conversation(&conversation).await;
        }
    }

    #[cfg(test)]
    pub(crate) fn respond_for_tests(
        &self,
        responder: impl Fn(&str, &Value) -> Result<Value, BrowserError> + Send + Sync + 'static,
    ) {
        *self
            .inner
            .responder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(responder));
    }
}

/// Why a headed browser cannot show here, if it cannot.
fn display_problem() -> Option<String> {
    // A Windows service session has no desktop to show windows on.
    #[cfg(target_os = "windows")]
    if let Some(reason) = crate::computer::platform::unsupported_reason() {
        return Some(reason);
    }
    if cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
        && !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|dir| dir.join("Xvfb").is_file())
    {
        return Some(
            "this computer has no graphical session; install Xvfb so the agent browser can run"
                .to_owned(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_reach_watchers_and_end_with_the_last_one() {
        let frames = Frames::default();
        // Nobody watching: frames are not kept.
        frames.publish(
            "c",
            "AAAA".into(),
            &json!({ "deviceWidth": 10, "deviceHeight": 5 }),
        );
        assert!(frames.latest("c").is_none());
        let (receiver, first) = frames.subscribe("c");
        assert!(first);
        let (_second, not_first) = frames.subscribe("c");
        assert!(!not_first);
        frames.publish(
            "c",
            "AAAA".into(),
            &json!({ "deviceWidth": 10, "deviceHeight": 5 }),
        );
        frames.publish(
            "c",
            "BBBB".into(),
            &json!({ "deviceWidth": 10, "deviceHeight": 5 }),
        );
        let latest = receiver.borrow().clone().unwrap();
        assert_eq!(
            (latest.seq, latest.data.as_str(), latest.width),
            (2, "BBBB", 10)
        );
        frames.tab_closed("c");
        assert!(receiver.borrow().is_none());
        assert!(!frames.unsubscribe("c"));
        assert!(frames.unsubscribe("c"));
        assert!(!frames.watched("c"));
    }
}
