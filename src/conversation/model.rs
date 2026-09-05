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
}

impl ProviderKind {
    pub const ALL: [Self; 5] = [
        Self::Acp,
        Self::Codex,
        Self::Pi,
        Self::ClaudeCode,
        Self::GrokBuild,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acp => "acp",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::ClaudeCode => "claude-code",
            Self::GrokBuild => "grok-build",
        }
    }

    pub const fn supports_image_input(self) -> bool {
        matches!(
            self,
            Self::Codex | Self::Pi | Self::ClaudeCode | Self::GrokBuild
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
            payload,
        }
    }
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
    match event_type {
        "turn.started" => ConversationStatus::Running,
        "permission.requested" => ConversationStatus::WaitingPermission,
        "permission.resolved" => ConversationStatus::Running,
        "turn.completed" | "turn.cancelled" => ConversationStatus::Idle,
        "conversation.interrupted" => ConversationStatus::Interrupted,
        "turn.failed" | "conversation.failed" => ConversationStatus::Failed,
        _ => current,
    }
}

pub fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
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
}
