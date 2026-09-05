use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{error::AppError, workspace_paths::validate_workspace_directory_text};

const WORKSPACES_FILE: &str = "workspaces.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub workspaces: Vec<WorkspaceRecord>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub id: String,
    pub name: String,
    pub path: String,
    pub session_id: String,
    pub tenant_id: String,
    #[serde(default)]
    pub thread_id: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub approval_policy: String,
    pub sandbox_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub local_adapter_state: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone)]
pub struct WorkspaceStore {
    path: Arc<PathBuf>,
    workspace_root: Arc<PathBuf>,
    inner: Arc<RwLock<WorkspaceSnapshot>>,
}

impl WorkspaceStore {
    pub async fn new(data_dir: PathBuf, workspace_root: PathBuf) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let path = data_dir.join(WORKSPACES_FILE);
        let snapshot = load_snapshot(&path, &workspace_root).await?;
        Ok(Self {
            path: Arc::new(path),
            workspace_root: Arc::new(workspace_root),
            inner: Arc::new(RwLock::new(snapshot)),
        })
    }

    pub async fn snapshot_owned(&self, owner_id: &str) -> WorkspaceSnapshot {
        let snapshot = self.inner.read().await;
        WorkspaceSnapshot {
            workspaces: snapshot
                .workspaces
                .iter()
                .filter(|workspace| workspace.tenant_id == owner_id)
                .cloned()
                .collect(),
            updated_at: snapshot.updated_at,
        }
    }

    pub(crate) async fn snapshot(&self) -> WorkspaceSnapshot {
        self.inner.read().await.clone()
    }

    pub async fn get_owned(
        &self,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<WorkspaceRecord, AppError> {
        self.inner
            .read()
            .await
            .workspaces
            .iter()
            .find(|workspace| workspace.tenant_id == owner_id && workspace.id == workspace_id)
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("workspace {workspace_id}")))
    }

    pub async fn merge_owned(
        &self,
        owner_id: &str,
        workspaces: Vec<WorkspaceRecord>,
    ) -> Result<WorkspaceSnapshot, AppError> {
        let incoming = normalize_workspaces(workspaces, &self.workspace_root, Some(owner_id))?;
        let mut current = self.inner.write().await;
        let mut by_id = current
            .workspaces
            .drain(..)
            .map(|workspace| {
                (
                    (workspace.tenant_id.clone(), workspace.id.clone()),
                    workspace,
                )
            })
            .collect::<HashMap<_, _>>();
        for workspace in incoming {
            by_id.insert(
                (workspace.tenant_id.clone(), workspace.id.clone()),
                workspace,
            );
        }
        let mut workspaces = by_id.into_values().collect::<Vec<_>>();
        workspaces.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        let snapshot = WorkspaceSnapshot {
            workspaces,
            updated_at: now_millis(),
        };
        write_snapshot(&self.path, &snapshot).await?;
        *current = snapshot.clone();
        drop(current);
        Ok(self.snapshot_owned(owner_id).await)
    }

    pub async fn delete_owned(&self, owner_id: &str, workspace_id: &str) -> Result<bool, AppError> {
        let mut current = self.inner.write().await;
        let before = current.workspaces.len();
        current
            .workspaces
            .retain(|workspace| workspace.tenant_id != owner_id || workspace.id != workspace_id);
        if current.workspaces.len() == before {
            return Ok(false);
        }
        current.updated_at = now_millis();
        write_snapshot(&self.path, &current).await?;
        Ok(true)
    }
}

async fn load_snapshot(path: &Path, workspace_root: &Path) -> Result<WorkspaceSnapshot, AppError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceSnapshot::default());
        }
        Err(error) => return Err(error.into()),
    };

    if text.trim().is_empty() {
        return Ok(WorkspaceSnapshot::default());
    }

    let mut snapshot: WorkspaceSnapshot = serde_json::from_str(&text)?;
    snapshot.workspaces = normalize_workspaces(snapshot.workspaces, workspace_root, None)?;
    snapshot
        .workspaces
        .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(snapshot)
}

async fn write_snapshot(path: &Path, snapshot: &WorkspaceSnapshot) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp_path = path.with_file_name(format!(".workspaces.{}.tmp", Uuid::new_v4().simple()));
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

fn normalize_workspaces(
    workspaces: Vec<WorkspaceRecord>,
    workspace_root: &Path,
    owner_id: Option<&str>,
) -> Result<Vec<WorkspaceRecord>, AppError> {
    let mut normalized: HashMap<(String, String), WorkspaceRecord> =
        HashMap::with_capacity(workspaces.len());
    for mut workspace in workspaces {
        if workspace.name.trim().is_empty() {
            return Err(AppError::InvalidRequest(
                "workspace name is required".to_owned(),
            ));
        }
        if workspace.path.trim().is_empty() {
            return Err(AppError::InvalidRequest(
                "workspace path is required".to_owned(),
            ));
        }
        let path = validate_workspace_directory_text(workspace_root, &workspace.path)?;
        workspace.path = path.display().to_string();
        workspace.id = stable_workspace_id(&path);
        if let Some(owner_id) = owner_id {
            workspace.tenant_id = owner_id.to_owned();
        }
        workspace.session_id = format!("cdxs_{}", workspace.id);
        workspace.thread_id.clear();
        workspace.local_adapter_state = None;
        let key = (workspace.tenant_id.clone(), workspace.id.clone());
        match normalized.get(&key) {
            Some(existing) if existing.updated_at > workspace.updated_at => {}
            _ => {
                normalized.insert(key, workspace);
            }
        }
    }
    Ok(normalized.into_values().collect())
}

