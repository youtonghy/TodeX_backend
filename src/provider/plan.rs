//! Normalized `plan.updated` payloads.
//!
//! Every provider reports its working plan in its own shape; clients read the
//! common `steps` list (`{ text, status }` with `pending`, `in_progress` or
//! `completed`) and an optional `explanation`. The upstream value stays under
//! `plan` for older clients.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// A provider step status as `pending`, `in_progress` or `completed`;
/// anything unknown counts as pending.
fn step_status(status: Option<&Value>) -> &'static str {
    match status.and_then(Value::as_str) {
        Some("completed") => "completed",
        Some("inProgress" | "in_progress") => "in_progress",
        _ => "pending",
    }
}

/// Steps from a list of items, reading the text from `text_key`; items
/// without text are skipped.
fn steps(items: Option<&Value>, text_key: &str) -> Vec<Value> {
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let text = item.get(text_key).and_then(Value::as_str)?.trim();
            (!text.is_empty())
                .then(|| json!({ "text": text, "status": step_status(item.get("status")) }))
        })
        .collect()
}

fn payload(provider: &str, explanation: Option<&Value>, steps: Vec<Value>, raw: Value) -> Value {
    let mut payload = Map::new();
    payload.insert("provider".to_owned(), json!(provider));
    if let Some(explanation) = explanation
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        payload.insert("explanation".to_owned(), json!(explanation));
    }
    payload.insert("steps".to_owned(), Value::Array(steps));
    payload.insert("plan".to_owned(), raw);
    Value::Object(payload)
}

/// Codex `turn/plan/updated`: `{ explanation?, plan: [{ step, status }] }`.
pub(super) fn codex_plan_payload(params: &Value) -> Value {
    payload(
        "codex",
        params.get("explanation"),
        steps(params.get("plan"), "step"),
        params.get("plan").cloned().unwrap_or(Value::Null),
    )
}

/// ACP `plan` session update: `{ entries: [{ content, priority, status }] }`.
pub(super) fn acp_plan_payload(provider: &str, update: &Value) -> Value {
    payload(
        provider,
        None,
        steps(update.get("entries"), "content"),
        update.clone(),
    )
}

/// Claude Code `TodoWrite` input `{ todos: [{ content, status, activeForm }] }`;
/// `None` for any other tool or input without a todo list.
pub(super) fn claude_todo_plan_payload(tool_name: Option<&Value>, input: &Value) -> Option<Value> {
    if tool_name.and_then(Value::as_str) != Some("TodoWrite") {
        return None;
    }
    let todos = input.get("todos").filter(|todos| todos.is_array())?;
    Some(payload(
        "claude-code",
        None,
        steps(Some(todos), "content"),
        todos.clone(),
    ))
}

#[derive(Clone, PartialEq)]
struct PiTask {
    subject: String,
    status: &'static str,
}

/// The `@tintinweb/pi-tasks` tasks of one Pi session, followed through its
/// task tools: Pi's RPC mode drops the extension's own task widget, so the
/// completed `TaskCreate`, `TaskUpdate` and `TaskList` calls are the only
/// signal. A new id only appears in `TaskCreate`'s result text
/// (`Task #N created successfully: …`).
#[derive(Default)]
pub(super) struct PiTasks {
    tasks: BTreeMap<u64, PiTask>,
}

