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
#[cfg(test)]
mod scripted;
mod session;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use self::{
    install::{InstallState, Installer},
    profiles::{ProfileRecord, Profiles, ProfilesState},
    session::Running,
};
use crate::{agent_desktop::KeyedLocks, error::AppError, secure_fs};

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

/// What a live view shows now.
#[derive(Clone, Debug)]
pub(crate) enum View {
    /// The tab (if any) has produced no frame yet: nothing to tell.
    Pending,
    Frame(Arc<Frame>),
    /// The conversation has no tab (closed, crashed, never opened).
    Closed,
}

struct Channel {
    sender: watch::Sender<View>,
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
        channel.sender.send_replace(View::Frame(Arc::new(frame)));
    }

    fn tab_closed(&self, conversation_id: &str) {
        if let Some(channel) = self.lock().get(conversation_id) {
            channel.sender.send_replace(View::Closed);
        }
    }

    /// Returns the receiver and whether this is the first watcher. A new
    /// channel starts [`View::Pending`]: whether the conversation has a tab
    /// is for the caller to settle.
    fn subscribe(&self, conversation_id: &str) -> (watch::Receiver<View>, bool) {
        let mut channels = self.lock();
        let channel = channels
            .entry(conversation_id.to_owned())
            .or_insert_with(|| Channel {
                sender: watch::channel(View::Pending).0,
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
            .and_then(|channel| match &*channel.sender.borrow() {
                View::Frame(frame) => Some(frame.clone()),
                View::Pending | View::Closed => None,
            })
    }
}

/// Why a conversation's tab went away; the `reason` of the journaled
/// `desktop.browser.tab` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// Nobody watched and no tool was called for a while.
    Idle,
    /// Chromium exited.
    Crash,
    /// `browser_close`, or the window closed at the host.
    User,
    /// The conversation's browser access was revoked.
    Revoked,
    /// The daemon stopped with the tab open.
    Restart,
}

impl CloseReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Crash => "crash",
            Self::User => "user",
            Self::Revoked => "revoked",
            Self::Restart => "restart",
        }
    }
}

/// A conversation's tab went away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabClosed {
    pub conversation_id: String,
    pub reason: CloseReason,
}

/// State the service and every running Chromium share: live frames, the tab
/// slots, and who is told when a tab goes away.
pub(crate) struct Shared {
    frames: Frames,
    /// Conversation → the profile its tab (slot) is in. A slot is taken
    /// before the tab opens, so concurrent opens cannot exceed [`MAX_TABS`].
    tabs: Mutex<HashMap<String, String>>,
    closures: broadcast::Sender<TabClosed>,
    /// `open-tabs.json`: conversations with a slot, so a restarted daemon
    /// can tell clients their tab is gone.
    state_path: PathBuf,
    /// Conversations the previous daemon left with an open tab, until
    /// their closure has been journaled.
    restarted: Mutex<BTreeSet<String>>,
}

