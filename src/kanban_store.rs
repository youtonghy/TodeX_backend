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

use crate::error::AppError;

const KANBAN_TASKS_FILE: &str = "kanban_tasks.json";
const KANBAN_TASK_TITLE_LIMIT: usize = 200;
const KANBAN_TASK_DESCRIPTION_LIMIT: usize = 2000;
const KANBAN_TASK_LIMIT_PER_TENANT: usize = 500;
const KANBAN_TOMBSTONE_RETENTION_MILLIS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KanbanTaskSnapshot {
    pub tasks: Vec<KanbanTaskRecord>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KanbanTaskRecord {
    pub id: String,
    #[serde(default)]
    pub tenant_id: String,
    pub workspace_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
}

#[derive(Clone)]
pub struct KanbanTaskStore {
    path: Arc<PathBuf>,
    inner: Arc<RwLock<KanbanTaskSnapshot>>,
}

impl KanbanTaskStore {
    pub async fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        tokio::fs::create_dir_all(&data_dir).await?;
        let path = data_dir.join(KANBAN_TASKS_FILE);
        let snapshot = load_snapshot(&path).await?;
        Ok(Self {
            path: Arc::new(path),
            inner: Arc::new(RwLock::new(snapshot)),
        })
    }

    /// Returns the tenant's records including tombstones: deletion only
    /// propagates to other devices when `deletedAt` survives the round trip.
    pub async fn snapshot_owned(&self, owner_id: &str) -> KanbanTaskSnapshot {
        let snapshot = self.inner.read().await;
        KanbanTaskSnapshot {
            tasks: snapshot
                .tasks
                .iter()
                .filter(|task| task.tenant_id == owner_id)
                .cloned()
                .collect(),
            updated_at: snapshot.updated_at,
        }
    }

    /// Upserts records by (tenant, id), keeping the write with the newest
    /// `updatedAt`; ties resolve to the incoming record so concurrent editors
    /// converge. `tenant_id` from the client is always replaced by the
    /// authenticated owner.
    pub async fn merge_owned(
        &self,
        owner_id: &str,
        tasks: Vec<KanbanTaskRecord>,
    ) -> Result<KanbanTaskSnapshot, AppError> {
        let incoming = normalize_tasks(tasks, owner_id)?;
        let now = now_millis();
        let mut current = self.inner.write().await;
        let mut by_id = current
            .tasks
            .drain(..)
            .map(|task| ((task.tenant_id.clone(), task.id.clone()), task))
            .collect::<HashMap<_, _>>();
        for task in incoming {
            let key = (task.tenant_id.clone(), task.id.clone());
            match by_id.get(&key) {
                Some(existing) if existing.updated_at > task.updated_at => {}
                _ => {
                    by_id.insert(key, task);
                }
            }
        }
        let mut tasks = by_id.into_values().collect::<Vec<_>>();
        tasks.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        prune_tombstones(&mut tasks, now);
        enforce_task_limit(&mut tasks, owner_id)?;
        let snapshot = KanbanTaskSnapshot {
            tasks,
            updated_at: now,
        };
        write_snapshot(&self.path, &snapshot).await?;
        *current = snapshot;
        drop(current);
        Ok(self.snapshot_owned(owner_id).await)
    }
}

async fn load_snapshot(path: &Path) -> Result<KanbanTaskSnapshot, AppError> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(KanbanTaskSnapshot::default());
        }
        Err(error) => return Err(error.into()),
    };

    if text.trim().is_empty() {
        return Ok(KanbanTaskSnapshot::default());
    }

    let mut snapshot: KanbanTaskSnapshot = serde_json::from_str(&text)?;
    let now = now_millis();
    let mut tasks = Vec::with_capacity(snapshot.tasks.len());
    for task in snapshot.tasks.drain(..) {
        // A record that fails validation is skipped rather than failing the
        // whole file, so one corrupt entry cannot wedge every client's sync.
        match normalize_tasks(vec![task], "") {
            Ok(mut normalized) => tasks.append(&mut normalized),
            Err(_) => continue,
        }
    }
    tasks.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    prune_tombstones(&mut tasks, now);
    snapshot.tasks = tasks;
    Ok(snapshot)
}

