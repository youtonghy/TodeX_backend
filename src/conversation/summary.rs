use serde_json::{Map, Value};

use super::model::{normalized_event_type, ConversationEvent};

/// Process events whose payloads only feed folded timeline rows. Every other
/// category produces visible output or drives control state, so it is never
/// reduced in `detail=summary` replays.
const DETAIL_BLOCK_CATEGORIES: [&str; 4] = ["tool", "reasoning", "status", "assistant_progress"];
const BLOCK_CATEGORIES: [&str; 8] = [
    "assistant_final",
    "assistant_progress",
    "reasoning",
    "tool",
    "approval",
    "status",
    "error",
    "usage",
];
const BLOCK_PHASES: [&str; 4] = ["started", "delta", "completed", "failed"];

/// Canonical type prefixes that drive control state, permissions, auxiliary
/// panels or visible output. `provider.event` is the generic provider wrapper
/// and is judged by its payload instead of this list.
const PROTECTED_TYPE_PREFIXES: [&str; 15] = [
    "turn.",
    "permission.",
    "queue.",
    "control.",
    "compaction.",
    "subagent.",
    "memory.",
    "usage.",
    "extension.",
    "workflow.",
    "conversation.",
    "provider.",
    "mcp.",
    "skill.",
    "kanban.",
];
const PROTECTED_TYPES: [&str; 1] = ["tool.awaitingApproval"];

/// Payload keys that feed non-timeline projections (permissions, queue,
/// configuration, compaction, subagents, memory, usage). An event carrying
/// any of them keeps its full payload regardless of how it would classify.
const CONTROL_PAYLOAD_KEYS: [&str; 13] = [
    "effectiveConfig",
    "effective",
    "requested",
    "control",
    "items",
    "paused",
    "subagentId",
    "agentId",
    "memoryId",
    "permissionId",
    "operationId",
    "usage",
    "tokenUsage",
];

/// Scalar fields the client classifiers read for category, phase, turn and
/// stream identity. Keeping them in the stub makes a summarized event project
/// to the same timeline entry as the full event.
const STUB_SCALAR_KEYS: [&str; 22] = [
    "turnId",
    "turn_id",
    "role",
    "scope",
    "status",
    "messageId",
    "clientRequestId",
    "requestId",
    "providerMethod",
    "provider_method",
    "method",
    "toolCallId",
    "tool_call_id",
    "callId",
    "call_id",
    "isError",
    "is_error",
    "stopReason",
    "stop_reason",
    "toolName",
    "tool_name",
    "name",
];
/// Tool marker keys: non-null presence alone classifies an event as a tool
/// call, and object `id`/`name` fields feed stream identity and labels.
const STUB_TOOL_KEYS: [&str; 8] = [
    "tool",
    "toolCall",
    "tool_call",
    "item",
    "command",
    "function",
    "functionCall",
    "function_call",
];
const MARKER_FIELD_KEYS: [&str; 6] = ["id", "name", "toolName", "tool_name", "title", "command"];
const STUB_DELTA_KEYS: [&str; 5] = [
    "type",
    "deltaType",
    "delta_type",
    "contentIndex",
    "content_index",
];
const STUB_MESSAGE_KEYS: [&str; 6] = [
    "id",
    "role",
    "stopReason",
    "stop_reason",
    "model",
    "modelId",
];
const MARKER_STRING_LIMIT: usize = 512;

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn valid_block_category(payload: &Map<String, Value>) -> Option<&str> {
    let block = payload.get("block")?.as_object()?;
    non_empty_str(block.get("id"))?;
    let category = non_empty_str(block.get("category"))?;
    let phase = non_empty_str(block.get("phase"))?;
    if !BLOCK_CATEGORIES.contains(&category) || !BLOCK_PHASES.contains(&phase) {
        return None;
    }
    Some(category)
}

fn carries_usage(payload: &Map<String, Value>) -> bool {
    if ["usage", "tokenUsage", "token_usage"]
        .iter()
        .any(|key| !payload.get(*key).is_none_or(Value::is_null))
    {
        return true;
    }
    let message_usage = payload
        .get("message")
        .and_then(Value::as_object)
        .is_some_and(|message| !message.get("usage").is_none_or(Value::is_null));
    let metadata_usage = payload
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| {
            ["tokenUsage", "token_usage"]
                .iter()
                .any(|key| !metadata.get(*key).is_none_or(Value::is_null))
        });
    message_usage || metadata_usage
}

fn is_protected_type(event: &ConversationEvent) -> bool {
    let normalized = normalized_event_type(&event.event_type);
    let candidates = [
        event.event_type.as_str(),
        normalized.as_str(),
        event.normalized_type.as_deref().unwrap_or(""),
    ];
    candidates.iter().any(|candidate| {
        PROTECTED_TYPES.contains(candidate)
            || (*candidate != "provider.event"
                && PROTECTED_TYPE_PREFIXES
                    .iter()
                    .any(|prefix| candidate.starts_with(prefix)))
    })
}