impl Shared {
    fn load(data_dir: &Path) -> Self {
        let state_path = data_dir.join("agent-browser").join("open-tabs.json");
        let restarted = match std::fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice::<BTreeSet<String>>(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %state_path.display(), "agent browser: open-tabs.json is not valid; ignoring it");
                BTreeSet::new()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => {
                tracing::warn!(%error, path = %state_path.display(), "agent browser: cannot read open-tabs.json");
                BTreeSet::new()
            }
        };
        Self {
            frames: Frames::default(),
            tabs: Mutex::new(HashMap::new()),
            closures: broadcast::channel(64).0,
            state_path,
            restarted: Mutex::new(restarted),
        }
    }

    fn lock_tabs(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.tabs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes the conversations with a slot (and those still to be
    /// reported as restarted); `tabs` is held by the caller so writes keep
    /// the order of the changes.
    fn persist(&self, tabs: &HashMap<String, String>) {
        let mut ids: BTreeSet<&str> = tabs.keys().map(String::as_str).collect();
        let restarted = self
            .restarted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ids.extend(restarted.iter().map(String::as_str));
        let result = serde_json::to_vec(&ids)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(parent) = self.state_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                secure_fs::write_owner_only_atomic(&self.state_path, &bytes)
            });
        if let Err(error) = result {
            tracing::warn!(%error, path = %self.state_path.display(), "agent browser: cannot record the open tabs");
        }
    }

    fn slot_of(&self, conversation_id: &str) -> Option<String> {
        self.lock_tabs().get(conversation_id).cloned()
    }

    fn slots_of(&self, profile: &str) -> Vec<String> {
        self.lock_tabs()
            .iter()
            .filter(|(_, owner)| owner.as_str() == profile)
            .map(|(conversation, _)| conversation.clone())
            .collect()
    }

    /// Takes the conversation's slot in `profile` (it keeps one it holds).
    /// `profile_exists` is asked under the slot lock, so a profile deleted
    /// meanwhile either refuses here or finds this slot when its tabs are
    /// closed.
    fn reserve(
        &self,
        conversation_id: &str,
        profile: &str,
        profile_exists: impl FnOnce() -> bool,
    ) -> Result<(), BrowserError> {
        let mut tabs = self.lock_tabs();
        if !profile_exists() {
            return Err(BrowserError::invalid(format!(
                "no browser profile {profile}"
            )));
        }
        if !tabs.contains_key(conversation_id) && tabs.len() >= MAX_TABS {
            return Err(BrowserError::new(
                "TAB_LIMIT",
                format!("At most {MAX_TABS} agent browser tabs can be open on this computer."),
            ));
        }
        tabs.insert(conversation_id.to_owned(), profile.to_owned());
        self.persist(&tabs);
        Ok(())
    }

    /// Gives the slot back unless the conversation has moved to another
    /// profile. Returns whether it was released.
    fn release(&self, conversation_id: &str, profile: &str) -> bool {
        let mut tabs = self.lock_tabs();
        if tabs
            .get(conversation_id)
            .is_none_or(|owner| owner != profile)
        {
            return false;
        }
        tabs.remove(conversation_id);
        self.persist(&tabs);
        true
    }

    /// The conversation's tab in `profile` is gone: its slot is released,
    /// live views end and [`Self::closures`] subscribers are told. Nothing
    /// happens when the conversation holds no slot in `profile` (it was
    /// already reported, or has moved on to another profile).
    fn tab_gone(&self, conversation_id: &str, profile: &str, target_id: &str, reason: CloseReason) {
        if !self.release(conversation_id, profile) {
            return;
        }
        self.frames.tab_closed(conversation_id);
        tracing::debug!(
            conversation_id,
            profile,
            target_id,
            reason = reason.as_str(),
            "agent browser tab closed"
        );
        // An error only means nobody is subscribed (no journal task).
        let _ = self.closures.send(TabClosed {
            conversation_id: conversation_id.to_owned(),
            reason,
        });
    }
}

/// A tab slot taken for an open in progress; released again (and live views
/// told) unless [`Self::commit`]ted, so an open that fails or is cancelled
/// part-way leaks nothing.
struct SlotReservation {
    shared: Arc<Shared>,
    conversation_id: String,
    profile: String,
    committed: bool,
}

impl SlotReservation {
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        if !self.committed && self.shared.release(&self.conversation_id, &self.profile) {
            self.shared.frames.tab_closed(&self.conversation_id);
        }
    }
}

/// A profile's place in the running count while its Chromium launches.
struct LaunchSlot<'a> {
    launching: &'a Mutex<HashSet<String>>,
    profile: String,
}

impl Drop for LaunchSlot<'_> {
    fn drop(&mut self) {
        self.launching
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.profile);
    }
}

/// Scripted tool results for tests of the MCP flow.
#[cfg(test)]
type TestResponder = Box<dyn Fn(&str, &Value) -> Result<Value, BrowserError> + Send + Sync>;

/// Starts a scripted Chromium for a profile instead of launching one.
#[cfg(test)]
type TestLauncher = Box<
    dyn Fn(
            &str,
            std::sync::Weak<Shared>,
        ) -> futures_util::future::BoxFuture<'static, Result<Arc<Running>, BrowserError>>
        + Send
        + Sync,
>;