async fn write_snapshot(path: &Path, snapshot: &KanbanTaskSnapshot) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp_path = path.with_file_name(format!(".kanban-tasks.{}.tmp", Uuid::new_v4().simple()));
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

const VALID_STATUSES: [&str; 3] = ["planned", "in-progress", "done"];

fn normalize_tasks(
    tasks: Vec<KanbanTaskRecord>,
    owner_id: &str,
) -> Result<Vec<KanbanTaskRecord>, AppError> {
    let mut normalized = Vec::with_capacity(tasks.len());
    for mut task in tasks {
        task.id = task.id.trim().to_owned();
        task.workspace_id = task.workspace_id.trim().to_owned();
        task.title = task.title.trim().to_owned();
        if task.id.is_empty() || task.workspace_id.is_empty() {
            return Err(AppError::InvalidRequest(
                "kanban task id and workspaceId are required".to_owned(),
            ));
        }
        if task.title.is_empty() || task.title.chars().count() > KANBAN_TASK_TITLE_LIMIT {
            return Err(AppError::InvalidRequest(format!(
                "kanban task title must be 1-{KANBAN_TASK_TITLE_LIMIT} characters"
            )));
        }
        task.description = task
            .description
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if task
            .description
            .as_ref()
            .is_some_and(|value| value.chars().count() > KANBAN_TASK_DESCRIPTION_LIMIT)
        {
            return Err(AppError::InvalidRequest(format!(
                "kanban task description must be at most {KANBAN_TASK_DESCRIPTION_LIMIT} characters"
            )));
        }
        if !VALID_STATUSES.contains(&task.status.as_str()) {
            return Err(AppError::InvalidRequest(format!(
                "kanban task status must be one of {VALID_STATUSES:?}"
            )));
        }
        task.due_date = task
            .due_date
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if let Some(due_date) = &task.due_date {
            if !is_valid_due_date(due_date) {
                return Err(AppError::InvalidRequest(
                    "kanban task dueDate must be YYYY-MM-DD".to_owned(),
                ));
            }
        }
        task.conversation_id = task
            .conversation_id
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if !owner_id.is_empty() {
            task.tenant_id = owner_id.to_owned();
        }
        normalized.push(task);
    }
    Ok(normalized)
}

fn is_valid_due_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn prune_tombstones(tasks: &mut Vec<KanbanTaskRecord>, now: u64) {
    tasks.retain(|task| {
        task.deleted_at
            .is_none_or(|deleted_at| now.saturating_sub(deleted_at) <= KANBAN_TOMBSTONE_RETENTION_MILLIS)
    });
}

