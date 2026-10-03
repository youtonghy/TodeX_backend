//! `todex-agentd agent-mcp-bridge`: a stdio MCP server for agents that
//! relays every JSON-RPC message, unchanged, to one of the daemon's
//! Streamable HTTP endpoints and back. Tools live only in the daemon.

use rmcp::{
    transport::{
        async_rw::AsyncRwTransport, streamable_http_client::StreamableHttpClientTransportConfig,
        StreamableHttpClientTransport, Transport,
    },
    RoleServer,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};

use super::{LEGACY_TOKEN_ENV, LEGACY_URL_ENV, TOKEN_ENV, URL_ENV};

/// Runs the bridge on stdin/stdout. stdout carries the protocol, so errors
/// are returned for `main` to print on stderr with a non-zero exit status.
pub(crate) async fn run_bridge() -> anyhow::Result<()> {
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
