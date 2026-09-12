use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const CONVERSATION_SCHEMA_VERSION: u32 = 2;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Acp,
    Codex,
    Pi,
    ClaudeCode,
    GrokBuild,
    Devin,
}

impl ProviderKind {
    pub const ALL: [Self; 6] = [
        Self::Acp,
        Self::Codex,
        Self::Pi,
        Self::ClaudeCode,
        Self::GrokBuild,
        Self::Devin,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acp => "acp",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::ClaudeCode => "claude-code",
            Self::GrokBuild => "grok-build",
            Self::Devin => "devin",
        }
    }

    pub const fn supports_image_input(self) -> bool {
        matches!(
            self,
            Self::Codex | Self::Pi | Self::ClaudeCode | Self::GrokBuild | Self::Devin
        )
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "acp" => Ok(Self::Acp),
            "codex" => Ok(Self::Codex),
            "pi" => Ok(Self::Pi),
            "claude" | "claude-code" | "claude_code" => Ok(Self::ClaudeCode),
            "grok" | "grok-build" | "grok_build" => Ok(Self::GrokBuild),
            "devin" | "devin-cli" | "devin_cli" => Ok(Self::Devin),
            other => Err(format!("unsupported provider: {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationStatus {
    #[default]
    Idle,
    Running,
    WaitingPermission,
    Interrupted,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationManifest {
    pub schema_version: u32,
    pub id: String,
    pub provider: ProviderKind,
    #[serde(default = "default_owner_id")]
    pub owner_id: String,
    pub workspace: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_profile: Option<String>,
    pub status: ConversationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
    pub last_sequence: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ConversationManifest {
    pub fn new(
        provider: ProviderKind,
        workspace: PathBuf,
        title: Option<String>,
        provider_profile: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            id: Uuid::new_v4().to_string(),
            provider,
            owner_id: default_owner_id(),
            workspace,
            workspace_id: None,
            title,
            provider_profile,
            status: ConversationStatus::Idle,
            archived_at: None,
            last_sequence: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

fn default_owner_id() -> String {
    "local".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationEvent {
    pub schema_version: u32,
    pub sequence: u64,
    pub event_id: String,
    pub conversation_id: String,
    pub time: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    pub payload: Value,
}

impl ConversationEvent {
    pub fn new(
        conversation_id: impl Into<String>,
        sequence: u64,
        event_type: impl Into<String>,
        payload: Value,
    ) -> Self {
        let event_type = event_type.into();
        let normalized_type = normalized_event_type(&event_type);
        Self {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            sequence,
            event_id: format!("evt_{}", Uuid::new_v4().simple()),
            conversation_id: conversation_id.into(),
            time: Utc::now(),
            raw_type: payload
                .get("providerMethod")
                .and_then(Value::as_str)
                .map(str::to_owned),
            provider: None,
            event_type,
            normalized_type: Some(normalized_type),
            payload,
        }
    }
}

pub fn normalized_event_type(event_type: &str) -> String {
    match event_type {
        "turn.started" | "codex.turn.started" => "turn.started",
        "turn.completed" | "codex.turn.completed" => "turn.completed",
        "turn.cancelled" => "turn.cancelled",
        "conversation.interrupted" => "conversation.interrupted",
        "turn.failed" | "conversation.failed" => "turn.failed",
        "message.delta" | "assistant.delta" | "text_delta" => "assistant.delta",
        "thought.delta" | "reasoning.delta" | "thinking_delta" => "reasoning.delta",
        "tool.started" | "tool.created" | "item.started" => "tool.started",
        "tool.completed" | "tool.result" | "item.completed" => "tool.completed",
        "tool.failed" | "tool.error" => "tool.failed",
        "permission.requested" | "tool.awaitingApproval" => "tool.awaitingApproval",
        "compaction.started" => "compaction.started",
        "compaction.completed" => "compaction.completed",
        "subagent.started" => "subagent.started",
        "subagent.completed" => "subagent.completed",
        "workflow.started" | "workflow.resumed" => "workflow.started",
        "workflow.paused" => "workflow.paused",
        "workflow.completed" => "workflow.completed",
        "workflow.failed" => "workflow.failed",
        "workflow.cancelled" => "workflow.cancelled",
        "tool.checkpoint.created" => "tool.checkpoint.created",
        "tool.checkpoint.committed" => "tool.checkpoint.committed",
        "tool.checkpoint.rolledBack" | "tool.checkpoint.rolled_back" => {
            "tool.checkpoint.rolledBack"
        }
        _ => event_type,
    }
    .to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSnapshot {
    pub schema_version: u32,
    pub conversation_id: String,
    pub status: ConversationStatus,
    pub last_sequence: u64,
    pub event_count: u64,
    pub updated_at: DateTime<Utc>,
}

impl ConversationSnapshot {
    pub fn from_manifest(manifest: &ConversationManifest) -> Self {
        Self {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            conversation_id: manifest.id.clone(),
            status: manifest.status,
            last_sequence: manifest.last_sequence,
            event_count: manifest.last_sequence,
            updated_at: manifest.updated_at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    pub schema_version: u32,
    pub provider: ProviderKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub recoverable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl ProviderState {
    pub fn new(provider: ProviderKind) -> Self {
        Self {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            provider,
            native_session_id: None,
            recoverable: false,
            last_error: None,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationReplay {
    pub conversation_id: String,
    pub from_sequence: u64,
    pub next_sequence: u64,
    pub has_more: bool,
    pub events: Vec<ConversationEvent>,
}

pub fn status_after_event(current: ConversationStatus, event_type: &str) -> ConversationStatus {
    match normalized_event_type(event_type).as_str() {
        "turn.started" => ConversationStatus::Running,
        "tool.awaitingApproval" => ConversationStatus::WaitingPermission,
        "permission.resolved" if current == ConversationStatus::WaitingPermission => {
            ConversationStatus::Running
        }
        "turn.completed" | "turn.cancelled" | "conversation.forked" => ConversationStatus::Idle,
        "conversation.interrupted" => ConversationStatus::Interrupted,
        "turn.failed" | "conversation.failed" => ConversationStatus::Failed,
        "workflow.started" | "workflow.resumed" => ConversationStatus::Running,
        "workflow.paused" => ConversationStatus::Interrupted,
        "workflow.completed" | "workflow.cancelled" => ConversationStatus::Idle,
        "workflow.failed" => ConversationStatus::Failed,
        _ => current,
    }
}

/// Automatic compaction is a child activity; only explicit standalone operations
/// own the conversation lifecycle. Recompute canonical names from raw history so
/// journals written with older lossy normalizedType values remain correct.
pub fn status_after_conversation_event(
    current: ConversationStatus,
    event: &ConversationEvent,
) -> ConversationStatus {
    // Extension dialogs can outlive a turn or arrive while the provider is
    // otherwise idle. They are actionable UI, not a new model execution.
    if event.payload.get("scope").and_then(Value::as_str) == Some("session")
        && matches!(
            event.event_type.as_str(),
            "permission.requested" | "tool.awaitingApproval" | "permission.resolved"
        )
    {
        return current;
    }
    if event
        .payload
        .get("operationId")
        .and_then(Value::as_str)
        .is_some()
    {
        match event.event_type.as_str() {
            "compaction.started" => return ConversationStatus::Running,
            "compaction.completed" | "compaction.cancelled" => return ConversationStatus::Idle,
            "compaction.failed" => return ConversationStatus::Failed,
            _ => {}
        }
    }
    status_after_event(current, &event.event_type)
}

pub fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
                let usage_count = value.is_number()
                    && matches!(
                        normalized.as_str(),
                        "inputtokens"
                            | "outputtokens"
                            | "totaltokens"
                            | "cachedinputtokens"
                            | "cachewritetokens"
                            | "cachecreationinputtokens"
                            | "cachereadinputtokens"
                            | "reasoningoutputtokens"
                            | "reasoningtokens"
                            | "cachedtokens"
                            | "tokencount"
                            | "contextwindowtokens"
                            | "contexttokens"
                            | "prompttokens"
                            | "completiontokens"
                    );
                let usage_container = (value.is_object() || value.is_array())
                    && matches!(
                        normalized.as_str(),
                        "tokenusage" | "tokenusages" | "inputtokensdetails" | "outputtokensdetails"
                    );
                if usage_count {
                    continue;
                }
                if usage_container {
                    redact_secrets(value);
                    continue;
                }
                if normalized.contains("token")
                    || normalized.contains("secret")
                    || normalized.contains("password")
                    || normalized.contains("authorization")
                    || normalized.contains("cookie")
                    || normalized.contains("apikey")
                {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_secrets(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

#[cfg(test)]
mod provider_kind_tests {
    use std::str::FromStr;

    use super::ProviderKind;

    #[test]
    fn redaction_preserves_numeric_usage_without_preserving_credentials() {
        let mut value = serde_json::json!({
            "inputTokens": 42, "output_tokens": 9, "tokenUsage": { "cachedInputTokens": 12, "authToken": "secret" },
            "access_token": "secret", "input_tokens_details": { "cached_tokens": 4 },
            "promptTokens": "credential-shaped-string",
        });
        super::redact_secrets(&mut value);
        assert_eq!(value["inputTokens"], 42);
        assert_eq!(value["output_tokens"], 9);
        assert_eq!(value["tokenUsage"]["cachedInputTokens"], 12);
        assert_eq!(value["tokenUsage"]["authToken"], "[REDACTED]");
        assert_eq!(value["access_token"], "[REDACTED]");
        assert_eq!(value["promptTokens"], "[REDACTED]");
    }

    #[test]
    fn event_origin_is_optional_for_old_journals_and_preserves_native_method() {
        let old = serde_json::json!({
            "schemaVersion": 2, "sequence": 1, "eventId": "evt_old",
            "conversationId": "conv_old", "time": "2026-09-05T00:00:00Z",
            "type": "provider.event", "payload": {"unknown": true}
        });
        let restored: super::ConversationEvent = serde_json::from_value(old.clone()).unwrap();
        assert!(restored.provider.is_none());
        assert!(restored.raw_type.is_none());
        assert_eq!(serde_json::to_value(restored).unwrap(), old);

        let mut event = super::ConversationEvent::new(
            "conv_new",
            1,
            "provider.event",
            serde_json::json!({"providerMethod": "item/started"}),
        );
        event.provider = Some(ProviderKind::Codex);
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["rawType"], "item/started");
        assert_eq!(wire["provider"], "codex");
        assert_eq!(wire["type"], "provider.event");
        assert_eq!(wire["normalizedType"], "provider.event");
        assert_eq!(
            super::normalized_event_type("codex.turn.completed"),
            "turn.completed"
        );
    }

    #[test]
    fn grok_build_provider_kind_has_stable_wire_name_and_aliases() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::GrokBuild).unwrap(),
            "\"grok-build\""
        );
        assert_eq!(
            serde_json::from_str::<ProviderKind>("\"grok-build\"").unwrap(),
            ProviderKind::GrokBuild
        );
        for alias in ["grok", "grok-build", "grok_build", " GROK "] {
            assert_eq!(
                ProviderKind::from_str(alias).unwrap(),
                ProviderKind::GrokBuild
            );
        }
        assert_eq!(
            ProviderKind::ALL
                .iter()
                .filter(|provider| **provider == ProviderKind::GrokBuild)
                .count(),
            1
        );
    }

    #[test]
    fn status_transitions_use_canonical_event_names() {
        assert_eq!(
            super::status_after_event(
                super::ConversationStatus::Idle,
                &super::normalized_event_type("codex.turn.started")
            ),
            super::ConversationStatus::Running
        );
        assert_eq!(
            super::status_after_event(
                super::ConversationStatus::Running,
                &super::normalized_event_type("codex.turn.completed")
            ),
            super::ConversationStatus::Idle
        );
    }
}
