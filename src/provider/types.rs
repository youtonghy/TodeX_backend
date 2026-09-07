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
    pub permission_config: PermissionConfigCapabilities,
    pub native_fork: bool,
    pub native_compact: bool,
    pub native_resume: bool,
    pub cancel: bool,
    pub permissions: bool,
    pub tool_events: bool,
    pub native_skills: bool,
    pub native_mcp: bool,
    pub managed_mcp: bool,
    pub model_selection: bool,
    pub image_input: bool,
    pub image_input_mode: ImageInputMode,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionConfigCapabilities {
    pub sandbox_modes: Vec<&'static str>,
    pub approval_policies: Vec<&'static str>,
    pub permission_profiles: Vec<&'static str>,
    pub enforcement: &'static str,
    pub description: &'static str,
}

pub fn permission_config_capabilities(provider: ProviderKind) -> PermissionConfigCapabilities {
    match provider {
        ProviderKind::Codex => PermissionConfigCapabilities {
            sandbox_modes: vec!["read-only", "workspace-write", "danger-full-access"],
            approval_policies: vec!["untrusted", "on-request", "never"],
            permission_profiles: vec!["read-only", "workspace-write", "danger-full-access"],
            enforcement: "sandbox", description: "Codex native sandbox and approval controls",
        },
        ProviderKind::ClaudeCode => PermissionConfigCapabilities {
            sandbox_modes: vec!["read-only", "workspace-write", "danger-full-access"],
            approval_policies: vec!["on-request", "never"],
            permission_profiles: vec!["read-only", "workspace-write", "danger-full-access"],
            enforcement: "agent-policy", description: "Claude plan / acceptEdits / bypassPermissions modes; not an operating-system sandbox. Unsupported combinations are rejected.",
        },
        _ => PermissionConfigCapabilities {
            sandbox_modes: vec![], approval_policies: vec![], permission_profiles: vec![],
            enforcement: "unsupported", description: "This provider does not expose sandbox or approval overrides through its active protocol.",
        },
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ImageInputMode {
    Always,
    Model,
    Profile,
    None,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_input: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderImageInputCapability {
    pub provider: ProviderKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub image_input: bool,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    pub permission_profile: Option<String>,
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
}

/// Validated controls actually supported by the selected provider.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePermissionConfig {
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
    pub provider_mode: Option<String>,
    pub source: String,
}

pub fn resolve_permission_config(
    provider: ProviderKind,
    profile: Option<&str>,
    sandbox: Option<&str>,
    approval: Option<&str>,
) -> Result<EffectivePermissionConfig, AppError> {
    let profile_sandbox = match profile {
        None => None,
        Some("read-only" | ":read-only") => Some("read-only"),
        Some("workspace-write" | ":workspace-write" | ":workspace" | "default") => {
            Some("workspace-write")
        }
        Some("full-access" | ":danger-full-access" | "danger-full-access") => {
            Some("danger-full-access")
        }
        Some(value) => {
            return Err(AppError::Unsupported(format!(
                "unsupported permission profile {value}"
            )))
        }
    };
    if sandbox.is_some_and(|value| {
        !matches!(
            value,
            "read-only" | "workspace-write" | "danger-full-access"
        )
    }) {
        return Err(AppError::InvalidRequest(
            "unsupported sandbox mode".to_owned(),
        ));
    }
    if approval.is_some_and(|value| !matches!(value, "untrusted" | "on-request" | "never")) {
        return Err(AppError::InvalidRequest(
            "unsupported approval policy".to_owned(),
        ));
    }
    if profile_sandbox.zip(sandbox).is_some_and(|(a, b)| a != b) {
        return Err(AppError::InvalidRequest(
            "permission profile conflicts with sandbox mode".to_owned(),
        ));
    }
    if !matches!(provider, ProviderKind::Codex | ProviderKind::ClaudeCode) {
        if profile.is_some() || sandbox.is_some() || approval.is_some() {
            return Err(AppError::Unsupported(format!(
                "{} does not expose sandbox or approval overrides",
                provider.as_str()
            )));
        }
        return Ok(EffectivePermissionConfig {
            sandbox_mode: None,
            approval_policy: None,
            provider_mode: None,
            source: "provider-default".to_owned(),
        });
    }
    let sandbox = profile_sandbox.or(sandbox).unwrap_or("workspace-write");
    let approval = approval.unwrap_or(if profile_sandbox == Some("danger-full-access") {
        "never"
    } else {
        "on-request"
    });
    let provider_mode = if provider == ProviderKind::ClaudeCode {
        Some(
            match (sandbox, approval) {
                ("read-only", "on-request" | "never") => "plan",
                ("workspace-write", "on-request") => "acceptEdits",
                ("danger-full-access", "never") => "bypassPermissions",
                _ => {
                    return Err(AppError::Unsupported(
                        "Claude stream-json does not support this permission combination"
                            .to_owned(),
                    ))
                }
            }
            .to_owned(),
        )
    } else {
        None
    };
    Ok(EffectivePermissionConfig {
        sandbox_mode: Some(sandbox.to_owned()),
        approval_policy: Some(approval.to_owned()),
        provider_mode,
        source: "validated-provider-controls".to_owned(),
    })
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
    AbortTurn,
    Answer,
}

impl PermissionOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowAlways => "allow_always",
            Self::RejectOnce => "reject_once",
            Self::RejectAlways => "reject_always",
            Self::AbortTurn => "abort_turn",
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
    current_turn_id: Option<String>,
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
            current_turn_id: None,
        }
    }

    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.current_turn_id = Some(turn_id.into());
        self
    }

    pub async fn emit(
        &self,
        event_type: impl Into<String>,
        mut payload: Value,
    ) -> Result<ConversationEvent, AppError> {
        if let (Some(turn_id), Some(object)) = (&self.current_turn_id, payload.as_object_mut()) {
            if let Some(native_turn_id) = object
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|value| *value != turn_id)
                .map(ToOwned::to_owned)
            {
                object
                    .entry("nativeTurnId")
                    .or_insert(Value::String(native_turn_id));
            }
            object.insert("turnId".to_owned(), Value::String(turn_id.clone()));
        }
        self.store
            .append_and_publish(&self.conversation_id, event_type, payload, &self.hub)
            .await
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
    async fn refresh_control_capabilities(&self) {}
    fn control_probe(&self) -> Option<Value> {
        None
    }

    /// Controls must target the currently running TodeX turn. A driver must
    /// reject a stale target before writing to the native transport.
    fn supports_live_controls(&self) -> bool {
        false
    }

    fn supports_native_queue(&self) -> bool {
        false
    }

    async fn control(
        &self,
        _conversation_id: &str,
        _expected_turn_id: &str,
        _request_id: &str,
        _control: ProviderControl,
    ) -> Result<Value, AppError> {
        Err(AppError::Unsupported(
            "This provider has no live control channel.".to_owned(),
        ))
    }

    async fn shutdown_session(&self, _conversation_id: &str) {}

    async fn shutdown(&self) {}

    fn supports_native_compact(&self) -> bool {
        false
    }

    async fn compact_session(
        &self,
        _context: DriverContext,
        _cancel: watch::Receiver<bool>,
        _launch_permit: WorkspaceTrustPermit,
    ) -> Result<(), AppError> {
        Err(AppError::Unsupported(
            "provider does not support native session compaction".to_owned(),
        ))
    }

    fn supports_native_fork(&self) -> bool {
        false
    }

    async fn fork_session(
        &self,
        _context: DriverContext,
        _launch_permit: WorkspaceTrustPermit,
    ) -> Result<ProviderState, AppError> {
        Err(AppError::Unsupported(
            "provider does not support native session fork".to_owned(),
        ))
    }

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

    async fn discover_image_input(
        &self,
        _workspace: &Path,
        _profile: Option<&str>,
    ) -> Result<bool, AppError> {
        Ok(self.descriptor().capabilities.image_input)
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

/// Deliberately bounded public controls. Native method names and arbitrary
/// provider configuration never cross this boundary unchecked.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum ProviderControl {
    Steer {
        text: String,
    },
    Configure {
        #[serde(default)]
        model: Option<String>,
        #[serde(default, rename = "reasoningEffort")]
        reasoning_effort: Option<String>,
    },
    QueueAdd {
        #[serde(rename = "itemId")]
        item_id: String,
        text: String,
    },
    QueueRemove {
        #[serde(rename = "itemId")]
        item_id: String,
    },
    QueueList,
    QueueClear,
}

