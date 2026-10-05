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

mod shots;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::{
    agent_browser::AgentBrowser,
    computer::{Computer, ComputerError},
    error::AppError,
    secure_fs,
};

pub(crate) use shots::ShotStore;

const SETTINGS_FILE: &str = "agent-desktop.json";
/// A screen lease nobody used for this long may be taken over.
pub(crate) const SCREEN_IDLE: Duration = Duration::from_secs(120);
const STATE_DIR: &str = "agent-desktop";
/// Live frames: at most one capture this often, this wide, this quality.
const FRAME_INTERVAL: Duration = Duration::from_millis(300);
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
    pub browser: Option<Grant>,
    pub computer: Option<Grant>,
    /// It held a screen lease, now released.
    pub screen_ended: bool,
}

/// Result of taking the host's screen for a conversation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScreenClaim {
    /// The conversation already held it.
    Continued,
    /// Newly taken; `displaced` held it before but had gone idle.
    Started { displaced: Option<String> },
}

struct Lease {
    conversation_id: String,
    last_used: Instant,
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

struct Inner {
    settings_path: PathBuf,
    settings: RwLock<DesktopSettings>,
    browser: AgentBrowser,
    grants: Mutex<HashMap<String, Grant>>,
    computer_grants: Mutex<HashMap<String, Grant>>,
    /// The conversation controlling the host's screen.
    lease: Mutex<Option<Lease>>,
    /// Conversation → bundle ids the user let its agent control.
    approved_apps: Mutex<HashMap<String, HashSet<String>>>,
    shots: ShotStore,
    computer: std::sync::RwLock<Computer>,
    /// The latest live frame, shared by concurrent viewers.
    frame: tokio::sync::Mutex<Option<(Instant, Arc<Vec<u8>>)>>,
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
        Ok(self.update_settings(Some(enabled), None).await?.0)
    }

    /// Persists the switches. Turning tools off revokes the affected grants
    /// so tabs close and screens are released; providers already running
    /// keep the server until their next start, and their calls fail while
    /// off. Returns the conversations whose screen session ended.
    pub(crate) async fn update_settings(
        &self,
        enabled: Option<bool>,
        computer_enabled: Option<bool>,
    ) -> Result<(DesktopSettings, Vec<String>), AppError> {
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
        let mut ended = Vec::new();
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
                if self.revoke(&conversation_id).screen_ended {
                    ended.push(conversation_id);
                }
            }
        } else if !next.computer_enabled {
            for conversation_id in self.computer_conversations() {
                if self.revoke_computer(&conversation_id).1 {
                    ended.push(conversation_id);
                }
            }
        }
        Ok((next, ended))
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
    /// conversation used it within [`SCREEN_IDLE`] (returned as `Err`).
    pub(crate) fn claim_screen(&self, conversation_id: &str) -> Result<ScreenClaim, String> {
        let displaced = {
            let mut lease = self.inner.lease.lock().expect("desktop lease lock");
            match lease.as_mut() {
                Some(current) if current.conversation_id == conversation_id => {
                    current.last_used = Instant::now();
                    return Ok(ScreenClaim::Continued);
                }
                Some(current) if current.last_used.elapsed() < SCREEN_IDLE => {
                    return Err(current.conversation_id.clone());
                }
                _ => {}
            }
            lease
                .replace(Lease {
                    conversation_id: conversation_id.to_owned(),
                    last_used: Instant::now(),
                })
                .map(|previous| previous.conversation_id)
        };
        self.computer().host().session(Some(""));
        Ok(ScreenClaim::Started { displaced })
    }

    /// A JPEG of the host's screen for viewers of the conversation that
    /// controls it; captures at most every [`FRAME_INTERVAL`].
    pub(crate) async fn live_frame(
        &self,
        conversation_id: &str,
    ) -> Result<Arc<Vec<u8>>, ComputerError> {
        if self.screen_holder().as_deref() != Some(conversation_id) {
            return Err(ComputerError::new(
                "NOT_CONTROLLING",
                "this conversation is not controlling the computer",
            ));
        }
        let mut frame = self.inner.frame.lock().await;
        if let Some((taken, jpeg)) = frame.as_ref() {
            if taken.elapsed() < FRAME_INTERVAL {
                return Ok(jpeg.clone());
            }
        }
        let jpeg = Arc::new(
            self.computer()
                .host()
                .frame(FRAME_MAX_WIDTH, FRAME_QUALITY)
                .await?,
        );
        *frame = Some((Instant::now(), jpeg.clone()));
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

    /// Ends the conversation's screen lease. Returns whether it held one.
    pub(crate) fn release_screen(&self, conversation_id: &str) -> bool {
        let released = {
            let mut lease = self.inner.lease.lock().expect("desktop lease lock");
            if lease
                .as_ref()
                .is_some_and(|lease| lease.conversation_id == conversation_id)
            {
                lease.take();
                true
            } else {
                false
            }
        };
        if released {
            self.computer().host().session(None);
        }
        released
    }

    /// Ends a lease idle for [`SCREEN_IDLE`]; returns its conversation.
    pub(crate) fn expire_screens(&self) -> Vec<String> {
        let expired = {
            let mut lease = self.inner.lease.lock().expect("desktop lease lock");
            if lease
                .as_ref()
                .is_some_and(|lease| lease.last_used.elapsed() >= SCREEN_IDLE)
            {
                lease.take().map(|lease| lease.conversation_id)
            } else {
                None
            }
        };
        if expired.is_some() {
            self.computer().host().session(None);
        }
        expired.into_iter().collect()
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

    pub(crate) fn approve_app(&self, conversation_id: &str, bundle_id: &str) {
        self.inner
            .approved_apps
            .lock()
            .expect("desktop app lock")
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(bundle_id.to_owned());
    }

    /// Drops the conversation's Computer Use grant, approved apps and screen
    /// lease. Returns the grant and whether a screen session ended.
    pub(crate) fn revoke_computer(&self, conversation_id: &str) -> (Option<Grant>, bool) {
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
        let screen_ended = self.release_screen(conversation_id);
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
            runtime.spawn(async move { browser.close_conversation(&conversation_id).await });
        }
        Some(grant)
    }

    /// Drops everything the conversation was granted.
    pub(crate) fn revoke(&self, conversation_id: &str) -> Revoked {
        let browser = self.revoke_browser(conversation_id);
        let (computer, screen_ended) = self.revoke_computer(conversation_id);
        Revoked {
            browser,
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
