use std::collections::HashMap;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::{
    async_rw::AsyncRwTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    StreamableHttpClientTransport, Transport,
};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::io::{AsyncRead, BufReader, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use crate::catalog::{McpRuntimeTarget, McpToolDescriptor, McpTransport};
use crate::error::AppError;
use crate::provider::process::{read_bounded_line, redact_sensitive_text, BoundedLine};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest stderr line of a user MCP server written to the daemon log.
const STDERR_LINE_BYTES: usize = 2048;
/// Stderr lines logged per server process; the rest is drained unlogged.
const STDERR_MAX_LINES: usize = 200;
/// A tool result is journaled whole (`mcp.completed`); keep it well under
/// the per-event limit so the event around it always fits.
const MAX_RESULT_BYTES: usize = crate::conversation::MAX_EVENT_PAYLOAD_BYTES / 2;
/// Room kept for the truncation note itself.
const RESULT_NOTE_BYTES: usize = 1024;
/// Longest JSON-RPC line read from a stdio server. Results are bounded to
/// [`MAX_RESULT_BYTES`] only after they are read; this caps the memory one
/// message can take before that.
const MAX_STDIO_LINE_BYTES: usize = 16 * 1024 * 1024;
/// How long a closed stdio server may take to exit before it is killed.
const STDIO_EXIT_WAIT: Duration = Duration::from_secs(3);
/// Tools and description length kept from one server's `tools/list`.
const MAX_LISTED_TOOLS: usize = 512;
const MAX_TOOL_DESCRIPTION_CHARS: usize = 4096;

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
    let oversized = transport.oversized.clone();
    let mut client = timeout(INITIALIZE_TIMEOUT, ClientInfo::default().serve(transport))
        .await
        .map_err(|_| mcp_timeout(target, "initialize"))?
        .map_err(|error| stdio_error(target, "initialize", error, &oversized))?;
    let result = timeout(INITIALIZE_TIMEOUT, client.list_tools(None))
        .await
        .map_err(|_| mcp_timeout(target, "tools/list"))?
        .map_err(|error| stdio_error(target, "tools/list", error, &oversized));
    close_client(&mut client).await;
    Ok(tool_descriptors(target, result?.tools))
}

async fn stdio_call(
    target: &McpRuntimeTarget,
    tool_name: &str,
    arguments: Value,
) -> Result<McpCallResult, AppError> {
    let arguments = call_arguments(arguments)?;
    let transport = stdio_transport(target)?;
    let oversized = transport.oversized.clone();
    let mut client = timeout(INITIALIZE_TIMEOUT, ClientInfo::default().serve(transport))
        .await
        .map_err(|_| mcp_timeout(target, "initialize"))?
        .map_err(|error| stdio_error(target, "initialize", error, &oversized))?;
    let result = timeout(
        CALL_TIMEOUT,
        client
            .call_tool(CallToolRequestParams::new(tool_name.to_owned()).with_arguments(arguments)),
    )
    .await
    .map_err(|_| mcp_timeout(target, "tools/call"))?
    .map_err(|error| stdio_error(target, "tools/call", error, &oversized));
    close_client(&mut client).await;
    convert_call_result(result?)
}

/// A stdio MCP server process: stdin/stdout carry JSON-RPC lines of at
/// most [`MAX_STDIO_LINE_BYTES`]; the process is killed when dropped or
/// when it does not exit soon after the transport closes.
struct StdioServer {
    child: Child,
    transport: AsyncRwTransport<RoleClient, LineCapped<ChildStdout>, ChildStdin>,
    /// Set when the server sent a line over the cap (the transport ended).
    oversized: Arc<AtomicBool>,
}

impl Transport<RoleClient> for StdioServer {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.transport.close().await?;
        match timeout(STDIO_EXIT_WAIT, self.child.wait()).await {
            Ok(status) => status.map(drop),
            Err(_) => self.child.kill().await,
        }
    }
}

/// Fails the read once a line (bytes since the last `\n`) exceeds `limit`,
/// so a server cannot make the reader buffer an unbounded message.
struct LineCapped<R> {
    inner: R,
    line: usize,
    limit: usize,
    exceeded: Arc<AtomicBool>,
}