/// The text of a Pi tool result: `{ content: [{ type: "text", text }] }`.
fn pi_result_text(result: Option<&Value>) -> String {
    match result {
        Some(Value::String(text)) => text.clone(),
        Some(result) => result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

/// A task id: `"3"` or `3`.
fn pi_task_id(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::String(id) => id.trim().parse().ok(),
        value => value.as_u64(),
    }
}

/// The leading `N` of `N…`.
fn leading_id(text: &str) -> Option<(u64, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    Some((text[..end].parse().ok()?, &text[end..]))
}

impl PiTasks {
    /// Applies a completed tool call (`tool.completed` payload); true when
    /// the task list changed. Other tools, failed calls and results it cannot
    /// read leave the list alone.
    pub(super) fn apply(&mut self, tool: &Value) -> bool {
        let name = tool.get("toolName").and_then(Value::as_str).unwrap_or("");
        if !matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList")
            || tool.get("isError").and_then(Value::as_bool) == Some(true)
        {
            return false;
        }
        let args = tool.get("arguments").unwrap_or(&Value::Null);
        let text = pi_result_text(tool.get("result"));
        let before = self.tasks.clone();
        match name {
            "TaskCreate" => self.create(args, &text),
            "TaskUpdate" => self.update(args, &text),
            _ => self.list(&text),
        }
        self.tasks != before
    }

    fn create(&mut self, args: &Value, text: &str) {
        let Some((id, rest)) = text.trim().strip_prefix("Task #").and_then(leading_id) else {
            return;
        };
        let Some(echoed) = rest.strip_prefix(" created successfully:") else {
            return;
        };
        let subject = args
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or(echoed)
            .trim();
        // pi-tasks clears a finished list when the next batch starts.
        if !self.has_open() {
            self.tasks.clear();
        }
        self.tasks.insert(
            id,
            PiTask {
                subject: subject.to_owned(),
                status: "pending",
            },
        );
    }

    fn update(&mut self, args: &Value, text: &str) {
        // `Task #N not found` and other refusals change nothing.
        if !text.trim().starts_with("Updated task #") {
            return;
        }
        let Some(id) = pi_task_id(args.get("taskId")) else {
            return;
        };
        if args.get("status").and_then(Value::as_str) == Some("deleted") {
            self.tasks.remove(&id);
            return;
        }
        let subject = args
            .get("subject")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|subject| !subject.is_empty());
        // A task made before TodeX followed this runtime is only worth
        // showing once its subject is known.
        if let (false, Some(subject)) = (self.tasks.contains_key(&id), subject) {
            self.tasks.insert(
                id,
                PiTask {
                    subject: subject.to_owned(),
                    status: "pending",
                },
            );
        }
        let Some(task) = self.tasks.get_mut(&id) else {
            return;
        };
        if let Some(subject) = subject {
            task.subject = subject.to_owned();
        }
        if let Some(status) = args.get("status") {
            task.status = step_status(Some(status));
        }
    }

    /// `TaskList` lists every task (`#N [status] subject`, optionally
    /// followed by ` (owner)` and ` [blocked by #a, #b]`), so it replaces
    /// the list; known tasks keep their subject.
    fn list(&mut self, text: &str) {
        let text = text.trim();
        if text == "No tasks found" {
            self.tasks.clear();
            return;
        }
        let mut tasks = BTreeMap::new();
        for line in text.lines() {
            let Some((id, rest)) = line.trim().strip_prefix('#').and_then(leading_id) else {
                return;
            };
            let Some((status, subject)) = rest
                .strip_prefix(" [")
                .and_then(|rest| rest.split_once("] "))
            else {
                return;
            };
            let subject = match self.tasks.get(&id) {
                Some(task) => task.subject.clone(),
                None => subject
                    .rsplit_once(" [blocked by #")
                    .map_or(subject, |(subject, _)| subject)
                    .trim()
                    .to_owned(),
            };
            tasks.insert(
                id,
                PiTask {
                    subject,
                    status: step_status(Some(&json!(status))),
                },
            );
        }
        self.tasks = tasks;
    }

    /// Whether any task is still pending or in progress.
    pub(super) fn has_open(&self) -> bool {
        self.tasks.values().any(|task| task.status != "completed")
    }

    /// The tasks as a `plan.updated` payload, in id order; `plan` lists
    /// `{ id, subject, status }`.
    pub(super) fn plan_payload(&self) -> Value {
        let steps = self
            .tasks
            .values()
            .map(|task| json!({ "text": task.subject, "status": task.status }))
            .collect();
        let raw = self
            .tasks
            .iter()
            .map(|(id, task)| json!({ "id": id.to_string(), "subject": task.subject, "status": task.status }))
            .collect();
        payload("pi", None, steps, Value::Array(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_plan_keeps_explanation_and_maps_steps() {
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "explanation": "  Fix the parser first ",
            "plan": [
                { "step": "Read the parser", "status": "completed" },
                { "step": "Patch the bug", "status": "inProgress" },
                { "step": "Add a test", "status": "pending" },
                { "step": "", "status": "pending" },
            ],
        });
        let payload = codex_plan_payload(&params);
        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["explanation"], "Fix the parser first");
        assert_eq!(
            payload["steps"],
            json!([
                { "text": "Read the parser", "status": "completed" },
                { "text": "Patch the bug", "status": "in_progress" },
                { "text": "Add a test", "status": "pending" },
            ])
        );
        assert_eq!(payload["plan"], params["plan"]);
        let bare = codex_plan_payload(&json!({ "plan": [] }));
        assert!(bare.get("explanation").is_none());
        assert_eq!(bare["steps"], json!([]));
    }

    #[test]
    fn acp_plan_maps_entries_and_unknown_statuses() {
        let update = json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "Scan files", "priority": "high", "status": "completed" },
                { "content": "Edit", "priority": "medium", "status": "in_progress" },
                { "content": "Ship", "priority": "low", "status": "blocked" },
                { "content": "Review", "priority": "low" },
            ],
        });
        let payload = acp_plan_payload("opencode", &update);
        assert_eq!(payload["provider"], "opencode");
        assert!(payload.get("explanation").is_none());
        assert_eq!(
            payload["steps"],
            json!([
                { "text": "Scan files", "status": "completed" },
                { "text": "Edit", "status": "in_progress" },
                { "text": "Ship", "status": "pending" },
                { "text": "Review", "status": "pending" },
            ])
        );
        assert_eq!(payload["plan"], update);
    }

    #[test]
    fn claude_todo_write_becomes_a_plan() {
        let input = json!({ "todos": [
            { "content": "Run tests", "status": "completed", "activeForm": "Running tests" },
            { "content": "Fix lint", "status": "in_progress", "activeForm": "Fixing lint" },
        ]});
        let payload = claude_todo_plan_payload(Some(&json!("TodoWrite")), &input).unwrap();
        assert_eq!(payload["provider"], "claude-code");
        assert_eq!(
            payload["steps"],
            json!([
                { "text": "Run tests", "status": "completed" },
                { "text": "Fix lint", "status": "in_progress" },
            ])
        );
        assert_eq!(payload["plan"], input["todos"]);
        assert!(claude_todo_plan_payload(Some(&json!("Bash")), &input).is_none());
        assert!(claude_todo_plan_payload(Some(&json!("TodoWrite")), &json!({})).is_none());
        assert!(claude_todo_plan_payload(None, &input).is_none());
    }

    /// A completed Pi tool call as `pi_tool_payload` reports it.
    fn pi_tool(name: &str, arguments: Value, text: &str) -> Value {
        json!({
            "provider": "pi", "toolName": name, "arguments": arguments, "isError": false,
            "result": { "content": [{ "type": "text", "text": text }] },
        })
    }

    fn pi_steps(tasks: &PiTasks) -> Value {
        tasks.plan_payload()["steps"].clone()
    }

    #[test]
    fn pi_task_tools_build_the_plan() {
        let mut tasks = PiTasks::default();
        assert!(tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "Start the server", "description": "…" }),
            "Task #1 created successfully: Start the server",
        )));
        assert!(tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "Run the matrix", "description": "…" }),
            "Task #2 created successfully: Run the matrix",
        )));
        assert!(tasks.apply(&pi_tool(
            "TaskUpdate",
            json!({ "taskId": "1", "status": "in_progress" }),
            "Updated task #1 status",
        )));
        assert_eq!(
            pi_steps(&tasks),
            json!([
                { "text": "Start the server", "status": "in_progress" },
                { "text": "Run the matrix", "status": "pending" },
            ])
        );
        let payload = tasks.plan_payload();
        assert_eq!(payload["provider"], "pi");
        assert_eq!(
            payload["plan"][1],
            json!({ "id": "2", "subject": "Run the matrix", "status": "pending" })
        );
        assert!(tasks.has_open());

        // Renames, completions and deletions follow the update.
        assert!(tasks.apply(&pi_tool(
            "TaskUpdate",
            json!({ "taskId": 1, "status": "completed", "subject": "Serve on 8300" }),
            "Updated task #1 status, subject",
        )));
        assert!(tasks.apply(&pi_tool(
            "TaskUpdate",
            json!({ "taskId": "2", "status": "deleted" }),
            "Updated task #2 deleted",
        )));
        assert_eq!(
            pi_steps(&tasks),
            json!([{ "text": "Serve on 8300", "status": "completed" }])
        );
        assert!(!tasks.has_open());

        // The next batch after a finished list starts over, as pi-tasks does.
        assert!(tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "Write the config" }),
            "Task #3 created successfully: Write the config",
        )));
        assert_eq!(
            pi_steps(&tasks),
            json!([{ "text": "Write the config", "status": "pending" }])
        );
    }

    #[test]
    fn pi_task_refusals_and_other_tools_change_nothing() {
        let mut tasks = PiTasks::default();
        tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "A" }),
            "Task #1 created successfully: A",
        ));
        assert!(!tasks.apply(&pi_tool(
            "TaskUpdate",
            json!({ "taskId": "9", "status": "completed" }),
            "Task #9 not found",
        )));
        // An update to a task created before it was followed, without a subject.
        assert!(!tasks.apply(&pi_tool(
            "TaskUpdate",
            json!({ "taskId": "7", "status": "completed" }),
            "Updated task #7 status",
        )));
        let mut failed = pi_tool(
            "TaskUpdate",
            json!({ "taskId": "1", "status": "completed" }),
            "boom",
        );
        failed["isError"] = json!(true);
        assert!(!tasks.apply(&failed));
        assert!(!tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "B" }),
            "Something else"
        )));
        assert!(!tasks.apply(&pi_tool(
            "bash",
            json!({ "command": "ls" }),
            "Task #2 created successfully: x"
        )));
        // A repeated identical update is not a change.
        assert!(!tasks.apply(&pi_tool(
            "TaskUpdate",
            json!({ "taskId": "1", "status": "pending" }),
            "Updated task #1 status",
        )));
        assert_eq!(
            pi_steps(&tasks),
            json!([{ "text": "A", "status": "pending" }])
        );
    }

    #[test]
    fn pi_task_list_resyncs_the_plan() {
        let mut tasks = PiTasks::default();
        tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "Keep (my) subject" }),
            "Task #1 created successfully: Keep (my) subject",
        ));
        tasks.apply(&pi_tool(
            "TaskCreate",
            json!({ "subject": "Gone" }),
            "Task #2 created successfully: Gone",
        ));
        // Task 2 was cleared silently, task 3 came from elsewhere, task 1
        // finished without an update.
        assert!(tasks.apply(&pi_tool(
            "TaskList",
            json!({}),
            "#3 [pending] Review results (agent-1) [blocked by #1]\n#1 [completed] Keep (my) subject",
        )));
        assert_eq!(
            pi_steps(&tasks),
            json!([
                { "text": "Keep (my) subject", "status": "completed" },
                { "text": "Review results (agent-1)", "status": "pending" },
            ])
        );
        // An unreadable list is ignored; an empty one clears the plan.
        assert!(!tasks.apply(&pi_tool(
            "TaskList",
            json!({}),
            "#1 [completed] ok\nnot a task line"
        )));
        assert!(tasks.apply(&pi_tool("TaskList", json!({}), "No tasks found")));
        assert_eq!(pi_steps(&tasks), json!([]));
    }
}
