//! Reading Claude Code and Pi session records: listing metadata (title, cwd,
//! creation time) from a bounded head and tail of a file, and the mapping of
//! records to transcript messages.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::NativeProvider;

/// Longest listed title, in characters.
pub(super) const TITLE_CHARS: usize = 200;
/// Longest transcript message text, in characters (`…` appended past it).
const MESSAGE_CHARS: usize = 8000;
/// Longest tool call summary (`name: <args>`), in characters.
const TOOL_CALL_CHARS: usize = 300;
/// Longest tool result preview, in characters.
const TOOL_RESULT_CHARS: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SessionMeta {
    /// The session id from a Pi header; Claude ids come from the file name.
    pub id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct TranscriptMessage {
    pub role: &'static str,
    pub text: String,
    pub at: Option<DateTime<Utc>>,
}

/// Listing metadata from the first and last lines of a session file (`tail`
/// follows `head` in the file and never overlaps it). `None` when the file
/// is not a session of `provider` (a Pi file without a session header).
pub(super) fn session_meta(
    provider: NativeProvider,
    head: &[String],
    tail: &[String],
) -> Option<SessionMeta> {
    match provider {
        NativeProvider::ClaudeCode => Some(claude_meta(head, tail)),
        NativeProvider::Pi => pi_meta(head, tail),
    }
}

/// Appends the transcript messages one record (one JSONL line) shows.
pub(super) fn record_messages(
    provider: NativeProvider,
    record: &Value,
    out: &mut Vec<TranscriptMessage>,
) {
    match provider {
        NativeProvider::ClaudeCode => claude_messages(record, out),
        NativeProvider::Pi => pi_messages(record, out),
    }
}

fn claude_meta(head: &[String], tail: &[String]) -> SessionMeta {
    let mut meta = SessionMeta::default();
    let (mut custom, mut ai, mut summary, mut first_user) = (None, None, None, None);
    for line in head.iter().chain(tail) {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if meta.cwd.is_none() {
            meta.cwd = non_empty_str(record.get("cwd"));
        }
        if meta.created_at.is_none() {
            meta.created_at = timestamp(record.get("timestamp"));
        }
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                custom = non_empty_str(record.get("customTitle")).or(custom);
            }
            Some("ai-title") => ai = non_empty_str(record.get("aiTitle")).or(ai),
            Some("summary") => summary = non_empty_str(record.get("summary")).or(summary),
            Some("user") if first_user.is_none() => first_user = claude_user_title(&record),
            _ => {}
        }
    }
    meta.title = custom
        .or(ai)
        .or(summary)
        .or(first_user)
        .and_then(|title| title_text(&title));
    meta
}

/// The text of a real user prompt: not a sidechain, meta, compact-summary or
/// tool-result record, and not a command or notification wrapper (`<…>`).
fn claude_user_title(record: &Value) -> Option<String> {
    if flag(record, "isSidechain") || flag(record, "isMeta") || flag(record, "isCompactSummary") {
        return None;
    }
    let content = record.pointer("/message/content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            if blocks
                .iter()
                .any(|block| block_type(block) == Some("tool_result"))
            {
                return None;
            }
            text_blocks(blocks)
        }
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('<')).then(|| text.to_owned())
}

/// The header of a Pi session file: `{"type":"session","id",…}`.
fn pi_header(line: &str) -> Option<SessionMeta> {
    let header = serde_json::from_str::<Value>(line).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    Some(SessionMeta {
        id: Some(non_empty_str(header.get("id"))?),
        title: None,
        cwd: non_empty_str(header.get("cwd")),
        created_at: timestamp(header.get("timestamp")),
    })
}

fn pi_meta(head: &[String], tail: &[String]) -> Option<SessionMeta> {
    let mut meta = pi_header(head.first()?)?;
    let (mut name, mut first_user) = (None, None);
    for line in head.iter().skip(1).chain(tail) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("session_info") => name = non_empty_str(entry.get("name")).or(name),
            Some("message")
                if first_user.is_none()
                    && entry.pointer("/message/role").and_then(Value::as_str) == Some("user") =>
            {
                first_user = match entry.pointer("/message/content") {
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(Value::Array(blocks)) => Some(text_blocks(blocks)),
                    _ => None,
                }
                .filter(|text| !text.trim().is_empty());
            }
            _ => {}
        }
    }
    meta.title = name.or(first_user).and_then(|title| title_text(&title));
    Some(meta)
}

