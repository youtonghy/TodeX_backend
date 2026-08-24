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
    pub model: Option<String>,
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
    #[serde(default)]
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

    async fn run_turn(
        &self,
        context: DriverContext,
        prompt: DriverPrompt,
        sink: DriverEventSink,
        cancel: watch::Receiver<bool>,
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
            self.pending.remove(&permission_id);
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
                Err(AppError::Conflict("turn was cancelled while awaiting permission".to_owned()))
            }
        };
        self.pending.remove(&permission_id);
        let decision = decision?;
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
}
