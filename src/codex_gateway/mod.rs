#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex, OnceLock},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex as AsyncMutex},
    time::{timeout, Duration},
};
use uuid::Uuid;

use crate::{
    config::expand_home,
    error::{AppError, Result},
    event::{EventBus, EventRecord},
};

use crate::server::protocol::{CodexLocalErrorCode, CodexLocalErrorPayload};

pub type GatewayRequestId = String;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexReviewDelivery {
    Inline,
    Detached,
}

impl Default for CodexReviewDelivery {
    fn default() -> Self {
        Self::Inline
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodexReviewTarget {
    UncommittedChanges,
    BaseBranch {
        branch: String,
    },
    Commit {
        sha: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Custom {
        instructions: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexApprovalOutcome {
    Pending,
    Accepted,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexMcpOutcome {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexMcpElicitationOutcome {
    Pending,
    Accepted,
    Declined,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexMcpElicitationResolution {
    pub request_id: GatewayRequestId,
    pub outcome: CodexMcpElicitationOutcome,
    pub response_type: String,
    pub response: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexApprovalResolution {
    pub request_id: GatewayRequestId,
    pub outcome: CodexApprovalOutcome,
    pub response_type: String,
    pub response: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexBackendKind {
    AppServer,
    Mcp,
    CloudTask,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexGatewayRequest {
    pub id: GatewayRequestId,
    pub backend: CodexBackendKind,
    #[serde(rename = "type")]
    pub request_type: String,
    pub payload: Value,
}

impl CodexGatewayRequest {
    pub fn new(
        id: impl Into<GatewayRequestId>,
        backend: CodexBackendKind,
        request_type: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            id: id.into(),
            backend,
            request_type: request_type.into(),
            payload,
        }
    }

    pub fn turn_start(
        id: impl Into<GatewayRequestId>,
        thread_id: impl Into<String>,
        input: Value,
        collaboration_mode: Option<CodexCollaborationMode>,
    ) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("threadId".to_string(), Value::String(thread_id.into()));
        payload.insert("input".to_string(), input);
        if let Some(collaboration_mode) = collaboration_mode {
            payload.insert(
                "collaborationMode".to_string(),
                serde_json::to_value(collaboration_mode)
                    .expect("serializing collaboration mode cannot fail"),
            );
        }

        Self::new(
            id,
            CodexBackendKind::AppServer,
            "codex.turn.start",
            Value::Object(payload),
        )
    }

    pub fn turn_input(
        id: impl Into<GatewayRequestId>,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        input: Value,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::AppServer,
            "codex.turn.input",
            json!({
                "threadId": thread_id.into(),
                "turnId": turn_id.into(),
                "input": input,
            }),
        )
    }

    pub fn turn_steer(
        id: impl Into<GatewayRequestId>,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        expected_turn_id: Option<String>,
        input: Value,
    ) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("threadId".to_string(), Value::String(thread_id.into()));
        payload.insert("turnId".to_string(), Value::String(turn_id.into()));
        payload.insert("input".to_string(), input);
        if let Some(expected_turn_id) = expected_turn_id {
            payload.insert(
                "expectedTurnId".to_string(),
                Value::String(expected_turn_id),
            );
        }
        Self::new(
            id,
            CodexBackendKind::AppServer,
            "codex.turn.steer",
            Value::Object(payload),
        )
    }

    pub fn turn_interrupt(
        id: impl Into<GatewayRequestId>,
        thread_id: impl Into<String>,
        turn_id: Option<String>,
    ) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("threadId".to_string(), Value::String(thread_id.into()));
        if let Some(turn_id) = turn_id {
            payload.insert("turnId".to_string(), Value::String(turn_id));
        }
        Self::new(
            id,
            CodexBackendKind::AppServer,
            "codex.turn.interrupt",
            Value::Object(payload),
        )
    }

    pub fn review_start(
        id: impl Into<GatewayRequestId>,
        thread_id: impl Into<String>,
        target: CodexReviewTarget,
        delivery: CodexReviewDelivery,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::AppServer,
            "codex.review.start",
            json!({
                "threadId": thread_id.into(),
                "target": target,
                "delivery": delivery,
            }),
        )
    }

    pub fn mcp_tool_call(
        id: impl Into<GatewayRequestId>,
        thread_id: impl Into<String>,
        server: impl Into<String>,
        tool: impl Into<String>,
        arguments: Option<Value>,
    ) -> Self {
        let mut payload = serde_json::Map::new();
        payload.insert("threadId".to_string(), Value::String(thread_id.into()));
        payload.insert("server".to_string(), Value::String(server.into()));
        payload.insert("tool".to_string(), Value::String(tool.into()));
        if let Some(arguments) = arguments {
            payload.insert("arguments".to_string(), arguments);
        }

        Self::new(
            id,
            CodexBackendKind::Mcp,
            "codex.mcp.tool.call",
            Value::Object(payload),
        )
    }

    pub fn cloud_task_create(
        id: impl Into<GatewayRequestId>,
        env_id: impl Into<String>,
        prompt: impl Into<String>,
        git_ref: impl Into<String>,
        qa_mode: bool,
        best_of_n: usize,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::CloudTask,
            "codex.cloudTask.create",
            json!({
                "envId": env_id.into(),
                "prompt": prompt.into(),
                "gitRef": git_ref.into(),
                "qaMode": qa_mode,
                "bestOfN": best_of_n,
            }),
        )
    }

    pub fn cloud_task_list(
        id: impl Into<GatewayRequestId>,
        env: Option<String>,
        limit: Option<i64>,
        cursor: Option<String>,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::CloudTask,
            "codex.cloudTask.list",
            json!({ "env": env, "limit": limit, "cursor": cursor }),
        )
    }

    pub fn cloud_task_get(
        id: impl Into<GatewayRequestId>,
        request_type: impl Into<String>,
        task_id: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::CloudTask,
            request_type,
            json!({ "taskId": task_id.into() }),
        )
    }

    pub fn cloud_task_list_sibling_attempts(
        id: impl Into<GatewayRequestId>,
        task_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::CloudTask,
            "codex.cloudTask.listSiblingAttempts",
            json!({ "taskId": task_id.into(), "turnId": turn_id.into() }),
        )
    }

    pub fn cloud_task_apply(
        id: impl Into<GatewayRequestId>,
        request_type: impl Into<String>,
        task_id: impl Into<String>,
        diff_override: Option<String>,
        turn_id: Option<String>,
        attempt_placement: Option<i64>,
    ) -> Self {
        Self::new(
            id,
            CodexBackendKind::CloudTask,
            request_type,
            json!({
                "taskId": task_id.into(),
                "diffOverride": diff_override,
                "turnId": turn_id,
                "attemptPlacement": attempt_placement,
            }),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexCloudTaskStatus {
    Pending,
    Ready,
    Applied,
    Error,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexCloudTaskAttemptStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCloudTaskDiffSummary {
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCloudTaskSummary {
    pub id: String,
    pub title: String,
    pub status: CodexCloudTaskStatus,
    pub updated_at: DateTime<Utc>,
    pub environment_id: Option<String>,
    pub environment_label: Option<String>,
    pub summary: CodexCloudTaskDiffSummary,
    #[serde(default)]
    pub is_review: bool,
    #[serde(default)]
    pub attempt_total: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCloudTaskTurnAttempt {
    pub turn_id: String,
    pub attempt_placement: Option<i64>,
    pub created_at: Option<DateTime<Utc>>,
    pub status: CodexCloudTaskAttemptStatus,
    pub diff: Option<String>,
    pub messages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexCloudTaskApplyStatus {
    Success,
    Partial,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCloudTaskApplyOutcome {
    pub applied: bool,
    pub status: CodexCloudTaskApplyStatus,
    pub message: String,
    #[serde(default)]
    pub skipped_paths: Vec<String>,
    #[serde(default)]
    pub conflict_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_placement: Option<i64>,
    #[serde(default)]
    pub preflight: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexModeKind {
    Default,
    Plan,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCollaborationSettings {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCollaborationMode {
    pub mode: CodexModeKind,
    pub settings: CodexCollaborationSettings,
}

impl CodexCollaborationMode {
    pub fn is_plan_mode(&self) -> bool {
        self.mode == CodexModeKind::Plan
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexGatewayResponse {
    pub id: GatewayRequestId,
    #[serde(rename = "type")]
    pub response_type: String,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexGatewayEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

impl CodexGatewayEvent {
    pub fn new(event_type: impl Into<String>, payload: Value) -> Self {
        Self {
            event_type: event_type.into(),
            payload,
        }
    }

    pub fn from_codex_notification(method: &str, payload: Value) -> Self {
        let event_type = match method {
            "error" => "codex.error",
            "thread/started" => "codex.thread.started",
            "thread/status/changed" => "codex.thread.statusChanged",
            "turn/started" => "codex.turn.started",
            "turn/completed" => "codex.turn.completed",
            "turn/diff/updated" => "codex.turn.diffUpdated",
            "turn/plan/updated" => "codex.turn.planUpdated",
            "serverRequest/resolved" => "codex.serverRequest.resolved",
            "item/agentMessage/delta" => "codex.item.agentMessage.delta",
            "item/plan/delta" => "codex.plan.delta",
            "item/started" => "codex.item.started",
            "item/completed" => "codex.item.completed",
            "item/commandExecution/outputDelta" => "codex.item.commandExecution.outputDelta",
            "item/commandExecution/terminalInteraction" => {
                "codex.item.commandExecution.terminalInteraction"
            }
            "item/commandExecution/requestApproval" => "codex.approval.commandExecution.request",
            "item/fileChange/requestApproval" => "codex.approval.fileChange.request",
            "item/permissions/requestApproval" => "codex.approval.permissions.request",
            "item/tool/requestUserInput" => "codex.tool.requestUserInput.request",
            "item/tool/call" => "codex.tool.call.request",
            "item/mcpToolCall/progress" => "codex.mcp.tool.progress",
            "mcpServer/elicitation/request" => "codex.mcp.elicitation.request",
            "mcpServer/startupStatus/updated" => "codex.mcp.server.statusUpdated",
            "mcpServer/oauthLogin/completed" => "codex.mcp.oauth.completed",
            _ => method,
        };
        Self::new(event_type, payload)
    }
}

pub type CodexGatewaySessionId = String;
pub type CodexGatewayEventId = String;
pub type CodexGatewayCursor = u64;

pub type CodexLocalAdapterCommandId = String;

const CODEX_LOCAL_ADAPTER_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexLocalAdapterLifecycleState {
    Idle,
    Starting,
    Ready,
    Busy,
    WaitingForApproval,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexLocalAdapterLifecycleErrorKind {
    InvalidTransition,
    AdapterUnavailable,
    CommandLaneBusy,
    SessionMismatch,
    MissingChildProcess,
    ChildProcessAlreadyOwned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{kind:?}: {message}")]
pub struct CodexLocalAdapterLifecycleError {
    pub kind: CodexLocalAdapterLifecycleErrorKind,
    pub codex_session_id: CodexGatewaySessionId,
    pub state: CodexLocalAdapterLifecycleState,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_command_id: Option<CodexLocalAdapterCommandId>,
}

impl CodexLocalAdapterLifecycleError {
    fn invalid_transition(
        codex_session_id: &str,
        state: CodexLocalAdapterLifecycleState,
        next_state: CodexLocalAdapterLifecycleState,
    ) -> Self {
        Self {
            kind: CodexLocalAdapterLifecycleErrorKind::InvalidTransition,
            codex_session_id: codex_session_id.to_owned(),
            state,
            message: format!(
                "cannot transition local CodeX adapter from {state:?} to {next_state:?}"
            ),
            in_flight_command_id: None,
        }
    }

    fn adapter_unavailable(codex_session_id: &str, state: CodexLocalAdapterLifecycleState) -> Self {
        Self {
            kind: CodexLocalAdapterLifecycleErrorKind::AdapterUnavailable,
            codex_session_id: codex_session_id.to_owned(),
            state,
            message: format!("local CodeX adapter is not ready; current state is {state:?}"),
            in_flight_command_id: None,
        }
    }

    fn command_lane_busy(
        codex_session_id: &str,
        state: CodexLocalAdapterLifecycleState,
        command_id: &str,
    ) -> Self {
        Self {
            kind: CodexLocalAdapterLifecycleErrorKind::CommandLaneBusy,
            codex_session_id: codex_session_id.to_owned(),
            state,
            message: format!(
                "local CodeX adapter already has in-flight command {command_id}; concurrent mutating commands are rejected"
            ),
            in_flight_command_id: Some(command_id.to_owned()),
        }
    }

    fn session_mismatch(
        codex_session_id: &str,
        state: CodexLocalAdapterLifecycleState,
        requested_session_id: &str,
    ) -> Self {
        Self {
            kind: CodexLocalAdapterLifecycleErrorKind::SessionMismatch,
            codex_session_id: codex_session_id.to_owned(),
            state,
            message: format!(
                "local CodeX adapter for session {codex_session_id} cannot own request for session {requested_session_id}"
            ),
            in_flight_command_id: None,
        }
    }

    fn missing_child_process(
        codex_session_id: &str,
        state: CodexLocalAdapterLifecycleState,
    ) -> Self {
        Self {
            kind: CodexLocalAdapterLifecycleErrorKind::MissingChildProcess,
            codex_session_id: codex_session_id.to_owned(),
            state,
            message: "local CodeX adapter cannot become ready without an owned child process"
                .to_owned(),
            in_flight_command_id: None,
        }
    }

    fn child_process_already_owned(
        codex_session_id: &str,
        state: CodexLocalAdapterLifecycleState,
    ) -> Self {
        Self {
            kind: CodexLocalAdapterLifecycleErrorKind::ChildProcessAlreadyOwned,
            codex_session_id: codex_session_id.to_owned(),
            state,
            message: "local CodeX adapter already owns one child process".to_owned(),
            in_flight_command_id: None,
        }
    }
}

pub type CodexLocalAdapterLifecycleResult<T> =
    std::result::Result<T, CodexLocalAdapterLifecycleError>;

pub type CodexLocalFixtureResult<T> = std::result::Result<T, CodexLocalFixtureError>;

#[derive(Debug)]
enum CodexLocalAdapterReaderMessage {
    Event(CodexGatewayEvent),
    ServerRequest(CodexServerRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexLocalFixtureScenario {
    HappyPath,
    MalformedEvent,
    DelayedReadyTimeout,
    Crash,
}

impl CodexLocalFixtureScenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::HappyPath => "happy-path",
            Self::MalformedEvent => "malformed-event",
            Self::DelayedReadyTimeout => "delayed-ready-timeout",
            Self::Crash => "crash",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodexLocalFixtureFrame {
    Event(CodexGatewayEvent),
    ServerRequest(CodexServerRequest),
    Outbound(CodexOutboundMessage),
    Error(CodexLocalErrorPayload),
}

impl CodexLocalFixtureFrame {
    pub fn event_type(&self) -> Option<&str> {
        match self {
            Self::Event(event) => Some(&event.event_type),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{code:?}: {message}")]
pub struct CodexLocalFixtureError {
    pub code: CodexLocalErrorCode,
    pub message: String,
    pub codex_session_id: CodexGatewaySessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
}

impl CodexLocalFixtureError {
    fn new(
        code: CodexLocalErrorCode,
        message: impl Into<String>,
        codex_session_id: &str,
        request_id: Option<&str>,
        operation: Option<&str>,
        upstream_request_id: Option<&str>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            codex_session_id: codex_session_id.to_owned(),
            request_id: request_id.map(ToOwned::to_owned),
            operation: operation.map(ToOwned::to_owned),
            upstream_request_id: upstream_request_id.map(ToOwned::to_owned),
        }
    }

    pub fn payload(&self) -> CodexLocalErrorPayload {
        CodexLocalErrorPayload {
            code: self.code,
            message: self.message.clone(),
            codex_session_id: self.codex_session_id.clone(),
            request_id: self.request_id.clone(),
            operation: self.operation.clone(),
            upstream_request_id: self.upstream_request_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexLocalFixtureTranscript {
    pub scenario: CodexLocalFixtureScenario,
    pub codex_session_id: CodexGatewaySessionId,
    pub frames: Vec<CodexLocalFixtureFrame>,
}

#[derive(Clone, Debug)]
pub struct CodexLocalFixture {
    scenario: CodexLocalFixtureScenario,
    runtime: CodexLocalAdapterRuntime,
    thread_id: String,
    turn_id: String,
    next_upstream_id: usize,
    frames: Vec<CodexLocalFixtureFrame>,
}

impl CodexLocalFixture {
    pub fn new(
        scenario: CodexLocalFixtureScenario,
        codex_session_id: impl Into<CodexGatewaySessionId>,
    ) -> Self {
        Self {
            scenario,
            runtime: CodexLocalAdapterRuntime::new(codex_session_id),
            thread_id: "thread-fixture-1".to_owned(),
            turn_id: "turn-fixture-1".to_owned(),
            next_upstream_id: 1,
            frames: Vec::new(),
        }
    }

    pub fn state(&self) -> CodexLocalAdapterLifecycleState {
        self.runtime.state
    }

    pub fn frames(&self) -> &[CodexLocalFixtureFrame] {
        &self.frames
    }

    pub fn into_transcript(self) -> CodexLocalFixtureTranscript {
        CodexLocalFixtureTranscript {
            scenario: self.scenario,
            codex_session_id: self.runtime.codex_session_id,
            frames: self.frames,
        }
    }

    pub fn start(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        self.runtime.start().map_err(|error| {
            CodexLocalFixtureError::new(
                CodexLocalErrorCode::UnsupportedAction,
                error.message,
                &self.runtime.codex_session_id,
                Some(request_id),
                Some("codex.local.start"),
                None,
            )
        })?;
        self.push_lifecycle_event(
            "codex.local.lifecycle",
            Some(request_id),
            "codex.local.start",
        );

        match self.scenario {
            CodexLocalFixtureScenario::DelayedReadyTimeout => {
                let error = CodexLocalFixtureError::new(
                    CodexLocalErrorCode::StartupTimeout,
                    "fixture delayed ready scenario timed out before ready",
                    &self.runtime.codex_session_id,
                    Some(request_id),
                    Some("codex.local.start"),
                    None,
                );
                self.runtime.fail();
                self.push_error(error.clone());
                Err(error)
            }
            CodexLocalFixtureScenario::Crash => {
                self.runtime
                    .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
                    .map_err(|error| {
                        CodexLocalFixtureError::new(
                            CodexLocalErrorCode::UnsupportedAction,
                            error.message,
                            &self.runtime.codex_session_id,
                            Some(request_id),
                            Some("codex.local.start"),
                            None,
                        )
                    })?;
                self.push_lifecycle_event(
                    "codex.local.lifecycle",
                    Some(request_id),
                    "codex.local.ready",
                );
                let error = CodexLocalFixtureError::new(
                    CodexLocalErrorCode::AdapterCrash,
                    "fixture crash scenario closed the structured app-server channel",
                    &self.runtime.codex_session_id,
                    Some(request_id),
                    Some("codex.local.start"),
                    None,
                );
                self.runtime.fail();
                self.push_error(error.clone());
                Err(error)
            }
            _ => {
                self.runtime
                    .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
                    .map_err(|error| {
                        CodexLocalFixtureError::new(
                            CodexLocalErrorCode::UnsupportedAction,
                            error.message,
                            &self.runtime.codex_session_id,
                            Some(request_id),
                            Some("codex.local.start"),
                            None,
                        )
                    })?;
                self.push_lifecycle_event(
                    "codex.local.lifecycle",
                    Some(request_id),
                    "codex.local.ready",
                );
                Ok(())
            }
        }
    }

    pub fn run_turn(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        if self.scenario == CodexLocalFixtureScenario::MalformedEvent {
            let upstream_request_id = self.next_upstream_request_id();
            let error = CodexLocalFixtureError::new(
                CodexLocalErrorCode::MalformedEvent,
                "fixture emitted malformed app-server event without a type field",
                &self.runtime.codex_session_id,
                Some(request_id),
                Some("codex.local.turn"),
                Some(&upstream_request_id),
            );
            self.runtime.fail();
            self.push_error(error.clone());
            return Err(error);
        }

        self.runtime
            .begin_mutating_command(request_id)
            .map_err(|error| {
                self.fixture_error_from_lifecycle(error, request_id, "codex.local.turn")
            })?;
        self.push_event(
            "codex.turn.started",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "lifecycleState": self.runtime.state,
                "input": [{ "type": "text", "text": "fixture turn input" }]
            }),
        );
        self.push_event(
            "codex.item.agentMessage.delta",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "itemId": "agent-message-fixture-1",
                "delta": "fixture output chunk 1"
            }),
        );
        self.push_event(
            "codex.item.agentMessage.delta",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "itemId": "agent-message-fixture-1",
                "delta": "fixture output chunk 2"
            }),
        );
        self.push_event(
            "codex.turn.planUpdated",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "planId": "plan-fixture-1",
                "items": [
                    { "text": "Use deterministic local fixture", "status": "completed" }
                ],
                "explanation": "fixture plan update"
            }),
        );
        self.runtime.wait_for_approval().map_err(|error| {
            self.fixture_error_from_lifecycle(error, request_id, "codex.local.turn")
        })?;
        self.push_server_request(CodexServerRequest {
            id: "approval-fixture-1".to_owned(),
            request_type: "codex.approval.commandExecution.request".to_owned(),
            payload: json!({
                "requestId": "approval-fixture-1",
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "command": "cargo test fixture",
                "outcome": "pending"
            }),
        });
        Ok(())
    }

    pub fn steer(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        self.push_outbound(CodexOutboundMessage::Request(CodexGatewayRequest::new(
            request_id,
            CodexBackendKind::AppServer,
            "codex.turn.steer",
            json!({
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "expectedTurnId": self.turn_id,
                "input": [{ "type": "text", "text": "fixture steering input" }]
            }),
        )));
        self.push_event(
            "codex.turn.steerUpdated",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "expectedTurnId": self.turn_id,
                "input": [{ "type": "text", "text": "fixture steering input" }]
            }),
        );
        Ok(())
    }

    pub fn respond_to_approval(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        self.runtime.resume_after_approval().map_err(|error| {
            self.fixture_error_from_lifecycle(error, request_id, "codex.local.approval.respond")
        })?;
        self.push_outbound(CodexOutboundMessage::ServerResponse(CodexServerResponse {
            id: "approval-fixture-1".to_owned(),
            response_type: "codex.approval.commandExecution.respond".to_owned(),
            payload: json!({ "decision": "accept" }),
        }));
        self.push_event(
            "codex.serverRequest.resolved",
            json!({
                "requestId": "approval-fixture-1",
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "outcome": "accepted",
                "responseType": "codex.approval.commandExecution.respond",
                "response": { "decision": "accept" }
            }),
        );
        self.runtime.complete_mutating_command().map_err(|error| {
            self.fixture_error_from_lifecycle(error, request_id, "codex.local.approval.respond")
        })?;
        self.push_event(
            "codex.turn.completed",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "lifecycleState": self.runtime.state
            }),
        );
        Ok(())
    }

    pub fn start_review(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        self.push_outbound(CodexOutboundMessage::Request(
            CodexGatewayRequest::review_start(
                request_id,
                &self.thread_id,
                CodexReviewTarget::Custom {
                    instructions: "fixture review".to_owned(),
                },
                CodexReviewDelivery::Inline,
            ),
        ));
        self.push_event(
            "codex.review.started",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "target": { "type": "custom", "instructions": "fixture review" },
                "delivery": "inline"
            }),
        );
        self.push_event(
            "codex.review.start.result",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "reviewThreadId": self.thread_id,
                "turn": { "id": self.turn_id, "status": "completed" }
            }),
        );
        Ok(())
    }

    pub fn interrupt(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        self.push_outbound(CodexOutboundMessage::Request(CodexGatewayRequest::new(
            request_id,
            CodexBackendKind::AppServer,
            "codex.turn.interrupt",
            json!({
                "threadId": self.thread_id,
                "turnId": self.turn_id
            }),
        )));
        self.push_event(
            "codex.turn.interrupted",
            json!({
                "requestId": request_id,
                "threadId": self.thread_id,
                "turnId": self.turn_id,
                "status": "interrupted"
            }),
        );
        Ok(())
    }

    pub fn stop(&mut self, request_id: &str) -> CodexLocalFixtureResult<()> {
        self.runtime.begin_stopping().map_err(|error| {
            self.fixture_error_from_lifecycle(error, request_id, "codex.local.stop")
        })?;
        self.push_lifecycle_event(
            "codex.local.lifecycle",
            Some(request_id),
            "codex.local.stopping",
        );
        self.runtime.finish_stopping().map_err(|error| {
            self.fixture_error_from_lifecycle(error, request_id, "codex.local.stop")
        })?;
        self.push_lifecycle_event(
            "codex.local.lifecycle",
            Some(request_id),
            "codex.local.stopped",
        );
        Ok(())
    }

    fn fixture_error_from_lifecycle(
        &self,
        error: CodexLocalAdapterLifecycleError,
        request_id: &str,
        operation: &str,
    ) -> CodexLocalFixtureError {
        let code = match error.kind {
            CodexLocalAdapterLifecycleErrorKind::CommandLaneBusy => {
                CodexLocalErrorCode::SessionBusy
            }
            CodexLocalAdapterLifecycleErrorKind::AdapterUnavailable
            | CodexLocalAdapterLifecycleErrorKind::InvalidTransition
            | CodexLocalAdapterLifecycleErrorKind::SessionMismatch
            | CodexLocalAdapterLifecycleErrorKind::MissingChildProcess
            | CodexLocalAdapterLifecycleErrorKind::ChildProcessAlreadyOwned => {
                CodexLocalErrorCode::UnsupportedAction
            }
        };
        CodexLocalFixtureError::new(
            code,
            error.message,
            &self.runtime.codex_session_id,
            Some(request_id),
            Some(operation),
            None,
        )
    }

    fn next_upstream_request_id(&mut self) -> String {
        let id = format!("fixture-upstream-{}", self.next_upstream_id);
        self.next_upstream_id += 1;
        id
    }

    fn push_lifecycle_event(
        &mut self,
        event_type: &str,
        request_id: Option<&str>,
        operation: &str,
    ) {
        self.push_event(
            event_type,
            json!({
                "requestId": request_id,
                "operation": operation,
                "codexSessionId": self.runtime.codex_session_id,
                "lifecycleState": self.runtime.state,
                "scenario": self.scenario.name()
            }),
        );
    }

    fn push_event(&mut self, event_type: &str, payload: Value) {
        self.frames
            .push(CodexLocalFixtureFrame::Event(CodexGatewayEvent::new(
                event_type, payload,
            )));
    }

    fn push_server_request(&mut self, request: CodexServerRequest) {
        self.frames
            .push(CodexLocalFixtureFrame::ServerRequest(request));
    }

    fn push_outbound(&mut self, message: CodexOutboundMessage) {
        self.frames.push(CodexLocalFixtureFrame::Outbound(message));
    }

    fn push_error(&mut self, error: CodexLocalFixtureError) {
        self.frames
            .push(CodexLocalFixtureFrame::Error(error.payload()));
        self.push_lifecycle_event(
            "codex.local.lifecycle",
            error.request_id.as_deref(),
            "fixture.error",
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexLocalAdapterChildProcess {
    pub pid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexLocalAdapterCommandLane {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_flight_command_id: Option<CodexLocalAdapterCommandId>,
}

impl CodexLocalAdapterCommandLane {
    pub fn is_idle(&self) -> bool {
        self.in_flight_command_id.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexLocalAdapterRuntime {
    pub codex_session_id: CodexGatewaySessionId,
    pub state: CodexLocalAdapterLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_process: Option<CodexLocalAdapterChildProcess>,
    pub command_lane: CodexLocalAdapterCommandLane,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexLocalAdapterStartOptions {
    pub codex_session_id: CodexGatewaySessionId,
    pub request_id: String,
    pub cwd: PathBuf,
    pub binary: String,
    #[serde(default = "default_codex_local_adapter_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
}

impl CodexLocalAdapterStartOptions {
    pub fn new(
        codex_session_id: impl Into<CodexGatewaySessionId>,
        request_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        binary: impl Into<String>,
    ) -> Self {
        Self {
            codex_session_id: codex_session_id.into(),
            request_id: request_id.into(),
            cwd: cwd.into(),
            binary: binary.into(),
            startup_timeout_ms: CODEX_LOCAL_ADAPTER_STARTUP_TIMEOUT.as_millis() as u64,
        }
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms)
    }
}

fn default_codex_local_adapter_startup_timeout_ms() -> u64 {
    CODEX_LOCAL_ADAPTER_STARTUP_TIMEOUT.as_millis() as u64
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{payload:?}")]
pub struct CodexLocalAdapterProcessError {
    pub payload: CodexLocalErrorPayload,
}

impl CodexLocalAdapterProcessError {
    fn new(
        code: CodexLocalErrorCode,
        message: impl Into<String>,
        codex_session_id: &str,
        request_id: Option<&str>,
        operation: &str,
    ) -> Self {
        Self {
            payload: CodexLocalErrorPayload {
                code,
                message: message.into(),
                codex_session_id: codex_session_id.to_owned(),
                request_id: request_id.map(ToOwned::to_owned),
                operation: Some(operation.to_owned()),
                upstream_request_id: None,
            },
        }
    }
}

pub type CodexLocalAdapterProcessResult<T> = std::result::Result<T, CodexLocalAdapterProcessError>;

#[derive(Clone)]
pub struct CodexLocalAdapterSupervisor {
    adapters: Arc<DashMap<CodexGatewaySessionId, Arc<LocalCodexAdapter>>>,
    store: CodexGatewayStore,
    events: EventBus,
}

impl CodexLocalAdapterSupervisor {
    pub fn new(store: CodexGatewayStore, events: EventBus) -> Self {
        Self {
            adapters: Arc::new(DashMap::new()),
            store,
            events,
        }
    }

    pub async fn start(
        &self,
        options: CodexLocalAdapterStartOptions,
    ) -> CodexLocalAdapterProcessResult<Arc<LocalCodexAdapter>> {
        if self.adapters.contains_key(&options.codex_session_id) {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter already owns this session",
                &options.codex_session_id,
                Some(&options.request_id),
                "codex.local.start",
            ));
        }

        let adapter =
            LocalCodexAdapter::start(options.clone(), self.store.clone(), self.events.clone())
                .await?;
        let adapter = Arc::new(adapter);
        if self.adapters.contains_key(&options.codex_session_id) {
            let _ = adapter.stop(&options.request_id, true).await;
            Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter already owns this session",
                &options.codex_session_id,
                Some(&options.request_id),
                "codex.local.start",
            ))
        } else {
            self.adapters
                .insert(options.codex_session_id.clone(), adapter.clone());
            Ok(adapter)
        }
    }

    pub fn get(&self, codex_session_id: &str) -> Option<Arc<LocalCodexAdapter>> {
        self.adapters
            .get(codex_session_id)
            .map(|entry| entry.value().clone())
    }

    pub async fn stop(
        &self,
        codex_session_id: &str,
        request_id: &str,
        force: bool,
    ) -> CodexLocalAdapterProcessResult<CodexLocalAdapterRuntime> {
        let Some((_, adapter)) = self.adapters.remove(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.stop",
            ));
        };
        adapter.stop(request_id, force).await
    }

    pub async fn run_turn(
        &self,
        codex_session_id: &str,
        request_id: &str,
        thread_id: &str,
        input: Value,
        collaboration_mode: Option<Value>,
    ) -> CodexLocalAdapterProcessResult<()> {
        let Some(adapter) = self.get(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.turn",
            ));
        };
        adapter
            .run_turn(request_id, thread_id, input, collaboration_mode)
            .await
    }

    pub async fn send_input(
        &self,
        codex_session_id: &str,
        request_id: &str,
        thread_id: &str,
        turn_id: &str,
        input: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let Some(adapter) = self.get(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.input",
            ));
        };
        adapter
            .send_input(request_id, thread_id, turn_id, input)
            .await
    }

    pub async fn steer(
        &self,
        codex_session_id: &str,
        request_id: &str,
        thread_id: &str,
        turn_id: &str,
        expected_turn_id: Option<String>,
        input: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let Some(adapter) = self.get(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.steer",
            ));
        };
        adapter
            .steer(request_id, thread_id, turn_id, expected_turn_id, input)
            .await
    }

    pub async fn interrupt(
        &self,
        codex_session_id: &str,
        request_id: &str,
        thread_id: &str,
        turn_id: Option<String>,
    ) -> CodexLocalAdapterProcessResult<()> {
        let Some(adapter) = self.get(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.interrupt",
            ));
        };
        adapter.interrupt(request_id, thread_id, turn_id).await
    }

    pub async fn respond_to_approval(
        &self,
        codex_session_id: &str,
        request_id: &str,
        upstream_request_id: &str,
        response_type: &str,
        response: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let Some(adapter) = self.get(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.approval.respond",
            ));
        };
        adapter
            .respond_to_approval(request_id, upstream_request_id, response_type, response)
            .await
    }

    pub async fn send_request(
        &self,
        codex_session_id: &str,
        request_id: &str,
        method: &str,
        params: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let Some(adapter) = self.get(codex_session_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter is not running for this session",
                codex_session_id,
                Some(request_id),
                "codex.local.request",
            ));
        };
        adapter.send_request(request_id, method, params).await
    }

    pub async fn shutdown_all(&self) {
        let adapters = self
            .adapters
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<Vec<_>>();
        for (session_id, adapter) in adapters {
            self.adapters.remove(&session_id);
            let _ = adapter.stop("shutdown", true).await;
        }
    }

    pub fn len(&self) -> usize {
        self.adapters.len()
    }
}

