//! `@tintinweb/pi-subagents` runs subagents inside the Pi process. They only
//! surface through its `Agent` tool (progress rides `partialResult.details`),
//! a `subagents:record` session entry written whenever a run settles, and a
//! `subagent-notification` custom message for background completions. This
//! maps those onto the `subagent.*` events the other providers already emit.
//!
//! The plugin's `details` shapes are not a stable contract: a frame that does
//! not look like the plugin's stays a plain tool card.
//!
//! Background runs (and `SubagentWorkflow` runs, always in the background)
//! outlive the turn that launched them and die with the Pi process, while
//! writing nothing to stdout until they settle. They are tracked here so the
//! resident process is kept until they settle and Pi has reported on them.
//!
//! `pi-agenticoding`'s `spawn` tool runs a child session in the foreground:
//! one `running` update, then the result with `details.outcome` and the
//! child's `stats`. Its child session writes nothing to stdout, so there is
//! no progress in between.

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio::time::Instant;

const AGENT_TOOL: &str = "Agent";
const WORKFLOW_TOOL: &str = "SubagentWorkflow";
const SPAWN_TOOL: &str = "spawn";
/// A `spawn` run is titled by its prompt's first line, cut to this many chars.
const SPAWN_TITLE_CHARS: usize = 60;
const RECORD_ENTRY: &str = "subagents:record";
const NOTIFICATION_MESSAGE: &str = "subagent-notification";
/// The plugin's status line key; its text counts running and queued agents
/// ("2 running, 1 queued agents") and is cleared once none are left.
pub(super) const STATUS_KEY: &str = "subagents";

