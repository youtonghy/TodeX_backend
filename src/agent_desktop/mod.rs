//! Agent desktop tools (the `todex_desktop` MCP server), run by the daemon
//! on its own host; clients only watch and answer prompts.
//!
//! The agent browser ([`crate::agent_browser`]) needs a per-conversation
//! grant any paired device may give. Grants live in memory: a daemon
//! restart asks again.
//!
//! Computer Use (`computer_*`) runs on the daemon's own host
//! ([`crate::computer`]) behind its own switch and grant, which the person
//! at the host confirms there. The screen is leased to one conversation at
//! a time; the lease ends with `computer_done`, a stop, a revoke, or
//! [`SCREEN_IDLE`].

mod keyed;
mod shots;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, RwLock};

use crate::{
    agent_browser::{AgentBrowser, CloseReason},
    computer::{Computer, ComputerError},
    error::AppError,
    secure_fs,
};

pub(crate) use keyed::KeyedLocks;
pub(crate) use shots::ShotStore;

const SETTINGS_FILE: &str = "agent-desktop.json";
/// A screen lease nobody used for this long may be taken over.
pub(crate) const SCREEN_IDLE: Duration = Duration::from_secs(120);
const STATE_DIR: &str = "agent-desktop";
/// Live frames: at most one capture this often, this wide, this quality.
const FRAME_INTERVAL: Duration = Duration::from_millis(300);
/// A capture taking longer than this fails instead of holding viewers.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_MAX_WIDTH: u32 = 960;
const FRAME_QUALITY: u8 = 60;
/// Device id of the daemon's own host in Computer Use grants and events.
pub(crate) const HOST_DEVICE_ID: &str = "host";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DesktopSettings {
    /// Agents get the `todex_desktop` MCP server. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// Agents also get the `computer_*` tools. Off by default; only
    /// effective while `enabled`.
    #[serde(default)]
    pub computer_enabled: bool,
}

/// What a revoke ended for a conversation.
#[derive(Debug, Default)]
pub(crate) struct Revoked {
    pub computer: Option<Grant>,
    /// It held a screen lease, now released.
    pub screen_ended: bool,
}

/// What changed when the switches were set.
#[derive(Debug)]
pub(crate) struct SettingsChange {
    /// Conversations whose screen session ended.
    pub screen_ended: Vec<String>,
    /// Conversations whose Computer Use grant was revoked.
    pub computer_revoked: Vec<String>,
}

/// How a conversation got the host's screen.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScreenClaim {
    /// The conversation already held it.
    Continued,
    /// Newly taken; `displaced` held it before but had gone idle.
    Started { displaced: Option<String> },
}

struct Lease {
    /// Unique per lease, so work started under an earlier one can tell it
    /// no longer holds the screen.
    id: u64,
    conversation_id: String,
    last_used: Instant,
    /// Calls running under the lease ([`ScreenUse`]); it is not idle then.
    in_flight: usize,
    /// Becomes `true` when the lease ends (release, revoke, expiry or
    /// takeover), to stop calls running under it.
    ended: watch::Sender<bool>,
}

/// One call's use of the screen lease; dropping it ends the call's claim.
pub(crate) struct ScreenUse {
    pub claim: ScreenClaim,
    /// The lease the call runs under.
    pub id: u64,
    ended: watch::Receiver<bool>,
    inner: Arc<Inner>,
}

impl ScreenUse {
    /// Resolves once the lease ended (stop, revoke, expiry or takeover).
    pub(crate) async fn ended(&self) {
        // A dropped sender means the lease is gone too.
        let _ = self.ended.clone().wait_for(|ended| *ended).await;
    }
}

impl Drop for ScreenUse {
    fn drop(&mut self) {
        let mut lease = self.inner.lease.lock().expect("desktop lease lock");
        if let Some(lease) = lease.as_mut().filter(|lease| lease.id == self.id) {
            lease.in_flight = lease.in_flight.saturating_sub(1);
            lease.last_used = Instant::now();
        }
    }
}

/// Takes the lease if it is `conversation_id`'s (and `id`, when given),
/// telling the calls running under it. Returns whether it did.
fn end_lease(lease: &mut Option<Lease>, conversation_id: &str, id: Option<u64>) -> bool {
    if !lease.as_ref().is_some_and(|lease| {
        lease.conversation_id == conversation_id && id.is_none_or(|id| lease.id == id)
    }) {
        return false;
    }
    if let Some(lease) = lease.take() {
        lease.ended.send_replace(true);
    }
    true
}