pub struct LocalCodexAdapter {
    runtime: Arc<AsyncMutex<CodexLocalAdapterRuntime>>,
    child: Arc<AsyncMutex<Option<Child>>>,
    stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
    stderr: Arc<AsyncMutex<Option<ChildStderr>>>,
    store: CodexGatewayStore,
    events: EventBus,
    pending_server_requests: Arc<DashMap<GatewayRequestId, CodexServerRequest>>,
}

impl std::fmt::Debug for LocalCodexAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalCodexAdapter")
            .finish_non_exhaustive()
    }
}

impl LocalCodexAdapter {
    async fn start(
        options: CodexLocalAdapterStartOptions,
        store: CodexGatewayStore,
        events: EventBus,
    ) -> CodexLocalAdapterProcessResult<Self> {
        let mut options = options;
        options.cwd = normalize_local_codex_cwd(options.cwd);
        validate_local_codex_cwd(&options)?;

        let mut initial_runtime = CodexLocalAdapterRuntime::new(options.codex_session_id.clone());
        initial_runtime.start().map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                error.message,
                &options.codex_session_id,
                Some(&options.request_id),
                "codex.local.start",
            )
        })?;
        append_local_lifecycle_event(
            &store,
            &initial_runtime,
            Some(&options.request_id),
            "codex.control.starting",
            "codex.local.start",
        )
        .await?;
        let runtime = Arc::new(AsyncMutex::new(initial_runtime));

        let spawn_result = Command::new(&options.binary)
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .current_dir(&options.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                let mut runtime = runtime.lock().await;
                runtime.fail();
                let process_error = spawn_error(&options, error);
                append_local_error_event(&store, &runtime, &process_error.payload).await?;
                return Err(process_error);
            }
        };

        let mut stdin = child.stdin.take().ok_or_else(|| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                "spawned Codex app-server did not expose stdin",
                &options.codex_session_id,
                Some(&options.request_id),
                "codex.local.start",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                "spawned Codex app-server did not expose stdout",
                &options.codex_session_id,
                Some(&options.request_id),
                "codex.local.start",
            )
        })?;
        let stderr = child.stderr.take();
        write_json_line_to_child_stdin(
            &mut stdin,
            &codex_initialize_request(&options.request_id),
            &options.codex_session_id,
            Some(&options.request_id),
            "codex.local.start",
        )
        .await?;

        let pid = child.id().unwrap_or_default();
        let pending_server_requests = Arc::new(DashMap::new());
        let (ready_tx, ready_rx) = oneshot::channel();
        spawn_local_stdout_reader(
            options.codex_session_id.clone(),
            options.request_id.clone(),
            stdout,
            store.clone(),
            events.clone(),
            runtime.clone(),
            pending_server_requests.clone(),
            ready_tx,
        );
        let ready = timeout(options.startup_timeout(), ready_rx).await;
        match ready {
            Ok(Ok(Ok(()))) => {
                write_json_line_to_child_stdin(
                    &mut stdin,
                    &json!({
                        "method": "initialized"
                    }),
                    &options.codex_session_id,
                    Some(&options.request_id),
                    "codex.local.start",
                )
                .await?;
                let mut runtime_guard = runtime.lock().await;
                runtime_guard
                    .attach_child_process(CodexLocalAdapterChildProcess { pid })
                    .map_err(|error| {
                        CodexLocalAdapterProcessError::new(
                            CodexLocalErrorCode::UnsupportedAction,
                            error.message,
                            &options.codex_session_id,
                            Some(&options.request_id),
                            "codex.local.start",
                        )
                    })?;
                append_local_lifecycle_event(
                    &store,
                    &runtime_guard,
                    Some(&options.request_id),
                    "codex.control.ready",
                    "codex.local.start",
                )
                .await?;
                drop(runtime_guard);
                Ok(Self {
                    runtime,
                    child: Arc::new(AsyncMutex::new(Some(child))),
                    stdin: Arc::new(AsyncMutex::new(Some(stdin))),
                    stderr: Arc::new(AsyncMutex::new(stderr)),
                    store,
                    events,
                    pending_server_requests,
                })
            }
            Ok(Ok(Err(error))) => {
                let mut runtime = runtime.lock().await;
                runtime.fail();
                let _ = child.kill().await;
                let process_error = CodexLocalAdapterProcessError::new(
                    CodexLocalErrorCode::AdapterCrash,
                    error,
                    &options.codex_session_id,
                    Some(&options.request_id),
                    "codex.local.start",
                );
                append_local_error_event(&store, &runtime, &process_error.payload).await?;
                Err(process_error)
            }
            Ok(Err(_)) => {
                let mut runtime = runtime.lock().await;
                runtime.fail();
                let _ = child.kill().await;
                let process_error = CodexLocalAdapterProcessError::new(
                    CodexLocalErrorCode::AdapterCrash,
                    "Codex app-server stdout reader stopped before ready",
                    &options.codex_session_id,
                    Some(&options.request_id),
                    "codex.local.start",
                );
                append_local_error_event(&store, &runtime, &process_error.payload).await?;
                Err(process_error)
            }
            Err(_) => {
                let mut runtime = runtime.lock().await;
                runtime.fail();
                let _ = child.kill().await;
                let process_error = CodexLocalAdapterProcessError::new(
                    CodexLocalErrorCode::StartupTimeout,
                    "Codex app-server did not emit structured codex.control.ready before startup timeout",
                    &options.codex_session_id,
                    Some(&options.request_id),
                    "codex.local.start",
                );
                append_local_error_event(&store, &runtime, &process_error.payload).await?;
                Err(process_error)
            }
        }
    }

    pub async fn runtime(&self) -> CodexLocalAdapterRuntime {
        self.runtime.lock().await.clone()
    }

    pub async fn run_turn(
        &self,
        request_id: &str,
        thread_id: &str,
        input: Value,
        collaboration_mode: Option<Value>,
    ) -> CodexLocalAdapterProcessResult<()> {
        let codex_session_id = self.runtime.lock().await.codex_session_id.clone();
        let collaboration_mode = collaboration_mode
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                CodexLocalAdapterProcessError::new(
                    CodexLocalErrorCode::MalformedEvent,
                    error.to_string(),
                    &codex_session_id,
                    Some(request_id),
                    "codex.local.turn",
                )
            })?;
        let request =
            CodexGatewayRequest::turn_start(request_id, thread_id, input, collaboration_mode);
        self.dispatch_mutating_request(request_id, "codex.local.turn", request)
            .await
    }

    pub async fn send_input(
        &self,
        request_id: &str,
        thread_id: &str,
        turn_id: &str,
        input: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let request = CodexGatewayRequest::turn_input(request_id, thread_id, turn_id, input);
        self.dispatch_mutating_request(request_id, "codex.local.input", request)
            .await
    }

    pub async fn steer(
        &self,
        request_id: &str,
        thread_id: &str,
        turn_id: &str,
        expected_turn_id: Option<String>,
        input: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let request = CodexGatewayRequest::turn_steer(
            request_id,
            thread_id,
            turn_id,
            expected_turn_id,
            input,
        );
        self.dispatch_control_request(request_id, "codex.local.steer", request)
            .await
    }

    pub async fn interrupt(
        &self,
        request_id: &str,
        thread_id: &str,
        turn_id: Option<String>,
    ) -> CodexLocalAdapterProcessResult<()> {
        let request = CodexGatewayRequest::turn_interrupt(request_id, thread_id, turn_id);
        self.dispatch_control_request(request_id, "codex.local.interrupt", request)
            .await
    }

    pub async fn respond_to_approval(
        &self,
        request_id: &str,
        upstream_request_id: &str,
        response_type: &str,
        response: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let runtime = self.runtime.lock().await.clone();
        let Some((_, request)) = self.pending_server_requests.remove(upstream_request_id) else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                "local Codex adapter has no pending server request for that response",
                &runtime.codex_session_id,
                Some(request_id),
                "codex.local.approval.respond",
            ));
        };
        let message = CodexOutboundMessage::ServerResponse(CodexServerResponse {
            id: upstream_request_id.to_string(),
            response_type: response_type.to_string(),
            payload: response.clone(),
        });
        let resolved_event = match request.request_type.as_str() {
            "codex.tool.requestUserInput.request" => Some(CodexGatewayEvent::new(
                "codex.serverRequest.resolved",
                json!({
                    "requestId": upstream_request_id,
                    "threadId": payload_string(&request.payload, &["threadId", "thread_id", "codexThreadId"]),
                }),
            )),
            "codex.mcp.elicitation.request" => {
                CodexGatewayEvent::elicitation_resolved(&CodexServerResponse {
                    id: upstream_request_id.to_string(),
                    response_type: response_type.to_string(),
                    payload: response.clone(),
                })
                .map_err(|error| {
                    CodexLocalAdapterProcessError::new(
                        CodexLocalErrorCode::MalformedEvent,
                        error.to_string(),
                        &runtime.codex_session_id,
                        Some(request_id),
                        "codex.local.approval.respond",
                    )
                })?
            }
            _ => CodexGatewayEvent::approval_resolved(&CodexServerResponse {
                id: upstream_request_id.to_string(),
                response_type: response_type.to_string(),
                payload: response.clone(),
            })
            .map_err(|error| {
                CodexLocalAdapterProcessError::new(
                    CodexLocalErrorCode::MalformedEvent,
                    error.to_string(),
                    &runtime.codex_session_id,
                    Some(request_id),
                    "codex.local.approval.respond",
                )
            })?,
        };
        if let Some(event) = resolved_event {
            let record = self
                .store
                .append_event(&runtime.codex_session_id, event)
                .await
                .map_err(|error| {
                    CodexLocalAdapterProcessError::new(
                        CodexLocalErrorCode::AdapterCrash,
                        error.to_string(),
                        &runtime.codex_session_id,
                        Some(request_id),
                        "codex.local.approval.respond",
                    )
                })?;
            publish_codex_gateway_record_to_bus(&self.events, record).await;
        }
        append_local_request_accepted_event(
            &self.store,
            &runtime,
            request_id,
            "codex.local.approval.respond",
            &json!({
                "requestId": upstream_request_id,
                "responseType": response_type,
                "response": response,
            }),
        )
        .await?;
        self.write_outbound(message).await
    }

    pub async fn send_request(
        &self,
        request_id: &str,
        method: &str,
        params: Value,
    ) -> CodexLocalAdapterProcessResult<()> {
        let message = CodexOutboundMessage::Request(CodexGatewayRequest::new(
            request_id.to_owned(),
            codex_backend_kind_for_method(method),
            codex_method_to_request_type(method),
            params,
        ));
        let runtime = self.runtime.lock().await.clone();
        append_local_request_accepted_event(
            &self.store,
            &runtime,
            request_id,
            "codex.local.request",
            &json!({ "method": method }),
        )
        .await?;
        self.write_outbound(message).await
    }

    async fn dispatch_mutating_request(
        &self,
        request_id: &str,
        operation: &str,
        request: CodexGatewayRequest,
    ) -> CodexLocalAdapterProcessResult<()> {
        let runtime_after_accept = {
            let mut runtime = self.runtime.lock().await;
            if let Err(error) = runtime.begin_mutating_command(request_id) {
                let process_error =
                    local_process_error_from_lifecycle(error, request_id, operation);
                append_local_request_rejected_event(&self.store, &runtime, &process_error.payload)
                    .await?;
                return Err(process_error);
            }
            runtime.clone()
        };

        append_local_request_accepted_event(
            &self.store,
            &runtime_after_accept,
            request_id,
            operation,
            &request.payload,
        )
        .await?;

        if let Err(error) = self
            .write_outbound(CodexOutboundMessage::Request(request))
            .await
        {
            let mut runtime = self.runtime.lock().await;
            runtime.fail();
            append_local_error_event(&self.store, &runtime, &error.payload).await?;
            return Err(error);
        }

        Ok(())
    }

    async fn dispatch_control_request(
        &self,
        request_id: &str,
        operation: &str,
        request: CodexGatewayRequest,
    ) -> CodexLocalAdapterProcessResult<()> {
        let runtime = self.runtime.lock().await.clone();
        append_local_request_accepted_event(
            &self.store,
            &runtime,
            request_id,
            operation,
            &request.payload,
        )
        .await?;
        self.write_outbound(CodexOutboundMessage::Request(request))
            .await
    }

    async fn write_outbound(
        &self,
        message: CodexOutboundMessage,
    ) -> CodexLocalAdapterProcessResult<()> {
        let codex_session_id = self.runtime.lock().await.codex_session_id.clone();
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                "local Codex adapter stdin is closed",
                &codex_session_id,
                outbound_request_id(&message).as_deref(),
                outbound_operation(&message)
                    .as_deref()
                    .unwrap_or("codex.local.turn"),
            ));
        };
        let wire_message = outbound_message_to_jsonrpc(message.clone());
        let line = serde_json::to_vec(&wire_message).map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::MalformedEvent,
                error.to_string(),
                &codex_session_id,
                outbound_request_id(&message).as_deref(),
                outbound_operation(&message)
                    .as_deref()
                    .unwrap_or("codex.local.turn"),
            )
        })?;
        stdin.write_all(&line).await.map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &codex_session_id,
                outbound_request_id(&message).as_deref(),
                outbound_operation(&message)
                    .as_deref()
                    .unwrap_or("codex.local.turn"),
            )
        })?;
        stdin.write_all(b"\n").await.map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &codex_session_id,
                outbound_request_id(&message).as_deref(),
                outbound_operation(&message)
                    .as_deref()
                    .unwrap_or("codex.local.turn"),
            )
        })?;
        stdin.flush().await.map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &codex_session_id,
                outbound_request_id(&message).as_deref(),
                outbound_operation(&message)
                    .as_deref()
                    .unwrap_or("codex.local.turn"),
            )
        })
    }

    pub async fn stop(
        &self,
        request_id: &str,
        force: bool,
    ) -> CodexLocalAdapterProcessResult<CodexLocalAdapterRuntime> {
        let mut runtime = self.runtime.lock().await;
        runtime.begin_stopping().map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                error.message,
                &runtime.codex_session_id,
                Some(request_id),
                "codex.local.stop",
            )
        })?;
        append_local_lifecycle_event(
            &self.store,
            &runtime,
            Some(request_id),
            "codex.control.stopping",
            "codex.local.stop",
        )
        .await?;

        self.stdin.lock().await.take();
        self.stderr.lock().await.take();
        if let Some(mut child) = self.child.lock().await.take() {
            if force {
                let _ = child.kill().await;
            } else if let Ok(None) = child.try_wait() {
                let _ = child.kill().await;
            }
            let _ = child.wait().await;
        }

        runtime.finish_stopping().map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::UnsupportedAction,
                error.message,
                &runtime.codex_session_id,
                Some(request_id),
                "codex.local.stop",
            )
        })?;
        append_local_lifecycle_event(
            &self.store,
            &runtime,
            Some(request_id),
            "codex.control.stopped",
            "codex.local.stop",
        )
        .await?;
        Ok(runtime.clone())
    }
}

