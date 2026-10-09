//! `todex-agentd agent-mcp-bridge`: a stdio MCP server for agents that
//! relays every JSON-RPC message, unchanged, to one of the daemon's
//! Streamable HTTP endpoints and back. Tools live only in the daemon.
//!
//! With `--route`, the bridge runs from a static global config entry
//! (Antigravity): the daemon URL, token and the routes enabled for the turn
//! come from the provider's environment. Started outside a TodeX turn, or
//! for a route the turn did not enable, it serves no tools instead of
//! failing, so the agent's own sessions are unaffected.

use rmcp::{
    transport::{
        async_rw::AsyncRwTransport, streamable_http_client::StreamableHttpClientTransportConfig,
        StreamableHttpClientTransport, Transport,
    },
    RoleServer,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
};

use super::{ENDPOINT_ENV, LEGACY_TOKEN_ENV, LEGACY_URL_ENV, ROUTES_ENV, TOKEN_ENV, URL_ENV};

/// Runs the bridge on stdin/stdout. stdout carries the protocol, so errors
/// are returned for `main` to print on stderr with a non-zero exit status.
pub(crate) async fn run_bridge(route: Option<String>) -> anyhow::Result<()> {
    if let Some(route) = route {
        let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let enabled =
            env(ROUTES_ENV).is_some_and(|routes| routes.split(',').any(|enabled| enabled == route));
        return match (env(ENDPOINT_ENV), env(TOKEN_ENV)) {
            (Some(endpoint), Some(token)) if enabled => {
                proxy(
                    &format!("{endpoint}{route}"),
                    &token,
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                )
                .await?;
                Ok(())
            }
            _ => Ok(serve_inert(BufReader::new(tokio::io::stdin()), tokio::io::stdout()).await?),
        };
    }
    let var = |name: &str, legacy: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(legacy))
            .ok()
            .filter(|value| !value.is_empty())
    };
    let (Some(url), Some(token)) = (
        var(URL_ENV, LEGACY_URL_ENV),
        var(TOKEN_ENV, LEGACY_TOKEN_ENV),
    ) else {
        anyhow::bail!(
            "{URL_ENV} and {TOKEN_ENV} must be set; TodeX starts this command for its agents"
        );
    };
    proxy(&url, &token, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BridgeError {
    #[error(
        "the TodeX agent tool endpoint {url} refused or dropped the connection ({detail}); \
         its token is reset when todex-agentd restarts, so start a new agent turn"
    )]
    Endpoint { url: String, detail: String },
    #[error("cannot write to the agent: {0}")]
    Agent(std::io::Error),
}

/// Relays messages between the agent (`reader`/`writer`) and `url` until
/// either side closes. A clean agent EOF is success.
pub(crate) async fn proxy<R, W>(
    url: &str,
    token: &str,
    reader: R,
    writer: W,
) -> Result<(), BridgeError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let mut agent = AsyncRwTransport::<RoleServer, R, W>::new_server(reader, writer);
    let mut daemon = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_owned())
            .auth_header(token.to_owned())
            .reinit_on_expired_session(true),
    );
    let endpoint_error = |detail: String| BridgeError::Endpoint {
        url: url.to_owned(),
        detail,
    };

    // Sends to the daemon complete only once the POST is answered; queue
    // them in order on a separate task so responses keep flowing meanwhile.
    let (pending_tx, mut pending_rx) = mpsc::unbounded_channel::<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>,
    >();
    let mut sender = tokio::spawn(async move {
        while let Some(send) = pending_rx.recv().await {
            send.await?;
        }
        Ok::<(), String>(())
    });

    let outcome = loop {
        tokio::select! {
            message = agent.receive() => match message {
                Some(message) => {
                    let send = daemon.send(message);
                    let send = Box::pin(async move { send.await.map_err(|error| error.to_string()) });
                    if pending_tx.send(send).is_err() {
                        break Err(endpoint_error("request queue closed".to_owned()));
                    }
                }
                // The agent closed stdin: it is done with the server.
                None => break Ok(()),
            },
            message = daemon.receive() => match message {
                Some(message) => {
                    if let Err(error) = agent.send(message).await {
                        break Err(BridgeError::Agent(error));
                    }
                }
                None => break Err(endpoint_error("connection closed".to_owned())),
            },
            result = &mut sender => {
                let detail = match result {
                    Ok(Ok(())) => "request queue closed".to_owned(),
                    Ok(Err(detail)) => detail,
                    Err(error) => error.to_string(),
                };
                break Err(endpoint_error(detail));
            }
        }
    };
    drop(pending_tx);
    sender.abort();
    if let Err(error) = daemon.close().await {
        tracing::debug!(%error, "closing the TodeX agent tool session failed");
    }
    let _ = agent.close().await;
    outcome
}

/// An MCP server without tools, for a static config entry started where it
/// has nothing to relay to. Answers the handshake and listings, rejects
/// everything else, and ends with the agent's stdin.
pub(crate) async fn serve_inert<R, W>(reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications get no answer.
        let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else {
            continue;
        };
        let reply = match message.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": message
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "todex", "version": crate::version::APP_VERSION },
                "instructions": "TodeX tools are only available in conversations TodeX runs.",
            }}),
            Some("tools/list") => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } }),
            Some("ping") => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({ "jsonrpc": "2.0", "id": id, "error": {
                "code": -32601,
                "message": "TodeX tools are only available in conversations TodeX runs",
            }}),
        };
        let mut bytes = serde_json::to_vec(&reply)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
        writer.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod inert_tests {
    use super::*;

    #[tokio::test]
    async fn the_inert_server_lists_no_tools_and_rejects_the_rest() {
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ssh_exec"}}"#,
        ]
        .join("\n");
        let mut output = Vec::new();
        serve_inert(input.as_bytes(), &mut output).await.unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 4);
        assert_eq!(replies[0]["error"]["code"], -32601);
        assert_eq!(replies[1]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(replies[2]["result"]["tools"], json!([]));
        assert_eq!(replies[3]["id"], 4);
        assert!(replies[3]["error"].is_object());
    }
}
