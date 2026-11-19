use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::warn;

use crate::{
    error::AppError,
    workspace_paths::{canonical_workspace_roots, validate_workspace_directory_text_within},
};

const WORKSPACES_FILE: &str = "workspaces.json";
/// Deleted workspaces remembered per store; the oldest are forgotten first.
const MAX_WORKSPACE_TOMBSTONES: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSnapshot {
    pub workspaces: Vec<WorkspaceRecord>,
    pub updated_at: u64,
    /// Workspaces deleted through `DELETE /v2/workspaces/{id}`. A client that
    /// still holds one pushes it back with its old `updated_at`; the merge
    /// skips it, and clients drop their local copy when they see it here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<WorkspaceTombstone>,
}

/// A deleted workspace: records of it not updated after `deleted_at` stay deleted.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTombstone {
    pub id: String,
    pub path: String,
    pub tenant_id: String,
    pub deleted_at: u64,
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
    /// User-picked sidebar icon key; opaque to the backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// User-picked sidebar icon tint as '#rrggbb'; opaque to the backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_color: Option<String>,
    /// User-picked sidebar status ring style key; opaque to the backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring_style: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<i64>,
    /// Sidebar group id; workspaces sharing it form one group. Opaque to the
    /// backend apart from trimming and length capping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Sidebar group display name, copied onto every member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    /// Sidebar group tags, copied onto every member like `group_name`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_tags: Vec<String>,
    /// The workspace's own sidebar tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A workspace record rejected during normalization (for example because its
/// directory no longer exists on disk).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RejectedWorkspace {
    pub id: String,
    pub name: String,
    pub path: String,
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct WorkspaceMerge {
    pub snapshot: WorkspaceSnapshot,
    pub rejected: Vec<RejectedWorkspace>,
}

#[derive(Default)]
struct NormalizedWorkspaces {
    workspaces: Vec<WorkspaceRecord>,
    rejected: Vec<RejectedWorkspace>,
}

#[derive(Clone)]
pub struct WorkspaceStore {
    path: Arc<PathBuf>,
    workspace_roots: Arc<Vec<PathBuf>>,
    inner: Arc<RwLock<WorkspaceSnapshot>>,
}