pub struct PendingProviderControl {
    pub expected_turn_id: String,
    pub request_id: String,
    pub control: ProviderControl,
    pub respond_to: oneshot::Sender<Result<Value, AppError>>,
}

#[derive(Clone, Default)]
pub struct PermissionBroker {
    pending: Arc<DashMap<String, PendingPermission>>,
}

struct PendingPermission {
    conversation_id: String,
    sender: oneshot::Sender<PermissionDecision>,
    kind: String,
    details: Value,
    options: Value,
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
        let options = normalize_permission_options(options)?;
        let permission_id = format!("perm_{}", Uuid::new_v4().simple());
        let (sender, receiver) = oneshot::channel();
        self.pending.insert(
            permission_id.clone(),
            PendingPermission {
                conversation_id: sink.conversation_id.clone(),
                sender,
                kind: kind.clone(),
                details: details.clone(),
                options: options.clone(),
            },
        );
        let _cleanup = PendingPermissionCleanup {
            pending: self.pending.clone(),
            permission_id: permission_id.clone(),
        };
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
        let entry = match self.pending.entry(permission_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry,
            dashmap::mapref::entry::Entry::Vacant(_) => {
                return Err(AppError::NotFound(format!(
                    "pending permission {permission_id}"
                )))
            }
        };
        let pending = entry.get();
        if pending.conversation_id != conversation_id {
            return Err(AppError::Unauthorized(
                "permission belongs to another conversation".to_owned(),
            ));
        }
        validate_permission_decision(&pending.kind, &pending.details, &pending.options, &decision)?;
        let pending = entry.remove();
        pending
            .sender
            .send(decision)
            .map_err(|_| AppError::Conflict("permission request is no longer active".to_owned()))
    }

    pub fn expire_all(&self) {
        self.pending.clear();
    }
}

