use std::{
    collections::BTreeSet,
    env, fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tungstenite::client::IntoClientRequest;

const TOKEN: &str = "todex_real_e2e_token";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Daemon {
    child: Child,
    port: u16,
    root: PathBuf,
    data_dir: PathBuf,
    workspace_root: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TODEX_REAL_E2E=1, real codex binary, Codex login, and model access"]
async fn real_codex_http_ws_auth_and_protocol_boundaries() {
    require_real_e2e();
    let daemon = spawn_daemon().await;

    assert_eq!(http_get(daemon.port, "/health").await.0, 200);
    let version = http_get(daemon.port, "/v1/version").await;
    assert_eq!(version.0, 200);
    assert!(version.1.contains("\"name\":\"todex-agentd\""));
    assert!(version.1.contains(&daemon.data_dir.display().to_string()));
    assert!(version
        .1
        .contains(&daemon.workspace_root.display().to_string()));
    assert_ne!(http_get(daemon.port, "/missing").await.0, 200);

    let mut unauth = connect_ws(daemon.port, None).await;
    send_json(
        &mut unauth,
        json!({
            "id": "unauth-status",
            "type": "codex.local.status",
            "payload": { "codexSessionId": "cdxs_auth", "tenantId": "local" }
        }),
    )
    .await;
    let status = wait_for_event(&mut unauth, |event| {
        event["type"] == "codex.control.status"
            && event["payload"]["data"]["requestId"] == "unauth-status"
    })
    .await;
    assert_eq!(status["payload"]["data"]["lifecycleState"], json!("idle"));

    let mut ws = connect_ws(daemon.port, Some(TOKEN)).await;
    send_json(
        &mut ws,
        json!({
            "id": "tenant-mismatch",
            "type": "codex.local.status",
            "payload": { "codexSessionId": "cdxs_auth", "tenantId": "other" }
        }),
    )
    .await;
    let unauthorized = wait_for_event(&mut ws, |event| {
        event["type"] == "error" && data_code(event) == Some("UNAUTHORIZED")
    })
    .await;
    assert_eq!(data_code(&unauthorized), Some("UNAUTHORIZED"));

    for (id, ty, payload) in [
        ("old-workspace", "create_workspace", json!({})),
        ("old-list", "list_workspaces", json!({})),
        ("old-terminal", "terminal_input", json!({})),
        (
            "local-cloud",
            "codex.local.cloudTask.create",
            json!({ "codexSessionId": "cdxs_auth", "tenantId": "local" }),
        ),
    ] {
        send_json(&mut ws, json!({ "id": id, "type": ty, "payload": payload })).await;
        let parse_error = wait_for_event(&mut ws, |event| {
            event["type"] == "error" && data_code(event) == Some("INVALID_REQUEST")
        })
        .await;
        assert_eq!(data_code(&parse_error), Some("INVALID_REQUEST"));
    }

    assert!(
        fs::read_to_string(daemon.data_dir.join("audit/audit.jsonl"))
            .expect("audit log should exist")
            .contains("NO_AUTH_REQUIRED")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TODEX_REAL_E2E=1, real codex binary, Codex login, and model access"]
async fn real_codex_single_session_plan_input_approval_replay_and_snapshot() {
    require_real_e2e();
    let daemon = spawn_daemon().await;
    let mut ws = connect_ws(daemon.port, Some(TOKEN)).await;
    let session = "cdxs_real_single";

    assert_status(&mut ws, session, "idle").await;
    start_session(&mut ws, session, &daemon.workspace_root).await;

    let thread_id = start_thread(&mut ws, session, "thread-start-single").await;
    send_json(
        &mut ws,
        json!({
            "id": "plan-turn-single",
            "type": "codex.local.turn",
            "payload": {
                "codexSessionId": session,
                "tenantId": "local",
                "threadId": thread_id,
                "collaborationMode": {
                    "mode": "plan",
                    "settings": {
                        "model": real_codex_model(),
                        "developerInstructions": null
                    }
                },
                "input": [{ "type": "text", "text": "制定一个两步测试计划。不要修改文件。" }]
            }
        }),
    )
    .await;
    let plan_or_message = wait_for_event(&mut ws, |event| {
        matches!(
            event["type"].as_str(),
            Some("codex.turn.planUpdated")
                | Some("codex.plan.delta")
                | Some("codex.item.agentMessage.delta")
                | Some("codex.item.completed")
        )
    })
    .await;
    assert!(plan_or_message["type"]
        .as_str()
        .unwrap()
        .starts_with("codex."));
    let turn_done = wait_for_event(&mut ws, |event| event["type"] == "codex.turn.completed").await;
    let turn_id = data_string(&turn_done, &["turnId", "turn_id"])
        .unwrap_or_else(|| "turn_missing_from_completion".to_owned());

    send_json(
        &mut ws,
        json!({
            "id": "input-after-plan",
            "type": "codex.local.input",
            "payload": {
                "codexSessionId": session,
                "tenantId": "local",
                "threadId": thread_id,
                "turnId": turn_id,
                "input": [{ "type": "text", "text": "补充一句：只输出简短结论。" }]
            }
        }),
    )
    .await;
    wait_for_event(&mut ws, |event| {
        event["type"] == "codex.control.request.accepted"
            || event["type"] == "codex.control.request.rejected"
            || event["type"] == "codex.control.error"
    })
    .await;

    send_json(
        &mut ws,
        json!({
            "id": "approval-probe",
            "type": "codex.local.turn",
            "payload": {
                "codexSessionId": session,
                "tenantId": "local",
                "threadId": thread_id,
                "input": [{ "type": "text", "text": "请求运行命令 `pwd`；如需要权限请发起批准请求。" }]
            }
        }),
    )
    .await;
    if let Ok(approval) = wait_for_event_maybe(&mut ws, Duration::from_secs(20), |event| {
        event["type"]
            .as_str()
            .is_some_and(|ty| ty.starts_with("codex.approval.") && ty.ends_with(".request"))
    })
    .await
    {
        let request_id = data_string(&approval, &["requestId", "request_id"])
            .expect("approval event should include requestId");
        let response_type = response_type_for_server_request(
            approval["type"]
                .as_str()
                .expect("server request event should include type"),
        );
        send_json(
            &mut ws,
            json!({
                "id": "approval-deny",
                "type": "codex.local.approval.respond",
                "payload": {
                    "codexSessionId": session,
                    "tenantId": "local",
                    "requestId": request_id,
                    "responseType": response_type,
                    "response": { "decision": "denied" }
                }
            }),
        )
        .await;
        wait_for_event(&mut ws, |event| {
            event["type"] == "codex.serverRequest.resolved"
                || event["type"]
                    .as_str()
                    .is_some_and(|ty| ty.contains("resolved"))
        })
        .await;
    }

    send_json(
        &mut ws,
        json!({
            "id": "snapshot-single",
            "type": "codex.local.snapshot",
            "payload": { "codexSessionId": session, "tenantId": "local", "maxBytes": 1024 }
        }),
    )
    .await;
    let snapshot = wait_for_event(&mut ws, |event| event["type"] == "codex.local.snapshot").await;
    assert_eq!(snapshot["payload"]["data"]["authoritative"], false);

    send_json(
        &mut ws,
        json!({
            "id": "replay-single",
            "type": "codex.local.replay",
            "payload": { "codexSessionId": session, "tenantId": "local", "afterCursor": 0, "limit": 20 }
        }),
    )
    .await;
    wait_for_event(&mut ws, |event| {
        event["payload"]["cursor"].as_u64().is_some()
    })
    .await;

    let mut attach_ws = connect_ws(daemon.port, Some(TOKEN)).await;
    send_json(
        &mut attach_ws,
        json!({
            "id": "attach-single",
            "type": "codex.local.attach",
            "payload": { "codexSessionId": session, "tenantId": "local", "replayLimit": 20 }
        }),
    )
    .await;
    wait_for_event(&mut attach_ws, |event| {
        event["payload"]["cursor"].as_u64().is_some()
    })
    .await;

    stop_session(&mut ws, session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TODEX_REAL_E2E=1, real codex binary, Codex login, and model access"]
async fn real_codex_common_native_controls_model_goal_permission_and_review() {
    require_real_e2e();
    let daemon = spawn_daemon().await;
    let mut ws = connect_ws(daemon.port, Some(TOKEN)).await;
    let session = "cdxs_real_controls";
    let review_session = "cdxs_real_review";

    start_session(&mut ws, session, &daemon.workspace_root).await;

    let model_list = send_local_request(
        &mut ws,
        session,
        "model-list-controls",
        "model/list",
        json!({ "limit": 5, "includeHidden": true }),
    )
    .await;
    assert!(
        model_list["payload"]["data"]["result"]["models"].is_array()
            || model_list["payload"]["data"]["result"]["items"].is_array()
            || model_list["payload"]["data"]["result"]["data"].is_array(),
        "unexpected model/list response: {model_list}"
    );

    let config_read = send_local_request(
        &mut ws,
        session,
        "config-read-controls",
        "config/read",
        json!({ "includeLayers": false, "cwd": daemon.workspace_root }),
    )
    .await;
    assert_eq!(
        config_read["payload"]["data"]["result"]["config"]["model"],
        real_codex_model()
    );

    let thread_start = send_local_request(
        &mut ws,
        session,
        "thread-start-controls",
        "thread/start",
        json!({
            "ephemeral": false,
            "approvalPolicy": {
                "granular": {
                    "sandbox_approval": true,
                    "rules": true,
                    "skill_approval": false,
                    "request_permissions": true,
                    "mcp_elicitations": true
                }
            },
            "permissions": { "type": "profile", "id": ":workspace", "modifications": null }
        }),
    )
    .await;
    let thread_id = data_result_string(&thread_start, &["thread", "id"])
        .or_else(|| data_result_string(&thread_start, &["id"]))
        .expect("thread/start response should include thread id");

    let mut review_ws = connect_ws(daemon.port, Some(TOKEN)).await;
    start_session(&mut review_ws, review_session, &daemon.workspace_root).await;
    let review_thread = start_thread_with_ephemeral(
        &mut review_ws,
        review_session,
        "review-thread-controls",
        false,
    )
    .await;
    send_json(
        &mut review_ws,
        json!({
            "id": "review-start-controls",
            "type": "codex.local.request",
            "payload": {
                "codexSessionId": review_session,
                "tenantId": "local",
                "method": "review/start",
                "params": {
                    "threadId": review_thread,
                    "delivery": "inline",
                    "target": {
                        "type": "custom",
                        "instructions": "仅检查当前对话是否完成控制验证，简短返回。"
                    }
                }
            }
        }),
    )
    .await;
    let review_progress = wait_for_event(&mut review_ws, |event| {
        is_session_turn_completion_or_approval_request(event, review_session)
    })
    .await;
    assert!(is_session_turn_completion_or_approval_request(
        &review_progress,
        review_session
    ));
    stop_session(&mut review_ws, review_session).await;

    let goal_set = send_local_request(
        &mut ws,
        session,
        "goal-set-controls",
        "thread/goal/set",
        json!({
            "threadId": thread_id,
            "objective": "验证 TodeX 对 Codex goal 原生控制",
            "tokenBudget": 10000
        }),
    )
    .await;
    assert_eq!(
        goal_set["payload"]["data"]["result"]["goal"]["objective"],
        "验证 TodeX 对 Codex goal 原生控制"
    );
    wait_for_event(&mut ws, |event| {
        event["payload"]["codex_session_id"].as_str() == Some(session)
            && matches!(
                event["type"].as_str(),
                Some("codex.thread.goal.updated") | Some("thread/goal/updated")
            )
    })
    .await;

    let goal_get = send_local_request(
        &mut ws,
        session,
        "goal-get-controls",
        "thread/goal/get",
        json!({ "threadId": thread_id }),
    )
    .await;
    assert_eq!(
        goal_get["payload"]["data"]["result"]["goal"]["status"],
        "active"
    );

    let goal_clear = send_local_request(
        &mut ws,
        session,
        "goal-clear-controls",
        "thread/goal/clear",
        json!({ "threadId": thread_id }),
    )
    .await;
    assert_eq!(goal_clear["payload"]["data"]["result"]["cleared"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TODEX_REAL_E2E=1, real codex binary, Codex login, and model access"]
async fn real_codex_parallel_sessions_and_busy_rejection() {
    require_real_e2e();
    let daemon = spawn_daemon().await;
    let mut ws = connect_ws(daemon.port, Some(TOKEN)).await;
    let sessions = ["cdxs_parallel_a", "cdxs_parallel_b", "cdxs_parallel_c"];

    for session in sessions {
        start_session(&mut ws, session, &daemon.workspace_root).await;
    }

    for session in sessions {
        let thread_id = start_thread(&mut ws, session, &format!("thread-{session}")).await;
        send_json(
            &mut ws,
            json!({
                "id": format!("turn-{session}"),
                "type": "codex.local.turn",
                "payload": {
                    "codexSessionId": session,
                    "tenantId": "local",
                    "threadId": thread_id,
                    "input": [{ "type": "text", "text": format!("回复当前 session id: {session}") }]
                }
            }),
        )
        .await;
    }

    wait_for_completed_sessions(&mut ws, &sessions).await;

    let busy_session = sessions[0];
    let busy_thread = start_thread(&mut ws, busy_session, "thread-busy").await;
    send_json(
        &mut ws,
        json!({
            "id": "busy-turn-1",
            "type": "codex.local.turn",
            "payload": {
                "codexSessionId": busy_session,
                "tenantId": "local",
                "threadId": busy_thread,
                "input": [{ "type": "text", "text": "等待几秒再回复，保持这个 turn 运行中。" }]
            }
        }),
    )
    .await;
    send_json(
        &mut ws,
        json!({
            "id": "busy-turn-2",
            "type": "codex.local.turn",
            "payload": {
                "codexSessionId": busy_session,
                "tenantId": "local",
                "threadId": busy_thread,
                "input": [{ "type": "text", "text": "这个请求应该在并发时被拒绝。" }]
            }
        }),
    )
    .await;
    wait_for_event(&mut ws, |event| {
        event["type"] == "codex.control.request.rejected"
            && event["payload"]["data"]["error"]["code"] == "SESSION_BUSY"
    })
    .await;

    for session in sessions {
        stop_session(&mut ws, session).await;
    }
}

fn require_real_e2e() {
    assert_eq!(
        env::var("TODEX_REAL_E2E").as_deref(),
        Ok("1"),
        "set TODEX_REAL_E2E=1 to run real Codex E2E tests"
    );
    let codex = codex_binary();
    let version = Command::new(&codex)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| {
            panic!("failed to run codex binary '{}': {error}", codex.display())
        });
    assert!(
        version.status.success(),
        "codex --version failed for '{}': {}",
        codex.display(),
        String::from_utf8_lossy(&version.stderr)
    );
}

async fn spawn_daemon() -> Daemon {
    let root = unique_tmp_dir("todex-real-e2e");
    let data_dir = root.join("data");
    let codex_home = prepare_codex_home(&root);
    let workspace_root = real_workspace_root().unwrap_or_else(|| root.join("workspace"));
    fs::create_dir_all(&data_dir).expect("create data dir");
    fs::create_dir_all(&workspace_root).expect("create workspace dir");
    let port = free_port();
    let bin = env!("CARGO_BIN_EXE_todex-agentd");
    let child = Command::new(bin)
        .arg("serve")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--workspace-root")
        .arg(&workspace_root)
        .env("TODEX_AGENTD_AUTH_TOKEN", TOKEN)
        .env("TODEX_AGENTD_CODEX_BIN", codex_binary())
        .env("CODEX_HOME", &codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn todex-agentd");

    let daemon = Daemon {
        child,
        port,
        root,
        data_dir,
        workspace_root,
    };
    wait_for_health(daemon.port).await;
    daemon
}

async fn start_session(ws: &mut Ws, session: &str, cwd: impl AsRef<Path>) {
    send_json(
        ws,
        json!({
            "id": format!("start-{session}"),
            "type": "codex.local.start",
            "payload": {
                "codexSessionId": session,
                "tenantId": "local",
                "cwd": cwd.as_ref().display().to_string(),
                "approvalPolicy": "on-request",
                "sandboxMode": "workspace-write",
                "configOverrides": {}
            }
        }),
    )
    .await;
    wait_for_event(ws, |event| {
        event["payload"]["codex_session_id"] == session && event["type"] == "codex.control.ready"
    })
    .await;
}

async fn stop_session(ws: &mut Ws, session: &str) {
    send_json(
        ws,
        json!({
            "id": format!("stop-{session}"),
            "type": "codex.local.stop",
            "payload": { "codexSessionId": session, "tenantId": "local", "force": true }
        }),
    )
    .await;
    wait_for_event(ws, |event| {
        event["payload"]["codex_session_id"] == session && event["type"] == "codex.control.stopped"
    })
    .await;
}

async fn assert_status(ws: &mut Ws, session: &str, expected: &str) {
    send_json(
        ws,
        json!({
            "id": format!("status-{session}"),
            "type": "codex.local.status",
            "payload": { "codexSessionId": session, "tenantId": "local" }
        }),
    )
    .await;
    let event = wait_for_event(ws, |event| {
        event["payload"]["codex_session_id"] == session && event["type"] == "codex.control.status"
    })
    .await;
    assert_eq!(event["payload"]["data"]["lifecycleState"], expected);
}

async fn start_thread(ws: &mut Ws, session: &str, request_id: &str) -> String {
    start_thread_with_ephemeral(ws, session, request_id, true).await
}

async fn start_thread_with_ephemeral(
    ws: &mut Ws,
    session: &str,
    request_id: &str,
    ephemeral: bool,
) -> String {
    send_json(
        ws,
        json!({
            "id": request_id,
            "type": "codex.local.request",
            "payload": {
                "codexSessionId": session,
                "tenantId": "local",
                "method": "thread/start",
                "params": { "ephemeral": ephemeral }
            }
        }),
    )
    .await;
    let response = wait_for_event(ws, |event| {
        event["payload"]["codex_session_id"] == session
            && event["type"] == "codex.control.response"
            && event["payload"]["data"]["requestId"] == request_id
    })
    .await;
    data_result_string(&response, &["thread", "id"])
        .or_else(|| data_result_string(&response, &["id"]))
        .expect("thread/start response should include thread id")
}

async fn send_local_request(
    ws: &mut Ws,
    session: &str,
    request_id: &str,
    method: &str,
    params: Value,
) -> Value {
    send_json(
        ws,
        json!({
            "id": request_id,
            "type": "codex.local.request",
            "payload": {
                "codexSessionId": session,
                "tenantId": "local",
                "method": method,
                "params": params
            }
        }),
    )
    .await;
    let response = wait_for_event(ws, |event| {
        event["payload"]["codex_session_id"] == session
            && event["payload"]["data"]["requestId"] == request_id
            && (event["type"] == "codex.control.response" || event["type"] == "codex.control.error")
    })
    .await;
    assert_eq!(
        response["type"], "codex.control.response",
        "{method} returned error: {}",
        response
    );
    response
}

async fn connect_ws(port: u16, token: Option<&str>) -> Ws {
    let mut request = format!("ws://127.0.0.1:{port}/v1/ws")
        .into_client_request()
        .expect("build websocket request");
    if let Some(token) = token {
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}")
                .parse()
                .expect("valid auth header"),
        );
    }
    let (ws, _) = connect_async(request).await.expect("connect websocket");
    ws
}

async fn send_json(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .expect("send websocket json");
}

async fn wait_for_event(ws: &mut Ws, predicate: impl FnMut(&Value) -> bool) -> Value {
    wait_for_event_maybe(ws, Duration::from_secs(90), predicate)
        .await
        .expect("timed out waiting for websocket event")
}

async fn wait_for_event_maybe(
    ws: &mut Ws,
    duration: Duration,
    mut predicate: impl FnMut(&Value) -> bool,
) -> Result<Value, String> {
    let mut seen = Vec::new();
    timeout(duration, async {
        while let Some(message) = ws.next().await {
            let message = message.map_err(|error| error.to_string())?;
            if let Message::Text(text) = message {
                let value: Value =
                    serde_json::from_str(&text).map_err(|error| error.to_string())?;
                seen.push(value.clone());
                if predicate(&value) {
                    return Ok(value);
                }
            }
        }
        Err("websocket closed".to_owned())
    })
    .await
    .map_err(|_| format!("timeout; recent events: {}", recent_events(&seen)))?
}

async fn wait_for_health(port: u16) {
    timeout(Duration::from_secs(15), async {
        loop {
            if http_get(port, "/health").await.0 == 200 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("daemon health check timed out");
}

async fn wait_for_completed_sessions(ws: &mut Ws, sessions: &[&str]) {
    let expected = sessions.iter().copied().collect::<BTreeSet<_>>();
    let mut completed = BTreeSet::new();
    wait_for_event(ws, |event| {
        if event["type"] == "codex.turn.completed" {
            if let Some(session) = event["payload"]["codex_session_id"].as_str() {
                if expected.contains(session) {
                    completed.insert(session.to_owned());
                }
            }
        }
        completed.len() == expected.len()
    })
    .await;
}

async fn http_get(port: u16, path: &str) -> (u16, String) {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await else {
        return (0, String::new());
    };
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write http request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read http response");
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, response)
}

fn codex_binary() -> PathBuf {
    env::var_os("TODEX_REAL_CODEX_BIN")
        .or_else(|| env::var_os("TODEX_AGENTD_CODEX_BIN"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn real_codex_model() -> String {
    env::var("TODEX_REAL_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.5".to_owned())
}

fn real_workspace_root() -> Option<PathBuf> {
    env::var_os("TODEX_REAL_WORKSPACE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn prepare_codex_home(root: &Path) -> PathBuf {
    if let Some(path) = env::var_os("TODEX_REAL_CODEX_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    let source = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .expect("CODEX_HOME or HOME should be set");
    let target = root.join("codex-home");
    fs::create_dir_all(&target).expect("create isolated CODEX_HOME");

    copy_if_exists(&source.join("auth.json"), &target.join("auth.json"));
    copy_if_exists(
        &source.join("installation_id"),
        &target.join("installation_id"),
    );

    let source_config = source.join("config.toml");
    if source_config.exists() {
        let raw = fs::read_to_string(&source_config).expect("read Codex config.toml");
        let mut value = raw
            .parse::<toml::Value>()
            .expect("parse Codex config.toml as TOML");
        if let Some(table) = value.as_table_mut() {
            table.remove("mcp_servers");
            table.remove("marketplaces");
        }
        fs::write(
            target.join("config.toml"),
            toml::to_string_pretty(&value).expect("serialize isolated Codex config.toml"),
        )
        .expect("write isolated Codex config.toml");
    }

    target
}

fn copy_if_exists(source: &Path, target: &Path) {
    if source.exists() {
        fs::copy(source, target)
            .unwrap_or_else(|error| panic!("copy '{}' failed: {error}", source.display()));
    }
}

fn free_port() -> u16 {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind port 0")
        .local_addr()
        .expect("local addr")
        .port()
}

fn unique_tmp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let path = env::temp_dir().join(format!("{prefix}-{nanos}-{}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

fn data_code(event: &Value) -> Option<&str> {
    event["payload"]["data"]["code"]
        .as_str()
        .or_else(|| event["payload"]["data"]["error"]["code"].as_str())
        .or_else(|| event["payload"]["code"].as_str())
}

fn data_string(event: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| event["payload"]["data"][*key].as_str())
        .map(ToOwned::to_owned)
}

fn data_result_string(event: &Value, path: &[&str]) -> Option<String> {
    let mut value = &event["payload"]["data"]["result"];
    for key in path {
        value = &value[*key];
    }
    value.as_str().map(ToOwned::to_owned)
}

fn response_type_for_server_request(event_type: &str) -> &'static str {
    match event_type {
        "codex.approval.commandExecution.request" => "codex.approval.commandExecution.respond",
        "codex.approval.fileChange.request" => "codex.approval.fileChange.respond",
        "codex.approval.permissions.request" => "codex.approval.permissions.respond",
        "codex.tool.requestUserInput.request" => "codex.tool.requestUserInput.respond",
        "codex.mcp.elicitation.request" => "codex.mcp.elicitation.respond",
        _ => "codex.approval.commandExecution.respond",
    }
}

fn is_session_turn_completion_or_approval_request(event: &Value, session: &str) -> bool {
    let Some(event_type) = event["type"].as_str() else {
        return false;
    };
    event["payload"]["codex_session_id"].as_str() == Some(session)
        && (event_type == "codex.turn.completed"
            || event_type == "codex.item.completed"
            || (event_type.starts_with("codex.approval.") && event_type.ends_with(".request")))
}

fn recent_events(events: &[Value]) -> String {
    events
        .iter()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|event| {
            json!({
                "type": event["type"],
                "payload": event["payload"],
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
