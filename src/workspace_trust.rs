use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{error::AppError, workspace_paths::validate_workspace_directory_text};

const WORKSPACE_TRUST_FILE: &str = "workspace-trust.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceTrustSnapshot {
    #[serde(default)]
    entries: Vec<WorkspaceTrustEntry>,
    #[serde(default)]
    updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceTrustEntry {
    owner_id: String,
    workspace_path: String,
    trusted_at: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTrustStatus {
    pub workspace_path: String,
    pub trusted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_at: Option<u64>,
}

#[derive(Clone)]
pub struct WorkspaceTrustStore {
    path: Arc<PathBuf>,
    workspace_root: Arc<PathBuf>,
    inner: Arc<RwLock<WorkspaceTrustSnapshot>>,
}

impl WorkspaceTrustStore {
    pub async fn new(data_dir: PathBuf, workspace_root: PathBuf) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let path = data_dir.join(WORKSPACE_TRUST_FILE);
        let snapshot = load_snapshot(&path, &workspace_root).await?;
        Ok(Self {
            path: Arc::new(path),
            workspace_root: Arc::new(workspace_root),
            inner: Arc::new(RwLock::new(snapshot)),
        })
    }

    pub async fn status_owned(
        &self,
        owner_id: &str,
        workspace: &Path,
    ) -> Result<WorkspaceTrustStatus, AppError> {
        let workspace = self.validate(workspace)?;
        let workspace_path = workspace.display().to_string();
        let snapshot = self.inner.read().await;
        let trusted_at = snapshot
            .entries
            .iter()
            .find(|entry| entry.owner_id == owner_id && entry.workspace_path == workspace_path)
            .map(|entry| entry.trusted_at);
        Ok(WorkspaceTrustStatus {
            workspace_path,
            trusted: trusted_at.is_some(),
            trusted_at,
        })
    }

    pub async fn set_owned(
        &self,
        owner_id: &str,
        workspace: &Path,
        trusted: bool,
    ) -> Result<WorkspaceTrustStatus, AppError> {
        let workspace = self.validate(workspace)?;
        let workspace_path = workspace.display().to_string();
        let mut snapshot = self.inner.write().await;
        snapshot
            .entries
            .retain(|entry| entry.owner_id != owner_id || entry.workspace_path != workspace_path);
        let trusted_at = trusted.then(now_millis);
        if let Some(trusted_at) = trusted_at {
            snapshot.entries.push(WorkspaceTrustEntry {
                owner_id: owner_id.to_owned(),
                workspace_path: workspace_path.clone(),
                trusted_at,
            });
        }
        snapshot.entries.sort_by(|left, right| {
            left.owner_id
                .cmp(&right.owner_id)
                .then_with(|| left.workspace_path.cmp(&right.workspace_path))
        });
        snapshot.updated_at = now_millis();
        write_snapshot(&self.path, &snapshot).await?;
        Ok(WorkspaceTrustStatus {
            workspace_path,
            trusted,
            trusted_at,
        })
    }

    pub async fn ensure_trusted(&self, owner_id: &str, workspace: &Path) -> Result<(), AppError> {
        let status = self.status_owned(owner_id, workspace).await?;
        if status.trusted {
            Ok(())
        } else {
            Err(AppError::WorkspaceTrustRequired(status.workspace_path))
        }
    }

    fn validate(&self, workspace: &Path) -> Result<PathBuf, AppError> {
        validate_workspace_directory_text(
            &self.workspace_root,
            workspace.to_str().ok_or_else(|| {
                AppError::InvalidRequest("workspace path is not UTF-8".to_owned())
            })?,
        )
    }
}

async fn load_snapshot(
    path: &Path,
    workspace_root: &Path,
) -> Result<WorkspaceTrustSnapshot, AppError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceTrustSnapshot::default());
        }
        Err(error) => return Err(error.into()),
    };
    if text.trim().is_empty() {
        return Ok(WorkspaceTrustSnapshot::default());
    }
    let mut snapshot: WorkspaceTrustSnapshot = serde_json::from_str(&text)?;
    let mut entries = HashMap::<(String, String), WorkspaceTrustEntry>::new();
    for mut entry in snapshot.entries {
        let Ok(workspace) =
            validate_workspace_directory_text(workspace_root, &entry.workspace_path)
        else {
            tracing::warn!(workspace = %entry.workspace_path, "dropping stale workspace trust entry");
            continue;
        };
        entry.workspace_path = workspace.display().to_string();
        entries.insert(
            (entry.owner_id.clone(), entry.workspace_path.clone()),
            entry,
        );
    }
    snapshot.entries = entries.into_values().collect();
    Ok(snapshot)
}

async fn write_snapshot(path: &Path, snapshot: &WorkspaceTrustSnapshot) -> Result<(), AppError> {
    let temporary =
        path.with_file_name(format!(".workspace-trust.{}.tmp", Uuid::new_v4().simple()));
    let mut bytes = serde_json::to_vec_pretty(snapshot)?;
    bytes.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await?;
    set_owner_only(&temporary).await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    #[cfg(windows)]
    if tokio::fs::try_exists(path).await? {
        tokio::fs::remove_file(path).await?;
    }
    tokio::fs::rename(&temporary, path).await?;
    set_owner_only(path).await
}

async fn set_owner_only(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trust_is_owner_scoped_and_persistent() {
        let root =
            std::env::temp_dir().join(format!("todex-workspace-trust-{}", Uuid::new_v4().simple()));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        let store = WorkspaceTrustStore::new(root.clone(), workspace_root.clone())
            .await
            .unwrap();

        assert!(
            !store
                .status_owned("owner-a", &workspace)
                .await
                .unwrap()
                .trusted
        );
        assert!(
            store
                .set_owned("owner-a", &workspace, true)
                .await
                .unwrap()
                .trusted
        );
        assert!(
            !store
                .status_owned("owner-b", &workspace)
                .await
                .unwrap()
                .trusted
        );

        let reloaded = WorkspaceTrustStore::new(root.clone(), workspace_root)
            .await
            .unwrap();
        assert!(
            reloaded
                .status_owned("owner-a", &workspace)
                .await
                .unwrap()
                .trusted
        );
        assert!(
            !reloaded
                .set_owned("owner-a", &workspace, false)
                .await
                .unwrap()
                .trusted
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