fn validate_local_codex_cwd(
    options: &CodexLocalAdapterStartOptions,
) -> CodexLocalAdapterProcessResult<()> {
    match std::fs::metadata(&options.cwd) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(CodexLocalAdapterProcessError::new(
            CodexLocalErrorCode::InvalidCwd,
            format!(
                "local Codex cwd is not a directory: {}",
                options.cwd.display()
            ),
            &options.codex_session_id,
            Some(&options.request_id),
            "codex.local.start",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::PermissionDenied,
                format!(
                    "permission denied accessing local Codex cwd: {}",
                    options.cwd.display()
                ),
                &options.codex_session_id,
                Some(&options.request_id),
                "codex.local.start",
            ))
        }
        Err(_) => Err(CodexLocalAdapterProcessError::new(
            CodexLocalErrorCode::InvalidCwd,
            format!("local Codex cwd does not exist: {}", options.cwd.display()),
            &options.codex_session_id,
            Some(&options.request_id),
            "codex.local.start",
        )),
    }
}

fn normalize_local_codex_cwd(path: PathBuf) -> PathBuf {
    let path = expand_home(path);
    if path.exists() {
        return path;
    }

    resolve_case_insensitive_path(&path).unwrap_or(path)
}

fn resolve_case_insensitive_path(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let first = components.next()?;
    let mut resolved = PathBuf::from(first.as_os_str());

    for component in components {
        let segment = component.as_os_str();
        let candidate = resolved.join(segment);
        if candidate.exists() {
            resolved = candidate;
            continue;
        }

        let corrected = unique_case_insensitive_child(&resolved, segment)?;
        resolved = corrected;
    }

    Some(resolved)
}

fn unique_case_insensitive_child(parent: &Path, target: &OsStr) -> Option<PathBuf> {
    let target = target.to_string_lossy();
    let mut matches = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(&target)
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();

    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

fn spawn_error(
    options: &CodexLocalAdapterStartOptions,
    error: std::io::Error,
) -> CodexLocalAdapterProcessError {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => CodexLocalErrorCode::MissingBinary,
        std::io::ErrorKind::PermissionDenied => CodexLocalErrorCode::PermissionDenied,
        _ => CodexLocalErrorCode::AdapterCrash,
    };
    CodexLocalAdapterProcessError::new(
        code,
        format!(
            "failed to spawn Codex app-server '{}': {error}",
            options.binary
        ),
        &options.codex_session_id,
        Some(&options.request_id),
        "codex.local.start",
    )
}

fn structured_ready_event(value: &Value) -> bool {
    if value.get("id").and_then(Value::as_str) == Some("initialize")
        && value.get("result").is_some()
    {
        return true;
    }
    value.get("type").and_then(Value::as_str) == Some("codex.control.ready")
        || (value.get("kind").and_then(Value::as_str) == Some("event")
            && value
                .get("event")
                .and_then(|event| event.get("type"))
                .and_then(Value::as_str)
                == Some("codex.control.ready"))
        || (value.get("kind").and_then(Value::as_str) == Some("event")
            && value.get("type").and_then(Value::as_str) == Some("codex.control.ready"))
}

fn codex_initialize_request(_request_id: &str) -> Value {
    json!({
        "id": "initialize",
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "todex-agentd",
                "title": "TodeX Agent Daemon",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": null
            }
        }
    })
}

async fn write_json_line_to_child_stdin(
    stdin: &mut ChildStdin,
    value: &Value,
    codex_session_id: &str,
    request_id: Option<&str>,
    operation: &str,
) -> CodexLocalAdapterProcessResult<()> {
    let line = serde_json::to_vec(value).map_err(|error| {
        CodexLocalAdapterProcessError::new(
            CodexLocalErrorCode::MalformedEvent,
            error.to_string(),
            codex_session_id,
            request_id,
            operation,
        )
    })?;
    stdin.write_all(&line).await.map_err(|error| {
        CodexLocalAdapterProcessError::new(
            CodexLocalErrorCode::AdapterCrash,
            error.to_string(),
            codex_session_id,
            request_id,
            operation,
        )
    })?;
    stdin.write_all(b"\n").await.map_err(|error| {
        CodexLocalAdapterProcessError::new(
            CodexLocalErrorCode::AdapterCrash,
            error.to_string(),
            codex_session_id,
            request_id,
            operation,
        )
    })?;
    stdin.flush().await.map_err(|error| {
        CodexLocalAdapterProcessError::new(
            CodexLocalErrorCode::AdapterCrash,
            error.to_string(),
            codex_session_id,
            request_id,
            operation,
        )
    })
}

fn spawn_local_stdout_reader(
    codex_session_id: String,
    startup_request_id: String,
    stdout: ChildStdout,
    store: CodexGatewayStore,
    events: EventBus,
    runtime: Arc<AsyncMutex<CodexLocalAdapterRuntime>>,
    pending_server_requests: Arc<DashMap<GatewayRequestId, CodexServerRequest>>,
    ready_tx: oneshot::Sender<std::result::Result<(), String>>,
) {
    tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let value: Value = match serde_json::from_str(&line) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Some(sender) = ready_tx.take() {
                                let _ = sender.send(Err(error.to_string()));
                            }
                            append_reader_error(
                                &store,
                                &runtime,
                                CodexLocalErrorCode::MalformedEvent,
                                error.to_string(),
                                Some(&startup_request_id),
                                "codex.local.turn",
                            )
                            .await;
                            break;
                        }
                    };
                    if structured_ready_event(&value) {
                        if let Some(sender) = ready_tx.take() {
                            let _ = sender.send(Ok(()));
                        }
                        continue;
                    }
                    if ready_tx.is_some() {
                        continue;
                    }
                    match parse_local_reader_message(value) {
                        Ok(Some(CodexLocalAdapterReaderMessage::Event(event))) => {
                            apply_reader_event_runtime(&runtime, &event).await;
                            if let Ok(record) = store.append_event(&codex_session_id, event).await {
                                publish_codex_gateway_record_to_bus(&events, record).await;
                            }
                        }
                        Ok(Some(CodexLocalAdapterReaderMessage::ServerRequest(request))) => {
                            apply_reader_server_request_runtime(&runtime, &request).await;
                            pending_server_requests.insert(request.id.clone(), request.clone());
                            let event = CodexGatewayEvent::approval_pending(&request)
                                .or_else(|| CodexGatewayEvent::elicitation_pending(&request))
                                .unwrap_or_else(|| {
                                    CodexGatewayEvent::new(
                                        request.request_type.clone(),
                                        with_request_metadata(request.payload, &request.id),
                                    )
                                });
                            if let Ok(record) = store.append_event(&codex_session_id, event).await {
                                publish_codex_gateway_record_to_bus(&events, record).await;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            append_reader_error(
                                &store,
                                &runtime,
                                CodexLocalErrorCode::MalformedEvent,
                                error,
                                Some(&startup_request_id),
                                "codex.local.turn",
                            )
                            .await;
                            break;
                        }
                    }
                }
                Ok(None) => {
                    if let Some(sender) = ready_tx.take() {
                        let _ = sender.send(Err(
                            "Codex app-server stdout closed before ready".to_string()
                        ));
                    } else if !reader_runtime_is_stopping_or_stopped(&runtime).await {
                        append_reader_error(
                            &store,
                            &runtime,
                            CodexLocalErrorCode::AdapterCrash,
                            "Codex app-server stdout closed".to_string(),
                            Some(&startup_request_id),
                            "codex.local.turn",
                        )
                        .await;
                    }
                    break;
                }
                Err(error) => {
                    if let Some(sender) = ready_tx.take() {
                        let _ = sender.send(Err(error.to_string()));
                    }
                    append_reader_error(
                        &store,
                        &runtime,
                        CodexLocalErrorCode::AdapterCrash,
                        error.to_string(),
                        Some(&startup_request_id),
                        "codex.local.turn",
                    )
                    .await;
                    break;
                }
            }
        }
    });
}

async fn reader_runtime_is_stopping_or_stopped(
    runtime: &Arc<AsyncMutex<CodexLocalAdapterRuntime>>,
) -> bool {
    matches!(
        runtime.lock().await.state,
        CodexLocalAdapterLifecycleState::Stopping | CodexLocalAdapterLifecycleState::Stopped
    )
}

fn parse_local_reader_message(
    value: Value,
) -> std::result::Result<Option<CodexLocalAdapterReaderMessage>, String> {
    if let Some(kind) = value.get("kind").and_then(Value::as_str) {
        return match kind {
            "event" => {
                let event_type = value
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "local Codex event frame missing type".to_string())?;
                let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                Ok(Some(CodexLocalAdapterReaderMessage::Event(
                    CodexGatewayEvent::new(event_type, payload),
                )))
            }
            "server_request" => Ok(Some(CodexLocalAdapterReaderMessage::ServerRequest(
                serde_json::from_value(value).map_err(|error| error.to_string())?,
            ))),
            "response" => Ok(None),
            _ => Err(format!("unsupported local Codex frame kind: {kind}")),
        };
    }
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let payload = value.get("params").cloned().unwrap_or(Value::Null);
        if value.get("id").is_some() {
            let id = value_to_request_id(value.get("id")).unwrap_or_default();
            return Ok(Some(CodexLocalAdapterReaderMessage::ServerRequest(
                CodexServerRequest {
                    id,
                    request_type: app_server_server_request_method_to_codex(method),
                    payload,
                },
            )));
        }
        return Ok(Some(CodexLocalAdapterReaderMessage::Event(
            CodexGatewayEvent::from_codex_notification(method, payload),
        )));
    }
    if value.get("id").is_some() && value.get("result").is_some() {
        return Ok(Some(CodexLocalAdapterReaderMessage::Event(
            CodexGatewayEvent::new(
                "codex.control.response",
                json!({
                    "requestId": value_to_request_id(value.get("id")).unwrap_or_default(),
                    "result": value.get("result").cloned().unwrap_or(Value::Null),
                }),
            ),
        )));
    }
    if value.get("id").is_some() && value.get("error").is_some() {
        return Ok(Some(CodexLocalAdapterReaderMessage::Event(
            CodexGatewayEvent::new(
                "codex.control.error",
                json!({
                    "requestId": value_to_request_id(value.get("id")).unwrap_or_default(),
                    "error": value.get("error").cloned().unwrap_or(Value::Null),
                }),
            ),
        )));
    }
    if let Some(event_type) = value.get("type").and_then(Value::as_str) {
        let payload = value.get("payload").cloned().unwrap_or_else(|| {
            let mut object = value.as_object().cloned().unwrap_or_default();
            object.remove("type");
            Value::Object(object)
        });
        return Ok(Some(CodexLocalAdapterReaderMessage::Event(
            CodexGatewayEvent::new(event_type, payload),
        )));
    }
    Err("local Codex frame missing kind, method, or type".to_string())
}

