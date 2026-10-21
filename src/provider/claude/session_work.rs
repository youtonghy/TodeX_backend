//! Work a resident Claude session still owes after its turn ended.
//!
//! Claude reports `session_state_changed: idle` once the model stops, even
//! while a backgrounded Bash command runs or a `ScheduleWakeup` is pending:
//! users may keep talking meanwhile. When that work finishes or fires,
//! Claude starts an invocation of its own (`running`), which the worker
//! hands to the supervisor as a provider-initiated turn. Until then the
//! process must outlive the provider keepalive, or the work is lost with it.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use tokio::time::{Duration, Instant};

/// How long past its due time a wakeup keeps the process: Claude rounds the
/// time up to the next minute.
const WAKEUP_GRACE: Duration = Duration::from_secs(2 * 60);
/// A wakeup this close to due counts as the trigger of new work.
const WAKEUP_DUE_WINDOW: Duration = Duration::from_secs(2 * 60);
/// How long a finished task keeps the process for the invocation Claude
/// starts on it: Claude dequeues the notification moments later, by which
/// time the keepalive may long have passed.
const FOLLOW_UP_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(super) struct SessionWork {
    /// Live background tasks of the main session: task id → its type.
    live: HashMap<String, Option<String>>,
    /// Known tasks of the session (including foreground tasks):
    /// task id → (task_type, is_subagent).
    known: HashMap<String, (Option<String>, bool)>,
    /// Background tasks that recently left `live` via `background_tasks_changed`,
    /// retained briefly in case `task_notification` arrives slightly after.
    recently_left: HashMap<String, (Option<String>, Instant)>,
    /// When scheduled wakeups are due.
    wakeups: Vec<Instant>,
    /// A recurring `CronCreate` job exists; it has no end Claude reports.
    cron: bool,
    /// Tasks that finished since the last turn, as `triggerDetail` parts.
    finished: Vec<Value>,
    /// When the last of `finished` did.
    finished_at: Option<Instant>,
    /// `ScheduleWakeup` calls awaiting their result: tool use id → the
    /// requested delay, or `None` for a `stop`.
    scheduling: HashMap<String, Option<u64>>,
    /// `CronCreate` calls awaiting their result.
    cron_creating: HashSet<String>,
}

impl SessionWork {
    /// A turn starts: what finished before it is its own business now.
    pub(super) fn begin_turn(&mut self) {
        self.finished.clear();
        self.recently_left.clear();
    }

