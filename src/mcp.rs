use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::{
    streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    TokioChildProcess,
};
use rmcp::ServiceExt;
use serde_json::Value;
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
    let transport = stdio_transport(target)?;
    let mut client = timeout(INITIALIZE_TIMEOUT, ClientInfo::default().serve(transport))
        .await
        .map_err(|_| mcp_timeout(target, "initialize"))?
        .map_err(|error| mcp_error(target, "initialize", error))?;
    let result = timeout(INITIALIZE_TIMEOUT, client.list_tools(None))
        .await
        .map_err(|_| mcp_timeout(target, "tools/list"))?
        .map_err(|error| mcp_error(target, "tools/list", error));
    close_client(&mut client).await;
    Ok(result?
        .tools
        .into_iter()
        .map(|tool| McpToolDescriptor {
            name: tool.name.into_owned(),
            description: tool.description.map(|value| value.into_owned()),
        })
        .collect())
}

async fn stdio_call(
    target: &McpRuntimeTarget,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallResult, AppError> {
    let arguments = call_arguments(arguments)?;
    let transport = stdio_transport(target)?;
    let mut client = timeout(INITIALIZE_TIMEOUT, ClientInfo::default().serve(transport))
        .await
        .map_err(|_| mcp_timeout(target, "initialize"))?
        .map_err(|error| mcp_error(target, "initialize", error))?;
    let result = timeout(
        CALL_TIMEOUT,
        client
            .call_tool(CallToolRequestParams::new(tool_name.to_owned()).with_arguments(arguments)),
    )
    .await
    .map_err(|_| mcp_timeout(target, "tools/call"))?
    .map_err(|error| mcp_error(target, "tools/call", error));
    close_client(&mut client).await;
    convert_call_result(result?)
}

fn stdio_transport(target: &McpRuntimeTarget) -> Result<TokioChildProcess, AppError> {
    let Some(program) = target.command.first() else {
        return Err(AppError::InvalidRequest(format!(
            "mcp server {} is missing a command",
            target.descriptor.name
        )));
    };
    let mut command = Command::new(program);
    command.args(&target.command[1..]);
    command.current_dir(&target.workspace).env_clear();
    inherit_base_env(&mut command);
    for (key, value) in &target.env {
        if key.starts_with("TODEX_AGENTD_") {
            return Err(AppError::InvalidRequest(
                "mcp env cannot include TODEX_AGENTD_ variables".to_owned(),
            ));
        }
        command.env(key, value);
    }
    TokioChildProcess::builder(command)
        .stderr(Stdio::inherit())
        .spawn()
        .map(|(transport, _)| transport)
        .map_err(|error| {
            AppError::InvalidRequest(format!(
                "failed to start mcp server {}: {error}",
                target.descriptor.name
            ))
        })
}

fn inherit_base_env(command: &mut Command) {
    for key in [
        "PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR", "TMP", "TEMP",
    ] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
}

async fn http_tools(target: &McpRuntimeTarget) -> Result<Vec<McpToolDescriptor>, AppError> {
    let transport = StreamableHttpClientTransport::from_config(http_config(target)?);
    let mut client = timeout(INITIALIZE_TIMEOUT, ClientInfo::default().serve(transport))
        .await
        .map_err(|_| mcp_timeout(target, "initialize"))?
        .map_err(|error| mcp_error(target, "initialize", error))?;
    let result = timeout(INITIALIZE_TIMEOUT, client.list_tools(None))
        .await
        .map_err(|_| mcp_timeout(target, "tools/list"))?
        .map_err(|error| mcp_error(target, "tools/list", error));
    close_client(&mut client).await;
    Ok(result?
        .tools
        .into_iter()
        .map(|tool| McpToolDescriptor {
            name: tool.name.into_owned(),
            description: tool.description.map(|value| value.into_owned()),
        })
        .collect())
}

async fn http_call(
    target: &McpRuntimeTarget,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallResult, AppError> {
    let arguments = call_arguments(arguments)?;
    let transport = StreamableHttpClientTransport::from_config(http_config(target)?);
    let mut client = timeout(INITIALIZE_TIMEOUT, ClientInfo::default().serve(transport))
        .await
        .map_err(|_| mcp_timeout(target, "initialize"))?
        .map_err(|error| mcp_error(target, "initialize", error))?;
    let result = timeout(
        CALL_TIMEOUT,
        client
            .call_tool(CallToolRequestParams::new(tool_name.to_owned()).with_arguments(arguments)),
    )
    .await
    .map_err(|_| mcp_timeout(target, "tools/call"))?
    .map_err(|error| mcp_error(target, "tools/call", error));
    close_client(&mut client).await;
    convert_call_result(result?)
}

fn http_config(target: &McpRuntimeTarget) -> Result<StreamableHttpClientTransportConfig, AppError> {
    let url = target.url.clone().ok_or_else(|| {
        AppError::InvalidRequest(format!(
            "mcp server {} is missing a URL",
            target.descriptor.name
        ))
    })?;
    let headers = target
        .headers
        .iter()
        .map(|(name, value)| {
            let name = axum::http::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP header name {name:?}: {error}"))
            })?;
            let value = axum::http::HeaderValue::from_str(value).map_err(|error| {
                AppError::InvalidRequest(format!("invalid MCP header value for {name}: {error}"))
            })?;
            Ok((name, value))
        })
        .collect::<Result<HashMap<_, _>, AppError>>()?;
    let config = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(headers)
        .reinit_on_expired_session(true);
    Ok(config)
}

