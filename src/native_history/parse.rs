//! Reading Claude Code, Pi and Codex session records — listing metadata
//! (title, cwd, creation time) from a bounded head and tail of a file — and
//! Grok Build's `summary.json`; plus the mapping of records (and Grok's ACP
//! update log) to transcript messages.

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
        NativeProvider::Omp => omp_meta(head, tail),
        NativeProvider::Codex => codex_meta(head),
        // Grok reads `summary.json`, Devin lists through its CLI.
        NativeProvider::GrokBuild | NativeProvider::Devin => None,
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
        NativeProvider::Pi | NativeProvider::Omp => pi_messages(record, out),
        NativeProvider::Codex => codex_messages(record, out),
        // Grok merges chunks across records: see [`GrokTranscript`].
        NativeProvider::GrokBuild | NativeProvider::Devin => {}
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

/// An omp session file: Pi's layout behind a fixed-width
/// `{"type":"title",…}` slot (legacy files start with the header). A title
/// in the slot or the header wins over the first prompt.
fn omp_meta(head: &[String], tail: &[String]) -> Option<SessionMeta> {
    let slot = head
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("title"));
    let head = if slot.is_some() { &head[1..] } else { head };
    let header_title = head
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|header| non_empty_str(header.get("title")));
    let mut meta = pi_meta(head, tail)?;
    if let Some(title) = slot
        .and_then(|slot| non_empty_str(slot.get("title")))
        .or(header_title)
        .and_then(|title| title_text(&title))
    {
        meta.title = Some(title);
    }
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

/// The header of a Codex rollout: `{"type":"session_meta","payload":{"id",
/// "timestamp","cwd",…}}`; the title is the first real user prompt in
/// `head` (names set with `thread/name/set` live in `session_index.jsonl`
/// and are applied by the caller).
fn codex_meta(head: &[String]) -> Option<SessionMeta> {
    let header = serde_json::from_str::<Value>(head.first()?).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = header.get("payload")?;
    let mut meta = SessionMeta {
        id: non_empty_str(payload.get("id")),
        title: None,
        cwd: non_empty_str(payload.get("cwd")),
        created_at: timestamp(payload.get("timestamp"))
            .or_else(|| timestamp(header.get("timestamp"))),
    };
    for line in head.iter().skip(1) {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(item) = record.get("payload") else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("user")
        {
            if let Some(text) = codex_user_text(item) {
                meta.title = title_text(&text);
                break;
            }
        }
    }
    Some(meta)
}

/// The text of a real Codex user message: context Codex injects as user
/// messages (`<environment_context>`, `<user_instructions>`, AGENTS.md
/// instructions, …) is left out.
fn codex_user_text(item: &Value) -> Option<String> {
    let text = codex_content_text(item.get("content")?);
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('<')
        || trimmed.starts_with("# AGENTS.md instructions")
    {
        return None;
    }
    Some(trimmed.to_owned())
}

/// `input_text` / `output_text` parts joined; images become `[image]`.
fn codex_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match block_type(part) {
                Some("input_text" | "output_text" | "text") => {
                    part.get("text").and_then(Value::as_str).map(str::to_owned)
                }
                Some("input_image" | "image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A Codex rollout record: `response_item` payloads are the conversation
/// (user and assistant messages, tool calls and their outputs); developer
/// and system messages, injected context and reasoning are skipped, and so
/// are the `event_msg` records that repeat the same content for UIs.
fn codex_messages(record: &Value, out: &mut Vec<TranscriptMessage>) {
    let at = timestamp(record.get("timestamp"));
    let Some(item) = record.get("payload") else {
        return;
    };
    match record.get("type").and_then(Value::as_str) {
        Some("response_item") => {}
        Some("compacted") => {
            if let Some(message) = item.get("message").and_then(Value::as_str) {
                push(out, "system", message, MESSAGE_CHARS, at);
            }
            return;
        }
        _ => return,
    }
    match item.get("type").and_then(Value::as_str) {
        Some("message") => match item.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = codex_user_text(item) {
                    push(out, "user", &text, MESSAGE_CHARS, at);
                }
            }
            Some("assistant") => {
                if let Some(content) = item.get("content") {
                    push(
                        out,
                        "assistant",
                        &codex_content_text(content),
                        MESSAGE_CHARS,
                        at,
                    );
                }
            }
            _ => {}
        },
        Some(
            kind @ ("function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call"),
        ) => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(match kind {
                    "local_shell_call" => "shell",
                    "web_search_call" => "web_search",
                    _ => "tool",
                });
            let args = match item
                .get("arguments")
                .or_else(|| item.get("input"))
                .or_else(|| item.get("action"))
            {
                Some(Value::String(text)) => text.clone(),
                Some(value) => value.to_string(),
                None => String::new(),
            };
            push(out, "tool", &format!("{name}: {args}"), TOOL_CALL_CHARS, at);
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let preview = match item.get("output") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(parts)) => codex_content_text(&Value::Array(parts.clone())),
                Some(Value::Object(output)) => match output.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(content) => codex_content_text(content),
                    None => String::new(),
                },
                _ => String::new(),
            };
            push_tool_result(out, &preview, at);
        }
        // `reasoning` and anything newer carry no transcript text.
        _ => {}
    }
}