impl WorkspaceStore {
    pub async fn new(data_dir: PathBuf, workspace_roots: Vec<PathBuf>) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let path = data_dir.join(WORKSPACES_FILE);
        let snapshot = load_snapshot(&path, &workspace_roots).await?;
        Ok(Self {
            path: Arc::new(path),
            workspace_roots: Arc::new(workspace_roots),
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
            deleted: snapshot
                .deleted
                .iter()
                .filter(|tombstone| tombstone.tenant_id == owner_id)
                .cloned()
                .collect(),
        }
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
    ) -> Result<WorkspaceMerge, AppError> {
        // Every record is checked on disk: off the async workers.
        let roots = self.workspace_roots.clone();
        let owner = owner_id.to_owned();
        let normalized = tokio::task::spawn_blocking(move || {
            normalize_workspaces(workspaces, &roots, Some(&owner))
        })
        .await
        .map_err(|error| AppError::Anyhow(error.into()))?;
        let mut current = self.inner.write().await;
        // Merge into a copy: a merge that is not written leaves the
        // in-memory snapshot (every tenant's records and tombstones) as is.
        let mut by_id = current
            .workspaces
            .iter()
            .cloned()
            .map(|workspace| {
                (
                    (workspace.tenant_id.clone(), workspace.id.clone()),
                    workspace,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut deleted = current.deleted.clone();
        for workspace in normalized.workspaces {
            let key = (workspace.tenant_id.clone(), workspace.id.clone());
            // A client pushing an older copy (it has not pulled the newer one
            // yet) does not undo the newer edit.
            if by_id
                .get(&key)
                .is_some_and(|stored| stored.updated_at > workspace.updated_at)
            {
                continue;
            }
            // A copy of a deleted workspace not edited since stays deleted; a
            // newer one (the workspace was added again) ends the tombstone.
            if let Some(index) = deleted.iter().position(|tombstone| {
                tombstone.tenant_id == workspace.tenant_id && tombstone.id == workspace.id
            }) {
                if workspace.updated_at <= deleted[index].deleted_at {
                    continue;
                }
                deleted.remove(index);
            }
            by_id.insert(key, workspace);
        }
        let mut workspaces = by_id.into_values().collect::<Vec<_>>();
        workspaces.sort_by_key(|workspace| std::cmp::Reverse(workspace.updated_at));
        let snapshot = WorkspaceSnapshot {
            workspaces,
            updated_at: now_millis(),
            deleted,
        };
        write_snapshot(&self.path, &snapshot).await?;
        *current = snapshot.clone();
        drop(current);
        Ok(WorkspaceMerge {
            snapshot: self.snapshot_owned(owner_id).await,
            rejected: normalized.rejected,
        })
    }

    pub async fn delete_owned(&self, owner_id: &str, workspace_id: &str) -> Result<bool, AppError> {
        let mut current = self.inner.write().await;
        let Some(index) = current
            .workspaces
            .iter()
            .position(|workspace| workspace.tenant_id == owner_id && workspace.id == workspace_id)
        else {
            return Ok(false);
        };
        // Changed on a copy and published only once written, so a failed
        // write leaves the workspace in memory as it is on disk.
        let mut next = current.clone();
        let removed = next.workspaces.remove(index);
        let now = now_millis();
        next.deleted
            .retain(|tombstone| tombstone.tenant_id != owner_id || tombstone.id != workspace_id);
        // At least the record's own time, so a copy carrying it stays deleted
        // even when this clock runs behind the client's.
        next.deleted.push(WorkspaceTombstone {
            id: removed.id,
            path: removed.path,
            tenant_id: removed.tenant_id,
            deleted_at: now.max(removed.updated_at),
        });
        if next.deleted.len() > MAX_WORKSPACE_TOMBSTONES {
            next.deleted
                .sort_by_key(|tombstone| std::cmp::Reverse(tombstone.deleted_at));
            next.deleted.truncate(MAX_WORKSPACE_TOMBSTONES);
        }
        next.updated_at = now;
        write_snapshot(&self.path, &next).await?;
        *current = next;
        Ok(true)
    }
}

async fn load_snapshot(
    path: &Path,
    workspace_roots: &[PathBuf],
) -> Result<WorkspaceSnapshot, AppError> {
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
    let normalized = normalize_workspaces(snapshot.workspaces, workspace_roots, None);
    for rejected in &normalized.rejected {
        warn!(
            workspace_id = %rejected.id,
            path = %rejected.path,
            code = %rejected.code,
            "stored workspace record skipped: {}", rejected.message
        );
    }
    snapshot.workspaces = normalized.workspaces;
    snapshot
        .workspaces
        .sort_by_key(|workspace| std::cmp::Reverse(workspace.updated_at));
    Ok(snapshot)
}

async fn write_snapshot(path: &Path, snapshot: &WorkspaceSnapshot) -> Result<(), AppError> {
    let mut bytes = serde_json::to_vec_pretty(snapshot)?;
    bytes.push(b'\n');
    crate::secure_fs::write_owner_only_atomic_async(
        path,
        bytes,
        crate::secure_fs::ParentDir::Create,
    )
    .await?;
    Ok(())
}

fn normalize_workspaces(
    workspaces: Vec<WorkspaceRecord>,
    workspace_roots: &[PathBuf],
    owner_id: Option<&str>,
) -> NormalizedWorkspaces {
    let mut normalized: HashMap<(String, String), WorkspaceRecord> =
        HashMap::with_capacity(workspaces.len());
    let mut rejected = Vec::new();
    let canonical_roots = canonical_workspace_roots(workspace_roots);
    for mut workspace in workspaces {
        let path = match validate_workspace_record(&canonical_roots, &workspace) {
            Ok(path) => path,
            Err(error) => {
                rejected.push(RejectedWorkspace {
                    id: workspace.id,
                    name: workspace.name,
                    path: workspace.path,
                    code: error.code().to_owned(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        workspace.path = path.display().to_string();
        workspace.id = stable_workspace_id(&path);
        if let Some(owner_id) = owner_id {
            workspace.tenant_id = owner_id.to_owned();
        }
        workspace.session_id = format!("cdxs_{}", workspace.id);
        workspace.thread_id.clear();
        workspace.local_adapter_state = None;
        workspace.group_id = normalize_group_field(workspace.group_id.take());
        workspace.group_name = normalize_group_field(workspace.group_name.take());
        workspace.group_tags = normalize_tags(std::mem::take(&mut workspace.group_tags));
        workspace.tags = normalize_tags(std::mem::take(&mut workspace.tags));
        let key = (workspace.tenant_id.clone(), workspace.id.clone());
        match normalized.get(&key) {
            Some(existing) if existing.updated_at > workspace.updated_at => {}
            _ => {
                normalized.insert(key, workspace);
            }
        }
    }
    NormalizedWorkspaces {
        workspaces: normalized.into_values().collect(),
        rejected,
    }
}

/// Max characters kept for `groupId` / `groupName`, matching the protocol.
const WORKSPACE_GROUP_FIELD_MAX: usize = 64;

fn normalize_group_field(value: Option<String>) -> Option<String> {
    let trimmed = value?
        .trim()
        .chars()
        .take(WORKSPACE_GROUP_FIELD_MAX)
        .collect::<String>();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Max characters kept per tag and tags kept per list, matching the protocol.
const WORKSPACE_TAG_MAX: usize = 32;
const WORKSPACE_TAGS_MAX: usize = 16;

/// Trims each tag (dropping control characters), caps its length, drops
/// empties and case-insensitive duplicates (the first spelling wins), and
/// keeps at most `WORKSPACE_TAGS_MAX`.
fn normalize_tags(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut tags = Vec::new();
    for value in values {
        let tag = value
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>()
            .trim()
            .chars()
            .take(WORKSPACE_TAG_MAX)
            .collect::<String>()
            .trim_end()
            .to_owned();
        if tag.is_empty() || !seen.insert(tag.to_lowercase()) {
            continue;
        }
        tags.push(tag);
        if tags.len() == WORKSPACE_TAGS_MAX {
            break;
        }
    }
    tags
}

fn validate_workspace_record(
    canonical_roots: &[PathBuf],
    workspace: &WorkspaceRecord,
) -> Result<PathBuf, AppError> {
    if workspace.name.trim().is_empty() {
        return Err(AppError::InvalidRequest(
            "workspace name is required".to_owned(),
        ));
    }
    validate_workspace_directory_text_within(canonical_roots, &workspace.path)
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
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root.clone()])
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
                    icon: Some("rocket".to_owned()),
                    icon_color: Some("#3b82f6".to_owned()),
                    ring_style: Some("beads".to_owned()),
                    created_at: 10,
                    updated_at: 20,
                    sort_order: Some(3),
                    group_id: Some(" wsg_1 ".to_owned()),
                    group_name: Some("x".repeat(80)),
                    group_tags: vec![" Team ".to_owned(), "team".to_owned()],
                    tags: vec![
                        "Rust".to_owned(),
                        String::new(),
                        "a\u{1}b".to_owned(),
                        "y".repeat(40),
                    ],
                }],
            )
            .await
            .unwrap()
            .snapshot;

        assert_eq!(snapshot.workspaces.len(), 1);

        let reloaded = WorkspaceStore::new(root.clone(), vec![workspace_root])
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
        assert_eq!(reloaded.workspaces[0].sort_order, Some(3));
        assert_eq!(reloaded.workspaces[0].group_id.as_deref(), Some("wsg_1"));
        assert_eq!(
            reloaded.workspaces[0]
                .group_name
                .as_deref()
                .map(|name| name.chars().count()),
            Some(WORKSPACE_GROUP_FIELD_MAX)
        );
        assert_eq!(reloaded.workspaces[0].group_tags, vec!["Team".to_owned()]);
        assert_eq!(
            reloaded.workspaces[0].tags,
            vec![
                "Rust".to_owned(),
                "ab".to_owned(),
                "y".repeat(WORKSPACE_TAG_MAX)
            ]
        );
        assert_eq!(reloaded.workspaces[0].icon.as_deref(), Some("rocket"));
        assert_eq!(
            reloaded.workspaces[0].icon_color.as_deref(),
            Some("#3b82f6")
        );
        assert_eq!(reloaded.workspaces[0].ring_style.as_deref(), Some("beads"));
        assert!(reloaded.updated_at > 0);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_store_merges_by_canonical_path_and_owner() {
        let root = make_temp_dir("todex-workspace-store-duplicate");
        let workspace_root = root.join("workspaces");
        let workspace_path = workspace_root.join("app");
        fs::create_dir_all(&workspace_path).unwrap();
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root])
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
            icon: None,
            icon_color: None,
            ring_style: None,
            created_at: 10,
            updated_at: 20,
            sort_order: None,
            group_id: None,
            group_name: None,
            group_tags: Vec::new(),
            tags: Vec::new(),
        };

        let snapshot = store
            .merge_owned("local", vec![workspace.clone(), workspace.clone()])
            .await
            .unwrap()
            .snapshot;

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
            .unwrap()
            .snapshot;
        assert_eq!(other_snapshot.workspaces.len(), 1);
        assert_eq!(other_snapshot.workspaces[0].tenant_id, "other");
        assert_eq!(store.snapshot_owned("local").await.workspaces.len(), 1);

        let workspace_id = snapshot.workspaces[0].id.clone();
        assert!(store.delete_owned("local", &workspace_id).await.unwrap());
        assert!(store.snapshot_owned("local").await.workspaces.is_empty());
        assert_eq!(store.snapshot_owned("other").await.workspaces.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn merge_skips_missing_workspace_paths_and_reports_rejections() {
        let root = make_temp_dir("todex-workspace-store-partial");
        let workspace_root = root.join("workspaces");
        let workspace_path = workspace_root.join("app");
        fs::create_dir_all(&workspace_path).unwrap();
        let missing = workspace_root.join("gone");
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root])
            .await
            .unwrap();

        let merged = store
            .merge_owned(
                "local",
                vec![
                    test_record("App", &workspace_path),
                    test_record("Gone", &missing),
                ],
            )
            .await
            .unwrap();

        assert_eq!(merged.snapshot.workspaces.len(), 1);
        assert_eq!(merged.rejected.len(), 1);
        assert_eq!(merged.rejected[0].name, "Gone");
        assert_eq!(merged.rejected[0].path, missing.display().to_string());
        assert_eq!(merged.rejected[0].code, "WORKSPACE_PATH_NOT_FOUND");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn merge_reports_rejections_when_every_workspace_is_invalid() {
        let root = make_temp_dir("todex-workspace-store-allbad");
        let workspace_root = root.join("workspaces");
        fs::create_dir_all(&workspace_root).unwrap();
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root.clone()])
            .await
            .unwrap();

        let merged = store
            .merge_owned(
                "local",
                vec![test_record("Gone", &workspace_root.join("missing"))],
            )
            .await
            .unwrap();

        assert_eq!(merged.rejected.len(), 1);
        assert_eq!(merged.rejected[0].code, "WORKSPACE_PATH_NOT_FOUND");
        assert!(merged.snapshot.workspaces.is_empty());
        assert!(store.snapshot_owned("local").await.workspaces.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn load_skips_stored_workspaces_with_missing_paths() {
        let root = make_temp_dir("todex-workspace-store-stale");
        let workspace_root = root.join("workspaces");
        let workspace_path = workspace_root.join("app");
        let stale_path = workspace_root.join("stale");
        fs::create_dir_all(&workspace_path).unwrap();
        fs::create_dir_all(&stale_path).unwrap();
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root.clone()])
            .await
            .unwrap();
        store
            .merge_owned(
                "local",
                vec![
                    test_record("App", &workspace_path),
                    test_record("Stale", &stale_path),
                ],
            )
            .await
            .unwrap();
        drop(store);
        fs::remove_dir_all(&stale_path).unwrap();

        let reloaded = WorkspaceStore::new(root.clone(), vec![workspace_root])
            .await
            .unwrap()
            .snapshot_owned("local")
            .await;

        assert_eq!(reloaded.workspaces.len(), 1);
        assert_eq!(reloaded.workspaces[0].name, "App");
        let _ = fs::remove_dir_all(root);
    }

