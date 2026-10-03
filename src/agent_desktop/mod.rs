//! Agent tools that run on a user's desktop rather than in the daemon.
//!
//! The daemon may be remote; the browser the agent drives belongs to a
//! desktop client. Desktops register as executors on a dedicated `/v2/ws`
//! connection ([`executors`]); the `todex_desktop` MCP server forwards each
//! tool call to the desktop a conversation is bound to. A conversation is
//! bound by its first grant, which only an online executor device may
//! answer. Grants live in memory: a daemon restart asks again.

pub(crate) mod executors;
mod shots;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::{error::AppError, secure_fs};

pub(crate) use executors::{ExecutorError, Executors, InvokeRequest, Registration};
pub(crate) use shots::ShotStore;

const SETTINGS_FILE: &str = "agent-desktop.json";
const STATE_DIR: &str = "agent-desktop";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DesktopSettings {
    /// Agents get the `todex_desktop` MCP server. Off by default.
    #[serde(default)]
    pub enabled: bool,
}

/// The desktop a conversation may drive.
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
    executors: Executors,
    grants: Mutex<HashMap<String, Grant>>,
    shots: ShotStore,
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
                executors: Executors::default(),
                grants: Mutex::new(HashMap::new()),
                shots: ShotStore::new(data_dir.join(STATE_DIR).join("shots")),
            }),
        })
    }

    pub(crate) fn executors(&self) -> &Executors {
        &self.inner.executors
    }

    pub(crate) fn shots(&self) -> &ShotStore {
        &self.inner.shots
    }

    pub(crate) async fn enabled(&self) -> bool {
        self.inner.settings.read().await.enabled
    }

    /// Persists the switch. Disabling revokes every grant so open tabs close;
    /// providers already running keep the server until their next start, and
    /// their calls fail while it is off.
    pub(crate) async fn set_enabled(&self, enabled: bool) -> Result<DesktopSettings, AppError> {
        let mut settings = self.inner.settings.write().await;
        let next = DesktopSettings { enabled };
        let bytes = serde_json::to_vec_pretty(&next)?;
        let path = self.inner.settings_path.clone();
        tokio::task::spawn_blocking(move || secure_fs::write_owner_only_atomic(&path, &bytes))
            .await
            .map_err(|error| AppError::Anyhow(error.into()))??;
        *settings = next;
        drop(settings);
        if !enabled {
            let conversations: Vec<String> = self
                .inner
                .grants
                .lock()
                .expect("desktop grant lock")
                .keys()
                .cloned()
                .collect();
            for conversation_id in conversations {
                self.revoke(&conversation_id);
            }
        }
        Ok(next)
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

    /// Drops the conversation's grant and tells its desktop to close what it
    /// opened. Returns the revoked grant.
    pub(crate) fn revoke(&self, conversation_id: &str) -> Option<Grant> {
        let grant = self
            .inner
            .grants
            .lock()
            .expect("desktop grant lock")
            .remove(conversation_id)?;
        for executor in self
            .inner
            .executors
            .online(executors::CAPABILITY_BROWSER)
            .into_iter()
            .filter(|executor| executor.device_id == grant.device_id)
        {
            self.inner
                .executors
                .release(executor.executor_id, conversation_id);
        }
        Some(grant)
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

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let _registration = desktop.executors().register(
            "dev_desk".into(),
            "Desk".into(),
            "darwin".into(),
            vec![executors::CAPABILITY_BROWSER.into()],
            tx,
        );
        let grant = Grant {
            device_id: "dev_desk".into(),
            device_name: "Desk".into(),
        };
        desktop.set_grant("conv", grant.clone());
        assert_eq!(desktop.grant("conv"), Some(grant));
        desktop.set_enabled(false).await.unwrap();
        assert_eq!(desktop.grant("conv"), None);
        let release = rx.recv().await.unwrap();
        assert_eq!(release["type"], "executor.release");
        assert_eq!(release["payload"]["conversationId"], "conv");
        assert!(!AgentDesktop::load(&root).await.unwrap().enabled().await);

        std::fs::write(root.join(SETTINGS_FILE), b"{\"enabled\":1}").unwrap();
        assert!(AgentDesktop::load(&root).await.is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