impl<R: AsyncRead + Unpin> AsyncRead for LineCapped<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.exceeded.load(Ordering::Relaxed) {
            return Poll::Ready(Err(line_too_long(this.limit)));
        }
        let before = buf.filled().len();
        std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        let mut line = this.line;
        for byte in &buf.filled()[before..] {
            if *byte == b'\n' {
                line = 0;
            } else {
                line += 1;
                if line > this.limit {
                    // A failed read hands out no bytes.
                    buf.set_filled(before);
                    this.exceeded.store(true, Ordering::Relaxed);
                    return Poll::Ready(Err(line_too_long(this.limit)));
                }
            }
        }
        this.line = line;
        Poll::Ready(Ok(()))
    }
}

fn line_too_long(limit: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("mcp server sent a message over {limit} bytes"),
    )
}

/// An rmcp error from a stdio server, naming the size cap when that is
/// what ended the connection.
fn stdio_error(
    target: &McpRuntimeTarget,
    operation: &str,
    error: impl std::fmt::Display,
    oversized: &AtomicBool,
) -> AppError {
    if oversized.load(Ordering::Relaxed) {
        mcp_error(
            target,
            operation,
            format!("the server sent a message over {MAX_STDIO_LINE_BYTES} bytes"),
        )
    } else {
        mcp_error(target, operation, error)
    }
}

fn stdio_transport(target: &McpRuntimeTarget) -> Result<StdioServer, AppError> {
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
    // stderr is captured rather than inherited: user servers print tokens
    // and request dumps there, which must not reach the daemon log verbatim.
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        AppError::InvalidRequest(format!(
            "failed to start mcp server {}: {error}",
            target.descriptor.name
        ))
    })?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(AppError::InvalidRequest(format!(
            "mcp server {} has no stdio pipes",
            target.descriptor.name
        )));
    };
    let oversized = Arc::new(AtomicBool::new(false));
    let stdout = LineCapped {
        inner: stdout,
        line: 0,
        limit: MAX_STDIO_LINE_BYTES,
        exceeded: oversized.clone(),
    };
    if let Some(stderr) = child.stderr.take() {
        let server = target.descriptor.name.clone();
        tokio::spawn(async move {
            forward_stderr(stderr, |line| {
                tracing::info!(mcp_server = %server, "mcp server stderr: {line}");
            })
            .await;
        });
    }
    Ok(StdioServer {
        child,
        transport: AsyncRwTransport::new_client(stdout, stdin),
        oversized,
    })
}

/// Passes each stderr line, redacted and bounded, to `emit` until the pipe
/// closes. Lines past [`STDERR_MAX_LINES`] are still read so the server never
/// blocks on a full pipe.
async fn forward_stderr(stderr: impl AsyncRead + Unpin, mut emit: impl FnMut(String)) {
    let mut reader = BufReader::new(stderr);
    let (mut buffer, mut discarding) = (Vec::new(), None);
    let mut lines = 0_usize;
    loop {
        let line =
            match read_bounded_line(&mut reader, &mut buffer, &mut discarding, STDERR_LINE_BYTES)
                .await
            {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(%error, "failed to read mcp server stderr");
                    break;
                }
            };
        let text = match line {
            BoundedLine::Line(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.trim_end();
                if text.is_empty() {
                    continue;
                }
                redact_sensitive_text(text)
            }
            BoundedLine::Oversized(bytes) => format!("[{bytes}-byte line omitted]"),
        };
        lines += 1;
        if lines < STDERR_MAX_LINES {
            emit(text);
        } else if lines == STDERR_MAX_LINES {
            emit("[further stderr output omitted]".to_owned());
        }
    }
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
    Ok(tool_descriptors(target, result?.tools))
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

