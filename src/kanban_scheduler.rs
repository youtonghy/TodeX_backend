//! Runs kanban task schedules: at the task's wall-clock time (backend local
//! time zone) it starts a new conversation for the task or sends to one of
//! its conversations, without any client connected.
//!
//! A run claims the schedule (`running`) before touching a conversation and
//! records the outcome on the task, which clients pick up through the
//! regular kanban sync. The prompt is added under the stable item id
//! `kanban-<scheduleId>`, so a run repeated after a crash returns the turn
//! the first attempt started instead of sending twice.

use std::{path::PathBuf, time::Duration};

use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::{
    app_state::AppState,
    conversation::ProviderKind,
    error::AppError,
    kanban_store::{
        DueKanbanSchedule, KanbanScheduleAction, KanbanScheduleStatus, KanbanTaskRecord,
        KanbanTaskSchedule,
    },
    provider::{ConversationPrompt, FollowUpAddOutcome},
};

/// How often due schedules are looked for. Re-reading the wall clock on
/// every tick keeps runs on time across machine sleep, which pauses
/// monotonic timers.
const TICK: Duration = Duration::from_secs(15);
/// A schedule the backend was not running for is skipped once it is this
/// late rather than started long after the time the user picked.
const MISSED_AFTER: chrono::Duration = chrono::Duration::hours(24);
const TITLE_LIMIT: usize = 80;

/// The backend's local time zone, the zone `KanbanTaskSchedule::at` is in.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackendTimeZone {
    /// IANA name such as `Asia/Shanghai`, when the platform reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Current offset from UTC.
    pub offset_minutes: i32,
}

pub fn backend_time_zone() -> BackendTimeZone {
    BackendTimeZone {
        name: iana_time_zone::get_timezone().ok(),
        offset_minutes: Local::now().offset().local_minus_utc() / 60,
    }
}

pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        recover_interrupted(&state).await;
        loop {
            run_due(&state, Utc::now()).await;
            tokio::time::sleep(TICK).await;
        }
    })
}

/// Finishes runs a previous process claimed. One that already recorded its
/// conversation is repeated (idempotent through the item id); one that may
/// or may not have created its conversation is failed instead of risking a
/// duplicate.
async fn recover_interrupted(state: &AppState) {
    for (tenant_id, task_id, schedule_id) in state.kanban_tasks.running_schedules().await {
        let task = state
            .kanban_tasks
            .snapshot_owned(&tenant_id)
            .await
            .tasks
            .into_iter()
            .find(|task| task.id == task_id);
        let Some((task, schedule)) = task.and_then(|task| {
            let schedule = task.schedule.clone()?;
            Some((task, schedule))
        }) else {
            continue;
        };
        if schedule.action == KanbanScheduleAction::Start
            && schedule.result_conversation_id.is_none()
        {
            finish(
                state,
                &tenant_id,
                &task_id,
                &schedule_id,
                Err("interrupted by a backend restart".to_owned()),
            )
            .await;
            continue;
        }
        let outcome = execute(state, &tenant_id, &task, &schedule).await;
        finish(state, &tenant_id, &task_id, &schedule_id, outcome).await;
    }
}

pub(crate) async fn run_due(state: &AppState, now: DateTime<Utc>) {
    for due in state.kanban_tasks.due_schedules(now).await {
        if state.conversations.is_shutting_down() {
            return;
        }
        run_one(state, &due, now).await;
    }
}

async fn run_one(state: &AppState, due: &DueKanbanSchedule, now: DateTime<Utc>) {
    let missed = now - due.due_at > MISSED_AFTER;
    let fired_at = now.timestamp_millis().max(0) as u64;
    let claimed = state
        .kanban_tasks
        .update_schedule(&due.tenant_id, &due.task_id, &due.schedule_id, |task| {
            let Some(schedule) = task.schedule.as_mut() else {
                return false;
            };
            if schedule.status != KanbanScheduleStatus::Pending {
                return false;
            }
            schedule.fired_at = Some(fired_at);
            if missed {
                schedule.status = KanbanScheduleStatus::Failed;
                schedule.error =
                    Some("missed: the backend was not running at the scheduled time".to_owned());
            } else {
                schedule.status = KanbanScheduleStatus::Running;
            }
            true
        })
        .await;
    let task = match claimed {
        Ok(Some(task)) => task,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(task = %due.task_id, error = %error, "failed to claim kanban schedule");
            return;
        }
    };
    let Some(schedule) = task.schedule.clone() else {
        return;
    };
    if schedule.status != KanbanScheduleStatus::Running {
        if missed {
            tracing::info!(task = %due.task_id, "kanban schedule missed");
        }
        return;
    }
    let outcome = execute(state, &due.tenant_id, &task, &schedule).await;
    finish(
        state,
        &due.tenant_id,
        &due.task_id,
        &due.schedule_id,
        outcome,
    )
    .await;
}