    /// Tracks a `system` frame, in or between turns.
    pub(super) fn apply_system(&mut self, message: &Value) {
        let task_id = message.get("task_id").and_then(Value::as_str);
        match message.get("subtype").and_then(Value::as_str) {
            Some("task_started") => {
                let Some(id) = task_id else { return };
                let is_subagent = message.get("owned_by_subagent").and_then(Value::as_bool)
                    == Some(true)
                    || message
                        .get("parent_task_id")
                        .and_then(Value::as_str)
                        .is_some();
                let task_type = message
                    .get("task_type")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.known
                    .insert(id.to_owned(), (task_type.clone(), is_subagent));
                if !is_subagent
                    && message.get("is_backgrounded").and_then(Value::as_bool) != Some(false)
                {
                    self.live.insert(id.to_owned(), task_type);
                }
            }
            Some("task_updated") => {
                let Some(id) = task_id else { return };
                if let Some(bg) = message
                    .pointer("/patch/is_backgrounded")
                    .and_then(Value::as_bool)
                {
                    let (task_type, is_subagent) =
                        self.known.get(id).cloned().unwrap_or((None, false));
                    if !is_subagent {
                        if bg {
                            self.live.insert(id.to_owned(), task_type);
                        } else {
                            self.live.remove(id);
                        }
                    }
                }
            }
            Some("task_notification") => {
                let Some(id) = task_id else { return };
                let started_type = self.live.remove(id).or_else(|| {
                    self.recently_left
                        .remove(id)
                        .filter(|(_, at)| at.elapsed() < Duration::from_secs(10))
                        .map(|(t, _)| t)
                });
                let Some(started_type) = started_type else {
                    return;
                };
                let text = |key: &str| {
                    message
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                };
                self.finished.push(json!({
                    "taskId": id,
                    "taskType": text("task_type").map(str::to_owned).or(started_type),
                    "status": text("status"),
                    "summary": text("summary"),
                }));
                self.finished_at = Some(Instant::now());
            }
            Some("background_tasks_changed") => {
                let mut current_main = HashSet::new();
                if let Some(tasks) = message.get("tasks").and_then(Value::as_array) {
                    for task in tasks {
                        let Some(id) = task.get("task_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let is_subagent = task.get("owned_by_subagent").and_then(Value::as_bool)
                            == Some(true)
                            || task.get("parent_task_id").and_then(Value::as_str).is_some()
                            || self.known.get(id).is_some_and(|(_, sub)| *sub);
                        if !is_subagent {
                            current_main.insert(id.to_owned());
                            let task_type = task
                                .get("task_type")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or_else(|| self.known.get(id).and_then(|(t, _)| t.clone()));
                            self.live.entry(id.to_owned()).or_insert(task_type);
                        }
                    }
                }
                let now = Instant::now();
                self.live.retain(|id, task_type| {
                    let keep = current_main.contains(id);
                    if !keep {
                        self.recently_left
                            .insert(id.clone(), (task_type.clone(), now));
                    }
                    keep
                });
            }
            _ => {}
        }
    }

    /// Tracks a main-session `tool_use` block.
    pub(super) fn apply_tool_use(&mut self, block: &Value) {
        let Some(id) = block.get("id").and_then(Value::as_str) else {
            return;
        };
        let input = block.get("input");
        match block.get("name").and_then(Value::as_str) {
            Some("ScheduleWakeup") => {
                let stop = input
                    .and_then(|input| input.get("stop"))
                    .and_then(Value::as_bool)
                    == Some(true);
                let delay = input
                    .and_then(|input| input.get("delaySeconds"))
                    .and_then(Value::as_f64)
                    .map(|seconds| seconds.max(0.0) as u64);
                self.scheduling.insert(
                    id.to_owned(),
                    if stop { None } else { Some(delay.unwrap_or(0)) },
                );
            }
            Some("CronCreate") => {
                self.cron_creating.insert(id.to_owned());
            }
            _ => {}
        }
    }

    /// Tracks a `tool_result` block, completing a scheduling call.
    pub(super) fn apply_tool_result(&mut self, block: &Value, now: Instant) {
        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
            return;
        };
        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
        if let Some(requested) = self.scheduling.remove(id) {
            if failed {
                return;
            }
            match requested {
                // The loop was stopped: nothing is due any more.
                None => self.wakeups.clear(),
                Some(requested) => {
                    // "Next wakeup scheduled for 13:52:00 (in 98s)" is the
                    // time Claude settled on, rounded up from the request.
                    let delay = scheduled_delay(&result_text(block)).unwrap_or(requested);
                    self.wakeups.push(now + Duration::from_secs(delay));
                }
            }
        } else if self.cron_creating.remove(id) && !failed {
            self.cron = true;
        }
    }

    /// Until when the process should be kept for this work; `cap` bounds
    /// work that has no end of its own (a task that never exits, cron).
    pub(super) fn hold_until(&mut self, now: Instant, cap: Instant) -> Option<Instant> {
        self.wakeups.retain(|due| *due + WAKEUP_GRACE > now);
        if !self.live.is_empty() || self.cron {
            return Some(cap);
        }
        // A finished task's follow-up has not started yet.
        let follow_up = self
            .finished_at
            .filter(|_| !self.finished.is_empty())
            .map(|at| at + FOLLOW_UP_GRACE)
            .filter(|until| *until > now);
        self.wakeups
            .iter()
            .map(|due| *due + WAKEUP_GRACE)
            .chain(follow_up)
            .max()
            .map(|until| until.min(cap))
    }

    /// Whether anything keeps the process beyond the keepalive.
    pub(super) fn pending(&self) -> bool {
        !self.live.is_empty() || self.cron || !self.wakeups.is_empty()
    }

