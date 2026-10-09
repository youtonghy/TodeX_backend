use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
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
const KANBAN_CONVERSATION_LIMIT: usize = 100;
const KANBAN_ID_LIMIT: usize = 128;
const KANBAN_SCHEDULE_TEXT_LIMIT: usize = 20_000;
const KANBAN_SCHEDULE_FIELD_LIMIT: usize = 200;
const KANBAN_SCHEDULE_ERROR_LIMIT: usize = 500;
/// `KanbanTaskSchedule::at` layout.
pub const KANBAN_SCHEDULE_TIME_FORMAT: &str = "%Y-%m-%dT%H:%M";

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
    /// Conversations started for the task, in display order; `conversationId`
    /// mirrors the first one for clients that only know the single field.
    /// Absent (not empty) from such clients, which then keep the stored list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_ids: Option<Vec<String>>,
    /// Manual order within the workspace status group; opaque to the backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<serde_json::Number>,
    /// Timed start/send run by the backend scheduler (`kanban_scheduler`).
    /// Clients cancel it through `status` rather than dropping the field, so
    /// an absent schedule always means a client that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<KanbanTaskSchedule>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum KanbanScheduleAction {
    /// Start a new conversation for the task and send `text` as its prompt.
    Start,
    /// Send `text` to the task's existing `conversationId`, queued behind a
    /// running turn.
    Send,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum KanbanScheduleStatus {
    Pending,
    /// Claimed by the scheduler; only the backend sets it.
    Running,
    Done,
    Failed,
    Cancelled,
}