struct Inner {
    #[cfg(test)]
    responder: Mutex<Option<TestResponder>>,
    #[cfg(test)]
    launcher: Mutex<Option<TestLauncher>>,
    installer: Installer,
    profiles: Profiles,
    /// Profile → its running Chromium. Held only for short lookups and
    /// inserts, never while one launches.
    running: tokio::sync::Mutex<HashMap<String, Arc<Running>>>,
    /// Profiles whose Chromium is launching: counted with `running`
    /// against [`MAX_RUNNING`].
    launching: Mutex<HashSet<String>>,
    /// Profile → lock held while it is checked, launched and recorded (and
    /// while it is deleted).
    profile_locks: KeyedLocks,
    /// Conversation → lock held for a whole open, close or idle park of its
    /// tab. Taken before `screencast_locks`.
    tab_locks: KeyedLocks,
    /// Conversation → the URL of its tab, closed for being idle; the next
    /// tool call reopens it there.
    parked: Mutex<HashMap<String, String>>,
    /// Conversation → lock serializing screencast start/stop.
    screencast_locks: KeyedLocks,
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub(crate) struct AgentBrowser {
    inner: Arc<Inner>,
}

/// A live view subscription; dropping it stops the screencast when nobody
/// else watches.
pub(crate) struct BrowserWatch {
    pub frames: watch::Receiver<View>,
    browser: AgentBrowser,
    conversation_id: String,
}

impl Drop for BrowserWatch {
    fn drop(&mut self) {
        if self
            .browser
            .inner
            .shared
            .frames
            .unsubscribe(&self.conversation_id)
        {
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
                #[cfg(test)]
                launcher: Mutex::new(None),
                installer: Installer::new(data_dir),
                profiles,
                running: tokio::sync::Mutex::new(HashMap::new()),
                launching: Mutex::new(HashSet::new()),
                profile_locks: KeyedLocks::default(),
                tab_locks: KeyedLocks::default(),
                parked: Mutex::new(HashMap::new()),
                screencast_locks: KeyedLocks::default(),
                shared: Arc::new(Shared::load(data_dir)),
            }),
        };
        browser.spawn_sweeper();
        Ok(browser)
    }

    /// Tabs going away, for the journal. Subscribe before anything can
    /// open one.
    pub(crate) fn tab_closures(&self) -> broadcast::Receiver<TabClosed> {
        self.inner.shared.closures.subscribe()
    }

    /// Conversations that had a tab when the previous daemon stopped and
    /// have not been reported yet; [`Self::settle_stale_tabs`] them once
    /// journaled.
    pub(crate) fn stale_tabs(&self) -> Vec<String> {
        self.inner
            .shared
            .restarted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// The `restart` closure of these conversations is journaled: forget
    /// them (and rewrite `open-tabs.json` without them).
    pub(crate) fn settle_stale_tabs(&self, conversations: &[String]) {
        let shared = &self.inner.shared;
        let tabs = shared.lock_tabs();
        shared
            .restarted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|conversation| !conversations.contains(conversation));
        shared.persist(&tabs);
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
                        // A Chromium that died took its tabs with it.
                        profile.release_all(CloseReason::Crash).await;
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
                // Serialized with this conversation's open and close, and
                // with watch start/stop: a viewer or tool call arriving now
                // keeps the tab.
                let tab_lock = self.inner.tab_locks.get(&conversation_id);
                let _tab = tab_lock.lock().await;
                let screencast_lock = self.inner.screencast_locks.get(&conversation_id);
                let _screencast = screencast_lock.lock().await;
                if self.inner.shared.frames.watched(&conversation_id)
                    || !running.is_idle(&conversation_id, TAB_IDLE).await
                {
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
                let Some(target_id) = running.close_tab(&conversation_id).await else {
                    continue;
                };
                if let Some(url) = url {
                    self.inner
                        .parked
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(conversation_id.clone(), url);
                }
                running.gone(&conversation_id, &target_id, CloseReason::Idle);
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
        let profile = self.inner.shared.slot_of(conversation_id)?;
        self.inner.running.lock().await.get(&profile).cloned()
    }

    /// The Chromium to launch: its executable (none in tests, which start
    /// scripted ones), or why it cannot start.
    fn launch_requirements(&self) -> Result<PathBuf, BrowserError> {
        #[cfg(test)]
        if self
            .inner
            .launcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
        {
            return Ok(PathBuf::new());
        }
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
        Ok(executable)
    }

    async fn launch(&self, executable: &Path, profile: &str) -> Result<Arc<Running>, BrowserError> {
        #[cfg(test)]
        {
            let scripted = self
                .inner
                .launcher
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|launcher| launcher(profile, Arc::downgrade(&self.inner.shared)));
            if let Some(started) = scripted {
                return started.await;
            }
        }
        let process = launch::launch(executable, &self.inner.profiles.dir_of(profile)).await?;
        Running::start(
            process,
            Arc::downgrade(&self.inner.shared),
            profile.to_owned(),
        )
        .await
    }

    /// The profile's Chromium, started if needed (closing an idle profile
    /// when [`MAX_RUNNING`] are up). One profile launches at a time, but a
    /// launch holds no lock other profiles and conversations need.
    async fn ensure_running(&self, profile: &str) -> Result<Arc<Running>, BrowserError> {
        let lock = self.inner.profile_locks.get(profile);
        let _profile = lock.lock().await;
        // A deleted profile must not get its directory (or a Chromium)
        // back.
        if !self.inner.profiles.contains(profile) {
            return Err(BrowserError::invalid(format!(
                "no browser profile {profile}"
            )));
        }
        let existing = self.inner.running.lock().await.get(profile).cloned();
        if let Some(existing) = existing {
            if existing.alive() {
                return Ok(existing);
            }
            // It died: report its tabs before a new Chromium takes over.
            existing.release_all(CloseReason::Crash).await;
            let mut running = self.inner.running.lock().await;
            if running
                .get(profile)
                .is_some_and(|current| Arc::ptr_eq(current, &existing))
            {
                running.remove(profile);
            }
        }
        let executable = self.launch_requirements()?;
        let reservation = {
            let mut running = self.inner.running.lock().await;
            let launching = self
                .inner
                .launching
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len();
            if running.len() + launching >= MAX_RUNNING {
                let mut spare = None;
                for (id, candidate) in running.iter() {
                    if candidate.tabs.lock().await.is_empty()
                        && self.inner.shared.slots_of(id).is_empty()
                    {
                        spare = Some(id.clone());
                        break;
                    }
                }
                match spare.and_then(|id| running.remove(&id)) {
                    Some(stopped) => stopped.shut_down(),
                    None => {
                        return Err(BrowserError::new(
                            "TAB_LIMIT",
                            format!(
                                "{MAX_RUNNING} browser profiles are in use; close a tab first."
                            ),
                        ))
                    }
                }
            }
            self.inner
                .launching
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(profile.to_owned());
            LaunchSlot {
                launching: &self.inner.launching,
                profile: profile.to_owned(),
            }
        };
        let started = self.launch(&executable, profile).await?;
        let mut running = self.inner.running.lock().await;
        running.insert(profile.to_owned(), started.clone());
        // Counted as running from here on.
        drop(reservation);
        Ok(started)
    }

    /// Runs one `browser_*` tool for a conversation of `workspace`
    /// (`{ id, path }`). Results have the shape the agent tools expect.
    /// `may_reload` allows reopening a tab closed for being idle (which
    /// loads its page again); it is false in Plan mode.
    pub(crate) async fn invoke(
        &self,
        conversation_id: &str,
        workspace: &Value,
        tool: &str,
        args: &Value,
        may_reload: bool,
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
                // URL for the next try if that cannot happen now).
                let keep = |url: String| {
                    self.inner
                        .parked
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .entry(conversation_id.to_owned())
                        .or_insert(url);
                };
                if !may_reload {
                    keep(url);
                    return Err(BrowserError::new(
                        "PLAN_MODE",
                        "This conversation's tab was closed for being idle; reopening it loads \
                         the page again, which is not done in Plan mode. It reopens on the first \
                         browser call outside Plan mode.",
                    ));
                }
                if let Err(error) = self.open(conversation_id, workspace, &url).await {
                    keep(url);
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
                self.close_conversation(conversation_id, CloseReason::User)
                    .await;
                Ok(json!({ "closed": true }))
            }
            other => Err(BrowserError::invalid(format!("unknown tool {other}"))),
        }
    }

    /// Opens (or reuses) the conversation's tab and loads `url`. Holds the
    /// conversation's tab lock throughout, so concurrent calls open one
    /// tab and a close or revoke waits for the open to finish.
    async fn open(
        &self,
        conversation_id: &str,
        workspace: &Value,
        url: &str,
    ) -> Result<Value, BrowserError> {
        let lock = self.inner.tab_locks.get(conversation_id);
        let _tab = lock.lock().await;
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
        // The workspace moved to another profile: reopen there.
        if self
            .inner
            .shared
            .slot_of(conversation_id)
            .is_some_and(|current| current != profile)
        {
            self.close_conversation_locked(conversation_id, CloseReason::User)
                .await;
        }
        self.inner
            .parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(conversation_id);
        let running = self.ensure_running(&profile).await?;
        running.touch_tab(conversation_id).await;
        if running.tab(conversation_id).await.is_err() {
            let mut slot = self.reserve_tab(conversation_id, &profile)?;
            running.open_tab(conversation_id).await?;
            slot.commit();
            self.sync_screencast(conversation_id).await;
        }
        running.load(conversation_id, url).await
    }

    /// Claims one of the [`MAX_TABS`] slots for the conversation's tab in
    /// `profile` before it is opened, so concurrent opens cannot exceed the
    /// limit. The slot goes back when the reservation is dropped without
    /// [`SlotReservation::commit`].
    fn reserve_tab(
        &self,
        conversation_id: &str,
        profile: &str,
    ) -> Result<SlotReservation, BrowserError> {
        self.inner.shared.reserve(conversation_id, profile, || {
            self.inner.profiles.contains(profile)
        })?;
        Ok(SlotReservation {
            shared: self.inner.shared.clone(),
            conversation_id: conversation_id.to_owned(),
            profile: profile.to_owned(),
            committed: false,
        })
    }

    /// The conversation's tab goes away (`browser_close`, revoke, profile
    /// change). Waits for an open in progress to finish first.
    pub(crate) async fn close_conversation(&self, conversation_id: &str, reason: CloseReason) {
        let lock = self.inner.tab_locks.get(conversation_id);
        let _tab = lock.lock().await;
        self.close_conversation_locked(conversation_id, reason)
            .await;
    }

    /// [`Self::close_conversation`] for a caller already holding the
    /// conversation's tab lock.
    async fn close_conversation_locked(&self, conversation_id: &str, reason: CloseReason) {
        self.inner
            .parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(conversation_id);
        let Some(profile) = self.inner.shared.slot_of(conversation_id) else {
            return;
        };
        let running = self.inner.running.lock().await.get(&profile).cloned();
        let target_id = match running {
            Some(running) => running.close_tab(conversation_id).await,
            None => None,
        };
        self.inner.shared.tab_gone(
            conversation_id,
            &profile,
            target_id.as_deref().unwrap_or_default(),
            reason,
        );
    }

    /// Live frames of the conversation's tab, from now on. The
    /// subscription is registered before anything is awaited, so a caller
    /// aborted mid-way still releases it.
    pub(crate) async fn watch(&self, conversation_id: &str) -> BrowserWatch {
        let (frames, first) = self.inner.shared.frames.subscribe(conversation_id);
        let watch = BrowserWatch {
            frames,
            browser: self.clone(),
            conversation_id: conversation_id.to_owned(),
        };
        if first {
            self.sync_screencast(conversation_id).await;
        }
        // A view starts Pending; with no tab there will be no frame to end
        // that, so say so.
        if self.inner.shared.slot_of(conversation_id).is_none() {
            self.inner.shared.frames.tab_closed(conversation_id);
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
                .set_screencast(
                    conversation_id,
                    self.inner.shared.frames.watched(conversation_id),
                )
                .await;
        }
    }

    /// A JPEG of the tab now: the latest live frame, else a screenshot.
    pub(crate) async fn frame(&self, conversation_id: &str) -> Result<Vec<u8>, BrowserError> {
        if let Some(frame) = self.inner.shared.frames.latest(conversation_id) {
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
        // From here on nothing opens a tab or launches a Chromium in it.
        self.inner.profiles.remove(id)?;
        self.close_profile_tabs(id).await;
        // Waits for a launch in progress; later ones find no profile.
        let lock = self.inner.profile_locks.get(id);
        let _profile = lock.lock().await;
        let stopped = self.inner.running.lock().await.remove(id);
        if let Some(running) = stopped {
            running.release_all(CloseReason::User).await;
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
        for conversation in self.inner.shared.slots_of(profile) {
            self.close_conversation(&conversation, CloseReason::User)
                .await;
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

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("todex-browser-{}", uuid::Uuid::new_v4()))
    }

    fn workspace(name: &str) -> Value {
        json!({ "id": name, "path": format!("/{name}") })
    }

    /// A service whose Chromium is scripted.
    fn scripted() -> (AgentBrowser, scripted::ScriptedChromium, PathBuf) {
        let root = temp_root();
        let browser = AgentBrowser::load(&root).unwrap();
        let chromium = browser.script_chromium();
        (browser, chromium, root)
    }

    async fn open_tab(
        browser: &AgentBrowser,
        conversation: &str,
        workspace_name: &str,
    ) -> Result<Value, BrowserError> {
        browser
            .invoke(
                conversation,
                &workspace(workspace_name),
                "browser_open",
                &json!({ "url": "http://localhost:5173/" }),
                true,
            )
            .await
    }

    /// Takes a slot for good.
    fn keep_slot(browser: &AgentBrowser, conversation: &str, profile: &str) {
        browser.reserve_tab(conversation, profile).unwrap().commit();
    }

    /// Lets the scripted browser's answers and unanswered commands land.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    #[tokio::test]
    async fn tab_slots_are_reserved_before_opening_and_given_back_on_failure() {
        let root = temp_root();
        let browser = AgentBrowser::load(&root).unwrap();
        let profile = browser.inner.profiles.create("p").unwrap().id;
        let other = browser.inner.profiles.create("q").unwrap().id;
        // Concurrent opens: exactly MAX_TABS win a slot.
        let reservations: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..MAX_TABS * 2)
                .map(|index| {
                    let (browser, profile) = (&browser, &profile);
                    scope.spawn(move || browser.reserve_tab(&format!("c{index}"), profile))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(reservations.iter().filter(|r| r.is_ok()).count(), MAX_TABS);
        let mut won: Vec<_> = reservations.into_iter().filter_map(Result::ok).collect();
        for slot in &mut won {
            slot.commit();
        }
        let holder = won[0].conversation_id.clone();
        // A conversation that already holds a slot reuses it.
        keep_slot(&browser, &holder, &profile);
        assert_eq!(
            browser.reserve_tab("late", &profile).err().unwrap().code,
            "TAB_LIMIT"
        );
        // A failed (dropped, uncommitted) open gives the slot back and tells
        // live views.
        let (mut view, _) = browser.inner.shared.frames.subscribe("late");
        assert!(matches!(&*view.borrow_and_update(), View::Pending));
        browser.inner.shared.release(&holder, &profile);
        drop(browser.reserve_tab("late", &profile).unwrap());
        assert!(browser.inner.shared.slot_of("late").is_none());
        assert!(matches!(&*view.borrow_and_update(), View::Closed));
        // Not when the conversation moved to another profile meanwhile.
        let reservation = browser.reserve_tab("late", &profile).unwrap();
        browser.inner.shared.release("late", &profile);
        keep_slot(&browser, "late", &other);
        drop(reservation);
        assert_eq!(
            browser.inner.shared.slot_of("late").as_deref(),
            Some(other.as_str())
        );
        // A deleted profile takes no slot.
        browser.inner.profiles.remove(&profile).unwrap();
        assert_eq!(
            browser.reserve_tab("c9", &profile).err().unwrap().code,
            "INVALID_ARGUMENT"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_opens_of_one_conversation_create_one_tab() {
        let (browser, chromium, root) = scripted();
        let (first, second) =
            tokio::join!(open_tab(&browser, "c", "w"), open_tab(&browser, "c", "w"));
        first.unwrap();
        second.unwrap();
        assert_eq!(chromium.calls("Target.createTarget").len(), 1);
        assert_eq!(chromium.launches(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_open_dropped_midway_closes_its_window_and_frees_the_slot() {
        let (browser, chromium, root) = scripted();
        // The tab is created and attached, then the call stalls.
        chromium.hold("Page.getFrameTree");
        let cancelled =
            tokio::time::timeout(Duration::from_millis(300), open_tab(&browser, "c", "w")).await;
        assert!(cancelled.is_err(), "the open is still waiting");
        settle().await;
        assert_eq!(chromium.calls("Target.createTarget").len(), 1);
        assert_eq!(
            chromium.calls("Target.closeTarget"),
            vec![json!({ "targetId": "T1" })]
        );
        assert!(browser.inner.shared.slot_of("c").is_none());
        let running = browser.inner.running.lock().await.values().next().cloned();
        assert!(running.unwrap().tabs.lock().await.is_empty());
        // The conversation can open again.
        chromium.release("Page.getFrameTree");
        open_tab(&browser, "c", "w").await.unwrap();
        assert!(browser.inner.shared.slot_of("c").is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn closing_or_crashing_frees_the_slot_and_tells_views_and_the_journal() {
        let (browser, chromium, root) = scripted();
        let mut closures = browser.tab_closures();
        let closed = |closures: &mut broadcast::Receiver<TabClosed>, reason| {
            let closed = closures.try_recv().unwrap();
            assert_eq!(closed.reason, reason);
            closed.conversation_id
        };

        // browser_close: reason user.
        open_tab(&browser, "a", "w").await.unwrap();
        let mut view = browser.watch("a").await;
        assert!(!matches!(&*view.frames.borrow_and_update(), View::Closed));
        browser
            .invoke("a", &workspace("w"), "browser_close", &json!({}), true)
            .await
            .unwrap();
        assert!(browser.inner.shared.slot_of("a").is_none());
        assert!(matches!(&*view.frames.borrow_and_update(), View::Closed));
        assert_eq!(closed(&mut closures, CloseReason::User), "a");
        // Closing what is not open says nothing.
        browser.close_conversation("a", CloseReason::Revoked).await;
        assert!(closures.try_recv().is_err());

        // Revoked.
        open_tab(&browser, "b", "w").await.unwrap();
        browser.close_conversation("b", CloseReason::Revoked).await;
        assert_eq!(closed(&mut closures, CloseReason::Revoked), "b");

        // The Chromium exits with a tab open.
        open_tab(&browser, "c", "w").await.unwrap();
        open_tab(&browser, "d", "w").await.unwrap();
        let mut view = browser.watch("c").await;
        chromium.crash(0);
        let mut crashed = vec![];
        for _ in 0..2 {
            crashed.push(
                tokio::time::timeout(Duration::from_secs(2), closures.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        crashed.sort_by(|a, b| a.conversation_id.cmp(&b.conversation_id));
        assert_eq!(
            crashed,
            vec![
                TabClosed {
                    conversation_id: "c".into(),
                    reason: CloseReason::Crash
                },
                TabClosed {
                    conversation_id: "d".into(),
                    reason: CloseReason::Crash
                }
            ]
        );
        assert!(browser.inner.shared.slot_of("c").is_none());
        assert!(browser.inner.shared.slot_of("d").is_none());
        assert!(matches!(&*view.frames.borrow_and_update(), View::Closed));
        // The next open starts a new Chromium and takes a slot again.
        open_tab(&browser, "c", "w").await.unwrap();
        assert_eq!(chromium.launches(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_idle_tab_is_parked_and_reported() {
        let (browser, _chromium, root) = scripted();
        let mut closures = browser.tab_closures();
        open_tab(&browser, "c", "w").await.unwrap();
        let running = browser.running_for("c").await.unwrap();
        // Not idle yet: untouched.
        browser.park_idle_tabs().await;
        assert!(browser.inner.shared.slot_of("c").is_some());
        running.tabs.lock().await.get_mut("c").unwrap().last_used = std::time::Instant::now()
            .checked_sub(TAB_IDLE + Duration::from_secs(1))
            .expect("the machine has been up for ten minutes");
        // A viewer keeps it.
        let watch = browser.watch("c").await;
        browser.park_idle_tabs().await;
        assert!(browser.inner.shared.slot_of("c").is_some());
        drop(watch);
        browser.park_idle_tabs().await;
        assert!(browser.inner.shared.slot_of("c").is_none());
        assert_eq!(
            closures.try_recv().unwrap(),
            TabClosed {
                conversation_id: "c".into(),
                reason: CloseReason::Idle
            }
        );
        assert_eq!(
            browser
                .inner
                .parked
                .lock()
                .unwrap()
                .get("c")
                .map(String::as_str),
            Some("http://localhost:5173/")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_launch_in_progress_blocks_neither_other_profiles_nor_the_running_limit() {
        let (browser, chromium, root) = scripted();
        open_tab(&browser, "a", "wa").await.unwrap();
        chromium.set_launch_delay(Duration::from_millis(400));
        let launching = {
            let browser = browser.clone();
            tokio::spawn(async move { open_tab(&browser, "b", "wb").await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(browser.inner.launching.lock().unwrap().len(), 1);
        // The other conversation keeps working while the launch is pending.
        let quick = Duration::from_millis(100);
        assert!(tokio::time::timeout(quick, browser.running_for("a"))
            .await
            .unwrap()
            .is_some());
        assert!(tokio::time::timeout(quick, browser.frame("a"))
            .await
            .is_ok());
        launching.await.unwrap().unwrap();

        // Three profiles launch at once: two fit.
        let results = tokio::join!(
            open_tab(&browser, "c1", "w1"),
            open_tab(&browser, "c2", "w2"),
            open_tab(&browser, "c3", "w3")
        );
        let codes: Vec<_> = [results.0, results.1, results.2]
            .into_iter()
            .map(|result| result.err().map(|error| error.code))
            .collect();
        // The two running ones had tabs; nothing was spare.
        assert_eq!(
            codes
                .iter()
                .filter(|code| code.as_deref() == Some("TAB_LIMIT"))
                .count(),
            3
        );
        assert!(chromium.peak_launches() <= MAX_RUNNING);
        assert!(browser.inner.running.lock().await.len() <= MAX_RUNNING);
        assert!(browser.inner.launching.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn simultaneous_launches_never_exceed_the_running_limit() {
        let (browser, chromium, root) = scripted();
        chromium.set_launch_delay(Duration::from_millis(100));
        let ids: Vec<String> = (0..3)
            .map(|index| {
                browser
                    .inner
                    .profiles
                    .create(&format!("p{index}"))
                    .unwrap()
                    .id
            })
            .collect();
        let sampler = {
            let browser = browser.clone();
            tokio::spawn(async move {
                let mut peak = 0;
                for _ in 0..40 {
                    let running = browser.inner.running.lock().await.len();
                    let launching = browser.inner.launching.lock().unwrap().len();
                    peak = peak.max(running + launching);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                peak
            })
        };
        let results = tokio::join!(
            browser.ensure_running(&ids[0]),
            browser.ensure_running(&ids[1]),
            browser.ensure_running(&ids[2])
        );
        let results = [results.0, results.1, results.2];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
        assert!(results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| error.code == "TAB_LIMIT"));
        assert!(sampler.await.unwrap() <= MAX_RUNNING);
        assert!(chromium.peak_launches() <= MAX_RUNNING);
        assert_eq!(chromium.launches(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn deleting_a_profile_while_it_opens_leaves_nothing_behind() {
        let (browser, chromium, root) = scripted();
        chromium.set_launch_delay(Duration::from_millis(50));
        // A launch is in progress when the profile is deleted.
        let profile = browser.inner.profiles.create("p").unwrap().id;
        let dir = browser.inner.profiles.dir_of(&profile);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cookies"), b"x").unwrap();
        let launching = {
            let browser = browser.clone();
            let profile = profile.clone();
            tokio::spawn(async move { browser.ensure_running(&profile).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        browser.delete_profile(&profile).await.unwrap();
        launching.await.unwrap().unwrap();
        assert!(!dir.exists());
        assert!(browser.inner.running.lock().await.get(&profile).is_none());
        // Later opens and launches find no profile.
        assert_eq!(
            browser.ensure_running(&profile).await.err().unwrap().code,
            "INVALID_ARGUMENT"
        );
        assert_eq!(
            browser.reserve_tab("c", &profile).err().unwrap().code,
            "INVALID_ARGUMENT"
        );
        assert!(!dir.exists());

        // An open racing a delete of its workspace's profile.
        open_tab(&browser, "c", "w").await.unwrap();
        let id = browser.profiles().workspaces["w"].clone();
        let (opened, deleted) =
            tokio::join!(open_tab(&browser, "c2", "w"), browser.delete_profile(&id));
        deleted.unwrap();
        let _ = opened;
        assert!(browser.inner.running.lock().await.get(&id).is_none());
        assert!(browser.inner.shared.slots_of(&id).is_empty());
        assert!(!browser.inner.profiles.dir_of(&id).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn open_tabs_are_recorded_and_reported_after_a_restart() {
        let (browser, _chromium, root) = scripted();
        let file = root.join("agent-browser").join("open-tabs.json");
        let recorded =
            || -> Vec<String> { serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap() };
        open_tab(&browser, "a", "w").await.unwrap();
        open_tab(&browser, "b", "w").await.unwrap();
        assert_eq!(recorded(), ["a", "b"]);
        browser.close_conversation("a", CloseReason::User).await;
        assert_eq!(recorded(), ["b"]);
        assert!(browser.stale_tabs().is_empty());

        // The daemon stops with "b" open.
        let restarted = AgentBrowser::load(&root).unwrap();
        assert_eq!(restarted.stale_tabs(), ["b"]);
        // New tabs do not erase what is still to be reported.
        let _chromium = restarted.script_chromium();
        open_tab(&restarted, "n", "w").await.unwrap();
        assert_eq!(recorded(), ["b", "n"]);
        restarted.settle_stale_tabs(&["b".to_owned()]);
        assert!(restarted.stale_tabs().is_empty());
        assert_eq!(recorded(), ["n"]);
        restarted.close_conversation("n", CloseReason::User).await;
        assert!(recorded().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_view_without_a_tab_ends_and_one_with_a_tab_waits_for_a_frame() {
        let (browser, chromium, root) = scripted();
        let mut view = browser.watch("none").await;
        assert!(matches!(&*view.frames.borrow_and_update(), View::Closed));
        open_tab(&browser, "c", "w").await.unwrap();
        let mut view = browser.watch("c").await;
        assert!(matches!(&*view.frames.borrow_and_update(), View::Pending));
        assert_eq!(chromium.calls("Page.startScreencast").len(), 1);
        chromium.emit(
            0,
            &json!({ "method": "Page.screencastFrame", "sessionId": "S-T1",
                "params": { "data": "AAAA", "sessionId": 1, "metadata": { "deviceWidth": 4, "deviceHeight": 2 } } }),
        );
        tokio::time::timeout(Duration::from_secs(2), view.frames.changed())
            .await
            .unwrap()
            .unwrap();
        let View::Frame(frame) = view.frames.borrow_and_update().clone() else {
            panic!("a frame");
        };
        assert_eq!((frame.data.as_str(), frame.width), ("AAAA", 4));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn an_idle_closed_tab_is_not_reloaded_in_plan_mode() {
        let root = std::env::temp_dir().join(format!("todex-browser-{}", uuid::Uuid::new_v4()));
        let browser = AgentBrowser::load(&root).unwrap();
        browser
            .inner
            .parked
            .lock()
            .unwrap()
            .insert("c".into(), "http://localhost:5173/".into());
        let workspace = json!({ "id": "w", "path": "/w" });
        let error = browser
            .invoke("c", &workspace, "browser_snapshot", &json!({}), false)
            .await
            .unwrap_err();
        assert_eq!(error.code, "PLAN_MODE");
        // Kept for the first call outside Plan mode.
        assert_eq!(
            browser
                .inner
                .parked
                .lock()
                .unwrap()
                .get("c")
                .map(String::as_str),
            Some("http://localhost:5173/")
        );
        let _ = std::fs::remove_dir_all(root);
    }

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
        // The first subscriber sees "nothing yet", not "closed".
        assert!(matches!(&*receiver.borrow(), View::Pending));
        assert!(frames.latest("c").is_none());
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
        let View::Frame(latest) = receiver.borrow().clone() else {
            panic!("a frame");
        };
        assert_eq!(
            (latest.seq, latest.data.as_str(), latest.width),
            (2, "BBBB", 10)
        );
        assert_eq!(frames.latest("c").unwrap().seq, 2);
        frames.tab_closed("c");
        assert!(matches!(&*receiver.borrow(), View::Closed));
        assert!(frames.latest("c").is_none());
        assert!(!frames.unsubscribe("c"));
        assert!(frames.unsubscribe("c"));
        assert!(!frames.watched("c"));
    }
}