/// The desktop a conversation may drive. For Computer Use this is the
/// daemon's host (`device_id` [`HOST_DEVICE_ID`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Grant {
    pub device_id: String,
    pub device_name: String,
}

/// Daemon-wide state for desktop tools.
#[derive(Clone)]
pub struct AgentDesktop {
    inner: Arc<Inner>,
}

/// A captured frame: the lease it was taken under, when, and the JPEG.
type LiveFrame = (u64, Instant, Arc<Vec<u8>>);

struct Inner {
    settings_path: PathBuf,
    settings: RwLock<DesktopSettings>,
    browser: AgentBrowser,
    grants: Mutex<HashMap<String, Grant>>,
    // Lock order: `lease`, then `computer_grants`, then `approved_apps`.
    // Revoking and approving hold the lease lock across the others so a
    // revoke cannot interleave with an approval of the same lease.
    computer_grants: Mutex<HashMap<String, Grant>>,
    /// The conversation controlling the host's screen.
    lease: Mutex<Option<Lease>>,
    /// Source of [`Lease::id`]s.
    next_lease: AtomicU64,
    /// Conversation → bundle ids the user let its agent control.
    approved_apps: Mutex<HashMap<String, HashSet<String>>>,
    shots: ShotStore,
    computer: std::sync::RwLock<Computer>,
    /// The latest live frame (and its lease), shared by concurrent viewers.
    frame: tokio::sync::Mutex<Option<LiveFrame>>,
}