fn call_arguments(arguments: Value) -> Result<serde_json::Map<String, Value>, AppError> {
    match arguments {
        Value::Object(arguments) => Ok(arguments),
        Value::Null => Ok(serde_json::Map::new()),
        _ => Err(AppError::InvalidRequest(
            "mcp tool arguments must be a JSON object".to_owned(),
        )),
    }
}

fn convert_call_result(result: rmcp::model::CallToolResult) -> Result<McpCallResult, AppError> {
    let is_error = result.is_error.unwrap_or(false);
    Ok(McpCallResult {
        content: serde_json::to_value(result)?,
        is_error,
    })
}

async fn close_client<T>(client: &mut rmcp::service::RunningService<rmcp::RoleClient, T>)
where
    T: rmcp::Service<rmcp::RoleClient>,
{
    if let Err(error) = client.close_with_timeout(Duration::from_secs(3)).await {
        tracing::warn!(%error, "failed to close MCP client");
    }
}

fn mcp_timeout(target: &McpRuntimeTarget, operation: &str) -> AppError {
    AppError::InvalidRequest(format!(
        "mcp server {} {operation} timed out",
        target.descriptor.name
    ))
}

fn mcp_error(
    target: &McpRuntimeTarget,
    operation: &str,
    error: impl std::fmt::Display,
) -> AppError {
    AppError::InvalidRequest(format!(
        "mcp server {} {operation} failed: {error}",
        target.descriptor.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
    line = sys.stdin.buffer.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

def write_msg(obj):
    sys.stdout.buffer.write(json.dumps(obj).encode() + b"\n")
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    method = msg.get("method")
    ident = msg.get("id")
    if method == "initialize":
        write_msg({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        write_msg({"jsonrpc": "2.0", "id": ident, "result": {"tools": [{"name": "echo", "description": "echo args", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        args = (msg.get("params") or {}).get("arguments") or {}
        write_msg({"jsonrpc": "2.0", "id": ident, "result": {"content": [{"type": "text", "text": json.dumps(args)}], "isError": False}})
"#,
        )
        .unwrap();
        path
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
        let result = call_tool(&target, "echo", json!({ "ping": "pong" }))
            .await
            .expect("call tool");
        assert!(!result.is_error);
        assert!(result.content.to_string().contains("pong"));
    }

    #[tokio::test]
    async fn http_lists_and_calls_tools() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let request = String::from_utf8_lossy(&buf);
                    if request.contains("notifications/initialized") {
                        let _ = stream
                            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let result = if request.contains("tools/list") {
                        json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
                    } else if request.contains("tools/call") {
                        json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"http-ok"}],"isError":false}})
                    } else {
                        json!({"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}})
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
        let result = call_tool(&target, "echo", json!({}))
            .await
            .expect("http call");
        assert!(result.content.to_string().contains("http-ok"));
    }
}