/// The listing fields of a Grok Build `summary.json` (`info.id`,
/// `info.cwd`, `generated_title` else `session_summary`, `created_at`).
/// `None` for an unreadable file and for sessions Grok itself hides from
/// listings (`hidden`, or a `subagent*` session kind without an explicit
/// `hidden: false`), which `grok sessions delete` cannot find either.
pub(super) fn grok_summary(summary: &Value) -> Option<SessionMeta> {
    let hidden = summary
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| {
            summary
                .get("session_kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("subagent"))
        });
    if hidden {
        return None;
    }
    let info = summary.get("info")?;
    let title = non_empty_str(summary.get("generated_title"))
        .or_else(|| non_empty_str(summary.get("session_summary")));
    Some(SessionMeta {
        id: Some(non_empty_str(info.get("id"))?),
        title: title.and_then(|title| title_text(&title)),
        cwd: non_empty_str(info.get("cwd")),
        created_at: timestamp(summary.get("created_at")),
    })
}

/// Grok Build's `updates.jsonl` — one persisted ACP `session/update` (or
/// `_x.ai/session/update`) notification per line, stamped with
/// `timestamp` in Unix seconds — mapped to transcript messages. Message
/// chunks stream across records, so consecutive chunks of one role are
/// merged; thoughts are skipped; a tool call becomes a `tool` message and
/// its first completed or failed update a `tool` result preview.
#[derive(Default)]
pub(super) struct GrokTranscript {
    role: Option<&'static str>,
    text: String,
    at: Option<DateTime<Utc>>,
    finished_tools: std::collections::HashSet<String>,
}

impl GrokTranscript {
    pub(super) fn push_record(&mut self, record: &Value, out: &mut Vec<TranscriptMessage>) {
        if !matches!(
            record.get("method").and_then(Value::as_str),
            None | Some("session/update" | "_x.ai/session/update")
        ) {
            return;
        }
        let Some(update) = record.pointer("/params/update") else {
            return;
        };
        let at = record
            .get("timestamp")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0));
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some(kind @ ("user_message_chunk" | "agent_message_chunk")) => {
                let role = if kind == "user_message_chunk" {
                    "user"
                } else {
                    "assistant"
                };
                let Some(content) = update.get("content") else {
                    return;
                };
                // Turns the host injected are not the user's words.
                if role == "user"
                    && update.pointer("/_meta/hostTurn").and_then(Value::as_bool) == Some(true)
                {
                    return;
                }
                let part = grok_chunk_text(content);
                if part.is_empty() {
                    return;
                }
                if self.role != Some(role) {
                    self.flush(out);
                    self.role = Some(role);
                    self.at = at;
                } else if role == "user" && !self.text.is_empty() {
                    // User chunks are whole content blocks, agent chunks
                    // stream tokens.
                    self.text.push('\n');
                }
                // Bounded: the message is cut to MESSAGE_CHARS anyway.
                if self.text.len() < 4 * MESSAGE_CHARS {
                    self.text.push_str(&part);
                }
            }
            Some("tool_call") => {
                self.flush(out);
                let title = update
                    .get("title")
                    .and_then(Value::as_str)
                    .or_else(|| update.get("kind").and_then(Value::as_str))
                    .unwrap_or("tool");
                let summary = match update.get("rawInput") {
                    Some(Value::Null) | None => title.to_owned(),
                    Some(input) => format!("{title}: {input}"),
                };
                push(out, "tool", &summary, TOOL_CALL_CHARS, at);
            }
            Some("tool_call_update") => {
                let status = update.get("status").and_then(Value::as_str);
                if !matches!(status, Some("completed" | "failed")) {
                    return;
                }
                let id = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if !self.finished_tools.insert(id) {
                    return;
                }
                self.flush(out);
                let mut preview = update
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| match block_type(item) {
                                Some("content") => item.get("content").map(grok_chunk_text),
                                Some("diff") => Some(format!(
                                    "diff: {}",
                                    item.get("path").and_then(Value::as_str).unwrap_or("")
                                )),
                                Some("terminal") => Some("[terminal]".to_owned()),
                                _ => None,
                            })
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if preview.trim().is_empty() {
                    preview = match update.get("rawOutput") {
                        Some(Value::String(text)) => text.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(value) => value.to_string(),
                    };
                }
                push_tool_result(out, &preview, at);
            }
            // Thoughts, plans, commands, mode changes, rewind markers and
            // xAI extensions carry no transcript text; they do not break a
            // run of message chunks either.
            _ => {}
        }
    }

    /// Emits the message being merged, if any.
    pub(super) fn flush(&mut self, out: &mut Vec<TranscriptMessage>) {
        if let Some(role) = self.role.take() {
            push(out, role, &self.text, MESSAGE_CHARS, self.at.take());
        }
        self.text.clear();
    }
}

/// The text of an ACP content block: text (or the typed `displayText` of
/// a mid-turn interjection), `[image]`, or a resource's name.
fn grok_chunk_text(content: &Value) -> String {
    match block_type(content) {
        Some("text") => content
            .pointer("/_meta/displayText")
            .or_else(|| content.get("displayText"))
            .or_else(|| content.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("image") => "[image]".to_owned(),
        Some("resource_link") => content
            .get("name")
            .or_else(|| content.get("uri"))
            .and_then(Value::as_str)
            .map(|name| format!("[{name}]"))
            .unwrap_or_default(),
        Some("resource") => content
            .pointer("/resource/uri")
            .and_then(Value::as_str)
            .map(|uri| format!("[{uri}]"))
            .unwrap_or_default(),
        _ => String::new(),
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