impl AgentDesktop {
    pub async fn load(data_dir: &Path) -> Result<Self, AppError> {
        let settings_path = data_dir.join(SETTINGS_FILE);
        let settings = match tokio::fs::read(&settings_path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                AppError::InvalidRequest(format!(
                    "{} is not valid desktop tool settings: {error}",
                    settings_path.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DesktopSettings::default()
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                settings_path,
                settings: RwLock::new(settings),
                browser: AgentBrowser::load(data_dir)?,
                grants: Mutex::new(HashMap::new()),
                computer_grants: Mutex::new(HashMap::new()),
                lease: Mutex::new(None),
                next_lease: AtomicU64::new(1),
                approved_apps: Mutex::new(HashMap::new()),
                shots: ShotStore::new(data_dir.join(STATE_DIR).join("shots")),
                computer: std::sync::RwLock::new(Computer::native()),
                frame: tokio::sync::Mutex::new(None),
            }),
        })
    }

    pub(crate) fn browser(&self) -> &AgentBrowser {
        &self.inner.browser
    }

    pub(crate) fn shots(&self) -> &ShotStore {
        &self.inner.shots
    }

    pub(crate) fn computer(&self) -> Computer {
        self.inner
            .computer
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn set_computer(&self, computer: Computer) {
        *self
            .inner
            .computer
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = computer;
    }

    pub(crate) async fn enabled(&self) -> bool {
        self.inner.settings.read().await.enabled
    }

    pub(crate) async fn settings(&self) -> DesktopSettings {
        *self.inner.settings.read().await
    }

    /// Computer Use is on: both switches.
    pub(crate) async fn computer_enabled(&self) -> bool {
        let settings = self.inner.settings.read().await;
        settings.enabled && settings.computer_enabled
    }

    #[cfg(test)]
    pub(crate) async fn set_enabled(&self, enabled: bool) -> Result<DesktopSettings, AppError> {
        self.update_settings(Some(enabled), None).await?;
        Ok(self.settings().await)
    }

    /// Persists the switches. Turning tools off revokes the affected grants
    /// so tabs close and screens are released; providers already running
    /// keep the server until their next start, and their calls fail while
    /// off. Callers that hold the daemon's [`crate::agent_mcp::AgentMcp`]
    /// use its `update_desktop_settings`, which also voids the pending
    /// answers of revoked conversations.
    pub(crate) async fn update_settings(
        &self,
        enabled: Option<bool>,
        computer_enabled: Option<bool>,
    ) -> Result<SettingsChange, AppError> {
        let mut settings = self.inner.settings.write().await;
        let next = DesktopSettings {
            enabled: enabled.unwrap_or(settings.enabled),
            computer_enabled: computer_enabled.unwrap_or(settings.computer_enabled),
        };
        let bytes = serde_json::to_vec_pretty(&next)?;
        let path = self.inner.settings_path.clone();
        tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&path, &bytes))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        *settings = next;
        drop(settings);
        if next.enabled {
            // The agent browser's Chromium downloads in the background.
            self.inner.browser.start_install();
        }
        let mut screen_ended = Vec::new();
        let mut computer_revoked = Vec::new();
        if !next.enabled {
            let mut conversations: HashSet<String> = self
                .inner
                .grants
                .lock()
                .expect("desktop grant lock")
                .keys()
                .cloned()
                .collect();
            conversations.extend(self.computer_conversations());
            for conversation_id in conversations {
                let revoked = self.revoke(&conversation_id);
                if revoked.computer.is_some() {
                    computer_revoked.push(conversation_id.clone());
                }
                if revoked.screen_ended {
                    screen_ended.push(conversation_id);
                }
            }
        } else if !next.computer_enabled {
            for conversation_id in self.computer_conversations() {
                let (grant, ended) = self.revoke_computer(&conversation_id);
                if grant.is_some() {
                    computer_revoked.push(conversation_id.clone());
                }
                if ended {
                    screen_ended.push(conversation_id);
                }
            }
        }
        Ok(SettingsChange {
            screen_ended,
            computer_revoked,
        })
    }

    fn computer_conversations(&self) -> Vec<String> {
        self.inner
            .computer_grants
            .lock()
            .expect("desktop grant lock")
            .keys()
            .cloned()
            .collect()
    }

    /// The daemon's own listener: the agent browser never opens it.
    pub fn set_daemon_port(&self, port: u16) {
        crate::agent_browser::set_daemon_port(port);
    }

    pub(crate) fn grant(&self, conversation_id: &str) -> Option<Grant> {
        self.inner
            .grants
            .lock()
            .expect("desktop grant lock")
            .get(conversation_id)
            .cloned()
    }

    pub(crate) fn set_grant(&self, conversation_id: &str, grant: Grant) {
        self.inner
            .grants
            .lock()
            .expect("desktop grant lock")
            .insert(conversation_id.to_owned(), grant);
    }

    pub(crate) fn computer_grant(&self, conversation_id: &str) -> Option<Grant> {
        self.inner
            .computer_grants
            .lock()
            .expect("desktop grant lock")
            .get(conversation_id)
            .cloned()
    }

    pub(crate) fn set_computer_grant(&self, conversation_id: &str, grant: Grant) {
        self.inner
            .computer_grants
            .lock()
            .expect("desktop grant lock")
            .insert(conversation_id.to_owned(), grant);
    }

    /// Takes the host's screen for the conversation, unless another
    /// conversation used it within [`SCREEN_IDLE`] or has a call running
    /// (returned as `Err`). The call holds the lease until it drops the
    /// returned [`ScreenUse`], and is told when the lease ends.
    pub(crate) fn claim_screen(&self, conversation_id: &str) -> Result<ScreenUse, String> {
        let (claim, id, ended) = {
            let mut lease = self.inner.lease.lock().expect("desktop lease lock");
            if let Some(current) = lease.as_mut() {
                if current.conversation_id == conversation_id {
                    current.last_used = Instant::now();
                    current.in_flight += 1;
                    let ended = current.ended.subscribe();
                    return Ok(self.screen_use(ScreenClaim::Continued, current.id, ended));
                }
                if current.in_flight > 0 || current.last_used.elapsed() < SCREEN_IDLE {
                    return Err(current.conversation_id.clone());
                }
            }
            let id = self.inner.next_lease.fetch_add(1, Ordering::SeqCst);
            let (sender, ended) = watch::channel(false);
            let displaced = lease
                .replace(Lease {
                    id,
                    conversation_id: conversation_id.to_owned(),
                    last_used: Instant::now(),
                    in_flight: 1,
                    ended: sender,
                })
                .map(|previous| {
                    previous.ended.send_replace(true);
                    previous.conversation_id
                });
            (ScreenClaim::Started { displaced }, id, ended)
        };
        self.computer().host().session(Some(""));
        Ok(self.screen_use(claim, id, ended))
    }

    fn screen_use(&self, claim: ScreenClaim, id: u64, ended: watch::Receiver<bool>) -> ScreenUse {
        ScreenUse {
            claim,
            id,
            ended,
            inner: self.inner.clone(),
        }
    }

    /// A JPEG of the host's screen for viewers of the conversation that
    /// controls it; captures at most every [`FRAME_INTERVAL`].
    pub(crate) async fn live_frame(
        &self,
        conversation_id: &str,
    ) -> Result<Arc<Vec<u8>>, ComputerError> {
        let Some(lease) = self.screen_lease(conversation_id) else {
            return Err(ComputerError::new(
                "NOT_CONTROLLING",
                "this conversation is not controlling the computer",
            ));
        };
        let mut frame = self.inner.frame.lock().await;
        if let Some((frame_lease, taken, jpeg)) = frame.as_ref() {
            if *frame_lease == lease && taken.elapsed() < FRAME_INTERVAL {
                return Ok(jpeg.clone());
            }
        }
        let computer = self.computer();
        // A capture waiting on a busy engine must not hold every viewer.
        let jpeg = tokio::time::timeout(
            FRAME_TIMEOUT,
            computer.host().frame(lease, FRAME_MAX_WIDTH, FRAME_QUALITY),
        )
        .await
        .map_err(|_| ComputerError::new("TIMEOUT", "capturing the screen took too long"))??;
        let jpeg = Arc::new(jpeg);
        *frame = Some((lease, Instant::now(), jpeg.clone()));
        Ok(jpeg)
    }

    /// The conversation controlling the host's screen, if any.
    pub(crate) fn screen_holder(&self) -> Option<String> {
        self.inner
            .lease
            .lock()
            .expect("desktop lease lock")
            .as_ref()
            .map(|lease| lease.conversation_id.clone())
    }

    /// The id of the lease the conversation holds, if any.
    fn screen_lease(&self, conversation_id: &str) -> Option<u64> {
        self.inner
            .lease
            .lock()
            .expect("desktop lease lock")
            .as_ref()
            .filter(|lease| lease.conversation_id == conversation_id)
            .map(|lease| lease.id)
    }

    /// Whether lease `id` is still the conversation's and its Computer Use
    /// grant stands: what a call started under that lease needs before it
    /// goes on.
    pub(crate) fn holds(&self, conversation_id: &str, id: u64) -> bool {
        let lease = self.inner.lease.lock().expect("desktop lease lock");
        self.holds_locked(&lease, conversation_id, id)
    }

    fn holds_locked(&self, lease: &Option<Lease>, conversation_id: &str, id: u64) -> bool {
        lease
            .as_ref()
            .is_some_and(|lease| lease.id == id && lease.conversation_id == conversation_id)
            && self
                .inner
                .computer_grants
                .lock()
                .expect("desktop grant lock")
                .contains_key(conversation_id)
    }

    /// Ends the conversation's screen lease. Returns whether it held one.
    pub(crate) fn release_screen(&self, conversation_id: &str) -> bool {
        self.release_lease(conversation_id, None)
    }

    /// Ends the conversation's screen lease, if it is still lease `id`.
    pub(crate) fn release_screen_use(&self, conversation_id: &str, id: u64) -> bool {
        self.release_lease(conversation_id, Some(id))
    }

    fn release_lease(&self, conversation_id: &str, id: Option<u64>) -> bool {
        let released = end_lease(
            &mut self.inner.lease.lock().expect("desktop lease lock"),
            conversation_id,
            id,
        );
        if released {
            self.computer().host().session(None);
        }
        released
    }

    /// Ends a lease idle for [`SCREEN_IDLE`] with no call running; returns
    /// its conversation.
    pub(crate) fn expire_screens(&self) -> Vec<String> {
        let expired = {
            let mut lease = self.inner.lease.lock().expect("desktop lease lock");
            let holder = lease
                .as_ref()
                .filter(|lease| lease.in_flight == 0 && lease.last_used.elapsed() >= SCREEN_IDLE)
                .map(|lease| lease.conversation_id.clone());
            holder.filter(|holder| end_lease(&mut lease, holder, None))
        };
        if expired.is_some() {
            self.computer().host().session(None);
        }
        expired.into_iter().collect()
    }

    #[cfg(test)]
    pub(crate) fn age_lease(&self, by: Duration) {
        if let Some(lease) = self
            .inner
            .lease
            .lock()
            .expect("desktop lease lock")
            .as_mut()
        {
            lease.last_used = Instant::now()
                .checked_sub(by)
                .expect("an instant that far back");
        }
    }

    pub(crate) fn approved_apps(&self, conversation_id: &str) -> Vec<String> {
        let mut apps: Vec<String> = self
            .inner
            .approved_apps
            .lock()
            .expect("desktop app lock")
            .get(conversation_id)
            .map(|apps| apps.iter().cloned().collect())
            .unwrap_or_default();
        apps.sort();
        apps
    }

    /// Lets the conversation's agent control `bundle_id`, unless lease `id`
    /// has ended or lost its grant in the meantime (a revoke clears the
    /// approvals, and none may reappear after it). Returns whether it did.
    pub(crate) fn approve_app_if_held(
        &self,
        conversation_id: &str,
        bundle_id: &str,
        id: u64,
    ) -> bool {
        let lease = self.inner.lease.lock().expect("desktop lease lock");
        if !self.holds_locked(&lease, conversation_id, id) {
            return false;
        }
        self.inner
            .approved_apps
            .lock()
            .expect("desktop app lock")
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(bundle_id.to_owned());
        true
    }

    /// Drops the conversation's Computer Use grant, approved apps and screen
    /// lease, and tells the calls running under the lease. Returns the grant
    /// and whether a screen session ended. Use
    /// [`crate::agent_mcp::AgentMcp::revoke_computer`], which also voids
    /// the host answers still waiting for the conversation.
    pub(crate) fn revoke_computer(&self, conversation_id: &str) -> (Option<Grant>, bool) {
        let (grant, screen_ended) = {
            // The lease lock is held throughout: an approval of this lease
            // either completes before the revoke or sees it.
            let mut lease = self.inner.lease.lock().expect("desktop lease lock");
            let grant = self
                .inner
                .computer_grants
                .lock()
                .expect("desktop grant lock")
                .remove(conversation_id);
            self.inner
                .approved_apps
                .lock()
                .expect("desktop app lock")
                .remove(conversation_id);
            (grant, end_lease(&mut lease, conversation_id, None))
        };
        if screen_ended {
            self.computer().host().session(None);
        }
        (grant, screen_ended)
    }

    /// Drops the conversation's browser grant and closes its tab.
    pub(crate) fn revoke_browser(&self, conversation_id: &str) -> Option<Grant> {
        let grant = self
            .inner
            .grants
            .lock()
            .expect("desktop grant lock")
            .remove(conversation_id)?;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let browser = self.inner.browser.clone();
            let conversation_id = conversation_id.to_owned();
            runtime.spawn(async move {
                browser
                    .close_conversation(&conversation_id, CloseReason::Revoked)
                    .await
            });
        }
        Some(grant)
    }

    /// Drops everything the conversation was granted.
    pub(crate) fn revoke(&self, conversation_id: &str) -> Revoked {
        self.revoke_browser(conversation_id);
        let (computer, screen_ended) = self.revoke_computer(conversation_id);
        Revoked {
            computer,
            screen_ended,
        }
    }

    /// Conversation deleted or expired: forget its grant and screenshots.
    pub(crate) async fn forget(&self, conversation_id: &str) {
        self.revoke(conversation_id);
        self.inner.shots.remove_conversation(conversation_id).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    async fn desktop() -> (AgentDesktop, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("todex-agent-desktop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        (AgentDesktop::load(&root).await.unwrap(), root)
    }

    /// Whether the screen use was told its lease ended.
    async fn ended(screen: &ScreenUse) -> bool {
        tokio::time::timeout(Duration::from_millis(200), screen.ended())
            .await
            .is_ok()
    }

    fn granted() -> Grant {
        Grant {
            device_id: HOST_DEVICE_ID.into(),
            device_name: "Host".into(),
        }
    }

    #[tokio::test]
    async fn a_running_call_keeps_the_lease_from_idling_out_or_being_taken() {
        let (desktop, root) = desktop().await;
        let aged = SCREEN_IDLE + Duration::from_secs(1);
        let first = desktop.claim_screen("a").unwrap();
        assert!(matches!(
            first.claim,
            ScreenClaim::Started { displaced: None }
        ));
        let again = desktop.claim_screen("a").unwrap();
        assert_eq!(again.claim, ScreenClaim::Continued);
        assert_eq!(again.id, first.id);

        // Idle by the clock, but two calls are running.
        desktop.age_lease(aged);
        assert!(desktop.expire_screens().is_empty());
        assert_eq!(desktop.claim_screen("b").err().as_deref(), Some("a"));
        drop(again);
        assert!(desktop.expire_screens().is_empty());
        assert!(desktop.claim_screen("b").is_err());

        // The last call ending counts as use; only then does idling start.
        drop(first);
        assert!(desktop.expire_screens().is_empty());
        assert!(desktop.claim_screen("b").is_err());
        desktop.age_lease(aged);
        let taken = desktop.claim_screen("b").unwrap();
        assert!(matches!(
            &taken.claim,
            ScreenClaim::Started { displaced: Some(previous) } if previous == "a"
        ));
        assert_ne!(taken.id, 0);
        desktop.age_lease(aged);
        drop(taken);
        desktop.age_lease(aged);
        assert_eq!(desktop.expire_screens(), ["b"]);
        assert_eq!(desktop.screen_holder(), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ending_a_lease_tells_its_calls_and_voids_what_they_were_about_to_do() {
        let (desktop, root) = desktop().await;
        desktop.set_computer_grant("a", granted());
        let first = desktop.claim_screen("a").unwrap();
        assert!(!ended(&first).await);
        assert!(desktop.holds("a", first.id));
        assert!(!desktop.holds("a", first.id + 1));
        assert!(!desktop.holds("b", first.id));
        assert!(desktop.approve_app_if_held("a", "com.example.app", first.id));
        assert_eq!(desktop.approved_apps("a"), ["com.example.app"]);

        // A revoke drops grant, approvals and lease, and signals the call.
        let (grant, screen_ended) = desktop.revoke_computer("a");
        assert_eq!(grant, Some(granted()));
        assert!(screen_ended);
        assert!(ended(&first).await);
        assert!(!desktop.holds("a", first.id));
        assert!(!desktop.approve_app_if_held("a", "com.example.other", first.id));
        assert!(desktop.approved_apps("a").is_empty());

        // The stale call cannot touch the next lease either.
        desktop.set_computer_grant("a", granted());
        let second = desktop.claim_screen("a").unwrap();
        assert_ne!(second.id, first.id);
        assert!(!desktop.holds("a", first.id));
        assert!(!desktop.approve_app_if_held("a", "com.example.app", first.id));
        drop(first);
        // Dropping the stale call did not count against the new lease.
        assert!(!ended(&second).await);

        // Releasing, expiring and taking over signal too; so does losing
        // only the grant (holds, not the signal).
        desktop.revoke_computer("a");
        desktop.set_computer_grant("a", granted());
        let third = desktop.claim_screen("a").unwrap();
        assert!(desktop.release_screen("a"));
        assert!(ended(&third).await);
        let fourth = desktop.claim_screen("a").unwrap();
        drop(third);
        desktop.age_lease(SCREEN_IDLE + Duration::from_secs(1));
        drop(fourth);
        desktop.age_lease(SCREEN_IDLE + Duration::from_secs(1));
        let fifth = desktop.claim_screen("a").unwrap();
        assert!(desktop.release_screen_use("a", fifth.id));
        assert!(!desktop.release_screen_use("a", fifth.id));
        let sixth = desktop.claim_screen("a").unwrap();
        assert!(!desktop.release_screen_use("a", sixth.id + 100));
        desktop.age_lease(SCREEN_IDLE + Duration::from_secs(1));
        drop(sixth);
        desktop.age_lease(SCREEN_IDLE + Duration::from_secs(1));
        let seventh = desktop.claim_screen("b").unwrap();
        assert!(matches!(seventh.claim, ScreenClaim::Started { .. }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn settings_persist_and_disabling_revokes_grants() {
        let root =
            std::env::temp_dir().join(format!("todex-agent-desktop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let desktop = AgentDesktop::load(&root).await.unwrap();
        assert!(!desktop.enabled().await);
        desktop.set_enabled(true).await.unwrap();
        assert!(AgentDesktop::load(&root).await.unwrap().enabled().await);

        let grant = Grant {
            device_id: "dev_desk".into(),
            device_name: "Desk".into(),
        };
        desktop.set_grant("conv", grant.clone());
        assert_eq!(desktop.grant("conv"), Some(grant));
        desktop.set_enabled(false).await.unwrap();
        assert_eq!(desktop.grant("conv"), None);
        assert!(!AgentDesktop::load(&root).await.unwrap().enabled().await);

        std::fs::write(root.join(SETTINGS_FILE), b"{\"enabled\":1}").unwrap();
        assert!(AgentDesktop::load(&root).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
