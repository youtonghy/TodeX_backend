use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use crate::catalog::{McpRuntimeTarget, McpToolDescriptor, McpTransport};
use crate::error::AppError;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct McpCallResult {
    pub content: Value,
    pub is_error: bool,
}

pub async fn list_tools(target: &McpRuntimeTarget) -> Result<Vec<McpToolDescriptor>, AppError> {
    match target.descriptor.transport {
        McpTransport::Stdio => stdio_tools(target).await,
        McpTransport::Http => http_tools(target).await,
        McpTransport::Unknown => Err(AppError::InvalidRequest(format!(
            "mcp server {} has unknown transport",
            target.descriptor.name
        ))),
    }
}

pub async fn call_tool(
    target: &McpRuntimeTarget,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallResult, AppError> {
    match target.descriptor.transport {
        McpTransport::Stdio => stdio_call(target, tool_name, arguments).await,
        McpTransport::Http => http_call(target, tool_name, arguments).await,
        McpTransport::Unknown => Err(AppError::InvalidRequest(format!(
            "mcp server {} has unknown transport",
            target.descriptor.name
        ))),
    }
}

async fn stdio_tools(target: &McpRuntimeTarget) -> Result<Vec<McpToolDescriptor>, AppError> {
    let mut session = StdioSession::spawn(target).await?;
    let result = session
        .request(
            "tools/list",
            json!({}),
            INITIALIZE_TIMEOUT,
        )
        .await;
    session.kill().await;
    let value = result?;
    Ok(parse_tools(value.get("tools").cloned().unwrap_or(Value::Null)))
}

async fn stdio_call(
    target: &McpRuntimeTarget,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallResult, AppError> {
    let mut session = StdioSession::spawn(target).await?;
    let result = session
        .request(
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
            CALL_TIMEOUT,
        )
        .await;
    session.kill().await;
    parse_call_result(result?)
}

struct StdioSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl StdioSession {
    async fn spawn(target: &McpRuntimeTarget) -> Result<Self, AppError> {
        if target.command.is_empty() {
            return Err(AppError::InvalidRequest(format!(
                "mcp server {} is missing a command",
                target.descriptor.name
            )));
        }
        let mut command = Command::new(&target.command[0]);
        if target.command.len() > 1 {
            command.args(&target.command[1..]);
        }
        command.current_dir(&target.workspace);
        command.env_clear();
        inherit_base_env(&mut command);
        for (key, value) in &target.env {
            if key.starts_with("TODEX_AGENTD_") {
                return Err(AppError::InvalidRequest(
                    "mcp env cannot include TODEX_AGENTD_ variables".to_owned(),
                ));
            }
            command.env(key, value);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| {
            AppError::InvalidRequest(format!(
                "failed to start mcp server {}: {error}",
                target.descriptor.name
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::InvalidRequest("mcp stdin missing".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::InvalidRequest("mcp stdout missing".to_owned()))?;
        let mut session = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        session
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "todex-agentd", "version": env!("CARGO_PKG_VERSION") }
                }),
                INITIALIZE_TIMEOUT,
            )
            .await?;
        session
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(session)
    }

    async fn request(&mut self, method: &str, params: Value, limit: Duration) -> Result<Value, AppError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        timeout(limit, self.read_result(id))
            .await
            .map_err(|_| AppError::InvalidRequest(format!("mcp {method} timed out")))?
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), AppError> {
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn write(&mut self, value: &Value) -> Result<(), AppError> {
        let body = serde_json::to_vec(value)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(&body).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_result(&mut self, expected_id: u64) -> Result<Value, AppError> {
        loop {
            let message = read_mcp_message(&mut self.stdout).await?;
            if message.get("id").and_then(Value::as_u64) == Some(expected_id)
                || message.get("id").and_then(Value::as_i64) == Some(expected_id as i64)
            {
                if let Some(error) = message.get("error") {
                    return Err(AppError::InvalidRequest(format!("mcp error: {error}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

fn inherit_base_env(command: &mut Command) {
    for key in ["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR", "TMP", "TEMP"] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
}

async fn read_mcp_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Value, AppError> {
    let mut header = String::new();
    loop {
        header.clear();
        let bytes = reader.read_line(&mut header).await?;
        if bytes == 0 {
            return Err(AppError::InvalidRequest("mcp stdout closed".to_owned()));
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            let length = rest
                .trim()
                .parse::<usize>()
                .map_err(|_| AppError::InvalidRequest("invalid MCP content length".to_owned()))?;
            while header != "\r\n" && header != "\n" {
                header.clear();
                let bytes = reader.read_line(&mut header).await?;
                if bytes == 0 {
                    return Err(AppError::InvalidRequest("mcp stdout closed".to_owned()));
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await?;
            return serde_json::from_slice(&body).map_err(Into::into);
        }
        let trimmed = header.trim();
        if trimmed.starts_with('{') {
            return serde_json::from_str(trimmed).map_err(Into::into);
        }
    }
    Err(AppError::InvalidRequest("mcp message missing content".to_owned()))
}

async fn http_tools(target: &McpRuntimeTarget) -> Result<Vec<McpToolDescriptor>, AppError> {
    let mut client = HttpSession::connect(target).await?;
    let value = client
        .request("tools/list", json!({}), INITIALIZE_TIMEOUT)
        .await?;
    Ok(parse_tools(value.get("tools").cloned().unwrap_or(Value::Null)))
}

async fn http_call(
    target: &McpRuntimeTarget,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallResult, AppError> {
    let mut client = HttpSession::connect(target).await?;
    let value = client
        .request(
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
            CALL_TIMEOUT,
        )
        .await?;
    parse_call_result(value)
}

struct HttpSession {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpSession {
    async fn connect(target: &McpRuntimeTarget) -> Result<Self, AppError> {
        let url = target
            .url
            .clone()
            .ok_or_else(|| AppError::InvalidRequest(format!("mcp server {} is missing a URL", target.descriptor.name)))?;
        let client = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|error| AppError::InvalidRequest(format!("mcp http client: {error}")))?;
        let mut session = Self {
            client,
            url,
            headers: target.headers.clone(),
            session_id: None,
            next_id: 1,
        };
        session
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "todex-agentd", "version": env!("CARGO_PKG_VERSION") }
                }),
                INITIALIZE_TIMEOUT,
            )
            .await?;
        session
            .request("notifications/initialized", json!({}), INITIALIZE_TIMEOUT)
            .await
            .ok();
        Ok(session)
    }

    async fn request(&mut self, method: &str, params: Value, limit: Duration) -> Result<Value, AppError> {
        let id = self.next_id;
        self.next_id += 1;
        let payload = if method.starts_with("notifications/") {
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
        } else {
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        };
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&payload);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = timeout(limit, request.send())
            .await
            .map_err(|_| AppError::InvalidRequest(format!("mcp {method} timed out")))?
            .map_err(classify_http_error)?;
        if let Some(session_id) = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()) {
            self.session_id = Some(session_id.to_owned());
        }
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(AppError::Unauthorized(format!(
                "mcp http authentication failed ({status})"
            )));
        }
        if !status.is_success() {
            return Err(AppError::InvalidRequest(format!(
                "mcp http error {status}"
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = response
            .text()
            .await
            .map_err(|error| AppError::InvalidRequest(format!("mcp http body: {error}")))?;
        if method.starts_with("notifications/") {
            return Ok(Value::Null);
        }
        let parsed = parse_http_body(&content_type, &body)?;
        if let Some(error) = parsed.get("error") {
            return Err(AppError::InvalidRequest(format!("mcp error: {error}")));
        }
        Ok(parsed.get("result").cloned().unwrap_or(parsed))
    }
}

fn classify_http_error(error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::InvalidRequest("mcp http timed out".to_owned())
    } else if error.is_connect() {
        AppError::InvalidRequest(format!("mcp http network error: {error}"))
    } else {
        AppError::InvalidRequest(format!("mcp http error: {error}"))
    }
}

fn parse_http_body(content_type: &str, body: &str) -> Result<Value, AppError> {
    if content_type.contains("text/event-stream") {
        for line in body.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
                return Ok(value);
            }
        }
        return Err(AppError::InvalidRequest("mcp sse response missing data".to_owned()));
    }
    serde_json::from_str(body).map_err(Into::into)
}

fn parse_tools(value: Value) -> Vec<McpToolDescriptor> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_owned();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            Some(McpToolDescriptor { name, description })
        })
        .collect()
}

fn parse_call_result(value: Value) -> Result<McpCallResult, AppError> {
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(McpCallResult {
        content: value,
        is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use crate::catalog::McpRuntimeTarget;
    use serde_json::json;

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()))
    }

    fn write_stdio_fixture() -> PathBuf {
        let dir = temp_dir("todex-mcp");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp-fixture.py");
        fs::write(
            &path,
            r#"#!/usr/bin/env python3
import json, sys

def read_msg():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise SystemExit(0)
        if line in (b"\r\n", b"\n"):
            break
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])
    body = sys.stdin.buffer.read(length or 0)
    return json.loads(body)

def write_msg(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    method = msg.get("method")
    ident = msg.get("id")
    if method == "initialize":
        write_msg({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fixture"}}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        write_msg({"jsonrpc": "2.0", "id": ident, "result": {"tools": [{"name": "echo", "description": "echo args"}]}})
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        write_msg({"jsonrpc": "2.0", "id": ident, "result": {"content": [{"type": "text", "text": json.dumps(args)}], "isError": False}})
"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn parse_tools_reads_name_and_description() {
        let tools = parse_tools(json!([
            { "name": "echo", "description": "echo args" },
            { "missing": true },
        ]));
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("echo args"));
    }

    #[test]
    fn parse_http_body_accepts_json_and_sse() {
        let json_body = parse_http_body("application/json", r#"{"result":{"ok":true}}"#).unwrap();
        assert_eq!(json_body["result"]["ok"], true);
        let sse = parse_http_body("text/event-stream", "event: message\ndata: {\"result\":{\"ok\":true}}\n\n").unwrap();
        assert_eq!(sse["result"]["ok"], true);
    }

    #[tokio::test]
    async fn stdio_lists_and_calls_tools() {
        let fixture = write_stdio_fixture();
        let target = McpRuntimeTarget::stdio_fixture(
            "echo",
            vec!["python3".to_owned(), fixture.to_string_lossy().into_owned()],
            fixture.parent().unwrap().to_path_buf(),
        );
        let tools = list_tools(&target).await.expect("list tools");
        assert_eq!(tools[0].name, "echo");
        let result = call_tool(&target, "echo", json!({ "ping": "pong" })).await.expect("call tool");
        assert!(!result.is_error);
        assert!(result.content.to_string().contains("pong"));
    }

    #[tokio::test]
    async fn http_lists_and_calls_tools() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let request = String::from_utf8_lossy(&buf);
                    let result = if request.contains("tools/list") {
                        json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"echo"}]}})
                    } else if request.contains("tools/call") {
                        json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"http-ok"}],"isError":false}})
                    } else {
                        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fixture"}}})
                    };
                    let body = result.to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let target = McpRuntimeTarget::http_fixture(
            "echo",
            format!("http://{addr}/mcp"),
            std::env::temp_dir(),
        );
        let tools = list_tools(&target).await.expect("http list tools");
        assert_eq!(tools[0].name, "echo");
        let result = call_tool(&target, "echo", json!({})).await.expect("http call");
        assert!(result.content.to_string().contains("http-ok"));
    }
}