/// At most [`MAX_LISTED_TOOLS`] tools, descriptions cut at
/// [`MAX_TOOL_DESCRIPTION_CHARS`] (marked with `…`).
fn tool_descriptors(
    target: &McpRuntimeTarget,
    tools: Vec<rmcp::model::Tool>,
) -> Vec<McpToolDescriptor> {
    if tools.len() > MAX_LISTED_TOOLS {
        tracing::warn!(
            mcp_server = %target.descriptor.name,
            tools = tools.len(),
            "mcp server lists more tools than TodeX shows; the rest are omitted"
        );
    }
    tools
        .into_iter()
        .take(MAX_LISTED_TOOLS)
        .map(|tool| McpToolDescriptor {
            name: tool.name.into_owned(),
            description: tool.description.map(|value| {
                if value.chars().count() > MAX_TOOL_DESCRIPTION_CHARS {
                    let mut cut: String = value.chars().take(MAX_TOOL_DESCRIPTION_CHARS).collect();
                    cut.push('…');
                    cut
                } else {
                    value.into_owned()
                }
            }),
        })
        .collect()
}

fn convert_call_result(result: rmcp::model::CallToolResult) -> Result<McpCallResult, AppError> {
    let is_error = result.is_error.unwrap_or(false);
    Ok(McpCallResult {
        content: bound_result(serde_json::to_value(result)?, MAX_RESULT_BYTES),
        is_error,
    })
}