fn value_to_request_id(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(id)) => Some(id.clone()),
        Some(Value::Number(id)) => Some(id.to_string()),
        _ => None,
    }
}

fn app_server_server_request_method_to_codex(method: &str) -> String {
    match method {
        "item/commandExecution/requestApproval" => "codex.approval.commandExecution.request",
        "item/fileChange/requestApproval" => "codex.approval.fileChange.request",
        "item/permissions/requestApproval" => "codex.approval.permissions.request",
        "item/tool/requestUserInput" => "codex.tool.requestUserInput.request",
        "item/tool/call" => "codex.tool.call.request",
        "mcpServer/elicitation/request" => "codex.mcp.elicitation.request",
        "account/chatgptAuthTokens/refresh" => "codex.account.chatgptAuthTokens.refresh",
        other => other,
    }
    .to_string()
}

async fn apply_reader_event_runtime(
    runtime: &Arc<AsyncMutex<CodexLocalAdapterRuntime>>,
    event: &CodexGatewayEvent,
) {
    let mut runtime = runtime.lock().await;
    match event.event_type.as_str() {
        "codex.turn.completed" | "codex.item.completed" => {
            let _ = runtime.complete_mutating_command();
        }
        "codex.turn.interrupted" => {
            let _ = runtime.complete_mutating_command();
        }
        "codex.control.error" => {
            let _ = runtime.complete_mutating_command();
        }
        "codex.turn.failed" | "codex.error" => runtime.fail(),
        _ => {}
    }
}

async fn apply_reader_server_request_runtime(
    runtime: &Arc<AsyncMutex<CodexLocalAdapterRuntime>>,
    request: &CodexServerRequest,
) {
    if is_approval_request_type(&request.request_type)
        || request.request_type == "codex.tool.requestUserInput.request"
        || request.request_type == "codex.mcp.elicitation.request"
    {
        let mut runtime = runtime.lock().await;
        let _ = runtime.wait_for_approval();
    }
}

async fn append_reader_error(
    store: &CodexGatewayStore,
    runtime: &Arc<AsyncMutex<CodexLocalAdapterRuntime>>,
    code: CodexLocalErrorCode,
    message: String,
    request_id: Option<&str>,
    operation: &str,
) {
    let mut runtime = runtime.lock().await;
    runtime.fail();
    let payload = CodexLocalErrorPayload {
        code,
        message,
        codex_session_id: runtime.codex_session_id.clone(),
        request_id: request_id.map(ToOwned::to_owned),
        operation: Some(operation.to_string()),
        upstream_request_id: None,
    };
    let _ = append_local_error_event(store, &runtime, &payload).await;
}

async fn publish_codex_gateway_record_to_bus(events: &EventBus, record: CodexGatewayEventRecord) {
    events
        .publish(EventRecord {
            time: record.time,
            event_id: record.event_id,
            event_type: record.event_type,
            workspace_id: None,
            window_id: None,
            pane_id: None,
            payload: json!({
                "cursor": record.cursor,
                "codex_session_id": record.session_id,
                "codex_thread_id": record.codex_thread_id,
                "codex_turn_id": record.codex_turn_id,
                "data": record.payload,
            }),
        })
        .await;
}

fn outbound_request_id(message: &CodexOutboundMessage) -> Option<String> {
    match message {
        CodexOutboundMessage::Request(request) => Some(request.id.clone()),
        CodexOutboundMessage::ServerResponse(response) => Some(response.id.clone()),
    }
}

fn outbound_operation(message: &CodexOutboundMessage) -> Option<String> {
    match message {
        CodexOutboundMessage::Request(request) => Some(match request.request_type.as_str() {
            "codex.turn.start" => "codex.local.turn".to_string(),
            "codex.turn.input" => "codex.local.input".to_string(),
            "codex.turn.steer" => "codex.local.steer".to_string(),
            "codex.turn.interrupt" => "codex.local.interrupt".to_string(),
            request_type => request_type.to_string(),
        }),
        CodexOutboundMessage::ServerResponse(response) => Some(response.response_type.clone()),
    }
}

fn outbound_message_to_jsonrpc(message: CodexOutboundMessage) -> Value {
    match message {
        CodexOutboundMessage::Request(request) => {
            let request_id = request.id.clone();
            let (method, params) = codex_request_to_app_server_method_and_params(request);
            json!({
                "id": request_id,
                "method": method,
                "params": params,
            })
        }
        CodexOutboundMessage::ServerResponse(response) => json!({
            "id": response.id,
            "result": response.payload,
        }),
    }
}

fn codex_backend_kind_for_method(method: &str) -> CodexBackendKind {
    match method {
        "mcpServer/tool/call"
        | "mcpServer/resource/read"
        | "mcpServer/oauth/login"
        | "mcpServerStatus/list"
        | "config/mcpServer/reload" => CodexBackendKind::Mcp,
        _ => CodexBackendKind::AppServer,
    }
}

fn codex_method_to_request_type(method: &str) -> String {
    match method {
        "turn/start" => "codex.turn.start".to_string(),
        "turn/steer" => "codex.turn.steer".to_string(),
        "turn/interrupt" => "codex.turn.interrupt".to_string(),
        "review/start" => "codex.review.start".to_string(),
        "item/commandExecution/requestApproval" => {
            "codex.approval.commandExecution.request".to_string()
        }
        "item/fileChange/requestApproval" => "codex.approval.fileChange.request".to_string(),
        "item/permissions/requestApproval" => "codex.approval.permissions.request".to_string(),
        "item/tool/requestUserInput" => "codex.tool.requestUserInput.request".to_string(),
        "mcpServer/elicitation/request" => "codex.mcp.elicitation.request".to_string(),
        "mcpServer/tool/call" => "codex.mcp.tool.call".to_string(),
        "mcpServer/resource/read" => "codex.mcp.resource.read".to_string(),
        "mcpServer/oauth/login" => "codex.mcp.oauth.login".to_string(),
        "mcpServerStatus/list" => "codex.mcp.server.listStatus".to_string(),
        "config/mcpServer/reload" => "codex.mcp.server.refresh".to_string(),
        other => other.to_string(),
    }
}

fn codex_request_to_app_server_method_and_params(request: CodexGatewayRequest) -> (String, Value) {
    let mut params = request.payload;
    let method = match request.request_type.as_str() {
        "codex.turn.start" => "turn/start".to_string(),
        "codex.turn.input" => {
            normalize_turn_input_as_steer(&mut params);
            "turn/steer".to_string()
        }
        "codex.turn.steer" => {
            normalize_turn_steer_params(&mut params);
            "turn/steer".to_string()
        }
        "codex.turn.interrupt" => "turn/interrupt".to_string(),
        "codex.review.start" => "review/start".to_string(),
        "codex.mcp.server.listStatus" => "mcpServerStatus/list".to_string(),
        "codex.mcp.resource.read" => "mcpServer/resource/read".to_string(),
        "codex.mcp.tool.call" => "mcpServer/tool/call".to_string(),
        "codex.mcp.server.refresh" => "config/mcpServer/reload".to_string(),
        "codex.mcp.oauth.login" => "mcpServer/oauth/login".to_string(),
        other => other.to_string(),
    };
    (method, params)
}

fn normalize_turn_input_as_steer(params: &mut Value) {
    let Some(object) = params.as_object_mut() else {
        return;
    };
    if let Some(turn_id) = object.remove("turnId") {
        object.insert("expectedTurnId".to_string(), turn_id);
    }
}

fn normalize_turn_steer_params(params: &mut Value) {
    let Some(object) = params.as_object_mut() else {
        return;
    };
    object.remove("turnId");
}

async fn append_local_lifecycle_event(
    store: &CodexGatewayStore,
    runtime: &CodexLocalAdapterRuntime,
    request_id: Option<&str>,
    event_type: &str,
    operation: &str,
) -> CodexLocalAdapterProcessResult<()> {
    store
        .append_event(
            &runtime.codex_session_id,
            CodexGatewayEvent::new(
                event_type,
                json!({
                    "requestId": request_id,
                    "operation": operation,
                    "codexSessionId": &runtime.codex_session_id,
                    "lifecycleState": runtime.state,
                    "childProcess": &runtime.child_process,
                }),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &runtime.codex_session_id,
                request_id,
                operation,
            )
        })
}

async fn append_local_error_event(
    store: &CodexGatewayStore,
    runtime: &CodexLocalAdapterRuntime,
    payload: &CodexLocalErrorPayload,
) -> CodexLocalAdapterProcessResult<()> {
    store
        .append_event(
            &runtime.codex_session_id,
            CodexGatewayEvent::new(
                "codex.control.error",
                json!({
                    "requestId": &payload.request_id,
                    "operation": &payload.operation,
                    "codexSessionId": &runtime.codex_session_id,
                    "lifecycleState": runtime.state,
                    "error": payload,
                }),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &runtime.codex_session_id,
                payload.request_id.as_deref(),
                payload.operation.as_deref().unwrap_or("codex.local.start"),
            )
        })
}

async fn append_local_request_accepted_event(
    store: &CodexGatewayStore,
    runtime: &CodexLocalAdapterRuntime,
    request_id: &str,
    operation: &str,
    request_payload: &Value,
) -> CodexLocalAdapterProcessResult<()> {
    let mut payload = json!({
        "requestId": request_id,
        "operation": operation,
        "codexSessionId": &runtime.codex_session_id,
        "lifecycleState": runtime.state,
        "commandLane": &runtime.command_lane,
    });
    if let Some(object) = payload.as_object_mut() {
        for key in ["threadId", "turnId", "collaborationMode"] {
            if let Some(value) = request_payload.get(key) {
                object.insert(key.to_string(), value.clone());
            }
        }
    }
    store
        .append_event(
            &runtime.codex_session_id,
            CodexGatewayEvent::new("codex.control.request.accepted", payload),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &runtime.codex_session_id,
                Some(request_id),
                operation,
            )
        })
}

async fn append_local_request_rejected_event(
    store: &CodexGatewayStore,
    runtime: &CodexLocalAdapterRuntime,
    payload: &CodexLocalErrorPayload,
) -> CodexLocalAdapterProcessResult<()> {
    store
        .append_event(
            &runtime.codex_session_id,
            CodexGatewayEvent::new(
                "codex.control.request.rejected",
                json!({
                    "requestId": &payload.request_id,
                    "operation": &payload.operation,
                    "codexSessionId": &runtime.codex_session_id,
                    "lifecycleState": runtime.state,
                    "commandLane": &runtime.command_lane,
                    "error": payload,
                }),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            CodexLocalAdapterProcessError::new(
                CodexLocalErrorCode::AdapterCrash,
                error.to_string(),
                &runtime.codex_session_id,
                payload.request_id.as_deref(),
                payload.operation.as_deref().unwrap_or("codex.local.turn"),
            )
        })
}

fn local_process_error_from_lifecycle(
    error: CodexLocalAdapterLifecycleError,
    request_id: &str,
    operation: &str,
) -> CodexLocalAdapterProcessError {
    let code = match error.kind {
        CodexLocalAdapterLifecycleErrorKind::CommandLaneBusy => CodexLocalErrorCode::SessionBusy,
        CodexLocalAdapterLifecycleErrorKind::AdapterUnavailable
        | CodexLocalAdapterLifecycleErrorKind::InvalidTransition
        | CodexLocalAdapterLifecycleErrorKind::SessionMismatch
        | CodexLocalAdapterLifecycleErrorKind::MissingChildProcess
        | CodexLocalAdapterLifecycleErrorKind::ChildProcessAlreadyOwned => {
            CodexLocalErrorCode::UnsupportedAction
        }
    };
    CodexLocalAdapterProcessError::new(
        code,
        error.message,
        &error.codex_session_id,
        Some(request_id),
        operation,
    )
}

impl CodexLocalAdapterRuntime {
    pub fn new(codex_session_id: impl Into<CodexGatewaySessionId>) -> Self {
        Self {
            codex_session_id: codex_session_id.into(),
            state: CodexLocalAdapterLifecycleState::Idle,
            child_process: None,
            command_lane: CodexLocalAdapterCommandLane {
                in_flight_command_id: None,
            },
        }
    }

    pub fn ensure_session(&self, codex_session_id: &str) -> CodexLocalAdapterLifecycleResult<()> {
        if self.codex_session_id == codex_session_id {
            Ok(())
        } else {
            Err(CodexLocalAdapterLifecycleError::session_mismatch(
                &self.codex_session_id,
                self.state,
                codex_session_id,
            ))
        }
    }

    pub fn start(&mut self) -> CodexLocalAdapterLifecycleResult<()> {
        match self.state {
            CodexLocalAdapterLifecycleState::Idle
            | CodexLocalAdapterLifecycleState::Stopped
            | CodexLocalAdapterLifecycleState::Failed => {
                self.child_process = None;
                self.command_lane.in_flight_command_id = None;
                self.state = CodexLocalAdapterLifecycleState::Starting;
                Ok(())
            }
            state => Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                state,
                CodexLocalAdapterLifecycleState::Starting,
            )),
        }
    }

    pub fn attach_child_process(
        &mut self,
        child_process: CodexLocalAdapterChildProcess,
    ) -> CodexLocalAdapterLifecycleResult<()> {
        if self.state != CodexLocalAdapterLifecycleState::Starting {
            return Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                self.state,
                CodexLocalAdapterLifecycleState::Ready,
            ));
        }
        if self.child_process.is_some() {
            return Err(
                CodexLocalAdapterLifecycleError::child_process_already_owned(
                    &self.codex_session_id,
                    self.state,
                ),
            );
        }

        self.child_process = Some(child_process);
        self.state = CodexLocalAdapterLifecycleState::Ready;
        Ok(())
    }

    pub fn fail(&mut self) {
        self.state = CodexLocalAdapterLifecycleState::Failed;
        self.command_lane.in_flight_command_id = None;
    }

    pub fn begin_mutating_command(
        &mut self,
        command_id: impl Into<CodexLocalAdapterCommandId>,
    ) -> CodexLocalAdapterLifecycleResult<()> {
        if self.state != CodexLocalAdapterLifecycleState::Ready {
            if let Some(command_id) = &self.command_lane.in_flight_command_id {
                return Err(CodexLocalAdapterLifecycleError::command_lane_busy(
                    &self.codex_session_id,
                    self.state,
                    command_id,
                ));
            }
            return Err(CodexLocalAdapterLifecycleError::adapter_unavailable(
                &self.codex_session_id,
                self.state,
            ));
        }
        if self.child_process.is_none() {
            return Err(CodexLocalAdapterLifecycleError::missing_child_process(
                &self.codex_session_id,
                self.state,
            ));
        }
        if let Some(command_id) = &self.command_lane.in_flight_command_id {
            return Err(CodexLocalAdapterLifecycleError::command_lane_busy(
                &self.codex_session_id,
                self.state,
                command_id,
            ));
        }

        self.command_lane.in_flight_command_id = Some(command_id.into());
        self.state = CodexLocalAdapterLifecycleState::Busy;
        Ok(())
    }

    pub fn wait_for_approval(&mut self) -> CodexLocalAdapterLifecycleResult<()> {
        if self.state != CodexLocalAdapterLifecycleState::Busy {
            return Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                self.state,
                CodexLocalAdapterLifecycleState::WaitingForApproval,
            ));
        }
        self.state = CodexLocalAdapterLifecycleState::WaitingForApproval;
        Ok(())
    }

    pub fn resume_after_approval(&mut self) -> CodexLocalAdapterLifecycleResult<()> {
        if self.state != CodexLocalAdapterLifecycleState::WaitingForApproval {
            return Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                self.state,
                CodexLocalAdapterLifecycleState::Busy,
            ));
        }
        self.state = CodexLocalAdapterLifecycleState::Busy;
        Ok(())
    }

    pub fn complete_mutating_command(&mut self) -> CodexLocalAdapterLifecycleResult<()> {
        match self.state {
            CodexLocalAdapterLifecycleState::Busy
            | CodexLocalAdapterLifecycleState::WaitingForApproval => {
                self.command_lane.in_flight_command_id = None;
                self.state = CodexLocalAdapterLifecycleState::Ready;
                Ok(())
            }
            state => Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                state,
                CodexLocalAdapterLifecycleState::Ready,
            )),
        }
    }

    pub fn begin_stopping(&mut self) -> CodexLocalAdapterLifecycleResult<()> {
        match self.state {
            CodexLocalAdapterLifecycleState::Starting
            | CodexLocalAdapterLifecycleState::Ready
            | CodexLocalAdapterLifecycleState::Busy
            | CodexLocalAdapterLifecycleState::WaitingForApproval
            | CodexLocalAdapterLifecycleState::Failed => {
                self.command_lane.in_flight_command_id = None;
                self.state = CodexLocalAdapterLifecycleState::Stopping;
                Ok(())
            }
            state => Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                state,
                CodexLocalAdapterLifecycleState::Stopping,
            )),
        }
    }

    pub fn finish_stopping(&mut self) -> CodexLocalAdapterLifecycleResult<()> {
        if self.state != CodexLocalAdapterLifecycleState::Stopping {
            return Err(CodexLocalAdapterLifecycleError::invalid_transition(
                &self.codex_session_id,
                self.state,
                CodexLocalAdapterLifecycleState::Stopped,
            ));
        }
        self.child_process = None;
        self.command_lane.in_flight_command_id = None;
        self.state = CodexLocalAdapterLifecycleState::Stopped;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexGatewayEventRecord {
    pub session_id: CodexGatewaySessionId,
    pub event_id: CodexGatewayEventId,
    pub cursor: CodexGatewayCursor,
    pub time: DateTime<Utc>,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_turn_id: Option<String>,
    pub payload: Value,
}

impl CodexGatewayEvent {
    pub fn review_started(request: &CodexGatewayRequest) -> Option<Self> {
        if request.request_type != "codex.review.start" {
            return None;
        }

        Some(Self {
            event_type: "codex.review.started".to_string(),
            payload: with_request_metadata(request.payload.clone(), &request.id),
        })
    }

    pub fn review_start_result(response: &CodexGatewayResponse) -> Option<Self> {
        if response.response_type != "codex.review.start.result" {
            return None;
        }

        let mut payload = response.payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("requestId".to_string(), Value::String(response.id.clone()));
        }
        Some(Self {
            event_type: response.response_type.clone(),
            payload,
        })
    }

    pub fn mcp_request_dispatched(request: &CodexGatewayRequest) -> Option<Self> {
        if request.backend != CodexBackendKind::Mcp || !is_mcp_request_type(&request.request_type) {
            return None;
        }

        let event_type = match request.request_type.as_str() {
            "codex.mcp.tool.call" => "codex.mcp.tool.call.started",
            "codex.mcp.resource.read" => "codex.mcp.resource.read.started",
            "codex.mcp.server.listStatus" => "codex.mcp.server.listStatus.started",
            "codex.mcp.server.refresh" => "codex.mcp.server.refresh.started",
            "codex.mcp.oauth.login" => "codex.mcp.oauth.login.started",
            _ => return None,
        };

        let mut payload = with_request_metadata(request.payload.clone(), &request.id);
        if request.request_type == "codex.mcp.tool.call" {
            payload = with_mcp_metadata(payload, CodexMcpOutcome::Pending);
        }
        Some(Self::new(event_type, payload))
    }

    pub fn mcp_response_received(response: &CodexGatewayResponse) -> Result<Option<Self>> {
        if !is_mcp_response_type(&response.response_type) {
            return Ok(None);
        }

        let mut payload = response.payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("requestId".to_string(), Value::String(response.id.clone()));
        }

        if response.response_type == "codex.mcp.tool.call.result" {
            payload = with_mcp_metadata(payload, mcp_outcome_from_response(response));
            return Ok(Some(Self::new("codex.mcp.tool.call.result", payload)));
        }

        Ok(Some(Self::new(response.response_type.clone(), payload)))
    }

    pub fn approval_pending(request: &CodexServerRequest) -> Option<Self> {
        if !is_approval_request_type(&request.request_type) {
            return None;
        }

        Some(Self {
            event_type: request.request_type.clone(),
            payload: with_approval_metadata(
                request.payload.clone(),
                &request.id,
                CodexApprovalOutcome::Pending,
            ),
        })
    }

    pub fn elicitation_pending(request: &CodexServerRequest) -> Option<Self> {
        if request.request_type != "codex.mcp.elicitation.request" {
            return None;
        }

        Some(Self::new(
            request.request_type.clone(),
            with_elicitation_metadata(
                request.payload.clone(),
                &request.id,
                CodexMcpElicitationOutcome::Pending,
            ),
        ))
    }

    pub fn approval_resolved(response: &CodexServerResponse) -> Result<Option<Self>> {
        if !is_approval_response_type(&response.response_type) {
            return Ok(None);
        }

        let outcome = approval_outcome_from_response(response)?;
        Ok(Some(Self {
            event_type: "codex.serverRequest.resolved".to_string(),
            payload: serde_json::to_value(CodexApprovalResolution {
                request_id: response.id.clone(),
                outcome,
                response_type: response.response_type.clone(),
                response: response.payload.clone(),
            })?,
        }))
    }

    pub fn elicitation_resolved(response: &CodexServerResponse) -> Result<Option<Self>> {
        if response.response_type != "codex.mcp.elicitation.respond" {
            return Ok(None);
        }

        let outcome = elicitation_outcome_from_response(response)?;
        Ok(Some(Self::new(
            "codex.mcp.elicitation.resolved",
            serde_json::to_value(CodexMcpElicitationResolution {
                request_id: response.id.clone(),
                outcome,
                response_type: response.response_type.clone(),
                response: response.payload.clone(),
            })?,
        )))
    }
}

