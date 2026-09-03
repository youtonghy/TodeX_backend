use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedRwLockReadGuard, RwLock};
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
    #[serde(default = "default_trusted")]
    trusted: bool,
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

pub(crate) struct WorkspaceTrustPermit {
    _snapshot: OwnedRwLockReadGuard<WorkspaceTrustSnapshot>,
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
        let decision = snapshot
            .entries
            .iter()
            .find(|entry| entry.owner_id == owner_id && entry.workspace_path == workspace_path);
        let trusted = decision.is_some_and(|entry| entry.trusted);
        Ok(WorkspaceTrustStatus {
            workspace_path,
            trusted,
            trusted_at: decision
                .filter(|entry| entry.trusted)
                .map(|entry| entry.trusted_at),
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
        let mut current = self.inner.write().await;
        let mut snapshot = current.clone();
        snapshot
            .entries
            .retain(|entry| entry.owner_id != owner_id || entry.workspace_path != workspace_path);
        let decided_at = now_millis();
        snapshot.entries.push(WorkspaceTrustEntry {
            owner_id: owner_id.to_owned(),
            workspace_path: workspace_path.clone(),
            trusted,
            trusted_at: decided_at,
        });
        snapshot.entries.sort_by(|left, right| {
            left.owner_id
                .cmp(&right.owner_id)
                .then_with(|| left.workspace_path.cmp(&right.workspace_path))
        });
        snapshot.updated_at = now_millis();
        write_snapshot(&self.path, &snapshot).await?;
        *current = snapshot;
        Ok(WorkspaceTrustStatus {
            workspace_path,
            trusted,
            trusted_at: trusted.then_some(decided_at),
        })
    }

    pub async fn auto_trust_undecided_owned(
        &self,
        owner_id: &str,
        workspaces: &[PathBuf],
    ) -> Result<Vec<WorkspaceTrustStatus>, AppError> {
        let workspace_paths = workspaces
            .iter()
            .map(|workspace| {
                self.validate(workspace)
                    .map(|path| path.display().to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut current = self.inner.write().await;
        let mut snapshot = current.clone();
        let decided_at = now_millis();
        let mut trusted = Vec::new();

        for workspace_path in workspace_paths {
            let already_decided = snapshot
                .entries
                .iter()
                .any(|entry| entry.owner_id == owner_id && entry.workspace_path == workspace_path);
            if already_decided {
                continue;
            }
            snapshot.entries.push(WorkspaceTrustEntry {
                owner_id: owner_id.to_owned(),
                workspace_path: workspace_path.clone(),
                trusted: true,
                trusted_at: decided_at,
            });
            trusted.push(WorkspaceTrustStatus {
                workspace_path,
                trusted: true,
                trusted_at: Some(decided_at),
            });
        }

        if trusted.is_empty() {
            return Ok(trusted);
        }
        snapshot.entries.sort_by(|left, right| {
            left.owner_id
                .cmp(&right.owner_id)
                .then_with(|| left.workspace_path.cmp(&right.workspace_path))
        });
        snapshot.updated_at = decided_at;
        write_snapshot(&self.path, &snapshot).await?;
        *current = snapshot;
        Ok(trusted)
    }

    pub async fn ensure_trusted(&self, owner_id: &str, workspace: &Path) -> Result<(), AppError> {
        let status = self.status_owned(owner_id, workspace).await?;
        if status.trusted {
            Ok(())
        } else {
            Err(AppError::WorkspaceTrustRequired(status.workspace_path))
        }
    }

    pub(crate) async fn acquire_owned(
        &self,
        owner_id: &str,
        workspace: &Path,
    ) -> Result<WorkspaceTrustPermit, AppError> {
        let workspace = self.validate(workspace)?;
        let workspace_path = workspace.display().to_string();
        let snapshot = self.inner.clone().read_owned().await;
        if snapshot.entries.iter().any(|entry| {
            entry.owner_id == owner_id && entry.workspace_path == workspace_path && entry.trusted
        }) {
            Ok(WorkspaceTrustPermit {
                _snapshot: snapshot,
            })
        } else {
            Err(AppError::WorkspaceTrustRequired(workspace_path))
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

fn default_trusted() -> bool {
    true
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

    #[tokio::test]
    async fn automatic_trust_only_applies_without_an_explicit_decision() {
        let root = std::env::temp_dir().join(format!(
            "todex-workspace-auto-trust-{}",
            Uuid::new_v4().simple()
        ));
        let workspace_root = root.join("workspaces");
        let first = workspace_root.join("first");
        let second = workspace_root.join("second");
        tokio::fs::create_dir_all(&first).await.unwrap();
        tokio::fs::create_dir_all(&second).await.unwrap();
        let store = WorkspaceTrustStore::new(root.clone(), workspace_root.clone())
            .await
            .unwrap();

        let granted = store
            .auto_trust_undecided_owned("owner", &[first.clone(), second.clone()])
            .await
            .unwrap();
        assert_eq!(granted.len(), 2);
        assert!(store.acquire_owned("owner", &first).await.is_ok());
        assert!(matches!(
            store.acquire_owned("other-owner", &first).await,
            Err(AppError::WorkspaceTrustRequired(_))
        ));
        store.set_owned("owner", &first, false).await.unwrap();

        let granted = store
            .auto_trust_undecided_owned("owner", &[first.clone(), second.clone()])
            .await
            .unwrap();
        assert!(granted.is_empty());
        assert!(!store.status_owned("owner", &first).await.unwrap().trusted);
        assert!(store.status_owned("owner", &second).await.unwrap().trusted);
        assert!(matches!(
            store.acquire_owned("owner", &first).await,
            Err(AppError::WorkspaceTrustRequired(_))
        ));

        let reloaded = WorkspaceTrustStore::new(root.clone(), workspace_root)
            .await
            .unwrap();
        assert!(
            !reloaded
                .status_owned("owner", &first)
                .await
                .unwrap()
                .trusted
        );
        assert!(
            reloaded
                .status_owned("owner", &second)
                .await
                .unwrap()
                .trusted
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn legacy_trust_entries_default_to_trusted() {
        let root = std::env::temp_dir().join(format!(
            "todex-workspace-legacy-trust-{}",
            Uuid::new_v4().simple()
        ));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        let workspace = tokio::fs::canonicalize(workspace).await.unwrap();
        tokio::fs::write(
            root.join(WORKSPACE_TRUST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "entries": [{
                    "ownerId": "owner",
                    "workspacePath": workspace.display().to_string(),
                    "trustedAt": 1
                }],
                "updatedAt": 1
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        let store = WorkspaceTrustStore::new(root.clone(), workspace_root)
            .await
            .unwrap();
        assert!(
            store
                .status_owned("owner", &workspace)
                .await
                .unwrap()
                .trusted
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn permit_linearizes_revoke_and_blocks_future_launches() {
        let root = std::env::temp_dir().join(format!(
            "todex-workspace-permit-{}",
            Uuid::new_v4().simple()
        ));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        let store = WorkspaceTrustStore::new(root.clone(), workspace_root)
            .await
            .unwrap();
        store.set_owned("owner", &workspace, true).await.unwrap();
        let permit = store.acquire_owned("owner", &workspace).await.unwrap();

        let revoke_store = store.clone();
        let revoke_workspace = workspace.clone();
        let mut revoke = tokio::spawn(async move {
            revoke_store
                .set_owned("owner", &revoke_workspace, false)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut revoke)
                .await
                .is_err()
        );

        drop(permit);
        assert!(!revoke.await.unwrap().unwrap().trusted);
        assert!(matches!(
            store.acquire_owned("owner", &workspace).await,
            Err(AppError::WorkspaceTrustRequired(_))
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn failed_persistence_does_not_change_in_memory_trust() {
        let root = std::env::temp_dir().join(format!(
            "todex-workspace-trust-rollback-{}",
            Uuid::new_v4().simple()
        ));
        let workspace_root = root.join("workspaces");
        let workspace = workspace_root.join("project");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        let store = WorkspaceTrustStore::new(root.clone(), workspace_root)
            .await
            .unwrap();
        tokio::fs::create_dir(&*store.path).await.unwrap();

        assert!(store.set_owned("owner", &workspace, true).await.is_err());
        assert!(
            !store
                .status_owned("owner", &workspace)
                .await
                .unwrap()
                .trusted
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
