use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

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

    pub async fn snapshot(&self) -> WorkspaceSnapshot {
        self.inner.read().await.clone()
    }

    pub async fn replace(
        &self,
        workspaces: Vec<WorkspaceRecord>,
    ) -> Result<WorkspaceSnapshot, AppError> {
        let workspaces = normalize_workspaces(workspaces, &self.workspace_root)?;
        let snapshot = WorkspaceSnapshot {
            workspaces,
            updated_at: now_millis(),
        };
        write_snapshot(&self.path, &snapshot).await?;
        *self.inner.write().await = snapshot.clone();
        Ok(snapshot)
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
    snapshot.workspaces = normalize_workspaces(snapshot.workspaces, workspace_root)?;
    snapshot
        .workspaces
        .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(snapshot)
}

async fn write_snapshot(path: &Path, snapshot: &WorkspaceSnapshot) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(snapshot)?;
    tokio::fs::write(&tmp_path, text).await?;
    tokio::fs::rename(tmp_path, path).await?;
    Ok(())
}

fn normalize_workspaces(
    workspaces: Vec<WorkspaceRecord>,
    workspace_root: &Path,
) -> Result<Vec<WorkspaceRecord>, AppError> {
    let mut seen_ids = HashSet::new();
    let mut normalized = Vec::with_capacity(workspaces.len());
    for mut workspace in workspaces {
        if workspace.id.trim().is_empty() {
            return Err(AppError::InvalidRequest(
                "workspace id is required".to_owned(),
            ));
        }
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
        if !seen_ids.insert(workspace.id.trim().to_owned()) {
            return Err(AppError::InvalidRequest(format!(
                "duplicate workspace id {}",
                workspace.id
            )));
        }
        let path = validate_workspace_directory_text(workspace_root, &workspace.path)?;
        workspace.path = path.display().to_string();
        normalized.push(workspace);
    }
    Ok(normalized)
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
            .replace(vec![WorkspaceRecord {
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
                service_tier: None,
                local_adapter_state: Some("idle".to_owned()),
                created_at: 10,
                updated_at: 20,
            }])
            .await
            .unwrap();

        assert_eq!(snapshot.workspaces.len(), 1);

        let reloaded = WorkspaceStore::new(root.clone(), workspace_root)
            .await
            .unwrap()
            .snapshot()
            .await;
        assert_eq!(reloaded.workspaces[0].id, "workspace-1");
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
    async fn workspace_store_rejects_duplicate_ids() {
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
            service_tier: None,
            local_adapter_state: None,
            created_at: 10,
            updated_at: 20,
        };

        let error = store
            .replace(vec![workspace.clone(), workspace])
            .await
            .expect_err("duplicate id must fail");

        assert!(matches!(error, AppError::InvalidRequest(_)));
        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nonce = now_millis();
        let path = std::env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
