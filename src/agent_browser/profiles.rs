//! Browser profiles: each workspace browses in its own persistent profile
//! (cookies, storage, cache) under `$DATA_DIR/agent-browser/profiles/<id>`,
//! created on first use and re-assignable, renamable and deletable from
//! settings. The index is `$DATA_DIR/agent-browser/profiles.json`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::BrowserError;
use crate::secure_fs;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileRecord {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

/// `GET /v2/agent-browser/profiles`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfilesState {
    pub profiles: Vec<ProfileRecord>,
    /// Workspace key (workspace id, else path) → profile id.
    pub workspaces: BTreeMap<String, String>,
}

pub(crate) struct Profiles {
    index: PathBuf,
    dir: PathBuf,
    state: Mutex<ProfilesState>,
}

const MAX_NAME_CHARS: usize = 60;

impl Profiles {
    pub(crate) fn load(data_dir: &Path) -> Result<Self, BrowserError> {
        let root = data_dir.join("agent-browser");
        let index = root.join("profiles.json");
        let state = match std::fs::read(&index) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                BrowserError::failed(format!("{} is not valid: {error}", index.display()))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProfilesState::default(),
            Err(error) => {
                return Err(BrowserError::failed(format!(
                    "cannot read {}: {error}",
                    index.display()
                )))
            }
        };
        Ok(Self {
            index,
            dir: root.join("profiles"),
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProfilesState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn save(&self, state: &ProfilesState) -> Result<(), BrowserError> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| BrowserError::failed(error.to_string()))?;
        if let Some(parent) = self.index.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                BrowserError::failed(format!("cannot save browser profiles: {error}"))
            })?;
        }
        secure_fs::write_owner_only_atomic(&self.index, &bytes)
            .map_err(|error| BrowserError::failed(format!("cannot save browser profiles: {error}")))
    }

    pub(crate) fn state(&self) -> ProfilesState {
        self.lock().clone()
    }

    /// Whether the profile exists (not deleted).
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.lock().profiles.iter().any(|profile| profile.id == id)
    }

    pub(crate) fn dir_of(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// The workspace's profile, created (named after it) on first use.
    pub(crate) fn for_workspace(
        &self,
        workspace: &str,
        label: &str,
    ) -> Result<String, BrowserError> {
        let mut state = self.lock();
        if let Some(id) = state.workspaces.get(workspace) {
            if state.profiles.iter().any(|profile| &profile.id == id) {
                return Ok(id.clone());
            }
        }
        let record = new_record(label);
        let id = record.id.clone();
        state.profiles.push(record);
        state.workspaces.insert(workspace.to_owned(), id.clone());
        self.save(&state)?;
        Ok(id)
    }

    pub(crate) fn create(&self, name: &str) -> Result<ProfileRecord, BrowserError> {
        let mut state = self.lock();
        let record = new_record(name);
        state.profiles.push(record.clone());
        self.save(&state)?;
        Ok(record)
    }

    pub(crate) fn rename(&self, id: &str, name: &str) -> Result<(), BrowserError> {
        let mut state = self.lock();
        let profile = state
            .profiles
            .iter_mut()
            .find(|profile| profile.id == id)
            .ok_or_else(|| BrowserError::invalid(format!("no browser profile {id}")))?;
        profile.name = clean_name(name);
        self.save(&state)
    }

    /// Points a workspace at another profile; its tabs reopen there.
    pub(crate) fn assign(&self, workspace: &str, id: &str) -> Result<(), BrowserError> {
        let mut state = self.lock();
        if !state.profiles.iter().any(|profile| profile.id == id) {
            return Err(BrowserError::invalid(format!("no browser profile {id}")));
        }
        state.workspaces.insert(workspace.to_owned(), id.to_owned());
        self.save(&state)
    }

    /// Forgets the profile (its workspaces get a new one on next use); the
    /// caller closes its browser and deletes [`Self::dir_of`].
    pub(crate) fn remove(&self, id: &str) -> Result<(), BrowserError> {
        let mut state = self.lock();
        let before = state.profiles.len();
        state.profiles.retain(|profile| profile.id != id);
        if state.profiles.len() == before {
            return Err(BrowserError::invalid(format!("no browser profile {id}")));
        }
        state.workspaces.retain(|_, profile| profile != id);
        self.save(&state)
    }
}

fn clean_name(name: &str) -> String {
    let name: String = name.trim().chars().take(MAX_NAME_CHARS).collect();
    if name.is_empty() {
        "Browser".to_owned()
    } else {
        name
    }
}

fn new_record(name: &str) -> ProfileRecord {
    ProfileRecord {
        // Hex only: safe as a directory name.
        id: Uuid::new_v4().simple().to_string(),
        name: clean_name(name),
        created_at: chrono::Utc::now().timestamp_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_get_profiles_that_persist_and_can_move() {
        let root = std::env::temp_dir().join(format!("todex-profiles-{}", Uuid::new_v4()));
        let profiles = Profiles::load(&root).unwrap();
        let first = profiles.for_workspace("ws-1", "project").unwrap();
        assert_eq!(profiles.for_workspace("ws-1", "project").unwrap(), first);
        let other = profiles.create("  Shared  ").unwrap();
        assert_eq!(other.name, "Shared");
        profiles.assign("ws-1", &other.id).unwrap();
        assert!(profiles.assign("ws-1", "nope").is_err());

        let reloaded = Profiles::load(&root).unwrap();
        assert_eq!(reloaded.state().workspaces["ws-1"], other.id);
        assert_eq!(reloaded.state().profiles.len(), 2);
        reloaded.remove(&other.id).unwrap();
        assert!(!reloaded.state().workspaces.contains_key("ws-1"));
        assert_ne!(reloaded.for_workspace("ws-1", "project").unwrap(), other.id);
        assert!(other.id.chars().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(root);
    }
}