    fn test_record(name: &str, path: &Path) -> WorkspaceRecord {
        WorkspaceRecord {
            id: format!("client-{name}"),
            name: name.to_owned(),
            path: path.display().to_string(),
            session_id: String::new(),
            tenant_id: "local".to_owned(),
            thread_id: String::new(),
            model: "gpt-5.5".to_owned(),
            reasoning_effort: None,
            approval_policy: "on-request".to_owned(),
            sandbox_mode: "workspace-write".to_owned(),
            permission_profile: None,
            approvals_reviewer: None,
            service_tier: None,
            local_adapter_state: None,
            icon: None,
            icon_color: None,
            ring_style: None,
            created_at: 10,
            updated_at: 20,
            sort_order: None,
            group_id: None,
            group_name: None,
            group_tags: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn record(path: &Path, name: &str, updated_at: u64) -> WorkspaceRecord {
        WorkspaceRecord {
            id: String::new(),
            name: name.to_owned(),
            path: path.display().to_string(),
            session_id: String::new(),
            tenant_id: "local".to_owned(),
            thread_id: String::new(),
            model: String::new(),
            reasoning_effort: None,
            approval_policy: "on-request".to_owned(),
            sandbox_mode: "workspace-write".to_owned(),
            permission_profile: None,
            approvals_reviewer: None,
            service_tier: None,
            local_adapter_state: None,
            icon: None,
            icon_color: None,
            ring_style: None,
            created_at: 1,
            updated_at,
            sort_order: None,
            group_id: None,
            group_name: None,
            group_tags: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn an_older_copy_does_not_undo_a_newer_edit() {
        let root = make_temp_dir("todex-workspace-store-newer");
        let workspace_root = root.join("workspaces");
        let path = workspace_root.join("app");
        fs::create_dir_all(&path).unwrap();
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root])
            .await
            .unwrap();
        store
            .merge_owned("local", vec![record(&path, "Renamed on the phone", 200)])
            .await
            .unwrap();
        // A desktop that has not pulled yet pushes its stale copy.
        let snapshot = store
            .merge_owned("local", vec![record(&path, "Old name", 100)])
            .await
            .unwrap()
            .snapshot;
        assert_eq!(snapshot.workspaces[0].name, "Renamed on the phone");
        // An equal time is the same edit sent again; a newer one wins.
        let snapshot = store
            .merge_owned("local", vec![record(&path, "Newer", 300)])
            .await
            .unwrap()
            .snapshot;
        assert_eq!(snapshot.workspaces[0].name, "Newer");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn a_deleted_workspace_stays_deleted_until_added_again() {
        let root = make_temp_dir("todex-workspace-store-tombstone");
        let workspace_root = root.join("workspaces");
        let path = workspace_root.join("app");
        fs::create_dir_all(&path).unwrap();
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root.clone()])
            .await
            .unwrap();
        let id = store
            .merge_owned("local", vec![record(&path, "App", 100)])
            .await
            .unwrap()
            .snapshot
            .workspaces[0]
            .id
            .clone();
        assert!(store.delete_owned("local", &id).await.unwrap());
        let deleted = store.snapshot_owned("local").await.deleted;
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].id, id);
        assert!(store.snapshot_owned("other").await.deleted.is_empty());