fn first_string<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| non_empty_str(payload.get(*key)))
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    let lowered = text.to_ascii_lowercase();
    needles.iter().any(|needle| lowered.contains(needle))
}

/// Heuristic mirror of the client classifiers for events without a valid
/// block: anything a client would render as a tool call or a reasoning row is
/// a folded detail, everything else stays full.
fn is_detail_heuristic(event: &ConversationEvent, payload: &Map<String, Value>) -> bool {
    let normalized = normalized_event_type(&event.event_type);
    let types = [
        event.event_type.as_str(),
        normalized.as_str(),
        event.normalized_type.as_deref().unwrap_or(""),
    ];
    let delta = payload.get("delta").and_then(Value::as_object);
    let delta_type =
        delta.and_then(|delta| first_string(delta, &["type", "deltaType", "delta_type"]));
    let method =
        first_string(payload, &["providerMethod", "provider_method", "method"]).unwrap_or("");
    if types
        .iter()
        .any(|ty| contains_any(ty, &["thought", "reasoning", "thinking", "analysis"]))
        || delta_type.is_some_and(|ty| contains_any(ty, &["reasoning", "thinking", "analysis"]))
        || contains_any(method, &["reasoning", "thinking", "analysis"])
        || [
            "thought",
            "thoughtText",
            "thought_text",
            "reasoning",
            "thinking",
            "analysis",
        ]
        .iter()
        .any(|key| !payload.get(*key).is_none_or(Value::is_null))
    {
        return true;
    }
    let message_role = payload
        .get("message")
        .and_then(Value::as_object)
        .and_then(|message| non_empty_str(message.get("role")))
        .unwrap_or("");
    types
        .iter()
        .any(|ty| contains_any(ty, &["tool", "command", "function", "mcp"]))
        || delta_type.is_some_and(|ty| contains_any(ty, &["tool", "command", "function", "mcp"]))
        || contains_any(method, &["tool", "command", "function", "mcp"])
        || message_role == "tool"
        || [
            "tool",
            "toolCall",
            "tool_call",
            "command",
            "function",
            "functionCall",
            "function_call",
        ]
        .iter()
        .any(|key| !payload.get(*key).is_none_or(Value::is_null))
}

fn bounded_string(text: &str) -> Value {
    if text.chars().count() <= MARKER_STRING_LIMIT {
        return Value::String(text.to_owned());
    }
    Value::String(text.chars().take(MARKER_STRING_LIMIT).collect())
}

/// Keep only the identity/label fields of a tool marker so presence checks and
/// stream-id derivation behave exactly like the full payload.
fn summarize_marker(value: &Value) -> Value {
    match value {
        Value::String(text) => bounded_string(text),
        Value::Object(map) => {
            let mut slim = Map::new();
            for key in MARKER_FIELD_KEYS {
                if let Some(field) = map.get(key) {
                    if !field.is_null() {
                        slim.insert(key.to_owned(), field.clone());
                    }
                }
            }
            Value::Object(slim)
        }
        Value::Null => Value::Null,
        _ => Value::Bool(true),
    }
}

fn stub_payload(payload: &Map<String, Value>) -> Value {
    let mut stub = Map::new();
    stub.insert("detailStub".to_owned(), Value::Bool(true));
    for key in STUB_SCALAR_KEYS {
        if let Some(value) = payload.get(key) {
            if !value.is_null() {
                stub.insert(key.to_owned(), value.clone());
            }
        }
    }
    if let Some(block) = payload.get("block") {
        if !block.is_null() {
            stub.insert("block".to_owned(), block.clone());
        }
    }
    if let Some(delta) = payload.get("delta").and_then(Value::as_object) {
        let mut slim = Map::new();
        for key in STUB_DELTA_KEYS {
            if let Some(value) = delta.get(key) {
                if !value.is_null() {
                    slim.insert(key.to_owned(), value.clone());
                }
            }
        }
        for key in ["toolCall", "tool_call"] {
            if let Some(value) = delta.get(key) {
                if !value.is_null() {
                    slim.insert(key.to_owned(), summarize_marker(value));
                }
            }
        }
        stub.insert("delta".to_owned(), Value::Object(slim));
    }
    for key in STUB_TOOL_KEYS {
        if let Some(value) = payload.get(key) {
            if !value.is_null() {
                stub.insert(key.to_owned(), summarize_marker(value));
            }
        }
    }
    if let Some(message) = payload.get("message").and_then(Value::as_object) {
        let mut slim = Map::new();
        for key in STUB_MESSAGE_KEYS {
            if let Some(value) = message.get(key) {
                if !value.is_null() {
                    slim.insert(key.to_owned(), value.clone());
                }
            }
        }
        stub.insert("message".to_owned(), Value::Object(slim));
    }
    Value::Object(stub)
}