fn claude_messages(record: &Value, out: &mut Vec<TranscriptMessage>) {
    if flag(record, "isSidechain") {
        return;
    }
    let at = timestamp(record.get("timestamp"));
    match record.get("type").and_then(Value::as_str) {
        Some("user") => {
            if flag(record, "isMeta") {
                return;
            }
            let role = if flag(record, "isCompactSummary") {
                "system"
            } else {
                "user"
            };
            match record.pointer("/message/content") {
                Some(Value::String(text)) => push(out, role, text, MESSAGE_CHARS, at),
                Some(Value::Array(blocks)) => content_messages(blocks, role, at, out),
                _ => {}
            }
        }
        Some("assistant") => {
            if let Some(Value::Array(blocks)) = record.pointer("/message/content") {
                content_messages(blocks, "assistant", at, out);
            } else if let Some(Value::String(text)) = record.pointer("/message/content") {
                push(out, "assistant", text, MESSAGE_CHARS, at);
            }
        }
        Some("system") => {
            if let Some(text) = record.get("content").and_then(Value::as_str) {
                push(out, "system", text, MESSAGE_CHARS, at);
            }
        }
        _ => {}
    }
}

fn pi_messages(entry: &Value, out: &mut Vec<TranscriptMessage>) {
    let at = timestamp(entry.get("timestamp"));
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => {}
        Some("compaction" | "branch_summary") => {
            if let Some(summary) = entry.get("summary").and_then(Value::as_str) {
                push(out, "system", summary, MESSAGE_CHARS, at);
            }
            return;
        }
        _ => return,
    }
    let Some(message) = entry.get("message") else {
        return;
    };
    let content = message.get("content");
    match message.get("role").and_then(Value::as_str) {
        Some(role @ ("user" | "assistant")) => {
            let role = if role == "user" { "user" } else { "assistant" };
            match content {
                Some(Value::String(text)) => push(out, role, text, MESSAGE_CHARS, at),
                Some(Value::Array(blocks)) => content_messages(blocks, role, at, out),
                _ => {}
            }
        }
        Some("toolResult") => {
            let preview = match content {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(blocks)) => text_blocks(blocks),
                _ => String::new(),
            };
            push_tool_result(out, &preview, at);
        }
        Some("bashExecution") => {
            let command = message.get("command").and_then(Value::as_str).unwrap_or("");
            let output = message.get("output").and_then(Value::as_str).unwrap_or("");
            push(
                out,
                "tool",
                &format!("$ {command}\n{output}"),
                TOOL_RESULT_CHARS,
                at,
            );
        }
        Some("compactionSummary" | "branchSummary") => {
            if let Some(summary) = message.get("summary").and_then(Value::as_str) {
                push(out, "system", summary, MESSAGE_CHARS, at);
            }
        }
        // `system` is the system prompt; other roles carry no transcript text.
        _ => {}
    }
}

/// Maps content blocks in order: adjacent text (and image placeholders)
/// become one `role` message, tool calls and results become `tool`
/// messages, thinking is skipped.
fn content_messages(
    blocks: &[Value],
    role: &'static str,
    at: Option<DateTime<Utc>>,
    out: &mut Vec<TranscriptMessage>,
) {
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<TranscriptMessage>| {
        push(out, role, text, MESSAGE_CHARS, at);
        text.clear();
    };
    for block in blocks {
        match block_type(block) {
            Some("text") => {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
            }
            Some("image") => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[image]");
            }
            // Claude `tool_use {name, input}`, Pi `toolCall {name, arguments}`.
            Some("tool_use" | "toolCall") => {
                flush(&mut text, out);
                let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                let args = block
                    .get("input")
                    .or_else(|| block.get("arguments"))
                    .map(Value::to_string)
                    .unwrap_or_default();
                push(out, "tool", &format!("{name}: {args}"), TOOL_CALL_CHARS, at);
            }
            Some("tool_result") => {
                flush(&mut text, out);
                let preview = match block.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(parts)) => text_blocks(parts),
                    _ => String::new(),
                };
                push_tool_result(out, &preview, at);
            }
            _ => {}
        }
    }
    flush(&mut text, out);
}

fn push_tool_result(out: &mut Vec<TranscriptMessage>, preview: &str, at: Option<DateTime<Utc>>) {
    let preview = if preview.trim().is_empty() {
        "(no output)"
    } else {
        preview
    };
    push(out, "tool", preview, TOOL_RESULT_CHARS, at);
}

fn push(
    out: &mut Vec<TranscriptMessage>,
    role: &'static str,
    text: &str,
    max_chars: usize,
    at: Option<DateTime<Utc>>,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    out.push(TranscriptMessage {
        role,
        text: cap(text, max_chars),
        at,
    });
}