pub(super) type SubagentEvent = (&'static str, Value);

/// Runs keyed by the surfaced `subagentId`: the spawning `Agent` toolCallId,
/// or the plugin's agent id for runs no tool call introduced.
#[derive(Default)]
pub(super) struct PiSubagents {
    known: HashSet<String>,
    /// Plugin agent id → subagentId.
    by_agent: HashMap<String, String>,
    /// subagentIds that already emitted a terminal event.
    finished: HashSet<String>,
    /// Background runs that have not settled, by plugin agent (or workflow
    /// run) id.
    background: BTreeMap<String, BackgroundRun>,
    /// Background runs that settled since the last turn began, as
    /// `triggerDetail` parts for the run Pi starts to report on them.
    settled: Vec<Value>,
    /// When the last of `settled` did.
    settled_at: Option<Instant>,
    /// The plugin's status line reports running agents, including ones no
    /// `Agent` result introduced.
    status_busy: bool,
    /// Runs started by a call in the foreground. One the turn ends without
    /// settling (stopped, process gone) is cancelled then.
    foreground: HashSet<String>,
}

/// A background run as clients see it while the agent waits on it.
struct BackgroundRun {
    title: Option<String>,
    /// The `Agent`/`SubagentWorkflow` call that launched it.
    tool_call_id: String,
    started_at: String,
}

impl PiSubagents {
    /// The `subagent.*` event implied by a `tool.*` event (`kind`) whose
    /// payload came from `pi_tool_payload`.
    pub(super) fn tool_event(&mut self, kind: &str, payload: &Value) -> Option<SubagentEvent> {
        if payload.get("toolName").and_then(Value::as_str) == Some(WORKFLOW_TOOL) {
            self.workflow_launched(kind, payload);
            return None;
        }
        if payload.get("toolName").and_then(Value::as_str) == Some(SPAWN_TOOL) {
            return self.spawn_event(kind, payload);
        }
        if payload.get("toolName").and_then(Value::as_str) != Some(AGENT_TOOL) {
            return None;
        }
        let id = payload
            .get("toolCallId")
            .and_then(Value::as_str)?
            .to_owned();
        let args = payload.get("arguments").unwrap_or(&Value::Null);
        match kind {
            "tool.started" => {
                // The plugin's schema requires `subagent_type`; another
                // extension's `Agent` tool would not carry it.
                let agent_kind = args.get("subagent_type").filter(|kind| kind.is_string())?;
                if !self.known.insert(id.clone()) {
                    return None;
                }
                self.foreground.insert(id.clone());
                let mut fields = Map::new();
                fields.insert("agentKind".into(), agent_kind.clone());
                insert_string(
                    &mut fields,
                    "title",
                    args.get("description").or(Some(agent_kind)),
                );
                insert_string(&mut fields, "task", args.get("prompt"));
                fields.insert("status".into(), json!("running"));
                Some(("subagent.started", event(&id, Some(payload), fields)))
            }
            "tool.updated" => {
                let details = plugin_details(payload.get("partialResult"))?;
                if self.finished.contains(&id) {
                    return None;
                }
                self.known.insert(id.clone());
                let mut fields = self.progress_fields(&id, details);
                insert_string(&mut fields, "task", args.get("prompt"));
                fields.insert("status".into(), json!("running"));
                Some(("subagent.updated", event(&id, Some(payload), fields)))
            }
            "tool.completed" => self.tool_completed(&id, payload),
            _ => None,
        }
    }

    /// `subagent.*` for a `pi-agenticoding` `spawn` call, keyed by its
    /// toolCallId. Frames without the plugin's shape stay plain tool cards.
    fn spawn_event(&mut self, kind: &str, payload: &Value) -> Option<SubagentEvent> {
        let id = payload
            .get("toolCallId")
            .and_then(Value::as_str)?
            .to_owned();
        match kind {
            "tool.started" => {
                let prompt = payload
                    .pointer("/arguments/prompt")
                    .and_then(Value::as_str)
                    .filter(|prompt| !prompt.trim().is_empty())?;
                if !self.known.insert(id.clone()) {
                    return None;
                }
                self.foreground.insert(id.clone());
                let mut fields = Map::new();
                fields.insert("agentKind".into(), json!(SPAWN_TOOL));
                fields.insert("title".into(), json!(spawn_title(prompt)));
                fields.insert("task".into(), json!(prompt));
                fields.insert("status".into(), json!("running"));
                Some(("subagent.started", event(&id, Some(payload), fields)))
            }
            "tool.updated" => {
                let details = spawn_details(payload.get("partialResult"))?;
                if !self.known.contains(&id) || self.finished.contains(&id) {
                    return None;
                }
                let mut fields = spawn_fields(details);
                fields.insert("status".into(), json!("running"));
                Some(("subagent.updated", event(&id, Some(payload), fields)))
            }
            "tool.completed" => {
                let details = spawn_details(payload.get("result"));
                if (details.is_none() && !self.known.contains(&id))
                    || !self.finished.insert(id.clone())
                {
                    return None;
                }
                self.known.insert(id.clone());
                let mut fields = details.map(spawn_fields).unwrap_or_default();
                let text = result_text(payload.get("result"));
                let outcome = details
                    .and_then(|details| details.get("outcome"))
                    .and_then(Value::as_str);
                let (kind, status) = if payload.get("isError").and_then(Value::as_bool)
                    == Some(true)
                    || outcome == Some("error")
                {
                    ("subagent.failed", "failed")
                } else if outcome == Some("aborted") {
                    ("subagent.cancelled", "cancelled")
                } else {
                    ("subagent.completed", "completed")
                };
                fields.insert("status".into(), json!(status));
                if let Some(text) = text {
                    let key = if status == "failed" {
                        "error"
                    } else {
                        "result"
                    };
                    fields.insert(key.into(), json!(text));
                }
                Some((kind, event(&id, Some(payload), fields)))
            }
            _ => None,
        }
    }

    /// `subagent.cancelled` for foreground runs the turn ended without
    /// settling: their call never returned (stopped, or the process went).
    pub(super) fn close_foreground(&mut self) -> Vec<SubagentEvent> {
        let launched: HashSet<&str> = self
            .background
            .values()
            .map(|run| run.tool_call_id.as_str())
            .collect();
        let mut open: Vec<String> = self
            .foreground
            .drain()
            .filter(|id| !self.finished.contains(id) && !launched.contains(id.as_str()))
            .collect();
        open.sort();
        open.into_iter()
            .map(|id| {
                self.finished.insert(id.clone());
                let mut fields = Map::new();
                fields.insert("status".into(), json!("cancelled"));
                ("subagent.cancelled", event(&id, None, fields))
            })
            .collect()
    }

    fn tool_completed(&mut self, id: &str, payload: &Value) -> Option<SubagentEvent> {
        let details = plugin_details(payload.get("result"));
        if details.is_none() && !self.known.contains(id) {
            return None;
        }
        self.known.insert(id.to_owned());
        let mut fields = details
            .map(|details| self.progress_fields(id, details))
            .unwrap_or_default();
        let status = details
            .and_then(|details| details.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = result_text(payload.get("result"));
        if status == "background" {
            // The launch returns at once; the run settles later through its
            // session record or completion notification.
            if let Some(file) = text.as_deref().and_then(output_file) {
                fields.insert("outputFile".into(), json!(file));
            }
            if let Some(agent_id) = fields.get("agentId").and_then(Value::as_str) {
                let title = fields
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.launch_background(agent_id.to_owned(), title, id);
            }
            fields.insert("status".into(), json!("running"));
            return Some(("subagent.updated", event(id, Some(payload), fields)));
        }
        if !self.finished.insert(id.to_owned()) {
            // The session record already settled this run with its clean
            // result; the tool result only adds the final counters.
            return Some(("subagent.updated", event(id, Some(payload), fields)));
        }
        let is_error = payload.get("isError").and_then(Value::as_bool) == Some(true);
        let (kind, status) = if is_error {
            ("subagent.failed", "failed")
        } else {
            terminal_status(status).unwrap_or(("subagent.completed", "completed"))
        };
        fields.insert("status".into(), json!(status));
        if status == "failed" {
            let error = details
                .and_then(|details| details.get("error"))
                .filter(|error| error.as_str().is_some_and(|error| !error.is_empty()))
                .cloned()
                .or_else(|| text.clone().map(Value::String));
            fields.insert("error".into(), error.unwrap_or(Value::Null));
        }
        if let Some(text) = text {
            fields.insert("result".into(), json!(text));
        }
        Some((kind, event(id, Some(payload), fields)))
    }

    /// `entry_appended` frames. `None` means the entry is not the plugin's;
    /// `Some` means it was consumed, even when it implies no event.
    pub(super) fn record_entry(&mut self, frame: &Value) -> Option<Vec<SubagentEvent>> {
        let entry = frame.get("entry")?;
        if entry.get("type").and_then(Value::as_str) != Some("custom")
            || entry.get("customType").and_then(Value::as_str) != Some(RECORD_ENTRY)
        {
            return None;
        }
        let data = entry.get("data").unwrap_or(&Value::Null);
        let Some(agent_id) = data.get("id").and_then(Value::as_str) else {
            return Some(Vec::new());
        };
        let mut fields = Map::new();
        insert_string(&mut fields, "title", data.get("description"));
        insert_string(&mut fields, "agentKind", data.get("type"));
        insert_string(&mut fields, "result", data.get("result"));
        insert_string(&mut fields, "error", data.get("error"));
        let status = data.get("status").and_then(Value::as_str).unwrap_or("");
        Some(vec![self.settle(agent_id, status, fields)])
    }

    /// Completion notifications for background runs (the `message` of a
    /// custom-role frame). A group notification lists the rest in `others`.
    pub(super) fn notification(&mut self, message: Option<&Value>) -> Vec<SubagentEvent> {
        let Some(message) = message.filter(|message| {
            message.get("customType").and_then(Value::as_str) == Some(NOTIFICATION_MESSAGE)
        }) else {
            return Vec::new();
        };
        let Some(details) = message.get("details").filter(|details| details.is_object()) else {
            return Vec::new();
        };
        let others = details.get("others").and_then(Value::as_array);
        std::iter::once(details)
            .chain(others.into_iter().flatten())
            .filter_map(|item| {
                let agent_id = item.get("id").and_then(Value::as_str)?;
                let mut fields = Map::new();
                insert_string(&mut fields, "title", item.get("description"));
                insert_string(&mut fields, "outputFile", item.get("outputFile"));
                insert_string(&mut fields, "error", item.get("error"));
                let usage = usage(
                    item,
                    &[
                        ("toolUses", "tool_uses"),
                        ("durationMs", "duration_ms"),
                        ("totalTokens", "total_tokens"),
                    ],
                );
                if !usage.is_empty() {
                    fields.insert("usage".into(), Value::Object(usage));
                }
                // Only a preview; the session record carries the full result.
                let id = self.subagent_id(agent_id);
                if !self.finished.contains(&id) {
                    insert_string(&mut fields, "result", item.get("resultPreview"));
                }
                let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                Some(self.settle(agent_id, status, fields))
            })
            .collect()
    }

    /// A `SubagentWorkflow` call returns its run id at once; the run settles
    /// through a completion notification for that id.
    fn workflow_launched(&mut self, kind: &str, payload: &Value) {
        if kind != "tool.completed" || payload.get("isError").and_then(Value::as_bool) == Some(true)
        {
            return;
        }
        let (Some(id), Some(run_id)) = (
            payload.get("toolCallId").and_then(Value::as_str),
            payload
                .pointer("/result/details/taskId")
                .and_then(Value::as_str),
        ) else {
            return;
        };
        let title = payload
            .pointer("/arguments/title")
            .or_else(|| payload.pointer("/arguments/name"))
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        self.launch_background(run_id.to_owned(), title, id);
    }

    fn launch_background(&mut self, agent_id: String, title: Option<String>, tool_call_id: &str) {
        self.background
            .entry(agent_id)
            .or_insert_with(|| BackgroundRun {
                title,
                tool_call_id: tool_call_id.to_owned(),
                started_at: chrono::Utc::now().to_rfc3339(),
            });
    }

    /// A turn starts: runs that settled before it are its own business now.
    pub(super) fn begin_turn(&mut self) {
        self.settled.clear();
        self.settled_at = None;
    }

    /// The plugin's status line changed (`None` clears it).
    pub(super) fn set_status(&mut self, text: Option<&str>) {
        self.status_busy = text.is_some_and(|text| !text.trim().is_empty());
    }

    /// Whether background runs are still going.
    pub(super) fn running(&self) -> bool {
        !self.background.is_empty() || self.status_busy
    }

    /// Runs settled since the last turn whose report Pi has not started yet
    /// keep the process until `grace` after the last of them settled.
    pub(super) fn report_due_until(&self, grace: std::time::Duration) -> Option<Instant> {
        self.settled_at
            .filter(|_| !self.settled.is_empty())
            .map(|at| at + grace)
    }

    /// The `tasks` of a `background_tasks_pending` notice.
    pub(super) fn background_tasks(&self) -> Vec<Value> {
        self.background
            .iter()
            .map(|(agent_id, run)| {
                let mut task = json!({
                    "taskId": agent_id,
                    "taskType": "subagent",
                    "toolUseId": run.tool_call_id,
                    "startedAt": run.started_at,
                });
                if let Some(title) = &run.title {
                    task["description"] = json!(title);
                }
                task
            })
            .collect()
    }

    /// Runs lost if the process stops now: still running, or settled with
    /// the report Pi starts on them still due (within `grace`).
    pub(super) fn lost_task_ids(&self, grace: std::time::Duration) -> Vec<String> {
        let mut ids: Vec<String> = self.background.keys().cloned().collect();
        let report_due = self
            .report_due_until(grace)
            .is_some_and(|until| until > Instant::now());
        ids.extend(
            self.settled
                .iter()
                .filter(|_| report_due)
                .filter_map(|task| task.get("taskId").and_then(Value::as_str))
                .map(str::to_owned),
        );
        ids.sort();
        ids.dedup();
        ids
    }

    /// `triggerDetail` for a run Pi just started on its own; takes the
    /// settled runs it reports on.
    pub(super) fn take_trigger(&mut self) -> Value {
        let settled = std::mem::take(&mut self.settled);
        self.settled_at = None;
        if settled.is_empty() {
            return json!({ "kind": "unknown" });
        }
        let text = |key: &str| {
            settled
                .iter()
                .find_map(|task| task.get(key).and_then(Value::as_str))
                .map(str::to_owned)
        };
        json!({
            "kind": "background_task",
            "taskIds": settled.iter().filter_map(|task| task.get("taskId")).collect::<Vec<_>>(),
            "taskType": "subagent",
            "summary": text("summary"),
        })
    }

    fn subagent_id(&self, agent_id: &str) -> String {
        self.by_agent
            .get(agent_id)
            .cloned()
            .unwrap_or_else(|| agent_id.to_owned())
    }

    fn settle(
        &mut self,
        agent_id: &str,
        status: &str,
        mut fields: Map<String, Value>,
    ) -> SubagentEvent {
        let id = self.subagent_id(agent_id);
        self.known.insert(id.clone());
        self.by_agent.insert(agent_id.to_owned(), id.clone());
        fields.insert("agentId".into(), json!(agent_id));
        if terminal_status(status).is_some() {
            if let Some(run) = self.background.remove(agent_id) {
                self.settled.push(json!({
                    "taskId": agent_id,
                    "summary": run.title.or_else(|| {
                        fields.get("title").and_then(Value::as_str).map(str::to_owned)
                    }),
                }));
                self.settled_at = Some(Instant::now());
            }
        }
        let Some((kind, status)) =
            terminal_status(status).filter(|_| self.finished.insert(id.clone()))
        else {
            return ("subagent.updated", event(&id, None, fields));
        };
        fields.insert("status".into(), json!(status));
        (kind, event(&id, None, fields))
    }

    /// Fields shared by every `AgentDetails` snapshot.
    fn progress_fields(&mut self, id: &str, details: &Value) -> Map<String, Value> {
        let mut fields = Map::new();
        if let Some(agent_id) = details.get("agentId").and_then(Value::as_str) {
            self.by_agent.insert(agent_id.to_owned(), id.to_owned());
            fields.insert("agentId".into(), json!(agent_id));
        }
        insert_string(&mut fields, "agentKind", details.get("subagentType"));
        insert_string(&mut fields, "title", details.get("description"));
        let mut metadata = Map::new();
        for key in ["activity", "modelName", "tokens", "turnCount", "maxTurns"] {
            if let Some(value) = details.get(key).filter(|value| !value.is_null()) {
                metadata.insert(key.into(), value.clone());
            }
        }
        if !metadata.is_empty() {
            fields.insert("metadata".into(), Value::Object(metadata));
        }
        let usage = usage(
            details,
            &[("toolUses", "tool_uses"), ("durationMs", "duration_ms")],
        );
        if !usage.is_empty() {
            fields.insert("usage".into(), Value::Object(usage));
        }
        fields
    }
}

/// `AgentDetails` from a tool result or partial result; `subagentType` sets
/// the plugin's shape apart from other tools' details.
fn plugin_details(result: Option<&Value>) -> Option<&Value> {
    result
        .and_then(|result| result.get("details"))
        .filter(|details| details.get("subagentType").is_some_and(Value::is_string))
}

/// `spawn`'s `SpawnResultDetails`; `outcome` and `model` set it apart from
/// another extension's `spawn` tool.
fn spawn_details(result: Option<&Value>) -> Option<&Value> {
    result
        .and_then(|result| result.get("details"))
        .filter(|details| {
            details.get("outcome").is_some_and(Value::is_string)
                && details.get("model").is_some_and(Value::is_string)
        })
}

/// Model, routing and usage of a `spawn` run.
fn spawn_fields(details: &Value) -> Map<String, Value> {
    let mut fields = Map::new();
    let mut metadata = Map::new();
    for (from, to) in [("model", "modelName"), ("thinking", "thinking")] {
        if let Some(value) = details.get(from).filter(|value| value.is_string()) {
            metadata.insert(to.into(), value.clone());
        }
    }
    if let Some(group) = details
        .pointer("/route/group")
        .or_else(|| details.pointer("/route/requestedGroup"))
        .filter(|group| group.as_str().is_some_and(|group| !group.is_empty()))
    {
        metadata.insert("group".into(), group.clone());
    }
    if let Some(stats) = details.get("stats") {
        for key in ["cost", "turns"] {
            if let Some(value) = stats.get(key).filter(|value| value.is_number()) {
                metadata.insert(key.into(), value.clone());
            }
        }
        let usage = usage(
            stats,
            &[
                ("inputTokens", "input_tokens"),
                ("outputTokens", "output_tokens"),
                ("totalTokens", "total_tokens"),
            ],
        );
        if !usage.is_empty() {
            fields.insert("usage".into(), Value::Object(usage));
        }
    }
    if !metadata.is_empty() {
        fields.insert("metadata".into(), Value::Object(metadata));
    }
    fields
}

/// The first non-empty line of a `spawn` prompt, cut to [`SPAWN_TITLE_CHARS`].
fn spawn_title(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= SPAWN_TITLE_CHARS {
        return line.to_owned();
    }
    let mut title: String = line.chars().take(SPAWN_TITLE_CHARS).collect();
    title.push('…');
    title
}

/// Plugin statuses that end a run, as a TodeX event kind and status.
fn terminal_status(status: &str) -> Option<(&'static str, &'static str)> {
    match status {
        "completed" | "steered" => Some(("subagent.completed", "completed")),
        "error" => Some(("subagent.failed", "failed")),
        "aborted" | "stopped" => Some(("subagent.cancelled", "cancelled")),
        _ => None,
    }
}

fn event(id: &str, tool: Option<&Value>, fields: Map<String, Value>) -> Value {
    let mut payload = json!({
        "provider": "pi",
        "source": "provider",
        "subagentId": id,
        "providerItemId": id,
    });
    if let Some(turn_id) = tool.and_then(|tool| tool.pointer("/block/turnId")) {
        payload["turnId"] = turn_id.clone();
    }
    if let Some(object) = payload.as_object_mut() {
        object.extend(fields);
    }
    payload
}

fn insert_string(fields: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        fields.insert(key.into(), json!(value));
    }
}

fn usage(source: &Value, keys: &[(&str, &str)]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|(from, to)| {
            let value = source.get(*from).filter(|value| value.is_number())?;
            Some(((*to).to_owned(), value.clone()))
        })
        .collect()
}