/// Bytes `character` takes inside a serde_json string.
fn escaped_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        character if (character as u32) < 0x20 => 6,
        character => character.len_utf8(),
    }
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// A serialized `CallToolResult` of at most `limit` bytes. Over the limit,
/// content blocks are kept in order while they fit, the first text block
/// that does not is cut, other blocks that do not are replaced by a note,
/// every top-level field but `isError` and `resultType` is dropped
/// (`structuredContent`, `_meta`, unknown fields), and the result says so
/// (`truncated` and a closing text block).
fn bound_result(result: Value, limit: usize) -> Value {
    let total = json_len(&result);
    if total <= limit {
        return result;
    }
    let Value::Object(mut result) = result else {
        return serde_json::json!({ "content": [], "truncated": true });
    };
    let blocks = match result.remove("content") {
        Some(Value::Array(blocks)) => blocks,
        _ => Vec::new(),
    };
    // Only the small, known fields survive: `_meta`, `structuredContent`
    // and anything unknown could be as large as the rest.
    let mut object = serde_json::Map::new();
    for key in ["isError", "resultType"] {
        if let Some(value) = result.remove(key).filter(|value| json_len(value) <= 64) {
            object.insert(key.to_owned(), value);
        }
    }
    let mut budget = limit
        .saturating_sub(RESULT_NOTE_BYTES)
        .saturating_sub(json_len(&Value::Object(object.clone())));
    let mut kept = Vec::new();
    for block in blocks {
        let size = json_len(&block).saturating_add(1);
        if size <= budget {
            budget -= size;
            kept.push(block);
            continue;
        }
        // This block is the last one kept (cut or noted): no budget after.
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("content")
            .to_owned();
        if kind == "text" {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // JSON escaping can grow text up to 6x (`\u0000`); cut by the
            // escaped size so the block is guaranteed to fit.
            let mut cut = String::new();
            let mut used = 32;
            for character in text.chars() {
                let escaped = escaped_len(character);
                if used + escaped > budget {
                    break;
                }
                used += escaped;
                cut.push(character);
            }
            if !cut.is_empty() {
                kept.push(serde_json::json!({ "type": "text", "text": cut }));
            }
        } else {
            let note = serde_json::json!({
                "type": "text",
                "text": format!("[{kind} block of {size} bytes omitted]"),
            });
            let note_size = json_len(&note) + 1;
            if note_size <= budget {
                kept.push(note);
            }
        }
        break;
    }
    kept.push(serde_json::json!({
        "type": "text",
        "text": format!(
            "[TodeX truncated this MCP result: it was {total} bytes, the limit is {limit}]"
        ),
    }));
    object.insert("content".to_owned(), Value::Array(kept));
    object.insert("truncated".to_owned(), Value::Bool(true));
    Value::Object(object)
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

    #[test]
    fn oversized_results_are_cut_to_the_limit_and_say_so() {
        let small =
            serde_json::json!({ "content": [{ "type": "text", "text": "ok" }], "isError": false });
        assert_eq!(bound_result(small.clone(), 4096), small);
        for character in ['a', '"', '\\', '\n', '\u{1}', '\u{7f}', 'é', '你', '😀'] {
            assert_eq!(
                escaped_len(character),
                json_len(&Value::String(character.to_string())) - 2,
                "{character:?}"
            );
        }

        let big = serde_json::json!({
            "content": [
                { "type": "text", "text": "head" },
                { "type": "text", "text": "\u{0}".repeat(10_000) },
                { "type": "image", "data": "A".repeat(10_000), "mimeType": "image/png" }
            ],
            "structuredContent": { "rows": "x".repeat(10_000) },
            "isError": true
        });
        let limit = 4096;
        let bounded = bound_result(big, limit);
        assert!(json_len(&bounded) <= limit, "{}", json_len(&bounded));
        assert_eq!(bounded["truncated"], true);
        assert_eq!(bounded["isError"], true);
        assert!(bounded.get("structuredContent").is_none());
        let content = bounded["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "head");
        assert!(content[1]["text"].as_str().unwrap().starts_with('\u{0}'));
        assert!(content.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("TodeX truncated this MCP result"));

        // `_meta` and unknown fields cannot carry the size past the limit.
        let padded = serde_json::json!({
            "content": [{ "type": "text", "text": "ok" }],
            "_meta": { "blob": "m".repeat(10_000) },
            "extra": "e".repeat(10_000),
            "resultType": "complete",
            "isError": false
        });
        let bounded = bound_result(padded, limit);
        assert!(json_len(&bounded) <= limit, "{}", json_len(&bounded));
        assert!(bounded.get("_meta").is_none());
        assert!(bounded.get("extra").is_none());
        assert_eq!(bounded["resultType"], "complete");
        assert_eq!(bounded["isError"], false);
        assert_eq!(bounded["content"][0]["text"], "ok");

        // A huge non-text block becomes a note.
        let image = serde_json::json!({
            "content": [{ "type": "image", "data": "A".repeat(10_000), "mimeType": "image/png" }]
        });
        let bounded = bound_result(image, limit);
        assert!(bounded["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("image block"));
        assert!(json_len(&bounded) <= limit);
    }
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
    async fn stdio_lines_over_the_cap_end_the_read() {
        use tokio::io::AsyncReadExt;
        let read = |input: &'static [u8], limit: usize| async move {
            let exceeded = Arc::new(AtomicBool::new(false));
            let mut capped = LineCapped {
                inner: input,
                line: 0,
                limit,
                exceeded: exceeded.clone(),
            };
            let mut out = Vec::new();
            let result = capped.read_to_end(&mut out).await;
            (result.is_ok(), exceeded.load(Ordering::Relaxed))
        };
        assert_eq!(read(b"1234\n12345\n123", 5).await, (true, false));
        assert_eq!(read(b"123456\n", 5).await, (false, true));
        assert_eq!(read(b"12\n1234567", 5).await, (false, true));
        assert_eq!(read(b"\n\n\n", 0).await, (true, false));
    }

    #[tokio::test]
    async fn stderr_is_redacted_bounded_and_capped() {
        let long = "x".repeat(STDERR_LINE_BYTES + 1);
        let mut input = format!("starting\n\nkey=sk-ant-abcdefghijklmnop0123\n{long}\n");
        for index in 0..STDERR_MAX_LINES {
            input.push_str(&format!("line {index}\n"));
        }
        let mut lines = Vec::new();
        forward_stderr(input.as_bytes(), |line| lines.push(line)).await;
        assert_eq!(lines[0], "starting");
        assert_eq!(lines[1], "key=[REDACTED]");
        assert_eq!(lines[2], format!("[{}-byte line omitted]", long.len()));
        assert_eq!(lines.len(), STDERR_MAX_LINES);
        assert_eq!(lines.last().unwrap(), "[further stderr output omitted]");
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