    /// Background tasks that are running or whose follow-up has not started:
    /// lost if the process stops now.
    pub(super) fn task_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.live.keys().cloned().collect();
        ids.extend(
            self.finished
                .iter()
                .filter_map(|task| task.get("taskId").and_then(Value::as_str))
                .map(str::to_owned),
        );
        ids.sort();
        ids
    }

    /// `triggerDetail` for work Claude just started on its own.
    pub(super) fn trigger(&mut self, now: Instant) -> Value {
        if !self.finished.is_empty() {
            let finished = std::mem::take(&mut self.finished);
            let text = |key: &str| {
                finished
                    .iter()
                    .find_map(|task| task.get(key).and_then(Value::as_str))
                    .map(str::to_owned)
            };
            return json!({
                "kind": "background_task",
                "taskIds": finished.iter().filter_map(|task| task.get("taskId")).collect::<Vec<_>>(),
                "taskType": text("taskType"),
                "summary": text("summary"),
            });
        }
        let due = self
            .wakeups
            .iter()
            .enumerate()
            .filter(|(_, due)| **due <= now + WAKEUP_DUE_WINDOW)
            .min_by_key(|(_, due)| **due)
            .map(|(index, _)| index);
        if let Some(index) = due {
            self.wakeups.remove(index);
            return json!({ "kind": "wakeup" });
        }
        if self.cron {
            return json!({ "kind": "cron" });
        }
        json!({ "kind": "unknown" })
    }
}

fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The `N` of "(in Ns)" in a `ScheduleWakeup` result.
fn scheduled_delay(text: &str) -> Option<u64> {
    let start = text.find("(in ")? + 4;
    let rest = &text[start..];
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    (rest[digits..].starts_with("s)"))
        .then(|| rest[..digits].parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system(subtype: &str, extra: Value) -> Value {
        let mut frame = json!({ "type": "system", "subtype": subtype });
        frame
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        frame
    }

    /// The frames Claude Code 2.1.299 sends for a backgrounded Bash command
    /// that finishes after the turn ended.
    #[test]
    fn a_background_task_holds_until_it_finishes_and_names_the_trigger() {
        let now = Instant::now();
        let cap = now + Duration::from_secs(3600);
        let mut work = SessionWork::default();
        work.apply_system(&system(
            "task_started",
            json!({ "task_id": "b74", "is_backgrounded": true, "task_type": "local_bash" }),
        ));
        work.apply_system(&system(
            "background_tasks_changed",
            json!({ "tasks": [{ "task_id": "b74" }] }),
        ));
        assert!(work.pending());
        assert_eq!(work.hold_until(now, cap), Some(cap));
        assert_eq!(work.task_ids(), ["b74"]);

        work.apply_system(&system(
            "task_notification",
            json!({ "task_id": "b74", "status": "completed", "task_type": "local_bash",
                "summary": "Background command \"x\" completed (exit code 0)" }),
        ));
        work.apply_system(&system("background_tasks_changed", json!({ "tasks": [] })));
        assert!(!work.pending());
        // Held just long enough for the follow-up Claude starts on it.
        let finished = work.finished_at.unwrap();
        assert_eq!(work.hold_until(now, cap), Some(finished + FOLLOW_UP_GRACE));
        assert_eq!(work.hold_until(finished + FOLLOW_UP_GRACE, cap), None);
        assert_eq!(
            work.trigger(now),
            json!({ "kind": "background_task", "taskIds": ["b74"], "taskType": "local_bash",
                "summary": "Background command \"x\" completed (exit code 0)" })
        );
        // Taken by that trigger.
        assert_eq!(work.trigger(now)["kind"], "unknown");
        assert!(work.task_ids().is_empty());
    }

    #[test]
    fn foreground_and_subagent_tasks_do_not_hold() {
        let mut work = SessionWork::default();
        work.apply_system(&system(
            "task_started",
            json!({ "task_id": "fg", "is_backgrounded": false }),
        ));
        work.apply_system(&system(
            "task_started",
            json!({ "task_id": "sub", "owned_by_subagent": true }),
        ));
        assert!(!work.pending());
    }

    #[test]
    fn a_foreground_task_moved_to_background_holds_and_triggers() {
        let now = Instant::now();
        let cap = now + Duration::from_secs(3600);
        let mut work = SessionWork::default();
        work.apply_system(&system(
            "task_started",
            json!({ "task_id": "b01", "is_backgrounded": false, "task_type": "local_bash" }),
        ));
        assert!(!work.pending());

        // Claude Code timed out on foreground execution and backgrounded the command.
        work.apply_system(&system(
            "background_tasks_changed",
            json!({ "tasks": [{ "task_id": "b01", "task_type": "local_bash" }] }),
        ));
        assert!(work.pending());
        assert_eq!(work.hold_until(now, cap), Some(cap));
        assert_eq!(work.task_ids(), ["b01"]);

        work.apply_system(&system(
            "task_notification",
            json!({ "task_id": "b01", "status": "completed",
                "summary": "Background command finished" }),
        ));
        work.apply_system(&system("background_tasks_changed", json!({ "tasks": [] })));
        assert!(!work.pending());
        let finished = work.finished_at.unwrap();
        assert_eq!(work.hold_until(now, cap), Some(finished + FOLLOW_UP_GRACE));
        assert_eq!(
            work.trigger(now),
            json!({ "kind": "background_task", "taskIds": ["b01"], "taskType": "local_bash",
                "summary": "Background command finished" })
        );
    }

    #[test]
    fn a_foreground_task_updated_to_background_holds() {
        let now = Instant::now();
        let cap = now + Duration::from_secs(3600);
        let mut work = SessionWork::default();
        work.apply_system(&system(
            "task_started",
            json!({ "task_id": "b02", "is_backgrounded": false, "task_type": "local_bash" }),
        ));
        assert!(!work.pending());

        work.apply_system(&system(
            "task_updated",
            json!({ "task_id": "b02", "patch": { "is_backgrounded": true } }),
        ));
        assert!(work.pending());
        assert_eq!(work.hold_until(now, cap), Some(cap));
        assert_eq!(work.task_ids(), ["b02"]);
    }

    #[test]
    fn a_task_finished_during_the_next_turn_is_that_turns_business() {
        let mut work = SessionWork::default();
        work.apply_system(&system("task_started", json!({ "task_id": "a" })));
        work.apply_system(&system("task_notification", json!({ "task_id": "a" })));
        work.begin_turn();
        assert_eq!(work.trigger(Instant::now())["kind"], "unknown");
    }

    #[test]
    fn a_scheduled_wakeup_holds_past_its_rounded_time_and_triggers() {
        let now = Instant::now();
        let cap = now + Duration::from_secs(6 * 3600);
        let mut work = SessionWork::default();
        work.apply_tool_use(
            &json!({ "type": "tool_use", "id": "w1", "name": "ScheduleWakeup",
            "input": { "delaySeconds": 60, "prompt": "p" } }),
        );
        work.apply_tool_result(
            &json!({ "type": "tool_result", "tool_use_id": "w1",
                "content": "Next wakeup scheduled for 13:35:00 (in 1258s). If you owe the user…" }),
            now,
        );
        let due = now + Duration::from_secs(1258);
        assert_eq!(work.hold_until(now, cap), Some(due + WAKEUP_GRACE));
        assert_eq!(work.trigger(now)["kind"], "unknown", "not due yet");
        assert_eq!(work.trigger(due)["kind"], "wakeup");
        assert!(!work.pending());
    }

    #[test]
    fn wakeup_results_without_a_time_use_the_request_and_failures_or_stops_clear() {
        let now = Instant::now();
        let cap = now + Duration::from_secs(60);
        let mut work = SessionWork::default();
        work.apply_tool_use(&json!({ "id": "w1", "name": "ScheduleWakeup",
            "input": { "delaySeconds": 1200 } }));
        work.apply_tool_result(
            &json!({ "tool_use_id": "w1", "content": [{ "type": "text", "text": "ok" }] }),
            now,
        );
        // Bounded by the cap.
        assert_eq!(work.hold_until(now, cap), Some(cap));
        work.apply_tool_use(&json!({ "id": "w2", "name": "ScheduleWakeup",
            "input": { "delaySeconds": 60 } }));
        work.apply_tool_result(
            &json!({ "tool_use_id": "w2", "is_error": true, "content": "no" }),
            now,
        );
        assert_eq!(work.wakeups.len(), 1);
        work.apply_tool_use(
            &json!({ "id": "w3", "name": "ScheduleWakeup", "input": { "stop": true } }),
        );
        work.apply_tool_result(&json!({ "tool_use_id": "w3", "content": "stopped" }), now);
        assert!(!work.pending());
        // A wakeup long past its grace no longer holds.
        work.wakeups.push(now);
        assert_eq!(work.hold_until(now + Duration::from_secs(600), cap), None);
    }

    #[test]
    fn cron_jobs_hold_until_the_cap() {
        let now = Instant::now();
        let cap = now + Duration::from_secs(60);
        let mut work = SessionWork::default();
        work.apply_tool_use(&json!({ "id": "c1", "name": "CronCreate", "input": {} }));
        assert!(!work.pending(), "not until it succeeded");
        work.apply_tool_result(&json!({ "tool_use_id": "c1", "content": "created" }), now);
        assert_eq!(work.hold_until(now, cap), Some(cap));
        assert_eq!(work.trigger(now)["kind"], "cron");
    }

    /// Replays frames recorded from Claude Code 2.1.299 up to the second
    /// `running` (the invocation Claude starts on its own), checking the
    /// hold at the end of the turn and the trigger it names.
    fn replay(scenario: &str) -> (bool, Value) {
        let start = Instant::now();
        let mut work = SessionWork::default();
        let mut pending_at_idle = None;
        let mut runs = 0;
        for line in include_str!("../../../tests/fixtures/claude_own_work_frames.jsonl").lines() {
            let entry: Value = serde_json::from_str(line).unwrap();
            if entry["scenario"] != scenario {
                continue;
            }
            let at = start + Duration::from_secs_f64(entry["t"].as_f64().unwrap());
            let frame = &entry["frame"];
            match frame["type"].as_str() {
                Some("system") if frame["subtype"] == "session_state_changed" => {
                    if frame["state"] == "running" {
                        runs += 1;
                        if runs == 2 {
                            return (pending_at_idle.unwrap(), work.trigger(at));
                        }
                    } else if pending_at_idle.is_none() {
                        pending_at_idle = Some(work.pending());
                    }
                }
                Some("system") => work.apply_system(frame),
                Some("assistant") => {
                    for block in frame["message"]["content"].as_array().unwrap() {
                        work.apply_tool_use(block);
                    }
                }
                Some("user") => {
                    for block in frame["message"]["content"].as_array().unwrap() {
                        work.apply_tool_result(block, at);
                    }
                }
                _ => {}
            }
        }
        panic!("{scenario}: Claude never started work of its own");
    }

    #[test]
    fn recorded_claude_frames_hold_the_process_and_name_their_trigger() {
        let (pending, trigger) = replay("background_bash");
        assert!(pending, "the background command outlives the turn");
        assert_eq!(trigger["kind"], "background_task");
        assert_eq!(trigger["taskIds"], json!(["b74ng0615"]));
        assert_eq!(trigger["taskType"], "local_bash");

        let (pending, trigger) = replay("schedule_wakeup");
        assert!(pending, "the wakeup outlives the turn");
        assert_eq!(trigger, json!({ "kind": "wakeup" }));
    }

    #[test]
    fn scheduled_delays_are_read_from_the_result_text() {
        assert_eq!(scheduled_delay("for 13:35:00 (in 1258s). If"), Some(1258));
        assert_eq!(scheduled_delay("(in 98s)"), Some(98));
        assert_eq!(scheduled_delay("(in soon)"), None);
        assert_eq!(scheduled_delay("(in 5m)"), None);
        assert_eq!(scheduled_delay("nothing"), None);
    }
}