pub fn stable_workspace_id(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(path.to_string_lossy().as_bytes());
    let bytes = digest.finalize();
    let encoded = bytes[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("ws_{encoded}")
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
    use std::fs;

    #[tokio::test]
    async fn workspace_store_persists_and_reloads_snapshot() {
        let root = make_temp_dir("todex-workspace-store");
        let workspace_root = root.join("workspaces");
        let workspace_path = workspace_root.join("app");
        fs::create_dir_all(&workspace_path).unwrap();
        let store = WorkspaceStore::new(root.clone(), workspace_root.clone())
            .await
            .unwrap();
        let snapshot = store
            .merge_owned(
                "local",
                vec![WorkspaceRecord {
                    id: "workspace-1".to_owned(),
                    name: "App".to_owned(),
                    path: workspace_path.display().to_string(),
                    session_id: "cdxs_app".to_owned(),
                    tenant_id: "local".to_owned(),
                    thread_id: String::new(),
                    model: "gpt-5.5".to_owned(),
                    reasoning_effort: Some("medium".to_owned()),
                    approval_policy: "on-request".to_owned(),
                    sandbox_mode: "workspace-write".to_owned(),
                    permission_profile: Some(":workspace".to_owned()),
                    approvals_reviewer: Some("user".to_owned()),
                    service_tier: None,
                    local_adapter_state: Some("idle".to_owned()),
                    created_at: 10,
                    updated_at: 20,
                }],
            )
            .await
            .unwrap();

        assert_eq!(snapshot.workspaces.len(), 1);

        let reloaded = WorkspaceStore::new(root.clone(), workspace_root)
            .await
            .unwrap()
            .snapshot_owned("local")
            .await;
        assert_eq!(
            reloaded.workspaces[0].id,
            stable_workspace_id(&fs::canonicalize(&workspace_path).unwrap())
        );
        assert_eq!(
            reloaded.workspaces[0].path,
            fs::canonicalize(&workspace_path)
                .unwrap()
                .display()
                .to_string()
        );
        assert!(reloaded.updated_at > 0);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_store_merges_by_canonical_path_and_owner() {
        let root = make_temp_dir("todex-workspace-store-duplicate");
        let workspace_root = root.join("workspaces");
        let workspace_path = workspace_root.join("app");
        fs::create_dir_all(&workspace_path).unwrap();
        let store = WorkspaceStore::new(root.clone(), workspace_root)
            .await
            .unwrap();
        let workspace = WorkspaceRecord {
            id: "workspace-1".to_owned(),
            name: "App".to_owned(),
            path: workspace_path.display().to_string(),
            session_id: "cdxs_app".to_owned(),
            tenant_id: "local".to_owned(),
            thread_id: String::new(),
            model: "gpt-5.5".to_owned(),
            reasoning_effort: None,
            approval_policy: "on-request".to_owned(),
            sandbox_mode: "workspace-write".to_owned(),
            permission_profile: Some(":workspace".to_owned()),
            approvals_reviewer: Some("user".to_owned()),
            service_tier: None,
            local_adapter_state: None,
            created_at: 10,
            updated_at: 20,
        };

        let snapshot = store
            .merge_owned("local", vec![workspace.clone(), workspace.clone()])
            .await
            .unwrap();

        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].tenant_id, "local");
        assert!(snapshot.workspaces[0].thread_id.is_empty());
        assert!(snapshot.workspaces[0].local_adapter_state.is_none());

        let other_snapshot = store
            .merge_owned(
                "other",
                vec![WorkspaceRecord {
                    tenant_id: "spoofed".to_owned(),
                    name: "Other owner".to_owned(),
                    ..workspace
                }],
            )
            .await
            .unwrap();
        assert_eq!(other_snapshot.workspaces.len(), 1);
        assert_eq!(other_snapshot.workspaces[0].tenant_id, "other");
        assert_eq!(store.snapshot_owned("local").await.workspaces.len(), 1);

        let workspace_id = snapshot.workspaces[0].id.clone();
        assert!(store.delete_owned("local", &workspace_id).await.unwrap());
        assert!(store.snapshot_owned("local").await.workspaces.is_empty());
        assert_eq!(store.snapshot_owned("other").await.workspaces.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nonce = now_millis();
        let path = std::env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