fn enforce_task_limit(tasks: &[KanbanTaskRecord], owner_id: &str) -> Result<(), AppError> {
    let active = tasks
        .iter()
        .filter(|task| task.tenant_id == owner_id && task.deleted_at.is_none())
        .count();
    if active > KANBAN_TASK_LIMIT_PER_TENANT {
        return Err(AppError::InvalidRequest(format!(
            "kanban task limit of {KANBAN_TASK_LIMIT_PER_TENANT} reached"
        )));
    }
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
    use std::fs;

    fn task(id: &str, title: &str, updated_at: u64) -> KanbanTaskRecord {
        KanbanTaskRecord {
            id: id.to_owned(),
            tenant_id: String::new(),
            workspace_id: "ws_1".to_owned(),
            title: title.to_owned(),
            description: None,
            due_date: None,
            status: "planned".to_owned(),
            conversation_id: None,
            created_at: updated_at,
            updated_at,
            deleted_at: None,
        }
    }

    #[tokio::test]
    async fn kanban_store_persists_and_reloads_snapshot() {
        let root = make_temp_dir("todex-kanban-store");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        let snapshot = store
            .merge_owned("local", vec![task("task-1", "ship it", 10)])
            .await
            .unwrap();
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].tenant_id, "local");

        let reloaded = KanbanTaskStore::new(root.clone())
            .await
            .unwrap()
            .snapshot_owned("local")
            .await;
        assert_eq!(reloaded.tasks.len(), 1);
        assert_eq!(reloaded.tasks[0].title, "ship it");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn kanban_store_merges_by_updated_at_and_forces_tenant() {
        let root = make_temp_dir("todex-kanban-merge");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        store
            .merge_owned("local", vec![task("task-1", "old", 10)])
            .await
            .unwrap();

        let mut stale = task("task-1", "stale write", 5);
        stale.status = "done".to_owned();
        store.merge_owned("local", vec![stale]).await.unwrap();
        let snapshot = store.snapshot_owned("local").await;
        assert_eq!(snapshot.tasks[0].title, "old");
        assert_eq!(snapshot.tasks[0].status, "planned");

        let mut fresh = task("task-1", "new write", 20);
        fresh.tenant_id = "spoofed".to_owned();
        fresh.status = "done".to_owned();
        let snapshot = store.merge_owned("local", vec![fresh]).await.unwrap();
        assert_eq!(snapshot.tasks[0].title, "new write");
        assert_eq!(snapshot.tasks[0].status, "done");
        assert_eq!(snapshot.tasks[0].tenant_id, "local");
        assert_eq!(store.snapshot_owned("spoofed").await.tasks.len(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn kanban_store_propagates_tombstones_and_prunes_old_ones() {
        let root = make_temp_dir("todex-kanban-tombstone");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        store
            .merge_owned("local", vec![task("task-1", "gone", 10)])
            .await
            .unwrap();

        let mut tombstone = task("task-1", "gone", now_millis());
        tombstone.deleted_at = Some(now_millis());
        let snapshot = store.merge_owned("local", vec![tombstone]).await.unwrap();
        assert!(snapshot.tasks[0].deleted_at.is_some());

        // An older write cannot resurrect the deleted record.
        store
            .merge_owned("local", vec![task("task-1", "revive", 10)])
            .await
            .unwrap();
        assert!(store.snapshot_owned("local").await.tasks[0].deleted_at.is_some());

        // Tombstones older than the retention window are dropped.
        let mut expired = task("task-2", "expired", 10);
        expired.deleted_at = Some(now_millis() - KANBAN_TOMBSTONE_RETENTION_MILLIS - 1);
        store.merge_owned("local", vec![expired]).await.unwrap();
        let snapshot = store.snapshot_owned("local").await;
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].id, "task-1");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn kanban_store_rejects_invalid_records() {
        let root = make_temp_dir("todex-kanban-invalid");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();

        let mut blank_title = task("task-1", "   ", 10);
        assert!(store.merge_owned("local", vec![blank_title.clone()]).await.is_err());
        blank_title.title = "x".repeat(201);
        assert!(store.merge_owned("local", vec![blank_title]).await.is_err());

        let mut bad_status = task("task-2", "ok", 10);
        bad_status.status = "archived".to_owned();
        assert!(store.merge_owned("local", vec![bad_status]).await.is_err());

        let mut bad_due_date = task("task-3", "ok", 10);
        bad_due_date.due_date = Some("13/01/2026".to_owned());
        assert!(store.merge_owned("local", vec![bad_due_date]).await.is_err());

        let mut bad_workspace = task("task-4", "ok", 10);
        bad_workspace.workspace_id = "  ".to_owned();
        assert!(store.merge_owned("local", vec![bad_workspace]).await.is_err());
        assert!(store.snapshot_owned("local").await.tasks.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn kanban_store_enforces_per_tenant_limit() {
        let root = make_temp_dir("todex-kanban-limit");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        let batch = (0..KANBAN_TASK_LIMIT_PER_TENANT)
            .map(|index| task(&format!("task-{index}"), "task", index as u64))
            .collect::<Vec<_>>();
        store.merge_owned("local", batch).await.unwrap();
        let overflow = task("task-overflow", "overflow", 10_000);
        assert!(store.merge_owned("local", vec![overflow]).await.is_err());

        // The limit applies per tenant, not globally.
        store
            .merge_owned("other", vec![task("task-other", "ok", 1)])
            .await
            .unwrap();
        assert_eq!(store.snapshot_owned("other").await.tasks.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nonce = now_millis();
        let path = std::env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