struct RunOutcome {
    conversation_id: String,
    turn_id: Option<String>,
}

async fn execute(
    state: &AppState,
    tenant_id: &str,
    task: &KanbanTaskRecord,
    schedule: &KanbanTaskSchedule,
) -> Result<RunOutcome, String> {
    let conversation_id = match (schedule.action, &schedule.result_conversation_id) {
        (KanbanScheduleAction::Send, _) => schedule
            .conversation_id
            .clone()
            .ok_or_else(|| "send needs a conversation".to_owned())?,
        (KanbanScheduleAction::Start, Some(id)) => id.clone(),
        (KanbanScheduleAction::Start, None) => {
            let id = start_conversation(state, tenant_id, task, schedule)
                .await
                .map_err(|error| error.to_string())?;
            // Recorded before the prompt so a restart repeats the send into
            // this conversation instead of creating another.
            let recorded = state
                .kanban_tasks
                .update_schedule(tenant_id, &task.id, &schedule.id, |task| {
                    let Some(schedule) = task.schedule.as_mut() else {
                        return false;
                    };
                    schedule.result_conversation_id = Some(id.clone());
                    add_conversation(task, &id);
                    true
                })
                .await;
            if let Err(error) = recorded {
                tracing::warn!(task = %task.id, error = %error, "failed to record scheduled conversation");
            }
            id
        }
    };
    let prompt = ConversationPrompt {
        permission_mode: schedule.permission_mode.clone(),
        work_mode: schedule.work_mode.clone(),
        client_request_id: None,
        text: schedule.text.clone(),
        model: schedule.model.clone(),
        reasoning_effort: schedule.reasoning_effort.clone(),
        skills: Vec::new(),
        content: Vec::new(),
        permission_profile: None,
        sandbox_mode: None,
        approval_policy: None,
    };
    let outcome = state
        .conversations
        .queue_add_owned(
            tenant_id,
            &conversation_id,
            &format!("kanban-{}", schedule.id),
            prompt,
            false,
            false,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(RunOutcome {
        conversation_id,
        turn_id: match outcome {
            FollowUpAddOutcome::Started(turn_id) => Some(turn_id),
            FollowUpAddOutcome::Queued => None,
        },
    })
}

async fn start_conversation(
    state: &AppState,
    tenant_id: &str,
    task: &KanbanTaskRecord,
    schedule: &KanbanTaskSchedule,
) -> Result<String, AppError> {
    let workspace = state
        .workspaces
        .get_owned(tenant_id, &task.workspace_id)
        .await?;
    let provider = schedule
        .provider
        .as_deref()
        .unwrap_or(&state.config.agent.default_agent)
        .parse::<ProviderKind>()
        .map_err(AppError::InvalidRequest)?;
    let title = task.title.chars().take(TITLE_LIMIT).collect::<String>();
    let manifest = state
        .conversations
        .create_owned(
            tenant_id,
            provider,
            PathBuf::from(workspace.path),
            Some(title),
            schedule.provider_profile.clone(),
        )
        .await?;
    Ok(manifest.id)
}

async fn finish(
    state: &AppState,
    tenant_id: &str,
    task_id: &str,
    schedule_id: &str,
    outcome: Result<RunOutcome, String>,
) {
    if let Err(error) = &outcome {
        tracing::warn!(task = %task_id, error = %error, "kanban schedule failed");
    }
    let result = state
        .kanban_tasks
        .update_schedule(tenant_id, task_id, schedule_id, |task| {
            let Some(schedule) = task.schedule.as_mut() else {
                return false;
            };
            match outcome {
                Ok(run) => {
                    schedule.status = KanbanScheduleStatus::Done;
                    schedule.result_conversation_id = Some(run.conversation_id.clone());
                    schedule.turn_id = run.turn_id;
                    schedule.error = None;
                    add_conversation(task, &run.conversation_id);
                }
                Err(error) => {
                    schedule.status = KanbanScheduleStatus::Failed;
                    schedule.error = Some(error.chars().take(500).collect());
                }
            }
            true
        })
        .await;
    if let Err(error) = result {
        tracing::warn!(task = %task_id, error = %error, "failed to record kanban schedule outcome");
    }
}

fn add_conversation(task: &mut KanbanTaskRecord, conversation_id: &str) {
    let ids = task
        .conversation_ids
        .get_or_insert_with(|| task.conversation_id.iter().cloned().collect());
    if !ids.iter().any(|id| id == conversation_id) {
        ids.push(conversation_id.to_owned());
    }
    task.conversation_id = ids.first().cloned();
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use super::*;
    use crate::{
        config::{AgentConfig, Config, PairingEncryption, SecurityConfig},
        kanban_store::KANBAN_SCHEDULE_TIME_FORMAT,
    };

    fn config(root: &Path) -> Config {
        let workspace_root = root.join("workspaces");
        fs::create_dir_all(workspace_root.join("project")).unwrap();
        Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            pairing_encryption: PairingEncryption::None,
            data_dir: root.join("data"),
            workspace_roots: vec![workspace_root],
            history_retention_days: None,
            agent: AgentConfig {
                default_agent: "codex".to_owned(),
                codex_bin: "codex".to_owned(),
                claude_bin: "claude".to_owned(),
                pi_bin: "pi".to_owned(),
                grok_bin: "grok".to_owned(),
                grok_auth_method: None,
                grok_env_allowlist: Vec::new(),
                devin_bin: "devin".to_owned(),
                devin_auth_method: None,
                devin_api_key_env: None,
                devin_env_allowlist: Vec::new(),
                opencode_bin: "opencode".to_owned(),
                opencode_env_allowlist: Vec::new(),
                antigravity_bin: "agy".to_owned(),
                antigravity_env_allowlist: Vec::new(),
                acp_profiles: BTreeMap::new(),
                ssh_bin: "ssh".to_owned(),
                provider_idle_timeout_minutes: 0,
            },
            security: SecurityConfig {
                enable_auth: true,
                enable_tls: false,
            },
            api: Default::default(),
        }
    }

    fn scheduled_task(
        id: &str,
        action: KanbanScheduleAction,
        at: DateTime<Utc>,
    ) -> KanbanTaskRecord {
        KanbanTaskRecord {
            id: id.to_owned(),
            tenant_id: String::new(),
            workspace_id: "ws_missing".to_owned(),
            title: "task".to_owned(),
            description: None,
            due_date: None,
            status: "planned".to_owned(),
            conversation_id: None,
            conversation_ids: None,
            sort_order: None,
            schedule: Some(KanbanTaskSchedule {
                id: format!("{id}-schedule"),
                at: at
                    .with_timezone(&Local)
                    .format(KANBAN_SCHEDULE_TIME_FORMAT)
                    .to_string(),
                action,
                conversation_id: (action == KanbanScheduleAction::Send)
                    .then(|| "conv_missing".to_owned()),
                text: "go".to_owned(),
                provider: None,
                provider_profile: None,
                model: None,
                reasoning_effort: None,
                permission_mode: None,
                work_mode: None,
                status: KanbanScheduleStatus::Pending,
                fired_at: None,
                result_conversation_id: None,
                turn_id: None,
                error: None,
            }),
            created_at: 1,
            updated_at: 1,
            deleted_at: None,
        }
    }

    #[tokio::test]
    async fn due_schedules_run_once_and_record_failures() {
        let root =
            std::env::temp_dir().join(format!("todex-kanban-scheduler-{}", uuid::Uuid::new_v4()));
        let state = AppState::new_for_tests(config(&root)).await.unwrap();
        let now = Utc::now();
        state
            .kanban_tasks
            .merge_owned(
                "local",
                vec![
                    scheduled_task(
                        "start",
                        KanbanScheduleAction::Start,
                        now - chrono::Duration::minutes(1),
                    ),
                    scheduled_task(
                        "send",
                        KanbanScheduleAction::Send,
                        now - chrono::Duration::minutes(1),
                    ),
                    scheduled_task(
                        "missed",
                        KanbanScheduleAction::Start,
                        now - chrono::Duration::hours(25),
                    ),
                    scheduled_task(
                        "later",
                        KanbanScheduleAction::Start,
                        now + chrono::Duration::hours(1),
                    ),
                ],
            )
            .await
            .unwrap();

        run_due(&state, now).await;
        let tasks = state.kanban_tasks.snapshot_owned("local").await.tasks;
        let schedule = |id: &str| {
            tasks
                .iter()
                .find(|task| task.id == id)
                .and_then(|task| task.schedule.clone())
                .unwrap()
        };
        for id in ["start", "send", "missed"] {
            let run = schedule(id);
            assert_eq!(run.status, KanbanScheduleStatus::Failed, "{id}");
            assert!(run.fired_at.is_some(), "{id}");
            assert!(run.error.is_some(), "{id}");
        }
        assert!(schedule("missed").error.unwrap().starts_with("missed"));
        assert_eq!(schedule("later").status, KanbanScheduleStatus::Pending);
        assert!(state.kanban_tasks.due_schedules(now).await.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn backend_time_zone_reports_an_offset() {
        let zone = backend_time_zone();
        assert!(zone.offset_minutes.abs() <= 14 * 60);
    }
}