/// `text` cut to `max_chars` characters, with `…` appended when cut.
pub(super) fn cap(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// A title on one line, whitespace collapsed, at most [`TITLE_CHARS`].
fn title_text(text: &str) -> Option<String> {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    Some(line.chars().take(TITLE_CHARS).collect())
}

fn text_blocks(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block_type(block) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn flag(record: &Value, key: &str) -> bool {
    record.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// An RFC 3339 string or epoch milliseconds.
fn timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value? {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|at| at.with_timezone(&Utc)),
        Value::Number(number) => DateTime::from_timestamp_millis(number.as_i64()?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lines(values: &[Value]) -> Vec<String> {
        values.iter().map(Value::to_string).collect()
    }

    fn messages(provider: NativeProvider, records: &[Value]) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for record in records {
            record_messages(provider, record, &mut out);
        }
        out.into_iter()
            .map(|message| (message.role, message.text))
            .collect()
    }

    #[test]
    fn claude_title_prefers_generated_titles_then_the_first_real_prompt() {
        let head = lines(&[
            json!({"type": "user", "isMeta": true, "cwd": "/w", "timestamp": "2026-10-01T08:00:00Z",
                   "message": {"role": "user", "content": "<local-command-caveat>x"}}),
            json!({"type": "user", "message": {"role": "user", "content": "<command-name>/clear</command-name>"}}),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "out"}]}}),
            json!({"type": "user", "isSidechain": true, "message": {"role": "user", "content": "side"}}),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "text", "text": "  Fix the\n build  "}]}}),
        ]);
        let meta = session_meta(NativeProvider::ClaudeCode, &head, &[]).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Fix the build"));
        assert_eq!(meta.cwd.as_deref(), Some("/w"));
        assert_eq!(
            meta.created_at,
            Some("2026-10-01T08:00:00Z".parse().unwrap())
        );

        let tail = lines(&[
            json!({"type": "ai-title", "aiTitle": "First title"}),
            json!({"type": "ai-title", "aiTitle": "Latest title"}),
        ]);
        let meta = session_meta(NativeProvider::ClaudeCode, &head, &tail).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Latest title"));
        let long = lines(&[json!({"type": "summary", "summary": "x".repeat(500)})]);
        let meta = session_meta(NativeProvider::ClaudeCode, &long, &[]).unwrap();
        assert_eq!(meta.title.unwrap().chars().count(), TITLE_CHARS);
    }

    #[test]
    fn pi_meta_needs_a_session_header() {
        assert_eq!(
            session_meta(NativeProvider::Pi, &lines(&[json!({"peer": 1})]), &[]),
            None
        );
        assert_eq!(session_meta(NativeProvider::Pi, &[], &[]), None);
        let head = lines(&[
            json!({"type": "session", "version": 3, "id": "s1", "timestamp": "2026-10-01T08:00:00.000Z", "cwd": "/p"}),
            json!({"type": "message", "id": "a", "message": {"role": "user", "content": [{"type": "text", "text": "hello pi"}]}}),
        ]);
        let meta = session_meta(NativeProvider::Pi, &head, &[]).unwrap();
        assert_eq!(meta.id.as_deref(), Some("s1"));
        assert_eq!(meta.title.as_deref(), Some("hello pi"));
        assert_eq!(meta.cwd.as_deref(), Some("/p"));
        let tail = lines(&[json!({"type": "session_info", "id": "b", "name": "Named"})]);
        let meta = session_meta(NativeProvider::Pi, &head, &tail).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Named"));
    }

    #[test]
    fn claude_records_map_to_transcript_messages() {
        let records = [
            json!({"type": "user", "timestamp": "2026-10-01T08:00:00Z", "message": {"role": "user", "content": "hi"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret"},
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "t", "name": "Read", "input": {"file_path": "/a"}}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": [{"type": "text", "text": "y".repeat(900)}]}]}}),
            json!({"type": "assistant", "isSidechain": true, "message": {"role": "assistant", "content": [{"type": "text", "text": "sub"}]}}),
            json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "meta"}}),
            json!({"type": "system", "content": "Conversation compacted"}),
            json!({"type": "attachment"}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "z".repeat(9000)}]}}),
        ];
        let out = messages(NativeProvider::ClaudeCode, &records);
        assert_eq!(out[0], ("user", "hi".to_owned()));
        assert_eq!(out[1], ("assistant", "Let me look.".to_owned()));
        assert_eq!(out[2], ("tool", r#"Read: {"file_path":"/a"}"#.to_owned()));
        assert_eq!(out[3].0, "tool");
        assert_eq!(out[3].1.chars().count(), 501);
        assert!(out[3].1.ends_with('…'));
        assert_eq!(out[4], ("system", "Conversation compacted".to_owned()));
        assert_eq!(out[5].1.chars().count(), 8001);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn pi_entries_map_to_transcript_messages() {
        let records = [
            json!({"type": "session", "id": "s"}),
            json!({"type": "message", "timestamp": "2026-10-01T08:00:00Z", "message": {"role": "system", "content": "prompt"}}),
            json!({"type": "message", "message": {"role": "user", "content": [{"type": "text", "text": "do it"}]}}),
            json!({"type": "message", "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "toolCall", "id": "c", "name": "bash", "arguments": {"command": "ls"}},
                {"type": "text", "text": "Done."}]}}),
            json!({"type": "message", "message": {"role": "toolResult", "toolName": "bash", "content": [{"type": "text", "text": "a.txt"}]}}),
            json!({"type": "custom", "data": {}}),
        ];
        let out = messages(NativeProvider::Pi, &records);
        assert_eq!(
            out,
            vec![
                ("user", "do it".to_owned()),
                ("tool", r#"bash: {"command":"ls"}"#.to_owned()),
                ("assistant", "Done.".to_owned()),
                ("tool", "a.txt".to_owned()),
            ]
        );
    }
}