fn result_text(result: Option<&Value>) -> Option<String> {
    let text = result?
        .get("content")?
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Background launches name the run's transcript on an `Output file:` line.
fn output_file(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("Output file:"))
        .map(str::trim)
        .filter(|file| !file.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(kind_phase: &str, id: &str, extra: Value) -> Value {
        let mut payload = json!({
            "provider": "pi",
            "toolCallId": id,
            "toolName": "Agent",
            "arguments": {
                "prompt": "Find the auth code",
                "description": "Find auth",
                "subagent_type": "Explore",
            },
            "block": { "category": "tool", "id": id, "turnId": "turn-1", "phase": kind_phase },
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        payload
    }

    fn details(status: &str, extra: Value) -> Value {
        let mut details = json!({
            "displayName": "Explore",
            "description": "Find auth",
            "subagentType": "Explore",
            "toolUses": 3,
            "tokens": "12.3k token",
            "turnCount": 2,
            "durationMs": 5400,
            "status": status,
        });
        details
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        details
    }

    fn text(text: &str) -> Value {
        json!([{ "type": "text", "text": text }])
    }

    #[test]
    fn foreground_run_starts_progresses_and_completes() {
        let mut runs = PiSubagents::default();
        let (kind, started) = runs
            .tool_event("tool.started", &tool("started", "call-1", json!({})))
            .unwrap();
        assert_eq!(kind, "subagent.started");
        assert_eq!(started["subagentId"], "call-1");
        assert_eq!(started["turnId"], "turn-1");
        assert_eq!(started["title"], "Find auth");
        assert_eq!(started["task"], "Find the auth code");
        assert_eq!(started["agentKind"], "Explore");

        let progress = tool(
            "delta",
            "call-1",
            json!({
                "partialResult": { "content": text("3 tool uses..."),
                    "details": details("running", json!({ "activity": "reading…" })) },
            }),
        );
        let (kind, updated) = runs.tool_event("tool.updated", &progress).unwrap();
        assert_eq!(kind, "subagent.updated");
        assert_eq!(updated["status"], "running");
        assert_eq!(updated["metadata"]["activity"], "reading…");
        assert_eq!(
            updated["usage"],
            json!({ "tool_uses": 3, "duration_ms": 5400 })
        );

        let done = tool(
            "completed",
            "call-1",
            json!({
                "result": { "content": text("Agent completed in 5.4s.\n\nFound it."),
                    "details": details("completed", json!({ "agentId": "agent-1" })) },
                "isError": false,
            }),
        );
        let (kind, completed) = runs.tool_event("tool.completed", &done).unwrap();
        assert_eq!(kind, "subagent.completed");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["agentId"], "agent-1");
        assert_eq!(completed["result"], "Agent completed in 5.4s.\n\nFound it.");
        // Late progress after the end changes nothing.
        assert!(runs.tool_event("tool.updated", &progress).is_none());
    }

    #[test]
    fn failed_foreground_run_reports_the_error() {
        let mut runs = PiSubagents::default();
        runs.tool_event("tool.started", &tool("started", "call-1", json!({})));
        let done = tool(
            "completed",
            "call-1",
            json!({
                "result": { "content": text("Agent failed: boom"),
                    "details": details("error", json!({ "error": "boom" })) },
                "isError": false,
            }),
        );
        let (kind, failed) = runs.tool_event("tool.completed", &done).unwrap();
        assert_eq!(kind, "subagent.failed");
        assert_eq!(failed["error"], "boom");
        // A plain tool error without details still closes an opened run.
        runs.tool_event("tool.started", &tool("started", "call-2", json!({})));
        let rejected = tool(
            "completed",
            "call-2",
            json!({
                "result": { "content": text("Unknown agent type") }, "isError": true,
            }),
        );
        let (kind, failed) = runs.tool_event("tool.completed", &rejected).unwrap();
        assert_eq!(kind, "subagent.failed");
        assert_eq!(failed["error"], "Unknown agent type");
    }

    #[test]
    fn background_run_settles_through_its_record_then_notification() {
        let mut runs = PiSubagents::default();
        runs.tool_event("tool.started", &tool("started", "call-1", json!({})));
        let launched = tool(
            "completed",
            "call-1",
            json!({
                "result": {
                    "content": text("Agent started in background.\nAgent ID: agent-1\nOutput file: /tmp/pi-subagents-1000/x/tasks/agent-1.output\n"),
                    "details": details("background", json!({ "agentId": "agent-1" })),
                },
            }),
        );
        let (kind, updated) = runs.tool_event("tool.completed", &launched).unwrap();
        assert_eq!(kind, "subagent.updated");
        assert_eq!(updated["status"], "running");
        assert_eq!(
            updated["outputFile"],
            "/tmp/pi-subagents-1000/x/tasks/agent-1.output"
        );

        let record = json!({ "type": "entry_appended", "entry": { "type": "custom", "customType": "subagents:record",
            "data": { "id": "agent-1", "type": "Explore", "description": "Find auth", "status": "completed", "result": "Found it." } } });
        let events = runs.record_entry(&record).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "subagent.completed");
        assert_eq!(events[0].1["subagentId"], "call-1");
        assert_eq!(events[0].1["result"], "Found it.");

        let notification = json!({ "role": "custom", "customType": "subagent-notification", "details": {
            "id": "agent-1", "description": "Find auth", "status": "completed", "toolUses": 5,
            "durationMs": 12300, "totalTokens": 12345, "outputFile": "/tmp/agent-1.output", "resultPreview": "Found" } });
        let events = runs.notification(Some(&notification));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "subagent.updated");
        assert!(events[0].1.get("status").is_none());
        assert!(events[0].1.get("result").is_none());
        assert_eq!(events[0].1["usage"]["total_tokens"], 12345);
    }

    #[test]
    fn notifications_settle_unrecorded_and_grouped_runs() {
        let mut runs = PiSubagents::default();
        let notification = json!({ "role": "custom", "customType": "subagent-notification", "details": {
            "id": "agent-1", "description": "One", "status": "completed", "resultPreview": "first",
            "others": [{ "id": "agent-2", "description": "Two", "status": "error", "error": "boom" },
                       { "id": "agent-3", "description": "Three", "status": "stopped" }] } });
        let events = runs.notification(Some(&notification));
        let kinds: Vec<_> = events.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            [
                "subagent.completed",
                "subagent.failed",
                "subagent.cancelled"
            ]
        );
        assert_eq!(events[0].1["subagentId"], "agent-1");
        assert_eq!(events[0].1["result"], "first");
        assert_eq!(events[1].1["error"], "boom");
        assert!(runs
            .notification(Some(&json!({ "customType": "other", "details": {} })))
            .is_empty());
    }

    #[test]
    fn background_runs_are_awaited_until_they_settle_and_name_the_report() {
        let mut runs = PiSubagents::default();
        runs.tool_event("tool.started", &tool("started", "call-1", json!({})));
        runs.tool_event(
            "tool.completed",
            &tool(
                "completed",
                "call-1",
                json!({ "result": { "content": text("Agent started in background."),
                    "details": details("background", json!({ "agentId": "agent-1" })) } }),
            ),
        );
        // `SubagentWorkflow` runs are always in the background.
        runs.tool_event(
            "tool.completed",
            &json!({ "toolCallId": "call-2", "toolName": "SubagentWorkflow",
                "arguments": { "title": "Audit" },
                "result": { "content": text("Workflow started"), "details": { "taskId": "wf-1" } } }),
        );
        assert!(runs.running());
        let tasks = runs.background_tasks();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["taskId"], "agent-1");
        assert_eq!(tasks[0]["toolUseId"], "call-1");
        assert_eq!(tasks[0]["description"], "Find auth");
        assert_eq!(tasks[1]["taskId"], "wf-1");
        assert_eq!(tasks[1]["description"], "Audit");
        assert_eq!(
            runs.lost_task_ids(std::time::Duration::from_secs(60)),
            ["agent-1", "wf-1"]
        );

        let record = json!({ "type": "entry_appended", "entry": { "type": "custom", "customType": "subagents:record",
            "data": { "id": "agent-1", "status": "completed", "result": "Found it." } } });
        runs.record_entry(&record);
        let workflow = json!({ "role": "custom", "customType": "subagent-notification",
            "details": { "id": "wf-1", "description": "Workflow audit", "status": "stopped" } });
        runs.notification(Some(&workflow));
        assert!(!runs.running());
        // Their report keeps the process a little longer.
        assert!(runs
            .report_due_until(std::time::Duration::from_secs(60))
            .is_some());
        assert_eq!(
            runs.lost_task_ids(std::time::Duration::from_secs(60)),
            ["agent-1", "wf-1"]
        );
        assert_eq!(
            runs.take_trigger(),
            json!({ "kind": "background_task", "taskIds": ["agent-1", "wf-1"],
                "taskType": "subagent", "summary": "Find auth" })
        );
        assert!(runs
            .report_due_until(std::time::Duration::from_secs(60))
            .is_none());
        assert!(runs
            .lost_task_ids(std::time::Duration::from_secs(60))
            .is_empty());
    }

    #[test]
    fn the_plugin_status_line_counts_as_running_work() {
        let mut runs = PiSubagents::default();
        runs.set_status(Some("2 running, 1 queued agents"));
        assert!(runs.running());
        // Nothing to show: the runs are unknown beyond the count.
        assert!(runs.background_tasks().is_empty());
        runs.set_status(None);
        assert!(!runs.running());
        runs.begin_turn();
        assert_eq!(runs.take_trigger()["kind"], "unknown");
    }

    #[test]
    fn unrelated_tools_and_entries_are_left_alone() {
        let mut runs = PiSubagents::default();
        let mut other = tool("started", "call-1", json!({ "toolName": "read" }));
        assert!(runs.tool_event("tool.started", &other).is_none());
        // Another extension's `Agent` tool without the plugin's schema.
        other["toolName"] = json!("Agent");
        other["arguments"] = json!({ "task": "x" });
        assert!(runs.tool_event("tool.started", &other).is_none());
        let plain_end = tool(
            "completed",
            "call-1",
            json!({ "result": { "content": text("ok"), "details": { "n": 1 } } }),
        );
        assert!(runs.tool_event("tool.completed", &plain_end).is_none());
        let entry = json!({ "type": "entry_appended", "entry": { "type": "custom", "customType": "other", "data": {} } });
        assert!(runs.record_entry(&entry).is_none());
    }

    fn spawn_tool(phase: &str, id: &str, extra: Value) -> Value {
        let mut payload = tool(phase, id, extra);
        payload["toolName"] = json!("spawn");
        payload["arguments"] =
            json!({ "prompt": "\n  Check the router's packages\nRemove the unused ones." });
        payload
    }

    fn spawn_details(outcome: &str, extra: Value) -> Value {
        let mut details = json!({
            "model": "glm-5.3", "thinking": "high", "truncated": false, "outcome": outcome,
            "route": { "status": "routed", "group": "fast", "provider": "zai", "modelId": "glm-5.3" },
        });
        details
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        details
    }

    #[test]
    fn spawn_runs_surface_as_foreground_subagents() {
        let mut runs = PiSubagents::default();
        let (kind, started) = runs
            .tool_event("tool.started", &spawn_tool("started", "spawn-1", json!({})))
            .unwrap();
        assert_eq!(kind, "subagent.started");
        assert_eq!(started["subagentId"], "spawn-1");
        assert_eq!(started["agentKind"], "spawn");
        assert_eq!(started["title"], "Check the router's packages");
        assert_eq!(
            started["task"],
            "\n  Check the router's packages\nRemove the unused ones."
        );
        assert_eq!(started["turnId"], "turn-1");

        let running = spawn_tool(
            "delta",
            "spawn-1",
            json!({ "partialResult": { "content": [], "details": spawn_details("running", json!({})) } }),
        );
        let (kind, updated) = runs.tool_event("tool.updated", &running).unwrap();
        assert_eq!(kind, "subagent.updated");
        assert_eq!(updated["status"], "running");
        assert_eq!(updated["metadata"]["modelName"], "glm-5.3");
        assert_eq!(updated["metadata"]["group"], "fast");

        let stats = json!({ "stats": { "inputTokens": 1200, "outputTokens": 300, "cacheReadTokens": 50,
            "totalTokens": 1550, "cost": 0.01, "turns": 4 } });
        let done = spawn_tool(
            "completed",
            "spawn-1",
            json!({ "result": { "content": text("Removed 3 packages."), "details": spawn_details("success", stats) } }),
        );
        let (kind, completed) = runs.tool_event("tool.completed", &done).unwrap();
        assert_eq!(kind, "subagent.completed");
        assert_eq!(completed["result"], "Removed 3 packages.");
        assert_eq!(
            completed["usage"],
            json!({ "input_tokens": 1200, "output_tokens": 300, "total_tokens": 1550 })
        );
        assert_eq!(completed["metadata"]["turns"], 4);
        // Settled runs stay settled and are not cancelled with the turn.
        assert!(runs.tool_event("tool.completed", &done).is_none());
        assert!(runs.tool_event("tool.updated", &running).is_none());
        assert!(runs.close_foreground().is_empty());
    }

    #[test]
    fn spawn_outcomes_and_errors_map_to_terminal_states() {
        let mut runs = PiSubagents::default();
        for id in ["aborted", "errored", "thrown"] {
            runs.tool_event("tool.started", &spawn_tool("started", id, json!({})));
        }
        let aborted = spawn_tool(
            "completed",
            "aborted",
            json!({ "result": { "content": [], "details": spawn_details("aborted", json!({})) } }),
        );
        let (kind, cancelled) = runs.tool_event("tool.completed", &aborted).unwrap();
        assert_eq!(
            (kind, &cancelled["status"]),
            ("subagent.cancelled", &json!("cancelled"))
        );

        let errored = spawn_tool(
            "completed",
            "errored",
            json!({ "result": { "content": text("rate limited"), "details": spawn_details("error", json!({})) } }),
        );
        let (kind, failed) = runs.tool_event("tool.completed", &errored).unwrap();
        assert_eq!(kind, "subagent.failed");
        assert_eq!(failed["error"], "rate limited");
        assert!(failed.get("result").is_none());

        // A thrown spawn returns a plain error result without details.
        let thrown = spawn_tool(
            "completed",
            "thrown",
            json!({ "result": { "content": text("Child agent produced no output.") }, "isError": true }),
        );
        let (kind, failed) = runs.tool_event("tool.completed", &thrown).unwrap();
        assert_eq!(kind, "subagent.failed");
        assert_eq!(failed["error"], "Child agent produced no output.");
    }

    #[test]
    fn other_spawn_tools_stay_plain_tool_cards() {
        let mut runs = PiSubagents::default();
        let mut started = spawn_tool("started", "other", json!({}));
        started["arguments"] = json!({ "command": "ls" });
        assert!(runs.tool_event("tool.started", &started).is_none());
        let done = spawn_tool(
            "completed",
            "other",
            json!({ "result": { "content": text("pid 42"), "details": { "pid": 42 } } }),
        );
        assert!(runs.tool_event("tool.completed", &done).is_none());
        assert!(runs.close_foreground().is_empty());
    }

    #[test]
    fn foreground_runs_left_open_are_cancelled_but_background_ones_are_not() {
        let mut runs = PiSubagents::default();
        runs.tool_event("tool.started", &spawn_tool("started", "spawn-1", json!({})));
        runs.tool_event("tool.started", &tool("started", "agent-fg", json!({})));
        runs.tool_event("tool.started", &tool("started", "agent-bg", json!({})));
        let launched = tool(
            "completed",
            "agent-bg",
            json!({ "result": {
                "content": text("Agent started in background.\nAgent ID: bg-1\n"),
                "details": details("background", json!({ "agentId": "bg-1" })),
            } }),
        );
        runs.tool_event("tool.completed", &launched).unwrap();

        let closed = runs.close_foreground();
        let ids: Vec<_> = closed
            .iter()
            .map(|(kind, payload)| (*kind, payload["subagentId"].as_str().unwrap()))
            .collect();
        assert_eq!(
            ids,
            [
                ("subagent.cancelled", "agent-fg"),
                ("subagent.cancelled", "spawn-1")
            ]
        );
        assert!(runs.running(), "the background run still holds the process");
        assert!(runs.close_foreground().is_empty());
        // A late result for a closed run adds nothing.
        let late = spawn_tool(
            "completed",
            "spawn-1",
            json!({ "result": { "content": text("done"), "details": spawn_details("success", json!({})) } }),
        );
        assert!(runs.tool_event("tool.completed", &late).is_none());
    }
}
