use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::conversation::ProviderKind;
use crate::error::AppError;

use super::{now_millis, AGENT_PROVIDERS_FILE, MAX_PROVIDER_SETTINGS_BYTES};

/// One account/provider profile as managed by TodeX, modeled on cc-switch's
/// `Provider` record. `settings_config` is opaque per-agent data: the whole
/// `settings.json` object for Claude Code, `{auth, config}` for Codex and
/// Grok Build, and the
/// native provider node for the additive agents (Pi `models.json`, OpenCode
/// `opencode.json`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProviderProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub settings_config: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_index: Option<i64>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProviderBucket {
    #[serde(default)]
    pub providers: BTreeMap<String, AgentProviderProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provider_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProviderSnapshot {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentProviderBucket>,
    #[serde(default)]
    pub updated_at: u64,
}

/// File-backed store for managed agent providers. Provider credentials live in
/// this file, so it is written owner-only like the kanban snapshot.
#[derive(Clone)]
pub struct AgentProviderStore {
    path: Arc<PathBuf>,
    inner: Arc<RwLock<AgentProviderSnapshot>>,
}

impl AgentProviderStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let path = data_dir.join(AGENT_PROVIDERS_FILE);
        let snapshot = load_snapshot(&path).await?;
        Ok(Self {
            path: Arc::new(path),
            inner: Arc::new(RwLock::new(snapshot)),
        })
    }

    pub async fn snapshot(&self) -> AgentProviderSnapshot {
        self.inner.read().await.clone()
    }

    pub async fn profile(&self, agent: ProviderKind, id: &str) -> Option<AgentProviderProfile> {
        self.inner
            .read()
            .await
            .agents
            .get(agent.as_str())
            .and_then(|bucket| bucket.providers.get(id))
            .cloned()
    }

    pub async fn upsert(
        &self,
        agent: ProviderKind,
        profile: AgentProviderProfile,
    ) -> Result<(), AppError> {
        let mut snapshot = self.inner.write().await;
        let bucket = snapshot
            .agents
            .entry(agent.as_str().to_owned())
            .or_default();
        bucket.providers.insert(profile.id.clone(), profile);
        snapshot.updated_at = now_millis();
        write_snapshot(&self.path, &snapshot).await
    }

    /// Rewrites only `settings_config` of an existing profile, preserving the
    /// timestamps and metadata the user set. Used by activate's backfill step.
    pub async fn backfill_settings(
        &self,
        agent: ProviderKind,
        id: &str,
        settings_config: Value,
    ) -> Result<bool, AppError> {
        let mut snapshot = self.inner.write().await;
        let Some(bucket) = snapshot.agents.get_mut(agent.as_str()) else {
            return Ok(false);
        };
        let Some(profile) = bucket.providers.get_mut(id) else {
            return Ok(false);
        };
        if profile.settings_config == settings_config {
            return Ok(true);
        }
        if serialized_len(&settings_config)? > MAX_PROVIDER_SETTINGS_BYTES {
            return Err(AppError::InvalidRequest(format!(
                "live config for {} exceeds the {} byte limit",
                agent.as_str(),
                MAX_PROVIDER_SETTINGS_BYTES
            )));
        }
        profile.settings_config = settings_config;
        profile.updated_at = now_millis();
        snapshot.updated_at = now_millis();
        write_snapshot(&self.path, &snapshot).await?;
        Ok(true)
    }

    pub async fn remove(
        &self,
        agent: ProviderKind,
        id: &str,
    ) -> Result<Option<AgentProviderProfile>, AppError> {
        let mut snapshot = self.inner.write().await;
        let Some(bucket) = snapshot.agents.get_mut(agent.as_str()) else {
            return Ok(None);
        };
        let removed = bucket.providers.remove(id);
        if removed.is_some() {
            if bucket.current_provider_id.as_deref() == Some(id) {
                bucket.current_provider_id = None;
            }
            snapshot.updated_at = now_millis();
            write_snapshot(&self.path, &snapshot).await?;
        }
        Ok(removed)
    }

    pub async fn set_current(
        &self,
        agent: ProviderKind,
        id: Option<String>,
    ) -> Result<(), AppError> {
        let mut snapshot = self.inner.write().await;
        let bucket = snapshot
            .agents
            .entry(agent.as_str().to_owned())
            .or_default();
        if bucket.current_provider_id == id {
            return Ok(());
        }
        bucket.current_provider_id = id;
        snapshot.updated_at = now_millis();
        write_snapshot(&self.path, &snapshot).await
    }
}

fn serialized_len(value: &Value) -> Result<usize, AppError> {
    Ok(serde_json::to_vec(value)?.len())
}

async fn load_snapshot(path: &std::path::Path) -> Result<AgentProviderSnapshot, AppError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentProviderSnapshot {
                schema_version: 1,
                ..AgentProviderSnapshot::default()
            });
        }
        Err(error) => return Err(error.into()),
    };
    if text.trim().is_empty() {
        return Ok(AgentProviderSnapshot {
            schema_version: 1,
            ..AgentProviderSnapshot::default()
        });
    }
    let mut snapshot: AgentProviderSnapshot = serde_json::from_str(&text)?;
    snapshot.schema_version = 1;
    Ok(snapshot)
}

async fn write_snapshot(
    path: &std::path::Path,
    snapshot: &AgentProviderSnapshot,
) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp_path = path.with_file_name(format!(".agent-providers.{}.tmp", Uuid::new_v4().simple()));
    let mut bytes = serde_json::to_vec_pretty(snapshot)?;
    bytes.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp_path)
        .await?;
    set_owner_only(&tmp_path).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    #[cfg(windows)]
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(&tmp_path, path).await?;
    set_owner_only(path).await?;
    Ok(())
}

async fn set_owner_only(path: &std::path::Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
