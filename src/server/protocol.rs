use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    codex_gateway::{CodexGatewayCursor, CodexGatewayEventRecord},
    event::EventRecord,
};

pub type CodexSessionId = String;

#[derive(Clone, Debug, Deserialize)]
pub struct ClientMessage {
    pub id: String,
    #[serde(flatten)]
    pub kind: ClientMessageKind,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessageKind {
    #[serde(rename = "codex.gateway.control")]
    CodexGatewayControl(CodexGatewayControlRequest),
    #[serde(rename = "codex.local.start")]
    CodexLocalStart(CodexLocalStartRequest),
    #[serde(rename = "codex.local.status")]
    CodexLocalStatus(CodexLocalSessionRequest),
    #[serde(rename = "codex.local.stop")]
    CodexLocalStop(CodexLocalStopRequest),
    #[serde(rename = "codex.local.turn")]
    CodexLocalTurn(CodexLocalTurnRequest),
    #[serde(rename = "codex.local.input")]
    CodexLocalInput(CodexLocalInputRequest),
    #[serde(rename = "codex.local.steer")]
    CodexLocalSteer(CodexLocalSteerRequest),
    #[serde(rename = "codex.local.interrupt")]
    CodexLocalInterrupt(CodexLocalInterruptRequest),
    #[serde(rename = "codex.local.approval.respond")]
    CodexLocalApprovalRespond(CodexLocalApprovalRespondRequest),
    #[serde(rename = "codex.local.request")]
    CodexLocalRequest(CodexLocalRequestRequest),
    #[serde(rename = "codex.local.replay")]
    CodexLocalReplay(CodexLocalReplayRequest),
    #[serde(rename = "codex.local.attach")]
    CodexLocalAttach(CodexLocalAttachRequest),
    #[serde(rename = "codex.local.snapshot")]
    CodexLocalSnapshot(CodexLocalSnapshotRequest),
    #[serde(rename = "codex.local.unsupported")]
    CodexLocalUnsupported(CodexLocalUnsupportedRequest),
    #[serde(rename = "codex.thread.start")]
    CodexThreadStart(CodexLifecycleRequest),
    #[serde(rename = "codex.turn.start")]
    CodexTurnStart(CodexLifecycleRequest),
    #[serde(rename = "codex.turn.steer")]
    CodexTurnSteer(CodexLifecycleRequest),
    #[serde(rename = "codex.turn.interrupt")]
    CodexTurnInterrupt(CodexLifecycleRequest),
    #[serde(rename = "codex.mcp.server.listStatus")]
    CodexMcpServerListStatus(CodexLifecycleRequest),
    #[serde(rename = "codex.mcp.resource.read")]
    CodexMcpResourceRead(CodexLifecycleRequest),
    #[serde(rename = "codex.mcp.tool.call")]
    CodexMcpToolCall(CodexLifecycleRequest),
    #[serde(rename = "codex.mcp.server.refresh")]
    CodexMcpServerRefresh(CodexLifecycleRequest),
    #[serde(rename = "codex.mcp.oauth.login")]
    CodexMcpOauthLogin(CodexLifecycleRequest),
    #[serde(rename = "codex.mcp.elicitation.respond")]
    CodexMcpElicitationRespond(CodexLifecycleRequest),
    #[serde(rename = "codex.cloudTask.create")]
    CodexCloudTaskCreate(CodexCloudTaskCreateRequest),
    #[serde(rename = "codex.cloudTask.list")]
    CodexCloudTaskList(CodexCloudTaskListRequest),
    #[serde(rename = "codex.cloudTask.getSummary")]
    CodexCloudTaskGetSummary(CodexCloudTaskIdRequest),
    #[serde(rename = "codex.cloudTask.getDiff")]
    CodexCloudTaskGetDiff(CodexCloudTaskIdRequest),
    #[serde(rename = "codex.cloudTask.getMessages")]
    CodexCloudTaskGetMessages(CodexCloudTaskIdRequest),
    #[serde(rename = "codex.cloudTask.getText")]
    CodexCloudTaskGetText(CodexCloudTaskIdRequest),
    #[serde(rename = "codex.cloudTask.listSiblingAttempts")]
    CodexCloudTaskListSiblingAttempts(CodexCloudTaskSiblingAttemptsRequest),
    #[serde(rename = "codex.cloudTask.applyPreflight")]
    CodexCloudTaskApplyPreflight(CodexCloudTaskApplyRequest),
    #[serde(rename = "codex.cloudTask.apply")]
    CodexCloudTaskApply(CodexCloudTaskApplyRequest),
}

#[derive(Clone, Debug, Deserialize)]
pub struct CodexGatewayControlRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub action: CodexGatewayAction,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalStartRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<String>,
    #[serde(default)]
    pub config_overrides: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalSessionRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalStopRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalTurnRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub thread_id: String,
    pub input: Value,
    #[serde(default)]
    pub approval_policy: Option<Value>,
    #[serde(default)]
    pub sandbox_policy: Option<Value>,
    #[serde(default)]
    pub service_tier: Option<Value>,
    #[serde(default)]
    pub collaboration_mode: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalInputRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub input: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalSteerRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub input: Value,
    #[serde(default)]
    pub expected_turn_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalInterruptRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub thread_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalApprovalRespondRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub request_id: String,
    pub response_type: String,
    pub response: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalRequestRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalReplayRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    #[serde(default)]
    pub after_cursor: Option<CodexGatewayCursor>,
    #[serde(default = "default_codex_local_replay_limit")]
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalAttachRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    #[serde(default)]
    pub after_cursor: Option<CodexGatewayCursor>,
    #[serde(default = "default_codex_local_replay_limit")]
    pub replay_limit: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalSnapshotRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    #[serde(default = "default_codex_local_snapshot_max_bytes")]
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct CodexLocalUnsupportedRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub operation: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CodexLifecycleRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CodexCloudTaskCreateRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub env_id: String,
    pub prompt: String,
    pub git_ref: String,
    #[serde(default)]
    pub qa_mode: bool,
    #[serde(default = "default_best_of_n")]
    pub best_of_n: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CodexCloudTaskListRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CodexCloudTaskIdRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub task_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CodexCloudTaskSiblingAttemptsRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub task_id: String,
    pub turn_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CodexCloudTaskApplyRequest {
    pub codex_session_id: CodexSessionId,
    pub tenant_id: String,
    pub task_id: String,
    #[serde(default)]
    pub diff_override: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub attempt_placement: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexGatewayAction {
    Control,
    Approval,
    Replay,
    ThreadStart,
    TurnStart,
    TurnSteer,
    TurnInterrupt,
    CloudTaskCreate,
    CloudTaskList,
    CloudTaskGetSummary,
    CloudTaskGetDiff,
    CloudTaskGetMessages,
    CloudTaskGetText,
    CloudTaskListSiblingAttempts,
    CloudTaskApplyPreflight,
    CloudTaskApply,
    McpServerListStatus,
    McpResourceRead,
    McpToolCall,
    McpServerRefresh,
    McpOauthLogin,
    McpElicitationRespond,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexLocalErrorCode {
    MissingBinary,
    PermissionDenied,
    InvalidCwd,
    StartupTimeout,
    MalformedEvent,
    UnsupportedAction,
    UnsupportedLocal,
    SessionBusy,
    AdapterCrash,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexLocalErrorPayload {
    pub code: CodexLocalErrorCode,
    pub message: String,
    pub codex_session_id: CodexSessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
}

fn default_codex_local_replay_limit() -> usize {
    200
}

fn default_codex_local_snapshot_max_bytes() -> usize {
    65_536
}

fn default_best_of_n() -> usize {
    1
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerEvent {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CodexGatewayCursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub payload: Value,
}

impl From<EventRecord> for ServerEvent {
    fn from(record: EventRecord) -> Self {
        Self {
            event_id: record.event_id,
            event_type: record.event_type,
            cursor: None,
            codex_session_id: None,
            codex_thread_id: None,
            codex_turn_id: None,
            workspace_id: record.workspace_id,
            window_id: record.window_id,
            pane_id: record.pane_id,
            payload: record.payload,
        }
    }
}

impl From<CodexGatewayEventRecord> for ServerEvent {
    fn from(record: CodexGatewayEventRecord) -> Self {
        Self {
            event_id: record.event_id,
            event_type: record.event_type,
            cursor: Some(record.cursor),
            codex_session_id: Some(record.session_id),
            codex_thread_id: record.codex_thread_id,
            codex_turn_id: record.codex_turn_id,
            workspace_id: None,
            window_id: None,
            pane_id: None,
            payload: record.payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{ClientMessage, ClientMessageKind, CodexLocalErrorCode, CodexLocalErrorPayload};

    fn parse_message(value: Value) -> ClientMessage {
        serde_json::from_value(value).expect("client message should deserialize")
    }

    #[test]
    fn codex_local_start_status_stop_json_deserializes() {
        let start = parse_message(json!({
            "id": "local-start-1",
            "type": "codex.local.start",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "cwd": "/tmp/project",
                "model": "gpt-5.5",
                "approvalPolicy": "on-request",
                "sandboxMode": "workspace-write",
                "configOverrides": { "reasoningEffort": "high" }
            }
        }));
        match start.kind {
            ClientMessageKind::CodexLocalStart(payload) => {
                assert_eq!(payload.codex_session_id, "cdxs_local");
                assert_eq!(payload.tenant_id, "local");
                assert_eq!(payload.cwd, "/tmp/project");
                assert_eq!(payload.model.as_deref(), Some("gpt-5.5"));
                assert_eq!(payload.config_overrides["reasoningEffort"], "high");
            }
            other => panic!("unexpected message kind: {other:?}"),
        }

        let status = parse_message(json!({
            "id": "local-status-1",
            "type": "codex.local.status",
            "payload": { "codexSessionId": "cdxs_local", "tenantId": "local" }
        }));
        assert!(matches!(
            status.kind,
            ClientMessageKind::CodexLocalStatus(_)
        ));

        let stop = parse_message(json!({
            "id": "local-stop-1",
            "type": "codex.local.stop",
            "payload": { "codexSessionId": "cdxs_local", "tenantId": "local", "force": true }
        }));
        match stop.kind {
            ClientMessageKind::CodexLocalStop(payload) => assert!(payload.force),
            other => panic!("unexpected message kind: {other:?}"),
        }
    }

    #[test]
    fn codex_local_turn_input_steer_interrupt_json_deserializes() {
        let turn = parse_message(json!({
            "id": "local-turn-1",
            "type": "codex.local.turn",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "threadId": "thread-1",
                "input": [{ "type": "text", "text": "hello" }],
                "collaborationMode": { "mode": "plan", "settings": { "model": "mock" } }
            }
        }));
        match turn.kind {
            ClientMessageKind::CodexLocalTurn(payload) => {
                assert_eq!(payload.thread_id, "thread-1");
                assert_eq!(payload.input[0]["text"], "hello");
                assert_eq!(payload.collaboration_mode.unwrap()["mode"], "plan");
            }
            other => panic!("unexpected message kind: {other:?}"),
        }

        let input = parse_message(json!({
            "id": "local-input-1",
            "type": "codex.local.input",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "threadId": "thread-1",
                "turnId": "turn-1",
                "input": [{ "type": "text", "text": "continue" }]
            }
        }));
        assert!(matches!(input.kind, ClientMessageKind::CodexLocalInput(_)));

        let steer = parse_message(json!({
            "id": "local-steer-1",
            "type": "codex.local.steer",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "threadId": "thread-1",
                "turnId": "turn-1",
                "expectedTurnId": "turn-1",
                "input": [{ "type": "text", "text": "adjust" }]
            }
        }));
        match steer.kind {
            ClientMessageKind::CodexLocalSteer(payload) => {
                assert_eq!(payload.turn_id, "turn-1");
                assert_eq!(payload.expected_turn_id.as_deref(), Some("turn-1"));
            }
            other => panic!("unexpected message kind: {other:?}"),
        }

        let interrupt = parse_message(json!({
            "id": "local-interrupt-1",
            "type": "codex.local.interrupt",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "threadId": "thread-1",
                "turnId": "turn-1"
            }
        }));
        assert!(matches!(
            interrupt.kind,
            ClientMessageKind::CodexLocalInterrupt(_)
        ));
    }

    #[test]
    fn codex_local_approval_replay_attach_snapshot_and_unsupported_json_deserializes() {
        let approval = parse_message(json!({
            "id": "local-approval-1",
            "type": "codex.local.approval.respond",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "requestId": "approval-1",
                "responseType": "codex.approval.commandExecution.respond",
                "response": { "decision": "accepted" }
            }
        }));
        match approval.kind {
            ClientMessageKind::CodexLocalApprovalRespond(payload) => {
                assert_eq!(payload.request_id, "approval-1");
                assert_eq!(payload.response["decision"], "accepted");
            }
            other => panic!("unexpected message kind: {other:?}"),
        }

        let replay = parse_message(json!({
            "id": "local-replay-1",
            "type": "codex.local.replay",
            "payload": { "codexSessionId": "cdxs_local", "tenantId": "local", "afterCursor": 7, "limit": 25 }
        }));
        match replay.kind {
            ClientMessageKind::CodexLocalReplay(payload) => {
                assert_eq!(payload.after_cursor, Some(7));
                assert_eq!(payload.limit, 25);
            }
            other => panic!("unexpected message kind: {other:?}"),
        }

        let attach = parse_message(json!({
            "id": "local-attach-1",
            "type": "codex.local.attach",
            "payload": { "codexSessionId": "cdxs_local", "tenantId": "local" }
        }));
        match attach.kind {
            ClientMessageKind::CodexLocalAttach(payload) => assert_eq!(payload.replay_limit, 200),
            other => panic!("unexpected message kind: {other:?}"),
        }

        let snapshot = parse_message(json!({
            "id": "local-snapshot-1",
            "type": "codex.local.snapshot",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "maxBytes": 4096
            }
        }));
        match snapshot.kind {
            ClientMessageKind::CodexLocalSnapshot(payload) => {
                assert_eq!(payload.max_bytes, 4096);
            }
            other => panic!("unexpected message kind: {other:?}"),
        }

        let unsupported = parse_message(json!({
            "id": "local-unsupported-1",
            "type": "codex.local.unsupported",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "operation": "codex.cloudTask.create",
                "reason": "cloud tasks are excluded from local Codex CLI control"
            }
        }));
        match unsupported.kind {
            ClientMessageKind::CodexLocalUnsupported(payload) => {
                assert_eq!(payload.operation, "codex.cloudTask.create");
                assert!(payload.reason.unwrap().contains("excluded"));
            }
            other => panic!("unexpected message kind: {other:?}"),
        }
    }

    #[test]
    fn codex_local_unknown_cloud_task_request_fails_closed() {
        let result = serde_json::from_value::<ClientMessage>(json!({
            "id": "local-cloud-task-1",
            "type": "codex.local.cloudTask.create",
            "payload": {
                "codexSessionId": "cdxs_local",
                "tenantId": "local",
                "prompt": "do cloud work"
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn codex_local_external_fields_are_camel_case_only() {
        let result = serde_json::from_value::<ClientMessage>(json!({
            "id": "local-start-snake-case",
            "type": "codex.local.start",
            "payload": {
                "codex_session_id": "cdxs_local",
                "tenant_id": "local",
                "cwd": "/tmp/project"
            }
        }));

        assert!(result.is_err());
    }

    #[test]
    fn codex_local_error_payload_serializes_stable_codes_and_correlation() {
        let payload = CodexLocalErrorPayload {
            code: CodexLocalErrorCode::SessionBusy,
            message: "local Codex session already has an in-flight command".to_string(),
            codex_session_id: "cdxs_local".to_string(),
            request_id: Some("turn-2".to_string()),
            operation: Some("codex.local.turn".to_string()),
            upstream_request_id: Some("jsonrpc-42".to_string()),
        };

        let value = serde_json::to_value(payload).unwrap();

        assert_eq!(value["code"], "SESSION_BUSY");
        assert_eq!(value["codexSessionId"], "cdxs_local");
        assert_eq!(value["requestId"], "turn-2");
        assert_eq!(value["operation"], "codex.local.turn");
        assert_eq!(value["upstreamRequestId"], "jsonrpc-42");
    }
}
