//! Hosts beyond this computer the person allowed the agent browser to open:
//! for one conversation ("allow once", kept in memory), or always (kept in
//! `hosts.json` until removed in Settings). A host is a name or address
//! without port, as [`host_key`] spells it; allowing it lets the agent's
//! tab show its pages and lets pages in that tab reach it even when it is
//! on the local network.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

const FILE: &str = "hosts.json";

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    hosts: BTreeSet<String>,
}

pub(crate) struct HostApprovals {
    path: PathBuf,
    always: Mutex<BTreeSet<String>>,
    /// Conversation id → hosts allowed for it.
    conversations: Mutex<HashMap<String, HashSet<String>>>,
}

/// The host part of `url` as approvals name it: lowercase, IPv6 without
/// brackets, no port.
pub(crate) fn host_key(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?.trim_matches(['[', ']']).to_lowercase();
    (!host.is_empty()).then_some(host)
}

impl HostApprovals {
    /// Reads `dir/hosts.json`; a missing or unreadable file allows nothing.
    pub(crate) fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let always = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Stored>(&bytes)
                .map(|stored| stored.hosts)
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "ignoring an unreadable agent browser host list");
                    BTreeSet::new()
                }),
            Err(_) => BTreeSet::new(),
        };
        Self {
            path,
            always: Mutex::new(always),
            conversations: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn allowed(&self, conversation_id: &str, host: &str) -> bool {
        self.always
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(host)
            || self
                .conversations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(conversation_id)
                .is_some_and(|hosts| hosts.contains(host))
    }

    /// "Allow once": `host` for this conversation, until the daemon stops.
    pub(crate) fn allow_for(&self, conversation_id: &str, host: &str) {
        self.conversations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(host.to_owned());
    }

    /// "Always allow": `host` for every conversation, saved.
    pub(crate) fn allow_always(&self, host: &str) -> std::io::Result<()> {
        let mut always = self
            .always
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if always.insert(host.to_owned()) {
            if let Err(error) = self.save(&always) {
                always.remove(host);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Takes `host` off the saved list; whether it was there.
    pub(crate) fn remove(&self, host: &str) -> std::io::Result<bool> {
        let mut always = self
            .always
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !always.remove(host) {
            return Ok(false);
        }
        if let Err(error) = self.save(&always) {
            always.insert(host.to_owned());
            return Err(error);
        }
        Ok(true)
    }

    /// The saved hosts, sorted.
    pub(crate) fn always(&self) -> Vec<String> {
        self.always
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn save(&self, hosts: &BTreeSet<String>) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&Stored {
            hosts: hosts.clone(),
        })
        .map_err(std::io::Error::other)?;
        crate::secure_fs::write_owner_only_atomic(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_allowed_per_conversation_or_always_and_saved() {
        let dir = std::env::temp_dir().join(format!("todex-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let approvals = HostApprovals::load(&dir);
        assert!(!approvals.allowed("a", "example.com"));
        approvals.allow_for("a", "example.com");
        assert!(approvals.allowed("a", "example.com"));
        // Another conversation was not asked.
        assert!(!approvals.allowed("b", "example.com"));

        approvals.allow_always("192.168.8.1").unwrap();
        assert!(approvals.allowed("b", "192.168.8.1"));
        // Saved: a restarted daemon still allows it, but forgot "once".
        let reloaded = HostApprovals::load(&dir);
        assert!(reloaded.allowed("c", "192.168.8.1"));
        assert!(!reloaded.allowed("a", "example.com"));
        assert_eq!(reloaded.always(), vec!["192.168.8.1".to_owned()]);

        assert!(reloaded.remove("192.168.8.1").unwrap());
        assert!(!reloaded.remove("192.168.8.1").unwrap());
        assert!(!HostApprovals::load(&dir).allowed("c", "192.168.8.1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hosts_are_keyed_without_port_or_case() {
        let key = |url: &str| host_key(&reqwest::Url::parse(url).unwrap());
        assert_eq!(
            key("https://Example.COM:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(key("http://[fd00::1]:80/").as_deref(), Some("fd00::1"));
        assert_eq!(key("http://192.168.8.1/").as_deref(), Some("192.168.8.1"));
    }
}