impl KanbanScheduleStatus {
    /// States only the scheduler produces; a client write never replaces them.
    fn backend_owned(self) -> bool {
        matches!(self, Self::Running | Self::Done | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KanbanTaskSchedule {
    /// Client-chosen id; also the idempotency key of the prompt it sends.
    pub id: String,
    /// Wall-clock time in the backend's local time zone, `YYYY-MM-DDTHH:MM`.
    pub at: String,
    pub action: KanbanScheduleAction,
    /// Backend conversation id the `send` action targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_mode: Option<String>,
    pub status: KanbanScheduleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired_at: Option<u64>,
    /// Conversation the run started or sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_conversation_id: Option<String>,
    /// Turn the prompt started; absent when it was queued behind a running one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A pending schedule whose time has come, as reported by
/// [`KanbanTaskStore::due_schedules`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueKanbanSchedule {
    pub tenant_id: String,
    pub task_id: String,
    pub schedule_id: String,
    pub due_at: DateTime<Utc>,
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
                Some(existing) => {
                    let merged = merge_incoming(existing, task);
                    by_id.insert(key, merged);
                }
                None => {
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

    /// Pending schedules of live tasks whose time is at or before `now`.
    pub async fn due_schedules(&self, now: DateTime<Utc>) -> Vec<DueKanbanSchedule> {
        let snapshot = self.inner.read().await;
        snapshot
            .tasks
            .iter()
            .filter(|task| task.deleted_at.is_none())
            .filter_map(|task| {
                let schedule = task.schedule.as_ref()?;
                if schedule.status != KanbanScheduleStatus::Pending {
                    return None;
                }
                let due_at = schedule_instant(&schedule.at)?;
                (due_at <= now).then(|| DueKanbanSchedule {
                    tenant_id: task.tenant_id.clone(),
                    task_id: task.id.clone(),
                    schedule_id: schedule.id.clone(),
                    due_at,
                })
            })
            .collect()
    }

    /// Schedules a previous process claimed but never finished.
    pub async fn running_schedules(&self) -> Vec<(String, String, String)> {
        let snapshot = self.inner.read().await;
        snapshot
            .tasks
            .iter()
            .filter(|task| task.deleted_at.is_none())
            .filter_map(|task| {
                let schedule = task.schedule.as_ref()?;
                (schedule.status == KanbanScheduleStatus::Running)
                    .then(|| (task.tenant_id.clone(), task.id.clone(), schedule.id.clone()))
            })
            .collect()
    }

    /// Applies a scheduler-side change to the task carrying schedule
    /// `schedule_id` and persists it when `update` reports a change. The
    /// write gets an `updatedAt` newer than any copy a client could hold, so
    /// it wins the next merge. Returns the updated task, or `None` when the
    /// task is gone or carries another schedule by now.
    pub async fn update_schedule<F>(
        &self,
        tenant_id: &str,
        task_id: &str,
        schedule_id: &str,
        update: F,
    ) -> Result<Option<KanbanTaskRecord>, AppError>
    where
        F: FnOnce(&mut KanbanTaskRecord) -> bool,
    {
        let mut current = self.inner.write().await;
        let Some(index) = current.tasks.iter().position(|task| {
            task.tenant_id == tenant_id
                && task.id == task_id
                && task.deleted_at.is_none()
                && task
                    .schedule
                    .as_ref()
                    .is_some_and(|schedule| schedule.id == schedule_id)
        }) else {
            return Ok(None);
        };
        let mut task = current.tasks[index].clone();
        if !update(&mut task) {
            return Ok(Some(task));
        }
        let now = now_millis();
        task.updated_at = now.max(task.updated_at + 1);
        let mut next = current.clone();
        next.tasks[index] = task.clone();
        next.updated_at = now;
        write_snapshot(&self.path, &next).await?;
        *current = next;
        Ok(Some(task))
    }
}

/// The instant `at` (backend local wall time) names. A time skipped by a DST
/// jump runs an hour later; an ambiguous one at its first occurrence.
pub fn schedule_instant(at: &str) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(at, KANBAN_SCHEDULE_TIME_FORMAT).ok()?;
    let local = Local.from_local_datetime(&naive).earliest().or_else(|| {
        Local
            .from_local_datetime(&(naive + chrono::Duration::hours(1)))
            .earliest()
    })?;
    Some(local.with_timezone(&Utc))
}

/// Merges a client write that won on `updatedAt` into the stored record.
/// Fields an older client does not know (absent conversation list, sort
/// order, schedule) keep their stored value, and scheduler-owned schedule
/// state is never rolled back by a client copy made before the run.
fn merge_incoming(existing: &KanbanTaskRecord, mut task: KanbanTaskRecord) -> KanbanTaskRecord {
    if task.conversation_ids.is_none() {
        if let Some(stored) = &existing.conversation_ids {
            let mut ids = stored.clone();
            if let Some(id) = &task.conversation_id {
                if !ids.contains(id) {
                    ids.insert(0, id.clone());
                }
            }
            task.conversation_ids = Some(ids);
        }
    }
    if task.sort_order.is_none() {
        task.sort_order = existing.sort_order.clone();
    }
    match (&existing.schedule, &task.schedule) {
        (Some(stored), None) => task.schedule = Some(stored.clone()),
        (Some(stored), Some(incoming))
            if stored.status.backend_owned()
                && (incoming.id == stored.id || stored.status == KanbanScheduleStatus::Running) =>
        {
            // The client never saw the run: keep its result, including the
            // conversation the run started.
            if incoming.id == stored.id && !incoming.status.backend_owned() {
                if let Some(id) = &stored.result_conversation_id {
                    let ids = task.conversation_ids.get_or_insert_with(Vec::new);
                    if !ids.contains(id) {
                        ids.push(id.clone());
                    }
                }
            }
            task.schedule = Some(stored.clone());
        }
        _ => {}
    }
    if let Some(ids) = &task.conversation_ids {
        task.conversation_id = ids.first().cloned();
    }
    task
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
        if let Some(ids) = task.conversation_ids.take() {
            let mut unique: Vec<String> = Vec::with_capacity(ids.len());
            for id in ids {
                let id = id.trim().to_owned();
                if !id.is_empty() && !unique.contains(&id) {
                    unique.push(id);
                }
            }
            if unique.len() > KANBAN_CONVERSATION_LIMIT
                || unique.iter().any(|id| id.len() > KANBAN_ID_LIMIT)
            {
                return Err(AppError::InvalidRequest(format!(
                    "kanban task conversationIds must hold at most {KANBAN_CONVERSATION_LIMIT} ids of at most {KANBAN_ID_LIMIT} bytes"
                )));
            }
            // An explicit empty list clears the conversations.
            task.conversation_id = unique.first().cloned();
            task.conversation_ids = Some(unique);
        }
        if let Some(schedule) = task.schedule.as_mut() {
            normalize_schedule(schedule)?;
        }
        if !owner_id.is_empty() {
            task.tenant_id = owner_id.to_owned();
        }
        normalized.push(task);
    }
    Ok(normalized)
}

fn normalize_schedule(schedule: &mut KanbanTaskSchedule) -> Result<(), AppError> {
    let invalid = |message: &str| {
        Err(AppError::InvalidRequest(format!(
            "kanban task schedule {message}"
        )))
    };
    schedule.id = schedule.id.trim().to_owned();
    if schedule.id.is_empty()
        || schedule.id.len() > KANBAN_ID_LIMIT
        || !schedule
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return invalid("id must be 1-128 characters of [A-Za-z0-9_-]");
    }
    schedule.at = schedule.at.trim().to_owned();
    if NaiveDateTime::parse_from_str(&schedule.at, KANBAN_SCHEDULE_TIME_FORMAT).is_err() {
        return invalid("at must be YYYY-MM-DDTHH:MM");
    }
    schedule.text = schedule.text.trim().to_owned();
    if schedule.text.is_empty() || schedule.text.chars().count() > KANBAN_SCHEDULE_TEXT_LIMIT {
        return invalid(&format!(
            "text must be 1-{KANBAN_SCHEDULE_TEXT_LIMIT} characters"
        ));
    }
    for field in [
        &mut schedule.conversation_id,
        &mut schedule.provider,
        &mut schedule.provider_profile,
        &mut schedule.model,
        &mut schedule.reasoning_effort,
        &mut schedule.permission_mode,
        &mut schedule.work_mode,
        &mut schedule.result_conversation_id,
        &mut schedule.turn_id,
    ] {
        *field = field
            .take()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if field
            .as_ref()
            .is_some_and(|value| value.chars().count() > KANBAN_SCHEDULE_FIELD_LIMIT)
        {
            return invalid(&format!(
                "fields must be at most {KANBAN_SCHEDULE_FIELD_LIMIT} characters"
            ));
        }
    }
    if schedule.action == KanbanScheduleAction::Send && schedule.conversation_id.is_none() {
        return invalid("send needs a conversationId");
    }
    schedule.error = schedule
        .error
        .take()
        .map(|value| value.chars().take(KANBAN_SCHEDULE_ERROR_LIMIT).collect());
    Ok(())
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
        task.deleted_at.is_none_or(|deleted_at| {
            now.saturating_sub(deleted_at) <= KANBAN_TOMBSTONE_RETENTION_MILLIS
        })
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
            conversation_ids: None,
            sort_order: None,
            schedule: None,
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
        assert!(store.snapshot_owned("local").await.tasks[0]
            .deleted_at
            .is_some());

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
        assert!(store
            .merge_owned("local", vec![blank_title.clone()])
            .await
            .is_err());
        blank_title.title = "x".repeat(201);
        assert!(store.merge_owned("local", vec![blank_title]).await.is_err());

        let mut bad_status = task("task-2", "ok", 10);
        bad_status.status = "archived".to_owned();
        assert!(store.merge_owned("local", vec![bad_status]).await.is_err());

        let mut bad_due_date = task("task-3", "ok", 10);
        bad_due_date.due_date = Some("13/01/2026".to_owned());
        assert!(store
            .merge_owned("local", vec![bad_due_date])
            .await
            .is_err());

        let mut bad_workspace = task("task-4", "ok", 10);
        bad_workspace.workspace_id = "  ".to_owned();
        assert!(store
            .merge_owned("local", vec![bad_workspace])
            .await
            .is_err());
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

    fn schedule(id: &str, status: KanbanScheduleStatus) -> KanbanTaskSchedule {
        KanbanTaskSchedule {
            id: id.to_owned(),
            at: "2026-10-09T09:30".to_owned(),
            action: KanbanScheduleAction::Start,
            conversation_id: None,
            text: "do it".to_owned(),
            provider: None,
            provider_profile: None,
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            work_mode: None,
            status,
            fired_at: None,
            result_conversation_id: None,
            turn_id: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn kanban_store_keeps_fields_older_clients_omit() {
        let root = make_temp_dir("todex-kanban-legacy-client");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        let mut current = task("task-1", "t", 10);
        current.conversation_ids = Some(vec!["c1".to_owned(), "c2".to_owned()]);
        current.sort_order = Some(3.into());
        current.schedule = Some(schedule("s1", KanbanScheduleStatus::Pending));
        store.merge_owned("local", vec![current]).await.unwrap();

        // A client that knows only `conversationId` renames the task.
        let mut legacy = task("task-1", "renamed", 20);
        legacy.conversation_id = Some("c1".to_owned());
        let snapshot = store.merge_owned("local", vec![legacy]).await.unwrap();
        let merged = &snapshot.tasks[0];
        assert_eq!(merged.title, "renamed");
        assert_eq!(
            merged.conversation_ids,
            Some(vec!["c1".to_owned(), "c2".to_owned()])
        );
        assert_eq!(merged.conversation_id.as_deref(), Some("c1"));
        assert_eq!(merged.sort_order, Some(3.into()));
        assert_eq!(merged.schedule.as_ref().unwrap().id, "s1");

        // An explicit empty list clears the conversations.
        let mut cleared = merged.clone();
        cleared.updated_at = 30;
        cleared.conversation_ids = Some(Vec::new());
        let snapshot = store.merge_owned("local", vec![cleared]).await.unwrap();
        assert_eq!(snapshot.tasks[0].conversation_ids, Some(Vec::new()));
        assert_eq!(snapshot.tasks[0].conversation_id, None);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn kanban_store_never_rolls_back_a_schedule_run() {
        let root = make_temp_dir("todex-kanban-schedule-merge");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        let mut pending = task("task-1", "t", 10);
        pending.schedule = Some(schedule("s1", KanbanScheduleStatus::Pending));
        store
            .merge_owned("local", vec![pending.clone()])
            .await
            .unwrap();

        store
            .update_schedule("local", "task-1", "s1", |task| {
                let schedule = task.schedule.as_mut().unwrap();
                schedule.status = KanbanScheduleStatus::Done;
                schedule.result_conversation_id = Some("conv_new".to_owned());
                task.conversation_ids = Some(vec!["conv_new".to_owned()]);
                true
            })
            .await
            .unwrap()
            .unwrap();

        // A client edit made before it saw the run, stamped later than it.
        let mut stale = pending.clone();
        stale.title = "edited".to_owned();
        stale.updated_at = u64::MAX / 2;
        let snapshot = store.merge_owned("local", vec![stale]).await.unwrap();
        let merged = &snapshot.tasks[0];
        assert_eq!(merged.title, "edited");
        let kept = merged.schedule.as_ref().unwrap();
        assert_eq!(kept.status, KanbanScheduleStatus::Done);
        assert_eq!(kept.result_conversation_id.as_deref(), Some("conv_new"));
        assert_eq!(merged.conversation_ids, Some(vec!["conv_new".to_owned()]));

        // A new schedule replaces a finished one.
        let mut next = merged.clone();
        next.updated_at = u64::MAX / 2 + 1;
        next.schedule = Some(schedule("s2", KanbanScheduleStatus::Pending));
        let snapshot = store.merge_owned("local", vec![next]).await.unwrap();
        assert_eq!(snapshot.tasks[0].schedule.as_ref().unwrap().id, "s2");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn kanban_store_reports_due_schedules_and_validates_them() {
        let root = make_temp_dir("todex-kanban-due");
        let store = KanbanTaskStore::new(root.clone()).await.unwrap();
        let mut past = task("task-past", "t", 10);
        past.schedule = Some(schedule("s-past", KanbanScheduleStatus::Pending));
        let mut future = task("task-future", "t", 10);
        let mut later = schedule("s-future", KanbanScheduleStatus::Pending);
        later.at = "2999-01-01T00:00".to_owned();
        future.schedule = Some(later);
        let mut cancelled = task("task-cancelled", "t", 10);
        cancelled.schedule = Some(schedule("s-cancelled", KanbanScheduleStatus::Cancelled));
        store
            .merge_owned("local", vec![past, future, cancelled])
            .await
            .unwrap();
        let due = store.due_schedules(Utc::now()).await;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].schedule_id, "s-past");

        for broken in [
            KanbanTaskSchedule {
                at: "2026-10-09 09:30".to_owned(),
                ..schedule("s", KanbanScheduleStatus::Pending)
            },
            KanbanTaskSchedule {
                id: "bad id".to_owned(),
                ..schedule("s", KanbanScheduleStatus::Pending)
            },
            KanbanTaskSchedule {
                text: "  ".to_owned(),
                ..schedule("s", KanbanScheduleStatus::Pending)
            },
            KanbanTaskSchedule {
                action: KanbanScheduleAction::Send,
                ..schedule("s", KanbanScheduleStatus::Pending)
            },
        ] {
            let mut record = task("task-broken", "t", 10);
            record.schedule = Some(broken);
            assert!(store.merge_owned("local", vec![record]).await.is_err());
        }
        let _ = fs::remove_dir_all(root);
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nonce = now_millis();
        let path = std::env::temp_dir().join(format!("{prefix}-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