impl CodexGatewayEventRecord {
    fn new(
        session_id: impl Into<CodexGatewaySessionId>,
        cursor: CodexGatewayCursor,
        event: CodexGatewayEvent,
    ) -> Self {
        let payload = event.payload;
        Self {
            session_id: session_id.into(),
            event_id: format!("evt_{}", Uuid::new_v4().simple()),
            cursor,
            time: Utc::now(),
            event_type: event.event_type,
            codex_thread_id: payload_string(&payload, &["threadId", "thread_id", "codexThreadId"]),
            codex_turn_id: payload_string(&payload, &["turnId", "turn_id", "codexTurnId"]),
            payload,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexGatewayReplay {
    pub session_id: CodexGatewaySessionId,
    pub from_cursor: Option<CodexGatewayCursor>,
    pub next_cursor: Option<CodexGatewayCursor>,
    pub events: Vec<CodexGatewayEventRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodexGatewaySessionState {
    pub session_id: CodexGatewaySessionId,
    pub last_cursor: CodexGatewayCursor,
    pub threads: BTreeMap<String, Value>,
    pub turns: BTreeMap<String, Value>,
    pub reviews: BTreeMap<String, Value>,
    pub approvals: BTreeMap<String, Value>,
    pub plans: BTreeMap<String, Value>,
    pub mcp_servers: BTreeMap<String, Value>,
    pub mcp_resources: BTreeMap<String, Value>,
    pub mcp_tool_calls: BTreeMap<String, Value>,
    pub mcp_elicitations: BTreeMap<String, Value>,
    pub collaboration_modes: BTreeMap<String, CodexCollaborationMode>,
    pub plan_states: BTreeMap<String, CodexPlanState>,
    pub cloud_tasks: BTreeMap<String, Value>,
    pub cloud_task_attempts: BTreeMap<String, Vec<CodexCloudTaskTurnAttempt>>,
    pub cloud_task_apply_outcomes: BTreeMap<String, CodexCloudTaskApplyOutcome>,
    pub local_adapters: BTreeMap<String, Value>,
    pub events: BTreeMap<CodexGatewayEventId, CodexGatewayCursor>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexPlanState {
    pub plan_id: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub explanation: Option<String>,
    pub items: Vec<CodexPlanItem>,
    pub deltas: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexPlanItem {
    pub text: String,
    pub status: CodexPlanItemStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexPlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

impl CodexGatewaySessionState {
    pub fn new(session_id: impl Into<CodexGatewaySessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            ..Self::default()
        }
    }

    fn apply(&mut self, record: &CodexGatewayEventRecord) {
        self.last_cursor = self.last_cursor.max(record.cursor);
        self.events.insert(record.event_id.clone(), record.cursor);

        if let Some(thread_id) = &record.codex_thread_id {
            self.threads
                .insert(thread_id.clone(), record.payload.clone());
        }
        if let Some(turn_id) = &record.codex_turn_id {
            self.turns.insert(turn_id.clone(), record.payload.clone());
            if let Some(collaboration_mode) = record
                .payload
                .get("collaborationMode")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
            {
                self.collaboration_modes
                    .insert(turn_id.clone(), collaboration_mode);
            }
        }
        if record.event_type.contains("review") {
            if let Some(review_id) = payload_string(
                &record.payload,
                &[
                    "requestId",
                    "request_id",
                    "reviewId",
                    "review_id",
                    "reviewThreadId",
                    "review_thread_id",
                ],
            ) {
                self.reviews.insert(review_id, record.payload.clone());
            }
        }
        if record.event_type.contains("approval")
            || record.event_type.contains("permissions")
            || record.event_type == "codex.serverRequest.resolved"
        {
            if let Some(request_id) =
                payload_string(&record.payload, &["requestId", "request_id", "id"])
            {
                self.approvals.insert(request_id, record.payload.clone());
            }
        }
        if record.event_type.starts_with("codex.mcp.server")
            || record.event_type.starts_with("codex.mcp.oauth")
        {
            if let Some(server_name) =
                payload_string(&record.payload, &["server", "name", "serverName"])
            {
                self.mcp_servers.insert(server_name, record.payload.clone());
            } else if let Some(request_id) =
                payload_string(&record.payload, &["requestId", "request_id"])
            {
                self.mcp_servers.insert(request_id, record.payload.clone());
            }
        }
        if record.event_type.starts_with("codex.mcp.resource") {
            if let Some(resource_id) = payload_string(
                &record.payload,
                &["uri", "resourceUri", "requestId", "request_id"],
            ) {
                self.mcp_resources
                    .insert(resource_id, record.payload.clone());
            }
        }
        if record.event_type.starts_with("codex.mcp.tool") {
            if let Some(request_id) =
                payload_string(&record.payload, &["requestId", "request_id", "id"])
            {
                self.mcp_tool_calls
                    .insert(request_id, record.payload.clone());
            }
        }
        if record.event_type.starts_with("codex.mcp.elicitation") {
            if let Some(request_id) =
                payload_string(&record.payload, &["requestId", "request_id", "id"])
            {
                self.mcp_elicitations
                    .insert(request_id, record.payload.clone());
            }
        }
        if record.event_type == "codex.turn.planUpdated" {
            let plan_id = plan_id_for_payload(&record.payload, &record.event_id);
            let plan_state =
                self.plan_states
                    .entry(plan_id.clone())
                    .or_insert_with(|| CodexPlanState {
                        plan_id: plan_id.clone(),
                        thread_id: payload_string(&record.payload, &["threadId", "thread_id"]),
                        turn_id: payload_string(&record.payload, &["turnId", "turn_id"]),
                        item_id: payload_string(&record.payload, &["itemId", "item_id"]),
                        ..CodexPlanState::default()
                    });
            plan_state.thread_id = payload_string(&record.payload, &["threadId", "thread_id"]);
            plan_state.turn_id = payload_string(&record.payload, &["turnId", "turn_id"]);
            plan_state.explanation = payload_string(&record.payload, &["explanation"]);
            plan_state.items = plan_items_from_payload(&record.payload);
            self.plans.insert(plan_id, record.payload.clone());
        } else if record.event_type == "codex.plan.delta" {
            let plan_id = plan_id_for_payload(&record.payload, &record.event_id);
            let plan_state =
                self.plan_states
                    .entry(plan_id.clone())
                    .or_insert_with(|| CodexPlanState {
                        plan_id: plan_id.clone(),
                        thread_id: payload_string(&record.payload, &["threadId", "thread_id"]),
                        turn_id: payload_string(&record.payload, &["turnId", "turn_id"]),
                        item_id: payload_string(&record.payload, &["itemId", "item_id"]),
                        ..CodexPlanState::default()
                    });
            if let Some(delta) = payload_string(&record.payload, &["delta"]) {
                plan_state.deltas.push(delta);
            }
            self.plans.insert(plan_id, record.payload.clone());
        } else if record.event_type == "codex.item.completed"
            && is_completed_plan_item(&record.payload)
        {
            let plan_id = plan_id_for_payload(&record.payload, &record.event_id);
            self.plans.insert(plan_id.clone(), record.payload.clone());
            let item = record.payload.get("item").unwrap_or(&record.payload);
            let plan_state =
                self.plan_states
                    .entry(plan_id.clone())
                    .or_insert_with(|| CodexPlanState {
                        plan_id: plan_id.clone(),
                        thread_id: payload_string(&record.payload, &["threadId", "thread_id"]),
                        turn_id: payload_string(&record.payload, &["turnId", "turn_id"]),
                        item_id: payload_string(item, &["id"]),
                        ..CodexPlanState::default()
                    });
            plan_state.thread_id = payload_string(&record.payload, &["threadId", "thread_id"]);
            plan_state.turn_id = payload_string(&record.payload, &["turnId", "turn_id"]);
            plan_state.item_id = payload_string(item, &["id"]);
            plan_state.items = vec![CodexPlanItem {
                text: payload_string(item, &["text"]).unwrap_or_default(),
                status: CodexPlanItemStatus::Completed,
            }];
        }
        if record.event_type.starts_with("codex.cloudTask.") {
            self.apply_cloud_task_event(record);
        }
        if record.event_type.starts_with("codex.control.") {
            self.apply_local_adapter_event(record);
        }
    }

    fn apply_cloud_task_event(&mut self, record: &CodexGatewayEventRecord) {
        if let Some(task_id) = cloud_task_id_from_payload(&record.payload) {
            self.cloud_tasks
                .insert(task_id.clone(), record.payload.clone());

            if let Some(attempts) = record.payload.get("attempts").cloned().and_then(|value| {
                serde_json::from_value::<Vec<CodexCloudTaskTurnAttempt>>(value).ok()
            }) {
                self.cloud_task_attempts.insert(task_id.clone(), attempts);
            }

            if let Some(outcome) = cloud_task_apply_outcome_from_payload(&record.payload) {
                let key = payload_string(&record.payload, &["requestId", "request_id"])
                    .unwrap_or_else(|| task_id.clone());
                self.cloud_task_apply_outcomes.insert(key, outcome);
            }
        }
    }

    fn apply_local_adapter_event(&mut self, record: &CodexGatewayEventRecord) {
        if let Some(codex_session_id) = payload_string(&record.payload, &["codexSessionId"]) {
            self.local_adapters
                .insert(codex_session_id, record.payload.clone());
        }
    }
}

fn cloud_task_id_from_payload(payload: &Value) -> Option<String> {
    payload_string(payload, &["taskId", "task_id", "id"]).or_else(|| {
        payload
            .get("task")
            .and_then(|task| payload_string(task, &["taskId", "task_id", "id"]))
    })
}

fn cloud_task_apply_outcome_from_payload(payload: &Value) -> Option<CodexCloudTaskApplyOutcome> {
    let mut outcome: CodexCloudTaskApplyOutcome = payload
        .get("outcome")
        .cloned()
        .or_else(|| payload.get("applyOutcome").cloned())
        .and_then(|value| serde_json::from_value(value).ok())?;
    if outcome.task_id.is_none() {
        outcome.task_id = cloud_task_id_from_payload(payload);
    }
    if outcome.turn_id.is_none() {
        outcome.turn_id = payload_string(payload, &["turnId", "turn_id"]);
    }
    if outcome.attempt_placement.is_none() {
        outcome.attempt_placement = payload
            .get("attemptPlacement")
            .or_else(|| payload.get("attempt_placement"))
            .and_then(Value::as_i64);
    }
    if payload
        .get("preflight")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        outcome.preflight = true;
    }
    Some(outcome)
}

fn plan_id_for_payload(payload: &Value, fallback: &str) -> String {
    payload_string(payload, &["planId", "plan_id", "itemId", "item_id"])
        .or_else(|| {
            payload
                .get("item")
                .and_then(|item| payload_string(item, &["id"]))
        })
        .unwrap_or_else(|| fallback.to_string())
}

fn plan_items_from_payload(payload: &Value) -> Vec<CodexPlanItem> {
    payload
        .get("plan")
        .or_else(|| payload.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let text = payload_string(item, &["step", "text"])?;
                    let status = match payload_string(item, &["status"])
                        .unwrap_or_else(|| "pending".to_string())
                        .as_str()
                    {
                        "inProgress" | "in_progress" => CodexPlanItemStatus::InProgress,
                        "completed" => CodexPlanItemStatus::Completed,
                        _ => CodexPlanItemStatus::Pending,
                    };
                    Some(CodexPlanItem { text, status })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn is_completed_plan_item(payload: &Value) -> bool {
    let item = payload.get("item").unwrap_or(payload);
    payload_string(item, &["type"]).as_deref() == Some("plan")
}

#[derive(Clone)]
pub struct CodexGatewayStore {
    data_dir: PathBuf,
}

impl CodexGatewayStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub async fn append_event(
        &self,
        session_id: &str,
        event: CodexGatewayEvent,
    ) -> Result<CodexGatewayEventRecord> {
        let lock = self.session_lock(session_id).lock_owned().await;
        let mut state = self.recover_session_locked(session_id).await?;
        let record = CodexGatewayEventRecord::new(session_id, state.last_cursor + 1, event);
        let dir = self.session_dir(session_id);
        fs::create_dir_all(&dir).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
            .await?;
        let line = serde_json::to_string(&record)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        state.apply(&record);
        self.save_session_state(&state).await?;
        drop(lock);
        Ok(record)
    }

    pub async fn replay_events(
        &self,
        session_id: &str,
        after_cursor: Option<CodexGatewayCursor>,
        limit: usize,
    ) -> Result<CodexGatewayReplay> {
        let lock = self.session_lock(session_id).lock_owned().await;
        let events = self.read_events(session_id).await?;
        drop(lock);
        let from_cursor = after_cursor.unwrap_or(0);
        let mut replayed = events
            .into_iter()
            .filter(|event| event.cursor > from_cursor)
            .collect::<Vec<_>>();
        if replayed.len() > limit {
            replayed.truncate(limit);
        }
        let next_cursor = replayed.last().map(|event| event.cursor);
        Ok(CodexGatewayReplay {
            session_id: session_id.to_string(),
            from_cursor: after_cursor,
            next_cursor,
            events: replayed,
        })
    }

    pub async fn recover_session(&self, session_id: &str) -> Result<CodexGatewaySessionState> {
        let lock = self.session_lock(session_id).lock_owned().await;
        let state = self.recover_session_locked(session_id).await?;
        drop(lock);
        Ok(state)
    }

    async fn recover_session_locked(&self, session_id: &str) -> Result<CodexGatewaySessionState> {
        let mut state = CodexGatewaySessionState::new(session_id);
        for record in self.read_events(session_id).await? {
            state.apply(&record);
        }
        Ok(state)
    }

    async fn save_session_state(&self, state: &CodexGatewaySessionState) -> Result<()> {
        let dir = self.session_dir(&state.session_id);
        fs::create_dir_all(&dir).await?;
        fs::write(dir.join("state.json"), serde_json::to_vec_pretty(state)?).await?;
        Ok(())
    }

    async fn read_events(&self, session_id: &str) -> Result<Vec<CodexGatewayEventRecord>> {
        let path = self.session_dir(session_id).join("events.jsonl");
        let file = match fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut lines = BufReader::new(file).lines();
        let mut events = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if !line.trim().is_empty() {
                let record: CodexGatewayEventRecord = serde_json::from_str(&line)?;
                if record.session_id != session_id {
                    return Err(AppError::InvalidRequest(format!(
                        "CodeX gateway event session mismatch: expected {session_id}, found {}",
                        record.session_id
                    )));
                }
                let expected_cursor = events.len() as u64 + 1;
                if record.cursor != expected_cursor {
                    return Err(AppError::InvalidRequest(format!(
                        "CodeX gateway cursor gap in session {session_id}: expected {expected_cursor}, found {}",
                        record.cursor
                    )));
                }
                events.push(record);
            }
        }
        Ok(events)
    }

    fn session_lock(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        static LOCKS: OnceLock<StdMutex<BTreeMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
        let key = self.session_dir(session_id);
        let mut locks = LOCKS
            .get_or_init(|| StdMutex::new(BTreeMap::new()))
            .lock()
            .expect("CodeX gateway lock registry is poisoned");
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.data_dir
            .join("codex_gateway")
            .join("sessions")
            .join(session_id)
    }
}

fn with_request_metadata(mut payload: Value, request_id: &str) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "requestId".to_string(),
            Value::String(request_id.to_string()),
        );
    }
    payload
}

fn with_approval_metadata(
    mut payload: Value,
    request_id: &str,
    outcome: CodexApprovalOutcome,
) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "requestId".to_string(),
            Value::String(request_id.to_string()),
        );
        object.insert(
            "outcome".to_string(),
            serde_json::to_value(outcome).expect("approval outcome serializes"),
        );
    }
    payload
}

fn with_mcp_metadata(mut payload: Value, outcome: CodexMcpOutcome) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "outcome".to_string(),
            serde_json::to_value(outcome).expect("MCP outcome serializes"),
        );
    }
    payload
}

fn with_elicitation_metadata(
    mut payload: Value,
    request_id: &str,
    outcome: CodexMcpElicitationOutcome,
) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "requestId".to_string(),
            Value::String(request_id.to_string()),
        );
        object.insert(
            "outcome".to_string(),
            serde_json::to_value(outcome).expect("elicitation outcome serializes"),
        );
    }
    payload
}

fn is_mcp_request_type(request_type: &str) -> bool {
    matches!(
        request_type,
        "codex.mcp.server.listStatus"
            | "codex.mcp.resource.read"
            | "codex.mcp.tool.call"
            | "codex.mcp.server.refresh"
            | "codex.mcp.oauth.login"
    )
}

fn is_mcp_response_type(response_type: &str) -> bool {
    matches!(
        response_type,
        "codex.mcp.server.listStatus.result"
            | "codex.mcp.resource.read.result"
            | "codex.mcp.tool.call.result"
            | "codex.mcp.server.refresh.result"
            | "codex.mcp.oauth.login.result"
    )
}

fn mcp_outcome_from_response(response: &CodexGatewayResponse) -> CodexMcpOutcome {
    if payload_string(&response.payload, &["error", "message"]).is_some()
        || response
            .payload
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        CodexMcpOutcome::Failed
    } else {
        CodexMcpOutcome::Succeeded
    }
}

