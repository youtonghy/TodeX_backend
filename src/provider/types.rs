use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::conversation::{
    ConversationEvent, ConversationEventHub, ConversationManifest, ConversationStore, ProviderKind,
    ProviderState,
};
use crate::error::AppError;
use crate::workspace_trust::WorkspaceTrustPermit;

const PERMISSION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    pub native_resume: bool,
    pub cancel: bool,
    pub permissions: bool,
    pub tool_events: bool,
    pub native_skills: bool,
    pub native_mcp: bool,
    pub managed_mcp: bool,
    pub model_selection: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelDescriptor {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub supported_reasoning_efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCommandDescriptor {
    pub name: String,
    pub description: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_info: Option<Value>,
    pub invocation: String,
    pub argument_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDescriptor {
    pub id: ProviderKind,
    pub display_name: &'static str,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    pub profiles: Vec<String>,
    pub capabilities: ProviderCapabilities,
    pub models: Vec<ProviderModelDescriptor>,
}

#[derive(Clone, Debug)]
pub struct DriverContext {
    pub manifest: ConversationManifest,
    pub provider_state: ProviderState,
}

#[derive(Clone, Debug)]
pub struct DriverPrompt {
    pub turn_id: String,
    pub text: String,
    pub content: Vec<DriverPromptContent>,
    pub skills: Vec<DriverSkill>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DriverSkill {
    pub name: String,
    pub path: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug)]
pub enum DriverPromptContent {
    Image {
        path: Option<PathBuf>,
        data: String,
        mime_type: String,
    },
    File {
        path: PathBuf,
        name: String,
    },
}

#[derive(Clone, Debug)]
pub struct DriverTurnResult {
    pub native_session_id: Option<String>,
    pub stop_reason: String,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecision {
    pub outcome: PermissionOutcome,
    #[serde(default, alias = "id")]
    pub option_id: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Answer,
}

impl PermissionOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowAlways => "allow_always",
            Self::RejectOnce => "reject_once",
            Self::RejectAlways => "reject_always",
            Self::Answer => "answer",
        }
    }
}

#[derive(Clone)]
pub struct DriverEventSink {
    store: ConversationStore,
    hub: ConversationEventHub,
    permissions: PermissionBroker,
    conversation_id: String,
}

impl DriverEventSink {
    pub fn new(
        store: ConversationStore,
        hub: ConversationEventHub,
        permissions: PermissionBroker,
        conversation_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            hub,
            permissions,
            conversation_id: conversation_id.into(),
        }
    }

    pub async fn emit(
        &self,
        event_type: impl Into<String>,
        payload: Value,
    ) -> Result<ConversationEvent, AppError> {
        let event = self
            .store
            .append(&self.conversation_id, event_type, payload)
            .await?;
        self.hub.publish(event.clone());
        Ok(event)
    }

    pub async fn save_provider_state(&self, state: ProviderState) -> Result<(), AppError> {
        self.store
            .save_provider_state(&self.conversation_id, state)
            .await
    }

    pub async fn request_permission(
        &self,
        provider_request_id: String,
        kind: impl Into<String>,
        title: impl Into<String>,
        details: Value,
        options: Value,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<PermissionDecision, AppError> {
        self.permissions
            .request(
                self.clone(),
                provider_request_id,
                kind.into(),
                title.into(),
                details,
                options,
                cancel,
            )
            .await
    }
}

#[async_trait]
pub trait ProviderDriver: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;

    async fn discover_models(
        &self,
        _workspace: &Path,
    ) -> Result<Vec<ProviderModelDescriptor>, AppError> {
        Ok(self.descriptor().models)
    }

    async fn discover_commands(
        &self,
        _workspace: &Path,
    ) -> Result<Vec<ProviderCommandDescriptor>, AppError> {
        Ok(Vec::new())
    }

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        cancel: watch::Receiver<bool>,
        launch_permit: WorkspaceTrustPermit,
    ) -> Result<DriverTurnResult, AppError>;
}

#[derive(Clone, Default)]
pub struct PermissionBroker {
    pending: Arc<DashMap<String, PendingPermission>>,
}

struct PendingPermission {
    conversation_id: String,
    sender: oneshot::Sender<PermissionDecision>,
}

struct PendingPermissionCleanup {
    pending: Arc<DashMap<String, PendingPermission>>,
    permission_id: String,
}

impl Drop for PendingPermissionCleanup {
    fn drop(&mut self) {
        self.pending.remove(&self.permission_id);
    }
}

