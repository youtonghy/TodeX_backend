//! Normalized `plan.updated` payloads.
//!
//! Every provider reports its working plan in its own shape; clients read the
//! common `steps` list (`{ text, status }` with `pending`, `in_progress` or
//! `completed`) and an optional `explanation`. The upstream value stays under
//! `plan` for older clients.

use serde_json::{json, Map, Value};

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
}