fn elicitation_outcome_from_response(
    response: &CodexServerResponse,
) -> Result<CodexMcpElicitationOutcome> {
    let Some(action) = payload_string(&response.payload, &["action", "outcome", "decision"]) else {
        return Err(AppError::InvalidRequest(format!(
            "missing CodeX MCP elicitation action for request {}",
            response.id
        )));
    };

    match action.to_ascii_lowercase().as_str() {
        "accept" | "accepted" => Ok(CodexMcpElicitationOutcome::Accepted),
        "decline" | "declined" | "deny" | "denied" => Ok(CodexMcpElicitationOutcome::Declined),
        "cancel" | "cancelled" | "canceled" => Ok(CodexMcpElicitationOutcome::Cancelled),
        other => Err(AppError::InvalidRequest(format!(
            "unknown CodeX MCP elicitation action {other} for request {}",
            response.id
        ))),
    }
}

fn is_approval_request_type(request_type: &str) -> bool {
    matches!(
        request_type,
        "codex.approval.commandExecution.request"
            | "codex.approval.fileChange.request"
            | "codex.approval.permissions.request"
            | "codex.tool.requestUserInput.request"
            | "codex.tool.call.request"
            | "codex.account.chatgptAuthTokens.refresh"
    )
}

fn is_approval_response_type(response_type: &str) -> bool {
    matches!(
        response_type,
        "codex.approval.commandExecution.respond"
            | "codex.approval.fileChange.respond"
            | "codex.approval.permissions.respond"
            | "codex.tool.requestUserInput.respond"
            | "codex.tool.call.respond"
            | "codex.account.chatgptAuthTokens.refresh.respond"
    )
}

fn approval_outcome_from_response(response: &CodexServerResponse) -> Result<CodexApprovalOutcome> {
    if let Some(decision) = payload_string(&response.payload, &["decision", "outcome", "status"]) {
        return match decision.to_ascii_lowercase().as_str() {
            "accept" | "accepted" | "approve" | "approved" | "allow" | "allowed" => {
                Ok(CodexApprovalOutcome::Accepted)
            }
            "deny" | "denied" | "reject" | "rejected" | "decline" | "declined" => {
                Ok(CodexApprovalOutcome::Denied)
            }
            other => Err(AppError::InvalidRequest(format!(
                "unknown CodeX approval decision {other} for request {}",
                response.id
            ))),
        };
    }

    if response.response_type == "codex.permissions.request.respond" {
        return Ok(CodexApprovalOutcome::Accepted);
    }

    Err(AppError::InvalidRequest(format!(
        "missing CodeX approval decision for request {}",
        response.id
    )))
}