/// `detail=summary` replay: replace the payload of process-only events with a
/// marker object that preserves classification identity. Returns `false` when
/// the event is left untouched, so callers can count reduced events.
pub fn summarize_event(event: &mut ConversationEvent) -> bool {
    let Some(payload) = event.payload.as_object() else {
        return false;
    };
    if carries_usage(payload) || is_protected_type(event) {
        return false;
    }
    if CONTROL_PAYLOAD_KEYS
        .iter()
        .any(|key| !payload.get(*key).is_none_or(Value::is_null))
    {
        return false;
    }
    let detail = match valid_block_category(payload) {
        Some(category) => DETAIL_BLOCK_CATEGORIES.contains(&category),
        None => is_detail_heuristic(event, payload),
    };
    if !detail {
        return false;
    }
    event.payload = stub_payload(payload);
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(event_type: &str, payload: Value) -> ConversationEvent {
        ConversationEvent::new("conv", 1, event_type, payload)
    }

    fn stubbed(event_type: &str, payload: Value) -> Value {
        let mut event = event(event_type, payload);
        assert!(summarize_event(&mut event));
        event.payload
    }

    fn untouched(event_type: &str, payload: Value) -> Value {
        let mut event = event(event_type, payload.clone());
        assert!(!summarize_event(&mut event));
        event.payload
    }

    #[test]
    fn tool_block_events_reduce_to_classification_metadata() {
        let stub = stubbed(
            "provider.event",
            json!({
                "turnId": "turn-1",
                "block": {"id": "call-1", "category": "tool", "phase": "completed", "turnId": "turn-1"},
                "toolCallId": "call-1",
                "toolName": "shell",
                "result": "x".repeat(50_000),
            }),
        );
        assert_eq!(stub["detailStub"], true);
        assert_eq!(stub["turnId"], "turn-1");
        assert_eq!(stub["block"]["category"], "tool");
        assert_eq!(stub["toolCallId"], "call-1");
        assert!(stub.get("result").is_none());
    }

    #[test]
    fn reasoning_status_and_progress_blocks_are_also_details() {
        for category in ["reasoning", "status", "assistant_progress"] {
            let stub = stubbed(
                "provider.event",
                json!({
                    "block": {"id": "b1", "category": category, "phase": "delta"},
                    "delta": {"type": "text_delta", "text": "large body"},
                }),
            );
            assert_eq!(stub["detailStub"], true, "{category}");
            assert_eq!(stub["delta"]["type"], "text_delta");
            assert!(stub["delta"].get("text").is_none());
        }
    }

    #[test]
    fn output_blocks_never_reduce() {
        for category in ["assistant_final", "approval", "error", "usage"] {
            let payload = json!({
                "block": {"id": "b1", "category": category, "phase": "completed"},
                "text": "final answer",
            });
            assert_eq!(untouched("provider.event", payload.clone()), payload);
        }
    }

    #[test]
    fn heuristic_tool_and_thought_events_reduce_without_blocks() {
        let tool = stubbed(
            "tool.completed",
            json!({"toolCallId": "c1", "result": "big", "toolCall": {"id": "c1", "name": "shell", "arguments": "big"}}),
        );
        assert_eq!(tool["toolCall"]["name"], "shell");
        assert!(tool["toolCall"].get("arguments").is_none());
        let thought = stubbed(
            "reasoning.delta",
            json!({"turnId": "t1", "thinking": "chain of thought"}),
        );
        assert_eq!(thought["turnId"], "t1");
    }

    #[test]
    fn usage_and_control_payloads_stay_full() {
        let usage_tool = json!({
            "block": {"id": "b1", "category": "tool", "phase": "completed"},
            "usage": {"totalTokens": 10},
        });
        assert_eq!(untouched("provider.event", usage_tool.clone()), usage_tool);
        let approval = json!({
            "permissionId": "perm-1", "title": "allow?", "toolCall": {"id": "c1"},
        });
        assert_eq!(
            untouched("tool.awaitingApproval", approval.clone()),
            approval
        );
        let queue = json!({"items": [{"id": "q1"}], "toolCall": {"id": "c1"}});
        assert_eq!(untouched("queue.updated", queue.clone()), queue);
        let configured = json!({"effectiveConfig": {"a": 1}, "toolCall": {"id": "c1"}});
        assert_eq!(untouched("provider.event", configured.clone()), configured);
    }

    #[test]
    fn lifecycle_and_visible_types_stay_full() {
        for event_type in [
            "turn.started",
            "turn.completed",
            "permission.requested",
            "permission.resolved",
            "subagent.started",
            "memory.created",
            "compaction.started",
            "extension.message",
            "provider.runtime",
            "provider.commands.updated",
            "conversation.interrupted",
        ] {
            let payload = json!({"toolCall": {"id": "c1"}, "thinking": "x"});
            assert_eq!(
                untouched(event_type, payload.clone()),
                payload,
                "{event_type}"
            );
        }
    }

    #[test]
    fn plain_assistant_and_user_events_stay_full() {
        let assistant = json!({"text": "answer", "message": {"role": "assistant"}});
        assert_eq!(untouched("assistant.delta", assistant.clone()), assistant);
        let user = json!({"role": "user", "text": "hello"});
        assert_eq!(untouched("message.created", user.clone()), user);
    }
}