fn validate_permission_decision(
    kind: &str,
    details: &Value,
    options: &Value,
    decision: &PermissionDecision,
) -> Result<(), AppError> {
    let invalid = || {
        AppError::InvalidRequest(
            "permission response does not match the pending request".to_owned(),
        )
    };
    let options = options.as_array().ok_or_else(invalid)?;
    if !options.iter().any(|option| {
        option.get("kind").and_then(Value::as_str) == Some(decision.outcome.as_str())
            && decision
                .option_id
                .as_deref()
                .is_none_or(|id| option.get("optionId").and_then(Value::as_str) == Some(id))
    }) {
        return Err(invalid());
    }
    if matches!(decision.outcome, PermissionOutcome::Answer) {
        // Grok choice answers are encoded by the advertised option id, without a data object.
        if kind == "question" && decision.data.is_none() && decision.option_id.is_some() {
            return Ok(());
        }
        let data = decision.data.as_ref().ok_or_else(invalid)?;
        if kind == "user_input" {
            let answers = data
                .get("answers")
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            let questions = details
                .get("questions")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            if questions.is_empty()
                || answers.len() != questions.len()
                || !questions.iter().all(|question| {
                    question
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| answers.get(id))
                        .and_then(|answer| answer.get("answers"))
                        .and_then(Value::as_array)
                        .is_some_and(|values| {
                            !values.is_empty() && values.iter().all(Value::is_string)
                        })
                })
            {
                return Err(invalid());
            }
        } else if kind == "extension_ui" {
            match details.get("method").and_then(Value::as_str) {
                Some("confirm") if data.get("confirmed").is_some_and(Value::is_boolean) => {}
                Some("input" | "editor") if data.get("value").is_some_and(Value::is_string) => {}
                Some("select")
                    if data
                        .get("value")
                        .and_then(Value::as_str)
                        .is_some_and(|value| {
                            details
                                .get("options")
                                .and_then(Value::as_array)
                                .is_some_and(|options| {
                                    options.iter().any(|option| option.as_str() == Some(value))
                                })
                        }) => {}
                _ => return Err(invalid()),
            }
        } else if kind == "elicitation" {
            if !data.is_object() {
                return Err(invalid());
            }
            if details.get("mode").and_then(Value::as_str) == Some("url") {
                if data != &json!({ "completed": true }) {
                    return Err(invalid());
                }
            } else if let Some(schema) = details
                .get("requestedSchema")
                .or_else(|| details.get("schema"))
            {
                validate_elicitation_value(schema, data, 0)?;
            }
        } else if !matches!(kind, "question" | "questions") {
            return Err(invalid());
        }
    } else if decision.data.is_some()
        && !(kind == "plan"
            && matches!(decision.outcome, PermissionOutcome::RejectOnce)
            && decision
                .data
                .as_ref()
                .and_then(|data| data.get("feedback"))
                .is_some_and(Value::is_string))
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_elicitation_value(schema: &Value, value: &Value, depth: usize) -> Result<(), AppError> {
    let invalid =
        || AppError::InvalidRequest("Answer does not match the requested form schema.".to_owned());
    if depth > 16 {
        return Err(invalid());
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            return Err(invalid());
        }
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        return Err(invalid());
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value.as_object().ok_or_else(invalid)?;
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|key| key.as_str().is_none_or(|key| !object.contains_key(key)))
                })
            {
                return Err(invalid());
            }
            for (key, value) in object {
                if let Some(property) = properties.and_then(|properties| properties.get(key)) {
                    validate_elicitation_value(property, value, depth + 1)?;
                } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                    return Err(invalid());
                }
            }
        }
        Some("array") => {
            let items = value.as_array().ok_or_else(invalid)?;
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    validate_elicitation_value(item_schema, item, depth + 1)?;
                }
            }
            if schema
                .get("minItems")
                .and_then(Value::as_u64)
                .is_some_and(|min| items.len() < min as usize)
                || schema
                    .get("maxItems")
                    .and_then(Value::as_u64)
                    .is_some_and(|max| items.len() > max as usize)
            {
                return Err(invalid());
            }
        }
        Some("string") => {
            let text = value.as_str().ok_or_else(invalid)?;
            if schema
                .get("minLength")
                .and_then(Value::as_u64)
                .is_some_and(|min| text.chars().count() < min as usize)
                || schema
                    .get("maxLength")
                    .and_then(Value::as_u64)
                    .is_some_and(|max| text.chars().count() > max as usize)
            {
                return Err(invalid());
            }
        }
        Some("boolean") if !value.is_boolean() => return Err(invalid()),
        Some("integer") if !value.is_i64() && !value.is_u64() => return Err(invalid()),
        Some("number") if !value.is_number() => return Err(invalid()),
        Some("null") if !value.is_null() => return Err(invalid()),
        _ => {}
    }
    if let Some(number) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|min| number < min)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|max| number > max)
        {
            return Err(invalid());
        }
    }
    Ok(())
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
                "allow_once"
                    | "allow_always"
                    | "reject_once"
                    | "reject_always"
                    | "abort_turn"
                    | "answer"
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

    #[test]
    fn elicitation_form_validates_required_values_and_url_confirmation() {
        let options = json!([{ "kind": "answer", "optionId": "submit" }]);
        let schema = json!({"requestedSchema": {"type":"object", "required":["count","color"],
            "additionalProperties":false, "properties": {"count":{"type":"integer","minimum":1},
            "color":{"type":"string","enum":["red","blue"]}}}});
        let decision = |data| PermissionDecision {
            outcome: PermissionOutcome::Answer,
            option_id: Some("submit".to_owned()),
            data: Some(data),
        };
        assert!(validate_permission_decision(
            "elicitation",
            &schema,
            &options,
            &decision(json!({"count":2,"color":"red"}))
        )
        .is_ok());
        for data in [
            json!({"count":0,"color":"red"}),
            json!({"count":1.5,"color":"red"}),
            json!({"count":2,"color":"green"}),
            json!({"count":2}),
            json!({"count":2,"color":"red","extra":true}),
        ] {
            assert!(validate_permission_decision(
                "elicitation",
                &schema,
                &options,
                &decision(data)
            )
            .is_err());
        }
        assert!(validate_permission_decision(
            "elicitation",
            &json!({"mode":"url"}),
            &options,
            &decision(json!({"completed":true}))
        )
        .is_ok());
        assert!(validate_permission_decision(
            "elicitation",
            &json!({"mode":"url"}),
            &options,
            &decision(json!({}))
        )
        .is_err());
    }

    #[test]
    fn provider_capabilities_publish_image_input_in_camel_case() {
        let value = serde_json::to_value(ProviderCapabilities {
            permission_config: permission_config_capabilities(ProviderKind::Codex),
            native_fork: true,
            native_compact: true,
            native_resume: true,
            cancel: true,
            permissions: true,
            tool_events: true,
            native_skills: true,
            native_mcp: true,
            managed_mcp: true,
            model_selection: true,
            image_input: true,
            image_input_mode: ImageInputMode::Always,
        })
        .unwrap();

        assert_eq!(value["imageInput"], true);
        assert_eq!(value["imageInputMode"], "always");
        assert!(value.get("image_input").is_none());
    }

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
    #[test]
    fn permission_controls_reject_unsupported_and_conflicting_inputs() {
        assert!(resolve_permission_config(ProviderKind::Codex, None, Some("root"), None).is_err());
        assert!(resolve_permission_config(
            ProviderKind::Codex,
            Some("read-only"),
            Some("danger-full-access"),
            None
        )
        .is_err());
        assert!(
            resolve_permission_config(ProviderKind::Pi, None, Some("read-only"), None).is_err()
        );
        assert_eq!(
            resolve_permission_config(ProviderKind::ClaudeCode, None, Some("read-only"), None)
                .unwrap()
                .provider_mode
                .as_deref(),
            Some("plan")
        );
        assert_eq!(
            resolve_permission_config(ProviderKind::Codex, None, None, None)
                .unwrap()
                .approval_policy
                .as_deref(),
            Some("on-request")
        );
    }

    #[tokio::test]
    async fn invalid_permission_answers_preserve_pending_request() {
        let broker = PermissionBroker::default();
        let (sender, receiver) = oneshot::channel();
        broker.pending.insert(
            "p".to_owned(),
            PendingPermission {
                conversation_id: "c".to_owned(),
                sender,
                kind: "command".to_owned(),
                details: Value::Null,
                options: normalize_permission_options(
                    json!([{ "id": "allow", "kind": "allow_once", "name": "Allow" }]),
                )
                .unwrap(),
            },
        );
        let answer = PermissionDecision {
            outcome: PermissionOutcome::Answer,
            option_id: None,
            data: Some(json!({"answers": {}})),
        };
        assert!(broker.resolve("c", "p", answer).await.is_err());
        assert!(broker.pending.contains_key("p"));
        let valid = PermissionDecision {
            outcome: PermissionOutcome::AllowOnce,
            option_id: Some("allow".to_owned()),
            data: None,
        };
        assert!(broker.resolve("other", "p", valid.clone()).await.is_err());
        assert!(broker
            .resolve(
                "c",
                "p",
                PermissionDecision {
                    option_id: Some("forged".to_owned()),
                    ..valid.clone()
                }
            )
            .await
            .is_err());
        broker.resolve("c", "p", valid.clone()).await.unwrap();
        assert!(matches!(
            receiver.await.unwrap().outcome,
            PermissionOutcome::AllowOnce
        ));
        assert!(broker.resolve("c", "p", valid).await.is_err());
    }

    #[test]
    fn user_input_and_extension_answers_require_the_expected_shape() {
        let options = normalize_permission_options(
            json!([{ "id": "answer", "kind": "answer", "name": "Answer" }]),
        )
        .unwrap();
        let mut decision = PermissionDecision {
            outcome: PermissionOutcome::Answer,
            option_id: None,
            data: Some(json!({"answers": {}})),
        };
        let details = json!({"questions": [{"id": "q"}]});
        assert!(validate_permission_decision("user_input", &details, &options, &decision).is_err());
        decision.data = Some(json!({"answers": {"q": {"answers": ["yes"]}}}));
        assert!(validate_permission_decision("user_input", &details, &options, &decision).is_ok());
        assert!(validate_permission_decision(
            "extension_ui",
            &json!({"method":"confirm"}),
            &options,
            &decision
        )
        .is_err());
        decision.data = Some(json!({"confirmed": false}));
        assert!(validate_permission_decision(
            "extension_ui",
            &json!({"method":"confirm"}),
            &options,
            &decision
        )
        .is_ok());
    }
    #[test]
    fn grok_advertised_choice_does_not_require_answer_data() {
        let options = normalize_permission_options(
            json!([{ "id":"answer:0:1", "kind":"answer", "name":"Choice" }]),
        )
        .unwrap();
        let decision = PermissionDecision {
            outcome: PermissionOutcome::Answer,
            option_id: Some("answer:0:1".to_owned()),
            data: None,
        };
        assert!(validate_permission_decision("question", &json!({}), &options, &decision).is_ok());
    }
    #[test]
    fn removing_sandbox_does_not_implicitly_disable_approvals() {
        let effective =
            resolve_permission_config(ProviderKind::Codex, None, Some("danger-full-access"), None)
                .unwrap();
        assert_eq!(effective.approval_policy.as_deref(), Some("on-request"));
        let preset =
            resolve_permission_config(ProviderKind::Codex, Some(":danger-full-access"), None, None)
                .unwrap();
        assert_eq!(preset.approval_policy.as_deref(), Some("never"));
    }
}