        // A client still holding the old copy pushes it back: it stays deleted.
        let snapshot = store
            .merge_owned("local", vec![record(&path, "App", 100)])
            .await
            .unwrap()
            .snapshot;
        assert!(snapshot.workspaces.is_empty());
        assert_eq!(snapshot.deleted.len(), 1);

        // The tombstone survives a restart.
        let reloaded = WorkspaceStore::new(root.clone(), vec![workspace_root])
            .await
            .unwrap();
        assert_eq!(reloaded.snapshot_owned("local").await.deleted.len(), 1);

        // Added again later: the newer record wins and the tombstone goes.
        let later = deleted[0].deleted_at + 1;
        let snapshot = reloaded
            .merge_owned("local", vec![record(&path, "App again", later)])
            .await
            .unwrap()
            .snapshot;
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].name, "App again");
        assert!(snapshot.deleted.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    /// A merge or deletion whose write fails changes nothing in memory: not
    /// the caller's records, not another tenant's, not the tombstones.
    #[tokio::test]
    async fn a_failed_write_leaves_the_snapshot_as_it_was() {
        let root = make_temp_dir("todex-workspace-store-failed-write");
        let workspace_root = root.join("workspaces");
        let (app, docs, gone) = (
            workspace_root.join("app"),
            workspace_root.join("docs"),
            workspace_root.join("gone"),
        );
        for path in [&app, &docs, &gone] {
            fs::create_dir_all(path).unwrap();
        }
        let store = WorkspaceStore::new(root.clone(), vec![workspace_root])
            .await
            .unwrap();
        let ids = store
            .merge_owned(
                "local",
                vec![record(&app, "App", 100), record(&gone, "Gone", 100)],
            )
            .await
            .unwrap()
            .snapshot
            .workspaces
            .into_iter()
            .map(|workspace| (workspace.name, workspace.id))
            .collect::<HashMap<_, _>>();
        store
            .merge_owned("other", vec![record(&docs, "Docs", 100)])
            .await
            .unwrap();
        assert!(store.delete_owned("local", &ids["Gone"]).await.unwrap());
        let before = (
            store.snapshot_owned("local").await,
            store.snapshot_owned("other").await,
        );

        // The snapshot file cannot be replaced: renaming onto a directory fails.
        let file = root.join(WORKSPACES_FILE);
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(store
            .merge_owned("local", vec![record(&app, "Renamed", 200)])
            .await
            .is_err());
        assert!(store.delete_owned("local", &ids["App"]).await.is_err());
        let after = (
            store.snapshot_owned("local").await,
            store.snapshot_owned("other").await,
        );
        assert_eq!(after, before);

        // The next successful write still carries everything.
        fs::remove_dir(&file).unwrap();
        store
            .merge_owned("local", vec![record(&app, "Renamed", 200)])
            .await
            .unwrap();
        let reloaded = WorkspaceStore::new(root.clone(), vec![root.join("workspaces")])
            .await
            .unwrap();
        let local = reloaded.snapshot_owned("local").await;
        assert_eq!(local.workspaces.len(), 1);
        assert_eq!(local.workspaces[0].name, "Renamed");
        assert_eq!(local.deleted.len(), 1);
        assert_eq!(reloaded.snapshot_owned("other").await.workspaces.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nonce = now_millis();
        let path = std::env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