fn payload_string(payload: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| payload.get(key).and_then(Value::as_str))
        .map(ToString::to_string)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexServerRequest {
    pub id: GatewayRequestId,
    #[serde(rename = "type")]
    pub request_type: String,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodexServerResponse {
    pub id: GatewayRequestId,
    #[serde(rename = "type")]
    pub response_type: String,
    pub payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodexInboundMessage {
    Response(CodexGatewayResponse),
    Event(CodexGatewayEvent),
    ServerRequest(CodexServerRequest),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodexOutboundMessage {
    Request(CodexGatewayRequest),
    ServerResponse(CodexServerResponse),
}

#[async_trait]
pub trait CodexGatewayTransport: Send + Sync {
    async fn send(&self, message: CodexOutboundMessage) -> Result<()>;
}

#[derive(Clone)]
pub struct CodexGatewayAdapter {
    transport: Arc<dyn CodexGatewayTransport>,
    pending_requests: Arc<DashMap<GatewayRequestId, CodexGatewayRequest>>,
    pending_server_requests: Arc<DashMap<GatewayRequestId, CodexServerRequest>>,
    events: mpsc::UnboundedSender<CodexGatewayEvent>,
    server_requests: mpsc::UnboundedSender<CodexServerRequest>,
}

impl CodexGatewayAdapter {
    pub fn new(
        transport: Arc<dyn CodexGatewayTransport>,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<CodexGatewayEvent>,
        mpsc::UnboundedReceiver<CodexServerRequest>,
    ) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (server_requests_tx, server_requests_rx) = mpsc::unbounded_channel();
        (
            Self {
                transport,
                pending_requests: Arc::new(DashMap::new()),
                pending_server_requests: Arc::new(DashMap::new()),
                events: events_tx,
                server_requests: server_requests_tx,
            },
            events_rx,
            server_requests_rx,
        )
    }

    pub async fn dispatch_request(&self, request: CodexGatewayRequest) -> Result<()> {
        self.pending_requests
            .insert(request.id.clone(), request.clone());
        if let Some(event) = CodexGatewayEvent::review_started(&request) {
            self.events.send(event).map_err(|_| {
                AppError::InvalidRequest("CodeX gateway event receiver is closed".to_string())
            })?;
        }
        if let Some(event) = CodexGatewayEvent::mcp_request_dispatched(&request) {
            self.events.send(event).map_err(|_| {
                AppError::InvalidRequest("CodeX gateway event receiver is closed".to_string())
            })?;
        }
        self.transport
            .send(CodexOutboundMessage::Request(request))
            .await
    }

    pub async fn respond_to_server_request(&self, response: CodexServerResponse) -> Result<()> {
        let Some((_, request)) = self.pending_server_requests.remove(&response.id) else {
            return Err(AppError::InvalidRequest(format!(
                "unknown CodeX server request id {}",
                response.id
            )));
        };

        let resolved_event = if request.request_type == "codex.tool.requestUserInput.request" {
            Some(CodexGatewayEvent::new(
                "codex.serverRequest.resolved",
                json!({
                    "requestId": response.id,
                    "threadId": payload_string(&request.payload, &["threadId", "thread_id", "codexThreadId"]),
                }),
            ))
        } else {
            CodexGatewayEvent::approval_resolved(&response)?
                .or(CodexGatewayEvent::elicitation_resolved(&response)?)
        };

        if let Some(event) = resolved_event {
            self.events.send(event).map_err(|_| {
                AppError::InvalidRequest("CodeX gateway event receiver is closed".to_string())
            })?;
        }

        self.transport
            .send(CodexOutboundMessage::ServerResponse(response))
            .await
    }

    pub fn handle_inbound(
        &self,
        message: CodexInboundMessage,
    ) -> Result<Option<CodexGatewayResponse>> {
        match message {
            CodexInboundMessage::Response(response) => {
                self.pending_requests.remove(&response.id);
                if let Some(event) = CodexGatewayEvent::review_start_result(&response) {
                    self.events.send(event).map_err(|_| {
                        AppError::InvalidRequest(
                            "CodeX gateway event receiver is closed".to_string(),
                        )
                    })?;
                }
                if let Some(event) = CodexGatewayEvent::mcp_response_received(&response)? {
                    self.events.send(event).map_err(|_| {
                        AppError::InvalidRequest(
                            "CodeX gateway event receiver is closed".to_string(),
                        )
                    })?;
                }
                Ok(Some(response))
            }
            CodexInboundMessage::Event(event) => {
                self.events.send(event).map_err(|_| {
                    AppError::InvalidRequest("CodeX gateway event receiver is closed".to_string())
                })?;
                Ok(None)
            }
            CodexInboundMessage::ServerRequest(request) => {
                self.pending_server_requests
                    .insert(request.id.clone(), request.clone());
                if let Some(event) = CodexGatewayEvent::approval_pending(&request) {
                    self.events.send(event).map_err(|_| {
                        AppError::InvalidRequest(
                            "CodeX gateway event receiver is closed".to_string(),
                        )
                    })?;
                }
                if let Some(event) = CodexGatewayEvent::elicitation_pending(&request) {
                    self.events.send(event).map_err(|_| {
                        AppError::InvalidRequest(
                            "CodeX gateway event receiver is closed".to_string(),
                        )
                    })?;
                }
                self.server_requests.send(request).map_err(|_| {
                    AppError::InvalidRequest(
                        "CodeX gateway server-request receiver is closed".to_string(),
                    )
                })?;
                Ok(None)
            }
        }
    }

    pub fn has_pending_request(&self, id: &str) -> bool {
        self.pending_requests.contains_key(id)
    }

    pub fn has_pending_server_request(&self, id: &str) -> bool {
        self.pending_server_requests.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingTransport {
        sent: Mutex<Vec<CodexOutboundMessage>>,
    }

    impl RecordingTransport {
        async fn sent(&self) -> Vec<CodexOutboundMessage> {
            self.sent.lock().await.clone()
        }
    }

    #[async_trait]
    impl CodexGatewayTransport for RecordingTransport {
        async fn send(&self, message: CodexOutboundMessage) -> Result<()> {
            self.sent.lock().await.push(message);
            Ok(())
        }
    }

    #[test]
    fn local_adapter_lifecycle_serializes_exact_state_names() {
        let states = [
            CodexLocalAdapterLifecycleState::Idle,
            CodexLocalAdapterLifecycleState::Starting,
            CodexLocalAdapterLifecycleState::Ready,
            CodexLocalAdapterLifecycleState::Busy,
            CodexLocalAdapterLifecycleState::WaitingForApproval,
            CodexLocalAdapterLifecycleState::Stopping,
            CodexLocalAdapterLifecycleState::Stopped,
            CodexLocalAdapterLifecycleState::Failed,
        ];

        let names = states
            .into_iter()
            .map(|state| serde_json::to_value(state).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                json!("idle"),
                json!("starting"),
                json!("ready"),
                json!("busy"),
                json!("waiting_for_approval"),
                json!("stopping"),
                json!("stopped"),
                json!("failed"),
            ]
        );
    }

    #[test]
    fn local_adapter_lifecycle_tracks_single_session_child_and_command_lane() {
        let mut runtime = CodexLocalAdapterRuntime::new("cdxs_local_1");

        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Idle);
        assert!(runtime.command_lane.is_idle());
        runtime.ensure_session("cdxs_local_1").unwrap();
        runtime.start().unwrap();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Starting);
        runtime
            .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
            .unwrap();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Ready);
        assert_eq!(
            runtime.child_process.as_ref().map(|child| child.pid),
            Some(4242)
        );

        runtime.begin_mutating_command("cmd-1").unwrap();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Busy);
        assert_eq!(
            runtime.command_lane.in_flight_command_id.as_deref(),
            Some("cmd-1")
        );
        runtime.wait_for_approval().unwrap();
        assert_eq!(
            runtime.state,
            CodexLocalAdapterLifecycleState::WaitingForApproval
        );
        runtime.resume_after_approval().unwrap();
        runtime.complete_mutating_command().unwrap();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Ready);
        assert!(runtime.command_lane.is_idle());

        runtime.begin_stopping().unwrap();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Stopping);
        runtime.finish_stopping().unwrap();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Stopped);
        assert!(runtime.child_process.is_none());
    }

    #[test]
    fn local_adapter_lifecycle_rejects_invalid_transitions_with_typed_errors() {
        let mut runtime = CodexLocalAdapterRuntime::new("cdxs_local_1");

        let error = runtime
            .begin_mutating_command("cmd-before-ready")
            .unwrap_err();
        assert_eq!(
            error.kind,
            CodexLocalAdapterLifecycleErrorKind::AdapterUnavailable
        );
        assert_eq!(error.state, CodexLocalAdapterLifecycleState::Idle);

        runtime.start().unwrap();
        let error = runtime.start().unwrap_err();
        assert_eq!(
            error.kind,
            CodexLocalAdapterLifecycleErrorKind::InvalidTransition
        );
        assert_eq!(error.state, CodexLocalAdapterLifecycleState::Starting);

        runtime
            .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
            .unwrap();
        runtime.fail();
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Failed);
        assert!(runtime.command_lane.is_idle());
        runtime.start().unwrap();
        runtime
            .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
            .unwrap();
        let error = runtime
            .attach_child_process(CodexLocalAdapterChildProcess { pid: 4243 })
            .unwrap_err();
        assert_eq!(
            error.kind,
            CodexLocalAdapterLifecycleErrorKind::InvalidTransition
        );
        assert_eq!(error.state, CodexLocalAdapterLifecycleState::Ready);

        let error = runtime.ensure_session("cdxs_other").unwrap_err();
        assert_eq!(
            error.kind,
            CodexLocalAdapterLifecycleErrorKind::SessionMismatch
        );
        assert_eq!(error.codex_session_id, "cdxs_local_1");
    }

    #[tokio::test]
    async fn control_error_completes_in_flight_command_without_failing_runtime() {
        let runtime = Arc::new(AsyncMutex::new({
            let mut runtime = CodexLocalAdapterRuntime::new("cdxs_local_1");
            runtime.start().unwrap();
            runtime
                .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
                .unwrap();
            runtime.begin_mutating_command("msg-1").unwrap();
            runtime
        }));

        apply_reader_event_runtime(
            &runtime,
            &CodexGatewayEvent::new(
                "codex.control.error",
                json!({
                    "requestId": "msg-1",
                    "error": {
                        "code": -32600,
                        "message": "invalid thread id"
                    }
                }),
            ),
        )
        .await;

        let runtime = runtime.lock().await;
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Ready);
        assert!(runtime.command_lane.is_idle());
    }

    #[test]
    fn local_adapter_lifecycle_rejects_concurrent_mutating_commands() {
        let mut runtime = CodexLocalAdapterRuntime::new("cdxs_local_1");
        runtime.start().unwrap();
        runtime
            .attach_child_process(CodexLocalAdapterChildProcess { pid: 4242 })
            .unwrap();
        runtime.begin_mutating_command("cmd-1").unwrap();

        let error = runtime.begin_mutating_command("cmd-2").unwrap_err();

        assert_eq!(
            error.kind,
            CodexLocalAdapterLifecycleErrorKind::CommandLaneBusy
        );
        assert_eq!(error.state, CodexLocalAdapterLifecycleState::Busy);
        assert_eq!(error.in_flight_command_id.as_deref(), Some("cmd-1"));
        assert_eq!(
            runtime.command_lane.in_flight_command_id.as_deref(),
            Some("cmd-1")
        );
    }

    #[test]
    fn turn_start_request_serializes_collaboration_mode_separately_from_input() {
        let request = CodexGatewayRequest::turn_start(
            "turn-start-1",
            "thread-1",
            json!([{ "type": "text", "text": "Plan this", "textElements": [] }]),
            Some(CodexCollaborationMode {
                mode: CodexModeKind::Plan,
                settings: CodexCollaborationSettings {
                    model: "mock-model".to_string(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
        );

        assert_eq!(request.request_type, "codex.turn.start");
        assert_eq!(request.payload["threadId"], "thread-1");
        assert_eq!(request.payload["input"][0]["text"], "Plan this");
        assert_eq!(request.payload["collaborationMode"]["mode"], "plan");
        assert_eq!(
            request.payload["collaborationMode"]["settings"]["model"],
            "mock-model"
        );
    }

    #[test]
    fn maps_upstream_plan_notification_to_typed_gateway_plan_delta() {
        let event = CodexGatewayEvent::from_codex_notification(
            "item/plan/delta",
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "turn-1-plan",
                "delta": "- first\n"
            }),
        );

        assert_eq!(event.event_type, "codex.plan.delta");
        assert_eq!(event.payload["delta"], "- first\n");
    }

    #[tokio::test]
    async fn gateway_store_persists_plan_mode_items_and_deltas() {
        let root = unique_tmp_dir("todex-codex-plan-mode");
        let store = CodexGatewayStore::new(root.clone());
        let collaboration_mode = CodexCollaborationMode {
            mode: CodexModeKind::Plan,
            settings: CodexCollaborationSettings {
                model: "mock-model".to_string(),
                reasoning_effort: None,
                developer_instructions: None,
            },
        };

        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.turn.started",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "collaborationMode": collaboration_mode
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::from_codex_notification(
                    "item/plan/delta",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "itemId": "turn-1-plan",
                        "delta": "# Final plan\n"
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.item.completed",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "item": {
                            "type": "plan",
                            "id": "turn-1-plan",
                            "text": "# Final plan\n- first\n- second\n"
                        }
                    }),
                ),
            )
            .await
            .unwrap();

        let recovered = CodexGatewayStore::new(root.clone())
            .recover_session("session-1")
            .await
            .unwrap();
        let plan_state = recovered
            .plan_states
            .get("turn-1-plan")
            .expect("plan item state should be persisted");

        assert!(recovered
            .collaboration_modes
            .get("turn-1")
            .expect("collaboration mode should be preserved")
            .is_plan_mode());
        assert_eq!(plan_state.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(plan_state.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(plan_state.item_id.as_deref(), Some("turn-1-plan"));
        assert_eq!(plan_state.deltas, vec!["# Final plan\n".to_string()]);
        assert_eq!(
            plan_state.items,
            vec![CodexPlanItem {
                text: "# Final plan\n- first\n- second\n".to_string(),
                status: CodexPlanItemStatus::Completed,
            }]
        );

        let replay = store.replay_events("session-1", None, 10).await.unwrap();
        assert_eq!(replay.events[1].event_type, "codex.plan.delta");
        assert_eq!(replay.events[2].event_type, "codex.item.completed");

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn non_plan_turn_and_assistant_delta_do_not_create_plan_items() {
        let root = unique_tmp_dir("todex-codex-no-plan-item");
        let store = CodexGatewayStore::new(root.clone());

        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.turn.started",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "collaborationMode": {
                            "mode": "default",
                            "settings": { "model": "mock-model" }
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::from_codex_notification(
                    "item/agentMessage/delta",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "itemId": "agent-message-1",
                        "delta": "Done"
                    }),
                ),
            )
            .await
            .unwrap();

        let recovered = store.recover_session("session-1").await.unwrap();

        assert!(!recovered
            .collaboration_modes
            .get("turn-1")
            .expect("collaboration mode should be preserved")
            .is_plan_mode());
        assert!(recovered.plan_states.is_empty());
        assert!(recovered.plans.is_empty());

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn gateway_store_recovers_session_state_after_restart() {
        let root = unique_tmp_dir("todex-codex-recovery");
        let store = CodexGatewayStore::new(root.clone());

        store
            .append_event(
                "session-1",
                CodexGatewayEvent {
                    event_type: "codex.thread.started".to_string(),
                    payload: json!({ "threadId": "thread-1", "title": "Implement gateway" }),
                },
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent {
                    event_type: "codex.turn.started".to_string(),
                    payload: json!({ "threadId": "thread-1", "turnId": "turn-1" }),
                },
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent {
                    event_type: "codex.turn.planUpdated".to_string(),
                    payload: json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "planId": "plan-1",
                        "items": [{ "text": "Persist state", "status": "in_progress" }]
                    }),
                },
            )
            .await
            .unwrap();

        let recovered = CodexGatewayStore::new(root.clone())
            .recover_session("session-1")
            .await
            .unwrap();

        assert_eq!(recovered.last_cursor, 3);
        assert!(recovered.threads.contains_key("thread-1"));
        assert!(recovered.turns.contains_key("turn-1"));
        assert!(recovered.plans.contains_key("plan-1"));
        assert_eq!(recovered.events.len(), 3);
        assert!(root
            .join("codex_gateway")
            .join("sessions")
            .join("session-1")
            .join("state.json")
            .exists());

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn gateway_store_persists_thread_turn_lifecycle_in_order() {
        let root = unique_tmp_dir("todex-codex-lifecycle");
        let store = CodexGatewayStore::new(root.clone());

        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.thread.started",
                    json!({ "threadId": "thread-1", "request_id": "thread-start-1" }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.turn.started",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "request_id": "turn-start-1",
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.turn.interrupted",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "request_id": "turn-interrupt-1",
                        "status": "interrupted",
                    }),
                ),
            )
            .await
            .unwrap();

        let replay = CodexGatewayStore::new(root.clone())
            .replay_events("session-1", None, 10)
            .await
            .unwrap();

        assert_eq!(replay.next_cursor, Some(3));
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| (event.cursor, event.event_type.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "codex.thread.started"),
                (2, "codex.turn.started"),
                (3, "codex.turn.interrupted"),
            ]
        );
        assert_eq!(
            replay.events[0].codex_thread_id.as_deref(),
            Some("thread-1")
        );
        assert_eq!(replay.events[1].codex_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(replay.events[2].codex_turn_id.as_deref(), Some("turn-1"));

        let recovered = store.recover_session("session-1").await.unwrap();
        assert_eq!(recovered.last_cursor, 3);
        assert!(recovered.threads.contains_key("thread-1"));
        assert_eq!(
            recovered
                .turns
                .get("turn-1")
                .and_then(|turn| turn.get("status"))
                .and_then(Value::as_str),
            Some("interrupted")
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn gateway_store_persists_turn_steer_update() {
        let root = unique_tmp_dir("todex-codex-steer");
        let store = CodexGatewayStore::new(root.clone());

        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.turn.started",
                    json!({ "threadId": "thread-1", "turnId": "turn-1" }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.turn.steerUpdated",
                    json!({
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "expectedTurnId": "turn-1",
                        "request_id": "turn-steer-1",
                        "input": [{ "type": "text", "text": "adjust course", "textElements": [] }],
                    }),
                ),
            )
            .await
            .unwrap();

        let replay = store.replay_events("session-1", Some(1), 10).await.unwrap();
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].cursor, 2);
        assert_eq!(replay.events[0].event_type, "codex.turn.steerUpdated");
        assert_eq!(replay.events[0].codex_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(
            replay.events[0]
                .payload
                .get("expectedTurnId")
                .and_then(Value::as_str),
            Some("turn-1")
        );

        let recovered = store.recover_session("session-1").await.unwrap();
        assert_eq!(recovered.last_cursor, 2);
        assert_eq!(
            recovered
                .turns
                .get("turn-1")
                .and_then(|turn| turn.get("request_id"))
                .and_then(Value::as_str),
            Some("turn-steer-1")
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn gateway_store_persists_cloud_task_lifecycle_and_attempts() {
        let root = unique_tmp_dir("todex-codex-cloud-task-lifecycle");
        let store = CodexGatewayStore::new(root.clone());

        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.cloudTask.create.result",
                    json!({
                        "requestId": "cloud-create-1",
                        "taskId": "task-1",
                        "title": "Implement remote task lifecycle",
                        "status": "pending",
                        "attemptTotal": 2,
                        "summary": {
                            "filesChanged": 1,
                            "linesAdded": 4,
                            "linesRemoved": 1
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.cloudTask.getSummary.result",
                    json!({
                        "requestId": "cloud-summary-1",
                        "taskId": "task-1",
                        "title": "Implement remote task lifecycle",
                        "status": "ready",
                        "attemptTotal": 2,
                        "summary": {
                            "filesChanged": 2,
                            "linesAdded": 8,
                            "linesRemoved": 2
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.cloudTask.getDiff.result",
                    json!({
                        "requestId": "cloud-diff-1",
                        "taskId": "task-1",
                        "diff": "diff --git a/src/lib.rs b/src/lib.rs\n"
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.cloudTask.listSiblingAttempts.result",
                    json!({
                        "requestId": "cloud-attempts-1",
                        "taskId": "task-1",
                        "turnId": "turn-base",
                        "attempts": [
                            {
                                "turnId": "turn-base",
                                "attemptPlacement": 0,
                                "createdAt": "2026-05-07T00:00:00Z",
                                "status": "completed",
                                "diff": "diff --git a/base b/base\n",
                                "messages": ["base attempt"]
                            },
                            {
                                "turnId": "turn-alt",
                                "attemptPlacement": 1,
                                "createdAt": "2026-05-07T00:01:00Z",
                                "status": "completed",
                                "diff": "diff --git a/alt b/alt\n",
                                "messages": ["alternate attempt"]
                            }
                        ]
                    }),
                ),
            )
            .await
            .unwrap();

        let recovered = store.recover_session("session-1").await.unwrap();
        assert_eq!(recovered.last_cursor, 4);
        assert_eq!(
            recovered
                .cloud_tasks
                .get("task-1")
                .and_then(|task| payload_str(task, "requestId")),
            Some("cloud-attempts-1")
        );
        let attempts = recovered
            .cloud_task_attempts
            .get("task-1")
            .expect("sibling attempts should be persisted");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].turn_id, "turn-base");
        assert_eq!(attempts[0].attempt_placement, Some(0));
        assert_eq!(attempts[0].status, CodexCloudTaskAttemptStatus::Completed);
        assert_eq!(attempts[1].turn_id, "turn-alt");
        assert_eq!(attempts[1].messages, vec!["alternate attempt".to_string()]);

        let replay = store.replay_events("session-1", Some(1), 10).await.unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "codex.cloudTask.getSummary.result",
                "codex.cloudTask.getDiff.result",
                "codex.cloudTask.listSiblingAttempts.result",
            ]
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn gateway_store_persists_cloud_task_apply_outcomes() {
        let root = unique_tmp_dir("todex-codex-cloud-task-apply");
        let store = CodexGatewayStore::new(root.clone());

        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.cloudTask.applyPreflight.result",
                    json!({
                        "requestId": "preflight-1",
                        "taskId": "task-1",
                        "turnId": "turn-alt",
                        "attemptPlacement": 1,
                        "preflight": true,
                        "outcome": {
                            "applied": false,
                            "status": "success",
                            "message": "Preflight passed for task task-1",
                            "skippedPaths": [],
                            "conflictPaths": []
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        store
            .append_event(
                "session-1",
                CodexGatewayEvent::new(
                    "codex.cloudTask.apply.result",
                    json!({
                        "requestId": "apply-1",
                        "taskId": "task-1",
                        "turnId": "turn-alt",
                        "attemptPlacement": 1,
                        "preflight": false,
                        "outcome": {
                            "applied": false,
                            "status": "partial",
                            "message": "Apply partially succeeded for task task-1",
                            "skippedPaths": ["src/generated.rs"],
                            "conflictPaths": ["src/lib.rs"]
                        }
                    }),
                ),
            )
            .await
            .unwrap();

        let recovered = store.recover_session("session-1").await.unwrap();
        let preflight = recovered
            .cloud_task_apply_outcomes
            .get("preflight-1")
            .expect("preflight outcome should be persisted");
        assert_eq!(preflight.status, CodexCloudTaskApplyStatus::Success);
        assert!(!preflight.applied);
        assert!(preflight.preflight);
        assert_eq!(preflight.task_id.as_deref(), Some("task-1"));
        assert_eq!(preflight.turn_id.as_deref(), Some("turn-alt"));
        assert_eq!(preflight.attempt_placement, Some(1));

        let apply = recovered
            .cloud_task_apply_outcomes
            .get("apply-1")
            .expect("apply outcome should be persisted");
        assert_eq!(apply.status, CodexCloudTaskApplyStatus::Partial);
        assert!(!apply.applied);
        assert!(!apply.preflight);
        assert_eq!(apply.skipped_paths, vec!["src/generated.rs".to_string()]);
        assert_eq!(apply.conflict_paths, vec!["src/lib.rs".to_string()]);

        let replay = store.replay_events("session-1", None, 10).await.unwrap();
        assert_eq!(replay.events.len(), 2);
        assert_eq!(
            replay.events[0].event_type,
            "codex.cloudTask.applyPreflight.result"
        );
        assert_eq!(replay.events[1].event_type, "codex.cloudTask.apply.result");

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn gateway_store_replays_after_cursor_with_contiguous_cursors() {
        let root = unique_tmp_dir("todex-codex-cursor");
        let store = CodexGatewayStore::new(root.clone());

        let first = store
            .append_event(
                "session-1",
                CodexGatewayEvent {
                    event_type: "codex.thread.started".to_string(),
                    payload: json!({ "threadId": "thread-1" }),
                },
            )
            .await
            .unwrap();
        let second = store
            .append_event(
                "session-1",
                CodexGatewayEvent {
                    event_type: "codex.turn.started".to_string(),
                    payload: json!({ "threadId": "thread-1", "turnId": "turn-1" }),
                },
            )
            .await
            .unwrap();
        let third = store
            .append_event(
                "session-1",
                CodexGatewayEvent {
                    event_type: "codex.approval.commandExecution.request".to_string(),
                    payload: json!({ "threadId": "thread-1", "requestId": "approval-1" }),
                },
            )
            .await
            .unwrap();

        assert_eq!(first.cursor, 1);
        assert_eq!(second.cursor, 2);
        assert_eq!(third.cursor, 3);

        let replay = CodexGatewayStore::new(root.clone())
            .replay_events("session-1", Some(first.cursor), 10)
            .await
            .unwrap();

        assert_eq!(replay.from_cursor, Some(1));
        assert_eq!(replay.next_cursor, Some(3));
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.cursor)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(replay.events[0].event_type, "codex.turn.started");
        assert_eq!(
            replay.events[1].event_type,
            "codex.approval.commandExecution.request"
        );

        let limited = store.replay_events("session-1", None, 2).await.unwrap();
        assert_eq!(limited.next_cursor, Some(2));
        assert_eq!(limited.events.len(), 2);

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    fn payload_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
        payload.get(key).and_then(Value::as_str)
    }

    fn unique_tmp_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{}-{}", prefix, Uuid::new_v4().simple()))
    }

    #[tokio::test]
    async fn local_codex_adapter_supervisor_start_ready_persists_recovered_state() {
        let root = unique_tmp_dir("todex-codex-local-adapter-ready");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_fake_codex_binary(&root, "ready").await;
        let store = CodexGatewayStore::new(root.join("data"));
        let supervisor = CodexLocalAdapterSupervisor::new(store.clone(), EventBus::new(16));

        let adapter = supervisor
            .start(CodexLocalAdapterStartOptions {
                startup_timeout_ms: 1_000,
                ..CodexLocalAdapterStartOptions::new(
                    "cdxs_adapter_ready",
                    "local-start-1",
                    cwd,
                    binary.to_string_lossy(),
                )
            })
            .await
            .unwrap();

        let runtime = adapter.runtime().await;
        assert_eq!(runtime.state, CodexLocalAdapterLifecycleState::Ready);
        assert!(runtime
            .child_process
            .as_ref()
            .is_some_and(|child| child.pid > 0));
        assert_eq!(supervisor.len(), 1);
        assert!(supervisor.get("cdxs_adapter_ready").is_some());

        let replay = store
            .replay_events("cdxs_adapter_ready", None, 10)
            .await
            .unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["codex.control.starting", "codex.control.ready"]
        );
        assert_eq!(replay.events[1].payload["lifecycleState"], json!("ready"));

        let recovered = store.recover_session("cdxs_adapter_ready").await.unwrap();
        assert_eq!(recovered.last_cursor, 2);
        assert_eq!(
            recovered.local_adapters["cdxs_adapter_ready"]["lifecycleState"],
            json!("ready")
        );

        supervisor
            .stop("cdxs_adapter_ready", "local-stop-1", true)
            .await
            .unwrap();
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn local_codex_adapter_supervisor_stop_cleans_up_child_and_registry() {
        let root = unique_tmp_dir("todex-codex-local-adapter-stop");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_fake_codex_binary(&root, "ready").await;
        let store = CodexGatewayStore::new(root.join("data"));
        let supervisor = CodexLocalAdapterSupervisor::new(store.clone(), EventBus::new(16));
        supervisor
            .start(CodexLocalAdapterStartOptions {
                startup_timeout_ms: 1_000,
                ..CodexLocalAdapterStartOptions::new(
                    "cdxs_adapter_stop",
                    "local-start-1",
                    cwd,
                    binary.to_string_lossy(),
                )
            })
            .await
            .unwrap();

        let stopped = supervisor
            .stop("cdxs_adapter_stop", "local-stop-1", false)
            .await
            .unwrap();

        assert_eq!(stopped.state, CodexLocalAdapterLifecycleState::Stopped);
        assert!(stopped.child_process.is_none());
        assert_eq!(supervisor.len(), 0);
        assert!(supervisor.get("cdxs_adapter_stop").is_none());
        let replay = store
            .replay_events("cdxs_adapter_stop", None, 10)
            .await
            .unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "codex.control.starting",
                "codex.control.ready",
                "codex.control.stopping",
                "codex.control.stopped",
            ]
        );
        assert_eq!(
            replay.events[3].payload["operation"],
            json!("codex.local.stop")
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn local_codex_adapter_supervisor_maps_start_failures_to_typed_errors() {
        let root = unique_tmp_dir("todex-codex-local-adapter-failures");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let store = CodexGatewayStore::new(root.join("data"));
        let supervisor = CodexLocalAdapterSupervisor::new(store.clone(), EventBus::new(16));

        let missing = supervisor
            .start(CodexLocalAdapterStartOptions::new(
                "cdxs_missing",
                "local-start-missing",
                &cwd,
                root.join("missing-codex").to_string_lossy(),
            ))
            .await
            .unwrap_err();
        assert_eq!(missing.payload.code, CodexLocalErrorCode::MissingBinary);

        let invalid_cwd = supervisor
            .start(CodexLocalAdapterStartOptions::new(
                "cdxs_invalid_cwd",
                "local-start-invalid-cwd",
                root.join("missing-project"),
                root.join("missing-codex").to_string_lossy(),
            ))
            .await
            .unwrap_err();
        assert_eq!(invalid_cwd.payload.code, CodexLocalErrorCode::InvalidCwd);

        let permission_binary = write_fake_codex_binary(&root, "ready").await;
        set_executable(&permission_binary, false).await;
        let permission = supervisor
            .start(CodexLocalAdapterStartOptions::new(
                "cdxs_permission",
                "local-start-permission",
                &cwd,
                permission_binary.to_string_lossy(),
            ))
            .await
            .unwrap_err();
        assert_eq!(
            permission.payload.code,
            CodexLocalErrorCode::PermissionDenied
        );

        let timeout_binary = write_fake_codex_binary(&root, "timeout").await;
        let startup_timeout = supervisor
            .start(CodexLocalAdapterStartOptions {
                startup_timeout_ms: 50,
                ..CodexLocalAdapterStartOptions::new(
                    "cdxs_timeout_process",
                    "local-start-timeout",
                    &cwd,
                    timeout_binary.to_string_lossy(),
                )
            })
            .await
            .unwrap_err();
        assert_eq!(
            startup_timeout.payload.code,
            CodexLocalErrorCode::StartupTimeout
        );

        let crash_binary = write_fake_codex_binary(&root, "crash").await;
        let crash = supervisor
            .start(CodexLocalAdapterStartOptions {
                startup_timeout_ms: 1_000,
                ..CodexLocalAdapterStartOptions::new(
                    "cdxs_crash_process",
                    "local-start-crash",
                    &cwd,
                    crash_binary.to_string_lossy(),
                )
            })
            .await
            .unwrap_err();
        assert_eq!(crash.payload.code, CodexLocalErrorCode::AdapterCrash);

        assert_error_recovered(&store, "cdxs_missing", CodexLocalErrorCode::MissingBinary).await;
        assert_error_recovered(
            &store,
            "cdxs_permission",
            CodexLocalErrorCode::PermissionDenied,
        )
        .await;
        assert_error_recovered(
            &store,
            "cdxs_timeout_process",
            CodexLocalErrorCode::StartupTimeout,
        )
        .await;
        assert_error_recovered(
            &store,
            "cdxs_crash_process",
            CodexLocalErrorCode::AdapterCrash,
        )
        .await;

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn normalize_local_codex_cwd_corrects_unique_case_mismatch() {
        let root = unique_tmp_dir("todex-codex-cwd-case");
        let actual = root.join("TodeX");
        std::fs::create_dir_all(&actual).unwrap();

        let normalized = normalize_local_codex_cwd(root.join("Todex"));

        assert_eq!(normalized, actual);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn local_codex_adapter_supervisor_rejects_duplicate_session_ownership() {
        let root = unique_tmp_dir("todex-codex-local-adapter-duplicate");
        let cwd = root.join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let binary = write_fake_codex_binary(&root, "ready").await;
        let store = CodexGatewayStore::new(root.join("data"));
        let supervisor = CodexLocalAdapterSupervisor::new(store, EventBus::new(16));
        let options = CodexLocalAdapterStartOptions {
            startup_timeout_ms: 1_000,
            ..CodexLocalAdapterStartOptions::new(
                "cdxs_duplicate",
                "local-start-1",
                &cwd,
                binary.to_string_lossy(),
            )
        };

        supervisor.start(options.clone()).await.unwrap();
        let duplicate = supervisor.start(options).await.unwrap_err();

        assert_eq!(
            duplicate.payload.code,
            CodexLocalErrorCode::UnsupportedAction
        );
        assert_eq!(supervisor.len(), 1);
        supervisor
            .stop("cdxs_duplicate", "local-stop-1", true)
            .await
            .unwrap();
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    async fn write_fake_codex_binary(root: &std::path::Path, mode: &str) -> std::path::PathBuf {
        let path = root.join(format!("fake-codex-{mode}"));
        let script = match mode {
            "ready" => {
                "#!/bin/sh\nprintf '{\"type\":\"codex.control.ready\"}\\n'\nwhile read line; do :; done\n"
            }
            "timeout" => "#!/bin/sh\nsleep 2\n",
            "crash" => "#!/bin/sh\nexit 42\n",
            other => panic!("unknown fake codex mode: {other}"),
        };
        tokio::fs::write(&path, script).await.unwrap();
        set_executable(&path, true).await;
        path
    }

    async fn set_executable(path: &std::path::Path, executable: bool) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = if executable { 0o755 } else { 0o644 };
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .await
                .unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = (path, executable);
        }
    }

    async fn assert_error_recovered(
        store: &CodexGatewayStore,
        session_id: &str,
        code: CodexLocalErrorCode,
    ) {
        let replay = store.replay_events(session_id, None, 10).await.unwrap();
        assert_eq!(replay.events[0].event_type, "codex.control.starting");
        let error = replay
            .events
            .iter()
            .find(|event| event.event_type == "codex.control.error")
            .expect("error event should be persisted");
        assert_eq!(
            error.payload["error"]["code"],
            serde_json::to_value(code).unwrap()
        );
    }

    #[tokio::test]
    async fn review_start_flow_emits_and_persists_inline_review_events() {
        let root = unique_tmp_dir("todex-codex-review-flow");
        let store = CodexGatewayStore::new(root.clone());
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, mut events, _server_requests) = CodexGatewayAdapter::new(transport.clone());
        let request = CodexGatewayRequest::review_start(
            "review-request-1",
            "thread-1",
            CodexReviewTarget::Commit {
                sha: "1234567deadbeef".to_string(),
                title: Some("Tidy UI colors".to_string()),
            },
            CodexReviewDelivery::Inline,
        );

        adapter.dispatch_request(request.clone()).await.unwrap();
        let started = events.recv().await.unwrap();
        assert_eq!(started.event_type, "codex.review.started");
        assert_eq!(
            payload_str(&started.payload, "requestId"),
            Some("review-request-1")
        );
        assert_eq!(payload_str(&started.payload, "threadId"), Some("thread-1"));
        assert_eq!(payload_str(&started.payload, "delivery"), Some("inline"));
        assert_eq!(
            started
                .payload
                .get("target")
                .and_then(|target| payload_str(target, "type")),
            Some("commit")
        );
        store.append_event("session-1", started).await.unwrap();
        assert_eq!(
            transport.sent().await,
            vec![CodexOutboundMessage::Request(request)]
        );

        let response = CodexGatewayResponse {
            id: "review-request-1".to_string(),
            response_type: "codex.review.start.result".to_string(),
            payload: json!({
                "threadId": "thread-1",
                "turn": { "id": "turn-review-1", "status": "inProgress" },
                "reviewThreadId": "thread-1"
            }),
        };
        let routed = adapter
            .handle_inbound(CodexInboundMessage::Response(response.clone()))
            .unwrap();
        assert_eq!(routed, Some(response));
        let result = events.recv().await.unwrap();
        assert_eq!(result.event_type, "codex.review.start.result");
        assert_eq!(
            payload_str(&result.payload, "requestId"),
            Some("review-request-1")
        );
        assert_eq!(
            payload_str(&result.payload, "reviewThreadId"),
            Some("thread-1")
        );
        store.append_event("session-1", result).await.unwrap();

        let recovered = CodexGatewayStore::new(root.clone())
            .recover_session("session-1")
            .await
            .unwrap();
        assert_eq!(recovered.last_cursor, 2);
        assert!(recovered.reviews.contains_key("review-request-1"));
        assert_eq!(
            payload_str(&recovered.reviews["review-request-1"], "reviewThreadId"),
            Some("thread-1")
        );
        let replay = store.replay_events("session-1", None, 10).await.unwrap();
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["codex.review.started", "codex.review.start.result"]
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn command_approval_pending_accept_and_deny_events_are_persisted() {
        let root = unique_tmp_dir("todex-codex-approval-flow");
        let store = CodexGatewayStore::new(root.clone());
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, mut events, mut server_requests) =
            CodexGatewayAdapter::new(transport.clone());

        let approve_request = CodexServerRequest {
            id: "approval-accept-1".to_string(),
            request_type: "codex.approval.commandExecution.request".to_string(),
            payload: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "call-1",
                "command": "cargo test"
            }),
        };
        adapter
            .handle_inbound(CodexInboundMessage::ServerRequest(approve_request.clone()))
            .unwrap();
        assert_eq!(server_requests.recv().await.unwrap(), approve_request);
        let pending_accept = events.recv().await.unwrap();
        assert_eq!(
            pending_accept.event_type,
            "codex.approval.commandExecution.request"
        );
        assert_eq!(
            payload_str(&pending_accept.payload, "requestId"),
            Some("approval-accept-1")
        );
        assert_eq!(
            payload_str(&pending_accept.payload, "outcome"),
            Some("pending")
        );
        store
            .append_event("session-1", pending_accept)
            .await
            .unwrap();

        let accept_response = CodexServerResponse {
            id: "approval-accept-1".to_string(),
            response_type: "codex.approval.commandExecution.respond".to_string(),
            payload: json!({ "decision": "accept" }),
        };
        adapter
            .respond_to_server_request(accept_response.clone())
            .await
            .unwrap();
        let accepted = events.recv().await.unwrap();
        assert_eq!(accepted.event_type, "codex.serverRequest.resolved");
        assert_eq!(
            payload_str(&accepted.payload, "requestId"),
            Some("approval-accept-1")
        );
        assert_eq!(payload_str(&accepted.payload, "outcome"), Some("accepted"));
        store.append_event("session-1", accepted).await.unwrap();

        let deny_request = CodexServerRequest {
            id: "approval-deny-1".to_string(),
            request_type: "codex.approval.commandExecution.request".to_string(),
            payload: json!({
                "threadId": "thread-1",
                "turnId": "turn-2",
                "itemId": "call-2",
                "command": "rm -rf target"
            }),
        };
        adapter
            .handle_inbound(CodexInboundMessage::ServerRequest(deny_request.clone()))
            .unwrap();
        assert_eq!(server_requests.recv().await.unwrap(), deny_request);
        let pending_deny = events.recv().await.unwrap();
        assert_eq!(
            payload_str(&pending_deny.payload, "outcome"),
            Some("pending")
        );
        store.append_event("session-1", pending_deny).await.unwrap();

        let deny_response = CodexServerResponse {
            id: "approval-deny-1".to_string(),
            response_type: "codex.approval.commandExecution.respond".to_string(),
            payload: json!({ "decision": "decline" }),
        };
        adapter
            .respond_to_server_request(deny_response.clone())
            .await
            .unwrap();
        let denied = events.recv().await.unwrap();
        assert_eq!(
            payload_str(&denied.payload, "requestId"),
            Some("approval-deny-1")
        );
        assert_eq!(payload_str(&denied.payload, "outcome"), Some("denied"));
        store.append_event("session-1", denied).await.unwrap();

        let recovered = CodexGatewayStore::new(root.clone())
            .recover_session("session-1")
            .await
            .unwrap();
        assert_eq!(recovered.last_cursor, 4);
        assert_eq!(
            payload_str(&recovered.approvals["approval-accept-1"], "outcome"),
            Some("accepted")
        );
        assert_eq!(
            payload_str(&recovered.approvals["approval-deny-1"], "outcome"),
            Some("denied")
        );
        let replay = store.replay_events("session-1", None, 10).await.unwrap();
        assert_eq!(replay.events.len(), 4);
        assert_eq!(
            replay.events[0].event_type,
            "codex.approval.commandExecution.request"
        );
        assert_eq!(replay.events[1].event_type, "codex.serverRequest.resolved");
        assert_eq!(
            transport.sent().await,
            vec![
                CodexOutboundMessage::ServerResponse(accept_response),
                CodexOutboundMessage::ServerResponse(deny_response),
            ]
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn mcp_tool_call_routing_emits_policy_checked_start_and_result() {
        let root = unique_tmp_dir("todex-codex-mcp-tool-call");
        let store = CodexGatewayStore::new(root.clone());
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, mut events, _server_requests) = CodexGatewayAdapter::new(transport.clone());
        let request = CodexGatewayRequest::mcp_tool_call(
            "mcp-tool-1",
            "thread-1",
            "filesystem",
            "read_file",
            Some(json!({ "path": "README.md" })),
        );

        adapter.dispatch_request(request.clone()).await.unwrap();
        let started = events.recv().await.unwrap();
        assert_eq!(started.event_type, "codex.mcp.tool.call.started");
        assert_eq!(
            payload_str(&started.payload, "requestId"),
            Some("mcp-tool-1")
        );
        assert_eq!(payload_str(&started.payload, "threadId"), Some("thread-1"));
        assert_eq!(payload_str(&started.payload, "server"), Some("filesystem"));
        assert_eq!(payload_str(&started.payload, "tool"), Some("read_file"));
        assert_eq!(payload_str(&started.payload, "outcome"), Some("pending"));
        store.append_event("session-1", started).await.unwrap();
        assert_eq!(
            transport.sent().await,
            vec![CodexOutboundMessage::Request(request)]
        );

        let response = CodexGatewayResponse {
            id: "mcp-tool-1".to_string(),
            response_type: "codex.mcp.tool.call.result".to_string(),
            payload: json!({
                "content": [{ "type": "text", "text": "contents" }],
                "structuredContent": { "ok": true },
                "isError": false
            }),
        };
        let routed = adapter
            .handle_inbound(CodexInboundMessage::Response(response.clone()))
            .unwrap();
        assert_eq!(routed, Some(response));
        let result = events.recv().await.unwrap();
        assert_eq!(result.event_type, "codex.mcp.tool.call.result");
        assert_eq!(
            payload_str(&result.payload, "requestId"),
            Some("mcp-tool-1")
        );
        assert_eq!(payload_str(&result.payload, "outcome"), Some("succeeded"));
        store.append_event("session-1", result).await.unwrap();

        let recovered = store.recover_session("session-1").await.unwrap();
        assert_eq!(recovered.last_cursor, 2);
        assert_eq!(
            payload_str(&recovered.mcp_tool_calls["mcp-tool-1"], "outcome"),
            Some("succeeded")
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn mcp_elicitation_pending_and_response_outcomes_are_persisted() {
        let root = unique_tmp_dir("todex-codex-mcp-elicitation");
        let store = CodexGatewayStore::new(root.clone());
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, mut events, mut server_requests) =
            CodexGatewayAdapter::new(transport.clone());
        let request = CodexServerRequest {
            id: "elicitation-1".to_string(),
            request_type: "codex.mcp.elicitation.request".to_string(),
            payload: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "serverName": "filesystem",
                "mode": "form",
                "message": "Allow filesystem access?",
                "requestedSchema": { "type": "object", "properties": {} }
            }),
        };

        adapter
            .handle_inbound(CodexInboundMessage::ServerRequest(request.clone()))
            .unwrap();
        assert_eq!(server_requests.recv().await.unwrap(), request);
        let pending = events.recv().await.unwrap();
        assert_eq!(pending.event_type, "codex.mcp.elicitation.request");
        assert_eq!(
            payload_str(&pending.payload, "requestId"),
            Some("elicitation-1")
        );
        assert_eq!(payload_str(&pending.payload, "outcome"), Some("pending"));
        store.append_event("session-1", pending).await.unwrap();

        let response = CodexServerResponse {
            id: "elicitation-1".to_string(),
            response_type: "codex.mcp.elicitation.respond".to_string(),
            payload: json!({ "action": "accept", "content": { "allow": true }, "_meta": null }),
        };
        adapter
            .respond_to_server_request(response.clone())
            .await
            .unwrap();
        let resolved = events.recv().await.unwrap();
        assert_eq!(resolved.event_type, "codex.mcp.elicitation.resolved");
        assert_eq!(
            payload_str(&resolved.payload, "requestId"),
            Some("elicitation-1")
        );
        assert_eq!(payload_str(&resolved.payload, "outcome"), Some("accepted"));
        store.append_event("session-1", resolved).await.unwrap();

        let recovered = store.recover_session("session-1").await.unwrap();
        assert_eq!(recovered.last_cursor, 2);
        assert_eq!(
            payload_str(&recovered.mcp_elicitations["elicitation-1"], "outcome"),
            Some("accepted")
        );
        assert_eq!(
            transport.sent().await,
            vec![CodexOutboundMessage::ServerResponse(response)]
        );

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn routes_request_response_callbacks_without_terminal_bridge() {
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, _events, _server_requests) = CodexGatewayAdapter::new(transport.clone());
        let request = CodexGatewayRequest::new(
            "client-1",
            CodexBackendKind::AppServer,
            "codex.thread.start",
            json!({ "prompt": "hello" }),
        );

        adapter.dispatch_request(request.clone()).await.unwrap();
        assert!(adapter.has_pending_request("client-1"));
        assert_eq!(
            transport.sent().await,
            vec![CodexOutboundMessage::Request(request)]
        );

        let response = CodexGatewayResponse {
            id: "client-1".to_string(),
            response_type: "codex.thread.start.result".to_string(),
            payload: json!({ "threadId": "thread-1" }),
        };
        let routed = adapter
            .handle_inbound(CodexInboundMessage::Response(response.clone()))
            .unwrap();

        assert_eq!(routed, Some(response));
        assert!(!adapter.has_pending_request("client-1"));
    }

    #[tokio::test]
    async fn routes_server_request_and_preserves_response_correlation() {
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, _events, mut server_requests) = CodexGatewayAdapter::new(transport.clone());
        let server_request = CodexServerRequest {
            id: "server-request-1".to_string(),
            request_type: "codex.approval.commandExecution.request".to_string(),
            payload: json!({ "command": "cargo test" }),
        };

        let routed = adapter
            .handle_inbound(CodexInboundMessage::ServerRequest(server_request.clone()))
            .unwrap();

        assert_eq!(routed, None);
        assert!(adapter.has_pending_server_request("server-request-1"));
        assert_eq!(server_requests.recv().await.unwrap(), server_request);

        let response = CodexServerResponse {
            id: "server-request-1".to_string(),
            response_type: "codex.approval.commandExecution.respond".to_string(),
            payload: json!({ "decision": "approved" }),
        };
        adapter
            .respond_to_server_request(response.clone())
            .await
            .unwrap();

        assert!(!adapter.has_pending_server_request("server-request-1"));
        assert_eq!(
            transport.sent().await,
            vec![CodexOutboundMessage::ServerResponse(response)]
        );
    }

    #[tokio::test]
    async fn emits_transport_events_separately_from_callbacks() {
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, mut events, _server_requests) = CodexGatewayAdapter::new(transport);
        let event = CodexGatewayEvent {
            event_type: "codex.turn.started".to_string(),
            payload: json!({ "turnId": "turn-1" }),
        };

        let routed = adapter
            .handle_inbound(CodexInboundMessage::Event(event.clone()))
            .unwrap();

        assert_eq!(routed, None);
        assert_eq!(events.recv().await.unwrap(), event);
    }

    #[tokio::test]
    async fn rejects_response_to_unknown_server_request() {
        let transport = Arc::new(RecordingTransport::default());
        let (adapter, _events, _server_requests) = CodexGatewayAdapter::new(transport.clone());
        let response = CodexServerResponse {
            id: "missing".to_string(),
            response_type: "codex.approval.commandExecution.respond".to_string(),
            payload: json!({ "decision": "approved" }),
        };

        let error = adapter
            .respond_to_server_request(response)
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::InvalidRequest(_)));
        assert!(transport.sent().await.is_empty());
    }

    #[tokio::test]
    async fn local_codex_fixture_happy_path_is_typed_and_deterministic() {
        let root = unique_tmp_dir("todex-codex-local-fixture-happy");
        let store = CodexGatewayStore::new(root.clone());
        let mut fixture =
            CodexLocalFixture::new(CodexLocalFixtureScenario::HappyPath, "cdxs_fixture_happy");

        fixture.start("local-start-1").unwrap();
        fixture.run_turn("local-turn-1").unwrap();
        assert_eq!(
            fixture.state(),
            CodexLocalAdapterLifecycleState::WaitingForApproval
        );
        fixture.steer("local-steer-1").unwrap();
        fixture.respond_to_approval("local-approval-1").unwrap();
        fixture.start_review("local-review-1").unwrap();
        fixture.interrupt("local-interrupt-1").unwrap();
        fixture.stop("local-stop-1").unwrap();
        assert_eq!(fixture.state(), CodexLocalAdapterLifecycleState::Stopped);

        let transcript = fixture.into_transcript();
        let second = {
            let mut fixture =
                CodexLocalFixture::new(CodexLocalFixtureScenario::HappyPath, "cdxs_fixture_happy");
            fixture.start("local-start-1").unwrap();
            fixture.run_turn("local-turn-1").unwrap();
            fixture.steer("local-steer-1").unwrap();
            fixture.respond_to_approval("local-approval-1").unwrap();
            fixture.start_review("local-review-1").unwrap();
            fixture.interrupt("local-interrupt-1").unwrap();
            fixture.stop("local-stop-1").unwrap();
            fixture.into_transcript()
        };
        assert_eq!(transcript, second);

        let event_types = transcript
            .frames
            .iter()
            .filter_map(CodexLocalFixtureFrame::event_type)
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec![
                "codex.local.lifecycle",
                "codex.local.lifecycle",
                "codex.turn.started",
                "codex.item.agentMessage.delta",
                "codex.item.agentMessage.delta",
                "codex.turn.planUpdated",
                "codex.turn.steerUpdated",
                "codex.serverRequest.resolved",
                "codex.turn.completed",
                "codex.review.started",
                "codex.review.start.result",
                "codex.turn.interrupted",
                "codex.local.lifecycle",
                "codex.local.lifecycle",
            ]
        );
        assert!(transcript.frames.iter().any(|frame| matches!(
            frame,
            CodexLocalFixtureFrame::ServerRequest(request)
                if request.request_type == "codex.approval.commandExecution.request"
        )));
        assert!(transcript.frames.iter().any(|frame| matches!(
            frame,
            CodexLocalFixtureFrame::Outbound(CodexOutboundMessage::Request(request))
                if request.request_type == "codex.turn.steer"
        )));
        assert!(transcript.frames.iter().any(|frame| matches!(
            frame,
            CodexLocalFixtureFrame::Outbound(CodexOutboundMessage::Request(request))
                if request.request_type == "codex.review.start"
        )));

        for frame in transcript.frames {
            if let CodexLocalFixtureFrame::Event(event) = frame {
                store
                    .append_event("cdxs_fixture_happy", event)
                    .await
                    .unwrap();
            }
        }

        let replay = store
            .replay_events("cdxs_fixture_happy", None, 20)
            .await
            .unwrap();
        assert_eq!(replay.events.len(), 14);
        assert_eq!(replay.events[2].event_type, "codex.turn.started");
        assert_eq!(
            replay.events[3].codex_turn_id.as_deref(),
            Some("turn-fixture-1")
        );
        assert_eq!(replay.events[5].event_type, "codex.turn.planUpdated");
        assert_eq!(replay.events[10].event_type, "codex.review.start.result");

        let recovered = store.recover_session("cdxs_fixture_happy").await.unwrap();
        assert!(recovered.turns.contains_key("turn-fixture-1"));
        assert!(recovered.reviews.contains_key("local-review-1"));
        assert!(recovered.approvals.contains_key("approval-fixture-1"));
        assert!(recovered.plan_states.contains_key("plan-fixture-1"));

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn local_codex_fixture_failure_modes_emit_stable_typed_errors() {
        let mut malformed =
            CodexLocalFixture::new(CodexLocalFixtureScenario::MalformedEvent, "cdxs_malformed");
        malformed.start("local-start-1").unwrap();
        let malformed_error = malformed.run_turn("local-turn-1").unwrap_err();
        assert_eq!(
            malformed_error.code,
            crate::server::protocol::CodexLocalErrorCode::MalformedEvent
        );
        assert_eq!(
            malformed_error.upstream_request_id.as_deref(),
            Some("fixture-upstream-1")
        );
        assert_eq!(malformed.state(), CodexLocalAdapterLifecycleState::Failed);
        assert_error_frame(
            malformed.frames(),
            crate::server::protocol::CodexLocalErrorCode::MalformedEvent,
            "codex.local.turn",
        );

        let mut timeout = CodexLocalFixture::new(
            CodexLocalFixtureScenario::DelayedReadyTimeout,
            "cdxs_timeout",
        );
        let timeout_error = timeout.start("local-start-1").unwrap_err();
        assert_eq!(
            timeout_error.code,
            crate::server::protocol::CodexLocalErrorCode::StartupTimeout
        );
        assert_eq!(timeout.state(), CodexLocalAdapterLifecycleState::Failed);
        assert_error_frame(
            timeout.frames(),
            crate::server::protocol::CodexLocalErrorCode::StartupTimeout,
            "codex.local.start",
        );

        let mut crash = CodexLocalFixture::new(CodexLocalFixtureScenario::Crash, "cdxs_crash");
        let crash_error = crash.start("local-start-1").unwrap_err();
        assert_eq!(
            crash_error.code,
            crate::server::protocol::CodexLocalErrorCode::AdapterCrash
        );
        assert_eq!(crash.state(), CodexLocalAdapterLifecycleState::Failed);
        assert_error_frame(
            crash.frames(),
            crate::server::protocol::CodexLocalErrorCode::AdapterCrash,
            "codex.local.start",
        );
    }

    fn assert_error_frame(
        frames: &[CodexLocalFixtureFrame],
        code: crate::server::protocol::CodexLocalErrorCode,
        operation: &str,
    ) {
        let error = frames
            .iter()
            .find_map(|frame| match frame {
                CodexLocalFixtureFrame::Error(error) if error.code == code => Some(error),
                _ => None,
            })
            .expect("typed fixture error frame should be present");
        assert_eq!(error.operation.as_deref(), Some(operation));
        assert!(error.message.contains("fixture"));
    }
}