impl PermissionBroker {
    async fn request(
        &self,
        sink: DriverEventSink,
        provider_request_id: String,
        kind: String,
        title: String,
        details: Value,
        options: Value,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<PermissionDecision, AppError> {
        let permission_id = format!("perm_{}", Uuid::new_v4().simple());
        let (sender, receiver) = oneshot::channel();
        self.pending.insert(
            permission_id.clone(),
            PendingPermission {
                conversation_id: sink.conversation_id.clone(),
                sender,
            },
        );
        let _cleanup = PendingPermissionCleanup {
            pending: self.pending.clone(),
            permission_id: permission_id.clone(),
        };
        let options = normalize_permission_options(options)?;
        if let Err(error) = sink
            .emit(
                "permission.requested",
                json!({
                    "permissionId": permission_id,
                    "providerRequestId": provider_request_id,
                    "kind": kind,
                    "title": title,
                    "details": details,
                    "options": options,
                }),
            )
            .await
        {
            return Err(error);
        }

        let decision = tokio::select! {
            result = timeout(PERMISSION_TIMEOUT, receiver) => {
                match result {
                    Ok(Ok(decision)) => Ok(decision),
                    Ok(Err(_)) => Err(AppError::InvalidRequest("permission request was closed".to_owned())),
                    Err(_) => Err(AppError::InvalidRequest("permission request expired".to_owned())),
                }
            }
            changed = cancel.changed() => {
                let _ = changed;
                Err(AppError::TurnCancelled)
            }
        };
        let decision = match decision {
            Ok(decision) => decision,
            Err(error) => {
                sink.emit(
                    "permission.resolved",
                    json!({
                        "permissionId": permission_id,
                        "outcome": "cancelled",
                        "optionId": Value::Null,
                    }),
                )
                .await?;
                return Err(error);
            }
        };
        sink.emit(
            "permission.resolved",
            json!({
                "permissionId": permission_id,
                "outcome": decision.outcome.as_str(),
                "optionId": decision.option_id,
            }),
        )
        .await?;
        Ok(decision)
    }

    pub async fn resolve(
        &self,
        conversation_id: &str,
        permission_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AppError> {
        let Some((_, pending)) = self.pending.remove(permission_id) else {
            return Err(AppError::NotFound(format!(
                "pending permission {permission_id}"
            )));
        };
        if pending.conversation_id != conversation_id {
            self.pending.insert(permission_id.to_owned(), pending);
            return Err(AppError::Unauthorized(
                "permission belongs to another conversation".to_owned(),
            ));
        }
        pending
            .sender
            .send(decision)
            .map_err(|_| AppError::Conflict("permission request is no longer active".to_owned()))
    }

    pub fn expire_all(&self) {
        self.pending.clear();
    }
}

fn normalize_permission_options(options: Value) -> Result<Value, AppError> {
    let Value::Array(options) = options else {
        if options.is_null() {
            return Ok(Value::Array(Vec::new()));
        }
        return Err(AppError::InvalidRequest(
            "permission options must be an array".to_owned(),
        ));
    };
    options
        .into_iter()
        .map(|option| {
            let Value::Object(option) = option else {
                return Err(AppError::InvalidRequest(
                    "permission option must be an object".to_owned(),
                ));
            };
            let option_id = option
                .get("optionId")
                .or_else(|| option.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AppError::InvalidRequest("permission option id is required".to_owned())
                })?;
            let name = option
                .get("name")
                .or_else(|| option.get("label"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AppError::InvalidRequest("permission option name is required".to_owned())
                })?;
            let kind = option
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or(option_id);
            if !matches!(
                kind,
                "allow_once" | "allow_always" | "reject_once" | "reject_always" | "answer"
            ) {
                return Err(AppError::InvalidRequest(format!(
                    "unsupported permission option kind {kind}"
                )));
            }
            Ok(json!({
                "optionId": option_id,
                "id": option_id,
                "name": name,
                "label": name,
                "kind": kind,
            }))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permission_persistence_failure_cleans_pending_request() {
        let root = std::env::temp_dir().join(format!(
            "todex-permission-cleanup-{}",
            Uuid::new_v4().simple()
        ));
        let store = ConversationStore::new(root.clone()).await.unwrap();
        let broker = PermissionBroker::default();
        let sink = DriverEventSink::new(
            store,
            ConversationEventHub::default(),
            broker.clone(),
            "not-a-conversation-id",
        );
        let (_cancel, mut cancel_rx) = watch::channel(false);

        assert!(broker
            .request(
                sink,
                "provider-request".to_owned(),
                "tool".to_owned(),
                "Allow tool?".to_owned(),
                Value::Null,
                Value::Null,
                &mut cancel_rx,
            )
            .await
            .is_err());
        assert!(broker.pending.is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn permission_options_are_canonical_and_keep_legacy_aliases() {
        let options = normalize_permission_options(json!([
            { "id": "allow_once", "label": "Allow once" },
            { "optionId": "answer", "name": "Answer", "kind": "answer" }
        ]))
        .unwrap();
        assert_eq!(options[0]["optionId"], "allow_once");
        assert_eq!(options[0]["name"], "Allow once");
        assert_eq!(options[0]["kind"], "allow_once");
        assert_eq!(options[1]["id"], "answer");
        assert_eq!(options[1]["label"], "Answer");
    }
}
